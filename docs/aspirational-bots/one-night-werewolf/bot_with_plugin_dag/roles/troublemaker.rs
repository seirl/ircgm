use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, PromptCtx, ResolveCtx, RolePlugin, Team,
};

pub struct Troublemaker;
pub static TROUBLEMAKER: Troublemaker = Troublemaker;

impl RolePlugin for Troublemaker {
    fn id(&self) -> &'static str {
        "troublemaker"
    }

    fn name(&self) -> &'static str {
        "Troublemaker"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Wake to optionally swap two OTHER players' cards without looking at them."
    }

    fn wake_order(&self) -> Option<u16> {
        Some(170)
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
                "😈 Troublemaker: Reply with TWO other players (`<a> <b>`) to swap, or `pass`. Seats: {}",
                ctx.roster.format_seats_marking_shields(ctx.shielded)
            );
            let turn = dm
                .ask(prompt, move |m| {
                    let t = m.text.trim();
                    if matches!(t.to_ascii_lowercase().as_str(), "pass" | "skip" | "none") {
                        return Match::Accept(None);
                    }
                    let parts: Vec<&str> = t.split_whitespace().collect();
                    if parts.len() != 2 {
                        return Match::Reject(Text::from(
                            "Usage: `<seat|nick> <seat|nick>` or `pass`.".yellow(),
                        ));
                    }
                    let Some(a) = ctx.roster.resolve_token(parts[0]) else {
                        return Match::Reject(Text::from(
                            format!("Unknown player `{}`.", parts[0]).yellow(),
                        ));
                    };
                    let Some(b) = ctx.roster.resolve_token(parts[1]) else {
                        return Match::Reject(Text::from(
                            format!("Unknown player `{}`.", parts[1]).yellow(),
                        ));
                    };
                    if a == seat || b == seat || a == b {
                        return Match::Reject(Text::from(
                            "Pick TWO DIFFERENT players other than yourself!".yellow(),
                        ));
                    }
                    if ctx.shielded.contains(&a) || ctx.shielded.contains(&b) {
                        return Match::Reject(Text::from(
                            "🛡️ Shielded players cannot have their cards swapped!".yellow(),
                        ));
                    }
                    Match::Accept(Some((a, b)))
                })
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left for Troublemaker swap!")
                .settled()
                .await?;

            // Troublemaker learns zero card info, so confirm immediately!
            if let Turn::Reply(Some((a, b))) = turn {
                let _ = dm.say(
                    "😈 Locked in swap between "
                        + ctx.roster.seats[a].user.nick().bold()
                        + " and "
                        + ctx.roster.seats[b].user.nick().bold()
                        + "!",
                );
            }
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
        let Ok(turn) = intent.downcast::<Turn<Option<(usize, usize)>>>() else {
            return;
        };
        let actor = &ctx.roster.seats[seat].user;
        if let Turn::Reply(Some((a, b))) = *turn {
            if board.is_shielded(a) || board.is_shielded(b) {
                return;
            }
            board.record_card_interaction(seat);
            board.players.swap(a, b);
            ctx.logs.push(format!(
                "{actor_label} ({}) swapped {} and {}.",
                actor.nick(),
                ctx.roster.seats[a].user.nick(),
                ctx.roster.seats[b].user.nick()
            ));
        } else {
            ctx.logs
                .push(format!("{actor_label} ({}) did not swap anyone.", actor.nick()));
        }
    }
}
