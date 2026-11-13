use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, PromptCtx, ResolveCtx, RolePlugin, Roster,
    Team,
};

pub struct Seer;
pub static SEER: Seer = Seer;

#[derive(Clone, Copy)]
enum SeerChoice {
    Player(usize),
    Center(usize, usize),
    Pass,
}

impl RolePlugin for Seer {
    fn id(&self) -> &'static str {
        "seer"
    }

    fn name(&self) -> &'static str {
        "Seer"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Wake to view EITHER 1 other player's card OR 2 center cards."
    }

    fn wake_order(&self) -> Option<u16> {
        Some(150)
    }

    fn copy_behavior(&self) -> CopyBehavior {
        CopyBehavior::ActImmediately
    }

    fn prompt_seat<'a>(
        &'a self,
        dm: &'a mut ExclusiveDm,
        seat: usize,
        ctx: &'a PromptCtx<'a>,
        _upstream_board: Option<&'a BoardCards>,
    ) -> BoxFuture<'a, ircgm::Result<ErasedIntent>> {
        Box::pin(async move {
            let prompt = format!(
                "🔮 Seer: Reply `player <seat|nick>` OR `center <1-3> <1-3>`, or `pass`. Seats: {}",
                ctx.roster.format_seats_marking_shields(ctx.shielded)
            );
            let turn = dm
                .ask(prompt, move |m| {
                    parse_seer_choice(ctx.roster, ctx.shielded, seat, &m.text)
                })
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left for Seer vision!")
                .settled()
                .await?;
            Ok(Box::new(turn) as ErasedIntent)
        })
    }

    fn resolve_seat(
        &self,
        seat: usize,
        actor_label: &str,
        intent: ErasedIntent,
        board: &mut BoardCards,
        ctx: &mut ResolveCtx<'_>,
    ) {
        let Ok(turn) = intent.downcast::<Turn<SeerChoice>>() else {
            return;
        };
        let actor = &ctx.roster.seats[seat].user;
        match *turn {
            Turn::Reply(SeerChoice::Player(idx)) if !board.is_shielded(idx) => {
                board.record_card_interaction(seat);
                let card = board.players[idx];
                let target_nick = ctx.roster.seats[idx].user.nick();
                let _ = actor.say(
                    format!("🔮 [{}] {}'s card is: ", idx + 1, target_nick)
                        + card.display_for_peek(),
                );
                ctx.logs.push(format!(
                    "{actor_label} ({}) viewed {} ({}).",
                    actor.nick(),
                    target_nick,
                    card.effective_role().name()
                ));
            }
            Turn::Reply(SeerChoice::Center(a, b)) => {
                board.record_card_interaction(seat);
                let (ca, cb) = (board.center[a], board.center[b]);
                let _ = actor.say(
                    format!("🔮 Center #{} is ", a + 1)
                        + ca.display_for_peek()
                        + format!(", Center #{} is ", b + 1)
                        + cb.display_for_peek()
                        + ".",
                );
                ctx.logs.push(format!(
                    "{actor_label} ({}) viewed Center #{}/#{}.",
                    actor.nick(),
                    a + 1,
                    b + 1
                ));
            }
            _ => ctx
                .logs
                .push(format!("{actor_label} ({}) did not view any cards.", actor.nick())),
        }
    }
}

fn parse_seer_choice(
    roster: &Roster,
    shielded: &std::collections::HashSet<usize>,
    actor_seat: usize,
    raw: &str,
) -> Match<SeerChoice> {
    let trimmed = raw.trim();
    if matches!(trimmed.to_ascii_lowercase().as_str(), "pass" | "skip" | "none") {
        return Match::Accept(SeerChoice::Pass);
    }
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts
        .first()
        .is_some_and(|w| w.eq_ignore_ascii_case("center") || w.eq_ignore_ascii_case("c"))
    {
        if parts.len() != 3 {
            return Match::Reject(Text::from("Usage: `center <1-3> <1-3>`.".yellow()));
        }
        let (Ok(a), Ok(b)) = (parts[1].parse::<usize>(), parts[2].parse::<usize>()) else {
            return Match::Reject(Text::from("Center slots must be `1`, `2`, or `3`.".yellow()));
        };
        if !(1..=3).contains(&a) || !(1..=3).contains(&b) || a == b {
            return Match::Reject(Text::from("Pick two DIFFERENT center slots (1..=3).".yellow()));
        }
        return Match::Accept(SeerChoice::Center(a - 1, b - 1));
    }

    let token = if parts
        .first()
        .is_some_and(|w| w.eq_ignore_ascii_case("player") || w.eq_ignore_ascii_case("p"))
    {
        if parts.len() != 2 {
            return Match::Reject(Text::from("Usage: `player <seat|nick>`.".yellow()));
        }
        parts[1]
    } else if parts.len() == 1 {
        parts[0]
    } else {
        return Match::Reject(Text::from(
            "Reply `player <seat|nick>`, `center <1-3> <1-3>`, or `pass`.".yellow(),
        ));
    };

    match roster.resolve_token(token) {
        Some(idx) if idx == actor_seat => {
            Match::Reject(Text::from("You cannot look at your own card!".yellow()))
        }
        Some(idx) if shielded.contains(&idx) => Match::Reject(Text::from(
            "🛡 That player's card has a Shield token and cannot be viewed!".yellow(),
        )),
        Some(idx) => Match::Accept(SeerChoice::Player(idx)),
        None => Match::Reject(Text::from("Unknown player.".yellow())),
    }
}
