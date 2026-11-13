use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, PromptCtx, ResolveCtx, RolePlugin, Team,
};

pub struct Robber;
pub static ROBBER: Robber = Robber;

impl RolePlugin for Robber {
    fn id(&self) -> &'static str {
        "robber"
    }

    fn name(&self) -> &'static str {
        "Robber"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Wake to optionally swap your card with another player's card and view your new card."
    }

    fn wake_order(&self) -> Option<u16> {
        Some(160)
    }

    fn copy_behavior(&self) -> CopyBehavior {
        CopyBehavior::ActImmediately
    }

    fn mutates_board(&self) -> bool {
        true
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
                "🦹 Robber: Reply with `<seat|nick>` to steal their card, or `pass`. Seats: {}",
                ctx.roster.format_seats_marking_shields(ctx.shielded)
            );
            let turn = dm
                .ask(prompt, move |m| {
                    let t = m.text.trim();
                    if matches!(t.to_ascii_lowercase().as_str(), "pass" | "skip" | "none") {
                        return Match::Accept(None);
                    }
                    match ctx.roster.resolve_token(t) {
                        Some(idx) if idx == seat => Match::Reject(Text::from(
                            "Pick ANOTHER player (or `pass`), not yourself!".yellow(),
                        )),
                        Some(idx) if ctx.shielded.contains(&idx) => Match::Reject(Text::from(
                            "🛡️ That player's card is shielded by the Sentinel!".yellow(),
                        )),
                        Some(idx) => Match::Accept(Some(idx)),
                        None => Match::Reject(Text::from("Unknown player (or `pass`).".yellow())),
                    }
                })
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left for Robber swap!")
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
        let Ok(turn) = intent.downcast::<Turn<Option<usize>>>() else {
            return;
        };
        let actor = &ctx.roster.seats[seat].user;
        if let Turn::Reply(Some(target_seat)) = *turn {
            if board.is_shielded(target_seat) || board.is_shielded(seat) {
                let _ = actor.say("🛡️ Robber swap blocked by a Sentinel shield!".yellow());
                return;
            }
            board.record_card_interaction(seat);
            board.players.swap(seat, target_seat);
            let stolen = board.players[seat];
            let victim_nick = ctx.roster.seats[target_seat].user.nick();
            let _ = actor.say(
                format!("🦹 You robbed [{}] {} and got: ", target_seat + 1, victim_nick)
                    + stolen.display_for_peek()
                    + "!",
            );
            ctx.logs.push(format!(
                "{actor_label} ({}) robbed {} (got {}).",
                actor.nick(),
                victim_nick,
                stolen.effective_role().name()
            ));
        } else {
            ctx.logs
                .push(format!("{actor_label} ({}) did not rob anyone.", actor.nick()));
        }
    }
}
