//! Vampire Expansion **Copycat** (also covers Super Villains **Mirror Man**!)
//!
//! Wakes at the start of Dusk (`wake_order = 5`), peeks 1 of the 3 center cards,
//! permanently becomes that role, and reuses the exact same polymorphic `CopyBehavior`
//! delegation as `Doppelgänger`!

use std::time::Duration;

use tokio::time::Instant;

use ircgm::{Colorize, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, Card, CopiedRecord, CopyBehavior, CopycatCtx, PromptCtx, ResolveCtx,
    RolePlugin, Team,
};
use crate::bot_with_plugin_dag::roles::villager::VILLAGER;

pub struct Copycat;
pub static COPYCAT: Copycat = Copycat;

impl RolePlugin for Copycat {
    fn id(&self) -> &'static str {
        "copycat"
    }

    fn name(&self) -> &'static str {
        "Copycat"
    }

    fn team(&self) -> Team {
        Team::Independent
    }

    fn night_summary(&self) -> &'static str {
        "Wake first in Dusk: view 1 center card (`1..=3`) and become that role & team for the rest of the game!"
    }

    fn make_card(&'static self) -> Card {
        Card::Copycat {
            base: self,
            copied: None,
            unawakened_fallback: &VILLAGER,
        }
    }

    fn wake_order(&self) -> Option<u16> {
        Some(5)
    }

    fn is_copycat(&self) -> bool {
        true
    }

    fn mutates_board(&self) -> bool {
        true
    }

    fn run_copycat<'a>(
        &'a self,
        seat: usize,
        mut board: BoardCards,
        ctx: &'a CopycatCtx<'a>,
    ) -> BoxFuture<'a, ircgm::Result<(BoardCards, Vec<String>)>> {
        Box::pin(async move {
            let mut logs = Vec::new();
            let user = &ctx.roster.seats[seat].user;

            let Ok(dm) = user.exclusive().await else {
                let c_idx = fastrand::usize(0..3);
                let copied = board.center[c_idx].effective_role();
                board.record_card_interaction(seat);
                board.players[seat] = Card::Copycat {
                    base: &COPYCAT,
                    copied: Some(copied),
                    unawakened_fallback: &VILLAGER,
                };
                let _ = ctx.copied_tx.send(Some(vec![CopiedRecord {
                    copycat_seat: seat,
                    copied_role: copied,
                }]));
                if copied.copy_behavior() == CopyBehavior::ActImmediately {
                    let label = format!("Copycat-{}", copied.name());
                    let mut rctx = ResolveCtx {
                        roster: ctx.roster,
                        logs: &mut logs,
                    };
                    copied.resolve_unavailable_seat(seat, &label, &mut board, &mut rctx);
                }
                return Ok((board, logs));
            };

            let mut dm = dm.with_ask_config(ctx.stage1_cfg).with_ignore_commands();
            let step1 = dm
                .ask(
                    "🪞 Copycat: Pick a center card (`1`, `2`, or `3`) to view and become!",
                    |m| match m.text.trim().to_ascii_lowercase().as_str() {
                        "1" | "c1" => Match::Accept(0usize),
                        "2" | "c2" => Match::Accept(1usize),
                        "3" | "c3" => Match::Accept(2usize),
                        _ => Match::Reject(Text::from("Reply `1`, `2`, or `3`.".yellow())),
                    },
                )
                .warn_at(Duration::from_secs(10), "⏳ 10s left to pick a center card!")
                .settled()
                .await?;

            let c_idx = match step1 {
                Turn::Reply(idx) => idx,
                Turn::TimedOut | Turn::Left { .. } => fastrand::usize(0..3),
            };

            let copied = board.center[c_idx].effective_role();
            board.record_card_interaction(seat);
            board.players[seat] = Card::Copycat {
                base: &COPYCAT,
                copied: Some(copied),
                unawakened_fallback: &VILLAGER,
            };

            let _ = ctx.copied_tx.send(Some(vec![CopiedRecord {
                copycat_seat: seat,
                copied_role: copied,
            }]));

            logs.push(format!(
                "Copycat ({}) copied Center #{} ({}).",
                user.nick(),
                c_idx + 1,
                copied.name()
            ));
            let _ = dm.say(
                format!("🪞 Center #{} is ", c_idx + 1)
                    + copied.styled_name()
                    + "! You are now that role.",
            );

            match copied.copy_behavior() {
                CopyBehavior::ActImmediately => {
                    let step2_deadline = ctx
                        .stage1_deadline
                        .max(Instant::now() + ctx.stage2_min_budget);
                    let step2_cfg = ctx.stage1_cfg.clone().reply_deadline(step2_deadline);
                    let pctx = PromptCtx {
                        roster: ctx.roster,
                        ask_cfg: &step2_cfg,
                        deadline: step2_deadline,
                        shielded: &board.shielded,
                    };
                    let intent = copied
                        .prompt_seat(&mut dm, seat, &pctx, Some(&board))
                        .await?;
                    drop(dm);

                    let label = format!("Copycat-{}", copied.name());
                    let mut rctx = ResolveCtx {
                        roster: ctx.roster,
                        logs: &mut logs,
                    };
                    copied.resolve_seat(seat, &label, intent, &mut board, &mut rctx);
                }
                CopyBehavior::JoinGroupWake | CopyBehavior::WakeAfterAllMutations => {
                    let _ = dm.say("🌙 Stand by—your copied role's action will fire shortly!");
                }
                CopyBehavior::Sleep => {
                    let _ = dm.say("🌙 You have no further actions tonight.");
                }
            }

            Ok((board, logs))
        })
    }
}
