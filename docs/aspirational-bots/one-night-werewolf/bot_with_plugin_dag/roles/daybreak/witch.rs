//! Daybreak **Witch** (also covers Super Villains **Voodoo Lou**!)
//!
//! Two-step sequential `ExclusiveDm` prompt:
//! 1. Optionally look at ONE center card (`1..=3`), seeing its current role (`AfterUpstreamBoard`).
//! 2. If they looked, they MUST swap that center card with ANY unshielded player (including themselves!).

use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, PromptCtx, PromptDep, ResolveCtx,
    RolePlugin, Team,
};

pub struct Witch;
pub static WITCH: Witch = Witch;

impl RolePlugin for Witch {
    fn id(&self) -> &'static str {
        "witch"
    }

    fn name(&self) -> &'static str {
        "Witch"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "May look at 1 center card; if you do, you MUST swap it with any player's card (including your own)!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(165)
    }

    fn copy_behavior(&self) -> CopyBehavior {
        CopyBehavior::ActImmediately
    }

    fn prompt_dep(&self) -> PromptDep {
        PromptDep::AfterUpstreamBoard
    }

    fn mutates_board(&self) -> bool {
        true
    }

    fn prompt_seat<'a>(
        &'a self,
        dm: &'a mut ExclusiveDm,
        seat: usize,
        ctx: &'a PromptCtx<'a>,
        upstream_board: Option<&'a BoardCards>,
    ) -> BoxFuture<'a, ircgm::Result<ErasedIntent>> {
        Box::pin(async move {
            let board = upstream_board.expect("`AfterUpstreamBoard` guarantees resolved board");
            let step1 = dm
                .ask(
                    "🧙‍♀️ Witch: Reply `1`, `2`, or `3` to view a center card (you MUST then give it to someone!), or `pass`:",
                    |m| match m.text.trim().to_ascii_lowercase().as_str() {
                        "pass" | "skip" | "none" => Match::Accept(None),
                        "1" | "c1" => Match::Accept(Some(0usize)),
                        "2" | "c2" => Match::Accept(Some(1usize)),
                        "3" | "c3" => Match::Accept(Some(2usize)),
                        _ => Match::Reject(Text::from("Reply `1`, `2`, `3`, or `pass`.".yellow())),
                    },
                )
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left for Witch center peek!")
                .settled()
                .await?;

            let Turn::Reply(Some(c_idx)) = step1 else {
                return Ok(Box::new(None::<(usize, usize)>) as ErasedIntent);
            };

            let seen_center = board.center[c_idx];
            let _ = dm.say(
                format!("🧙‍♀️ Center #{} is ", c_idx + 1)
                    + seen_center.display_for_peek()
                    + "!",
            );

            let unshielded: Vec<usize> = (0..ctx.roster.len())
                .filter(|&i| !ctx.shielded.contains(&i))
                .collect();
            if unshielded.is_empty() {
                return Ok(Box::new(None::<(usize, usize)>) as ErasedIntent);
            }

            let step2 = dm
                .ask(
                    format!(
                        "🧙‍♀️ Give Center #{} to which player (including yourself)? Seats: {}",
                        c_idx + 1,
                        ctx.roster.format_seats_marking_shields(ctx.shielded)
                    ),
                    move |m| match ctx.roster.resolve_token(&m.text) {
                        Some(idx) if ctx.shielded.contains(&idx) => Match::Reject(Text::from(
                            "🛡️ That player is shielded!".yellow(),
                        )),
                        Some(idx) => Match::Accept(idx),
                        None => Match::Reject(Text::from("Unknown seat or nickname.".yellow())),
                    },
                )
                .with_config(ctx.ask_cfg)
                .settled()
                .await?;

            let target = match step2 {
                Turn::Reply(idx) => idx,
                Turn::TimedOut | Turn::Left { .. } => {
                    let auto = if !ctx.shielded.contains(&seat) {
                        seat
                    } else {
                        unshielded[fastrand::usize(..unshielded.len())]
                    };
                    let _ = dm.say(format!(
                        "🧙‍♀️ Time expired—swapped Center #{} with [{}] {}!",
                        c_idx + 1,
                        auto + 1,
                        ctx.roster.seats[auto].user.nick()
                    ));
                    auto
                }
            };
            Ok(Box::new(Some((c_idx, target))) as ErasedIntent)
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
        let Ok(choice) = intent.downcast::<Option<(usize, usize)>>() else {
            return;
        };
        let actor_nick = ctx.roster.seats[seat].user.nick();
        if let Some((c_idx, target)) = *choice {
            if !board.is_shielded(target) {
                board.record_card_interaction(seat);
                std::mem::swap(&mut board.center[c_idx], &mut board.players[target]);
                ctx.logs.push(format!(
                    "{actor_label} ({actor_nick}) peeked Center #{} and swapped it onto {}.",
                    c_idx + 1,
                    ctx.roster.seats[target].user.nick()
                ));
            }
        } else {
            ctx.logs
                .push(format!("{actor_label} ({actor_nick}) did not brew anything."));
        }
    }
}
