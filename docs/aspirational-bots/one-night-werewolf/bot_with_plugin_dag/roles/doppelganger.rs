//! Doppelgänger Plugin — 100% Polymorphic Over Any Other `RolePlugin`!
//!
//! Notice what is *not* imported here: `seer`, `robber`, `troublemaker`, `drunk`,
//! `werewolf`, `minion`, `mason`, or `insomniac`!
//! `Doppelganger` inspects `copied_role.copy_behavior()` and delegates directly to
//! `copied_role.prompt_seat(&mut dm, ...)` + `copied_role.resolve_seat(...)` using the
//! already-held `ExclusiveDm` guard!

use std::time::Duration;

use tokio::time::Instant;

use ircgm::{Colorize, GameFutureExt, Match, Text, Turn};

use super::villager::VILLAGER;
use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, Card, CopiedRecord, CopyBehavior, CopycatCtx, PromptCtx, ResolveCtx,
    RolePlugin, Team,
};

pub struct Doppelganger;
pub static DOPPELGANGER: Doppelganger = Doppelganger;

impl RolePlugin for Doppelganger {
    fn id(&self) -> &'static str {
        "doppelganger"
    }

    fn name(&self) -> &'static str {
        "Doppelgänger"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Wake 1st: look at another player's card and permanently become that role & team."
    }

    fn make_card(&'static self) -> Card {
        Card::Copycat {
            base: self,
            copied: None,
            unawakened_fallback: &VILLAGER,
        }
    }

    fn wake_order(&self) -> Option<u16> {
        Some(10)
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
            let seats_str = ctx.roster.format_seats();

            let pick_random_other = || {
                let others: Vec<usize> = (0..ctx.roster.len()).filter(|&i| i != seat).collect();
                others[fastrand::usize(..others.len())]
            };

            let Ok(dm) = user.exclusive().await else {
                let target_idx = pick_random_other();
                let copied = board.players[target_idx].effective_role();
                board.players[seat] = Card::Copycat {
                    base: &DOPPELGANGER,
                    copied: Some(copied),
                    unawakened_fallback: &VILLAGER,
                };
                let _ = ctx.copied_tx.send(Some(vec![CopiedRecord {
                    copycat_seat: seat,
                    copied_role: copied,
                }]));
                if copied.copy_behavior() == CopyBehavior::ActImmediately {
                    let label = format!("Doppelgänger-{}", copied.name());
                    let mut rctx = ResolveCtx {
                        roster: ctx.roster,
                        logs: &mut logs,
                    };
                    copied.resolve_unavailable_seat(seat, &label, &mut board, &mut rctx);
                }
                return Ok((board, logs));
            };

            let mut dm = dm.with_ask_config(ctx.stage1_cfg).with_ignore_commands();
            let step1a = dm
                .ask(
                    format!(
                        "🎭 Doppelgänger: Pick ONE other player (`<seat|nick>`) to copy! Seats: {seats_str}"
                    ),
                    move |m| match ctx.roster.resolve_token(&m.text) {
                        Some(idx) if idx == seat => {
                            Match::Reject(Text::from("Pick ANOTHER player, not yourself!".yellow()))
                        }
                        Some(idx) => Match::Accept(idx),
                        None => Match::Reject(Text::from("Unknown seat or nickname.".yellow())),
                    },
                )
                .warn_at(Duration::from_secs(10), "⏳ 10s left to pick a player to copy!")
                .settled()
                .await?;

            let target_idx = match step1a {
                Turn::Reply(idx) => idx,
                Turn::TimedOut | Turn::Left { .. } => {
                    let auto = pick_random_other();
                    let _ = dm.say(format!(
                        "🎲 Randomly looked at [{}] {}.",
                        auto + 1,
                        ctx.roster.seats[auto].user.nick()
                    ));
                    auto
                }
            };

            let copied = board.players[target_idx].effective_role();
            board.record_card_interaction(seat);
            board.players[seat] = Card::Copycat {
                base: &DOPPELGANGER,
                copied: Some(copied),
                unawakened_fallback: &VILLAGER,
            };

            // Immediately publish Step 1a so `AfterCopycats` roles (`Werewolf`, `Minion`, `Mason`)
            // wake right now without waiting for Step 1b!
            let _ = ctx.copied_tx.send(Some(vec![CopiedRecord {
                copycat_seat: seat,
                copied_role: copied,
            }]));

            logs.push(format!(
                "Doppelgänger ({}) copied {} ({}).",
                user.nick(),
                ctx.roster.seats[target_idx].user.nick(),
                copied.name()
            ));
            let _ = dm.say(
                format!(
                    "🎭 [{}] {}'s card is ",
                    target_idx + 1,
                    ctx.roster.seats[target_idx].user.nick()
                ) + copied.styled_name()
                    + "! You are now that role.",
            );

            match copied.copy_behavior() {
                CopyBehavior::ActImmediately => {
                    let step1b_deadline = ctx
                        .stage1_deadline
                        .max(Instant::now() + ctx.stage2_min_budget);
                    let step1b_cfg = ctx.stage1_cfg.clone().reply_deadline(step1b_deadline);
                    let pctx = PromptCtx {
                        roster: ctx.roster,
                        ask_cfg: &step1b_cfg,
                        deadline: step1b_deadline,
                        shielded: &board.shielded,
                    };

                    // Polymorphically delegate Step 1b to the copied plugin while holding `dm`!
                    let intent = copied
                        .prompt_seat(&mut dm, seat, &pctx, Some(&board))
                        .await?;
                    drop(dm);

                    let label = format!("Doppelgänger-{}", copied.name());
                    let mut rctx = ResolveCtx {
                        roster: ctx.roster,
                        logs: &mut logs,
                    };
                    copied.resolve_seat(seat, &label, intent, &mut board, &mut rctx);
                }
                CopyBehavior::JoinGroupWake | CopyBehavior::WakeAfterAllMutations => {
                    let _ = dm.say("🌙 Stand by—your copied role's night info will arrive shortly!");
                }
                CopyBehavior::Sleep => {
                    let _ = dm.say("🌙 You have no further night actions.");
                }
            }

            Ok((board, logs))
        })
    }
}
