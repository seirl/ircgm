//! Generic Dusk + Night Causal-DAG Scheduler
//!
//! Zero concrete role names appear in this file!
//! Automatically runs:
//! 1. **Copycats** (`is_copycat() == true`, e.g. `Copycat`, `Mirror Man`, `Doppelgänger`)
//! 2. **Dusk Stage (`wake_order < 100`)** (`Sentinel` shields + Vampire Dusk Marks — 0ms if none in deck!)
//! 3. **Night Stage (`wake_order >= 100`)** (parallel prompts with `shielded` visibility + `Mark::Fear` blocking,
//!    committing along a lock-free `watch` board pipeline!)
//! 4. **Post-Mutation Copycats** (`CopyBehavior::WakeAfterAllMutations`, e.g. Doppelgänger-Insomniac)

use std::collections::HashSet;
use std::time::Duration;

use futures::future::try_join_all;
use tokio::sync::watch;
use tokio::time::Instant;

use ircgm::{AskConfig, Bot, Colorize, Result, Room};

use super::plugin::{
    BoardCards, BoxFuture, CopiedRecord, CopyBehavior, CopycatCtx, GroupPromptCtx, Mark, PromptCtx,
    PromptDep, ResolveCtx, RolePlugin, Roster,
};

const STAGE_BUDGET: Duration = Duration::from_secs(40);
const FOLLOWUP_MIN_BUDGET: Duration = Duration::from_secs(25);

pub async fn wait_ready<T: Clone>(rx: &mut watch::Receiver<Option<T>>) -> T {
    loop {
        if let Some(val) = rx.borrow_and_update().clone() {
            return val;
        }
        let _ = rx.changed().await;
    }
}

pub async fn run_plugin_night_dag(
    bot: &Bot,
    room: &Room,
    roster: &Roster,
    mut initial_board: BoardCards,
) -> Result<(BoardCards, Vec<String>)> {
    let phase_start = Instant::now();

    // Run `on_deal` setup hooks (e.g. `Renfield` starting with `Mark::Bat`, `Blob` marking initial Blob seats):
    for (seat, info) in roster.seats.iter().enumerate() {
        info.initial_role.on_deal(seat, &mut initial_board, roster);
    }

    // Collect distinct waking roles in the deck:
    let mut waking: Vec<&'static dyn RolePlugin> = Vec::new();
    for &r in &roster.deck {
        if r.wake_order().is_some() && !waking.iter().any(|e| e.id() == r.id()) {
            waking.push(r);
        }
    }
    waking.sort_by_key(|r| r.wake_order().unwrap_or(u16::MAX));

    let copycat_seats: Vec<(usize, &'static dyn RolePlugin)> = roster
        .seats
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.initial_role.is_copycat().then_some((i, s.initial_role)))
        .collect();

    let dusk_roles: Vec<&'static dyn RolePlugin> = waking
        .iter()
        .copied()
        .filter(|r| !r.is_copycat() && r.wake_order().unwrap_or(0) < 100)
        .collect();
    let night_roles: Vec<&'static dyn RolePlugin> = waking
        .iter()
        .copied()
        .filter(|r| !r.is_copycat() && r.wake_order().unwrap_or(0) >= 100)
        .collect();

    let mut all_logs: Vec<String> = Vec::new();

    // ------------------------------------------------------------------------
    // Stage A: Copycats + Dusk (`wake_order < 100`, e.g. Sentinel & Vampire Marks)
    //          (Takes 0ms with zero announcements if neither Copycat nor Dusk roles are in deck!)
    // ------------------------------------------------------------------------
    let (copied_tx, mut copied_rx) = watch::channel::<Option<Vec<CopiedRecord>>>(
        if copycat_seats.is_empty() {
            Some(Vec::new())
        } else {
            None
        },
    );
    let (after_copy_tx, after_copy_rx) = watch::channel::<Option<BoardCards>>(
        if copycat_seats.is_empty() {
            Some(initial_board.clone())
        } else {
            None
        },
    );

    let (dusk_board, copied_records) = if copycat_seats.is_empty() && dusk_roles.is_empty() {
        (initial_board, Vec::new())
    } else {
        if !dusk_roles.is_empty() {
            room.say("🌆 Dusk falls! Dusk & pre-night roles check your DMs...".bold().orange())?;
        } else {
            room.say("🌙 Night falls! Active roles check your DMs...".bold().light_blue())?;
        }

        let dusk_deadline = Instant::now() + STAGE_BUDGET;
        let dusk_cfg = AskConfig::new()
            .reply_deadline(dusk_deadline)
            .ignore_commands()
            .on_timeout("⏳ Time's up!".yellow());
        let empty_shields = HashSet::new();

        let mut dusk_tasks: Vec<BoxFuture<'_, Result<(u32, Vec<String>)>>> = Vec::new();

        if !copycat_seats.is_empty() {
            let dusk_cfg_ref = &dusk_cfg;
            let copied_tx_ref = &copied_tx;
            let board_in = initial_board.clone();
            dusk_tasks.push(Box::pin(async move {
                let mut board = board_in;
                let mut logs = Vec::new();
                let mut all_copied = Vec::new();
                for (seat, plugin) in copycat_seats {
                    let (item_tx, mut item_rx) = watch::channel(None);
                    let ctx = CopycatCtx {
                        roster,
                        stage1_cfg: dusk_cfg_ref,
                        stage1_deadline: dusk_deadline,
                        stage2_min_budget: FOLLOWUP_MIN_BUDGET,
                        copied_tx: &item_tx,
                    };
                    let (next_board, step_logs) = plugin.run_copycat(seat, board, &ctx).await?;
                    if let Some(recs) = item_rx.borrow_and_update().clone() {
                        all_copied.extend(recs);
                    }
                    board = next_board;
                    logs.extend(step_logs);
                }
                let _ = copied_tx_ref.send(Some(all_copied));
                let _ = after_copy_tx.send(Some(board));
                Ok((5, logs))
            }));
        }

        let final_dusk_rx = schedule_role_stage(
            bot,
            roster,
            &dusk_roles,
            after_copy_rx,
            copied_rx.clone(),
            &empty_shields,
            &HashSet::new(),
            &dusk_cfg,
            dusk_deadline,
            &mut dusk_tasks,
        );

        let mut dusk_out = try_join_all(dusk_tasks).await?;
        dusk_out.sort_by_key(|(k, _)| *k);
        all_logs.extend(dusk_out.into_iter().flat_map(|(_, l)| l));

        let mut final_rx = final_dusk_rx;
        let board = wait_ready(&mut final_rx).await;
        let copied = wait_ready(&mut copied_rx).await;
        (board, copied)
    };

    // If Dusk granted `Mark::Fear` to any player, notify them and suppress their Night wake!
    let feared_seats: HashSet<usize> = (0..roster.len())
        .filter(|&s| dusk_board.marks[s] == Mark::Fear)
        .collect();
    for &s in &feared_seats {
        let _ = roster.seats[s].user.say(
            "😱 You received the "
                + Mark::Fear.styled()
                + " during Dusk and are too terrified to wake up tonight!",
        );
    }

    // ------------------------------------------------------------------------
    // Stage B: Night (`wake_order >= 100`) — All `PromptDep::None` roles fire in parallel!
    // ------------------------------------------------------------------------
    if !dusk_roles.is_empty() || copycat_seats.is_empty() {
        room.say("🌙 Night deepens! All night roles have been prompted in DM...".bold().light_blue())?;
    }

    let night_deadline = Instant::now() + STAGE_BUDGET;
    let night_cfg = AskConfig::new()
        .reply_deadline(night_deadline)
        .ignore_commands()
        .on_timeout("⏳ Time's up!".yellow());

    let shielded_snapshot = dusk_board.shielded.clone();
    let (_night_in_tx, night_in_rx) = watch::channel(Some(dusk_board));
    let (_resolved_copied_tx, resolved_copied_rx) = watch::channel(Some(copied_records.clone()));

    let mut night_tasks: Vec<BoxFuture<'_, Result<(u32, Vec<String>)>>> = Vec::new();
    let mut final_night_rx = schedule_role_stage(
        bot,
        roster,
        &night_roles,
        night_in_rx,
        resolved_copied_rx,
        &shielded_snapshot,
        &feared_seats,
        &night_cfg,
        night_deadline,
        &mut night_tasks,
    );

    // Post-mutation `WakeAfterAllMutations` copycats (e.g. Doppelgänger-Insomniac, MirrorMan-SelfAwarenessGirl):
    let mut tail_board_rx = final_night_rx.clone();
    let copied_for_tail = copied_records;
    let feared_ref = &feared_seats;
    night_tasks.push(Box::pin(async move {
        let mut board = wait_ready(&mut tail_board_rx).await;
        let mut logs = Vec::new();
        for rec in copied_for_tail {
            if rec.copied_role.copy_behavior() == CopyBehavior::WakeAfterAllMutations
                && !feared_ref.contains(&rec.copycat_seat)
            {
                let base_name = roster.seats[rec.copycat_seat].initial_role.name();
                let label = format!("{base_name}-{}", rec.copied_role.name());
                let mut rctx = ResolveCtx {
                    roster,
                    logs: &mut logs,
                };
                rec.copied_role.resolve_seat(
                    rec.copycat_seat,
                    &label,
                    Box::new(()),
                    &mut board,
                    &mut rctx,
                );
            }
        }
        Ok((u32::MAX, logs))
    }));

    let mut night_out = try_join_all(night_tasks).await?;
    night_out.sort_by_key(|(k, _)| *k);
    all_logs.extend(night_out.into_iter().flat_map(|(_, l)| l));

    let final_board = wait_ready(&mut final_night_rx).await;

    let min_floor = Duration::from_millis(fastrand::u64(15_000..=20_000));
    if let Some(rem) = min_floor.checked_sub(phase_start.elapsed()) {
        tokio::time::sleep(rem).await;
    }

    Ok((final_board, all_logs))
}

#[allow(clippy::too_many_arguments)]
fn schedule_role_stage<'a>(
    bot: &'a Bot,
    roster: &'a Roster,
    roles: &[&'static dyn RolePlugin],
    initial_rx: watch::Receiver<Option<BoardCards>>,
    copied_rx: watch::Receiver<Option<Vec<CopiedRecord>>>,
    shielded: &'a HashSet<usize>,
    feared_seats: &'a HashSet<usize>,
    stage_cfg: &'a AskConfig,
    stage_deadline: Instant,
    tasks: &mut Vec<BoxFuture<'a, Result<(u32, Vec<String>)>>>,
) -> watch::Receiver<Option<BoardCards>> {
    let mut current_board_rx = initial_rx;

    for &role in roles {
        let order_key = u32::from(role.wake_order().unwrap_or(999)) * 100;

        if role.is_group_wake() {
            let mut upstream_rx = current_board_rx.clone();
            let mut copied_watch = copied_rx.clone();
            let next_tx = if role.mutates_board() {
                let (tx, rx) = watch::channel(None);
                current_board_rx = rx;
                Some(tx)
            } else {
                None
            };

            tasks.push(Box::pin(async move {
                let copied_records = match role.prompt_dep() {
                    PromptDep::None => Vec::new(),
                    PromptDep::AfterCopycats | PromptDep::AfterUpstreamBoard => {
                        wait_ready(&mut copied_watch).await
                    }
                };

                let mut active_seats: Vec<usize> = roster
                    .initial_holders(role)
                    .into_iter()
                    .filter(|s| !feared_seats.contains(s))
                    .collect();
                for rec in &copied_records {
                    if rec.copied_role.id() == role.id()
                        && rec.copied_role.copy_behavior() == CopyBehavior::JoinGroupWake
                        && !feared_seats.contains(&rec.copycat_seat)
                        && !active_seats.contains(&rec.copycat_seat)
                    {
                        active_seats.push(rec.copycat_seat);
                    }
                }

                if active_seats.is_empty() {
                    if let Some(tx) = next_tx {
                        let board = wait_ready(&mut upstream_rx).await;
                        let _ = tx.send(Some(board));
                    }
                    return Ok((order_key, Vec::new()));
                }

                let mut logs = Vec::new();
                let upstream_for_prompt = if role.prompt_dep() == PromptDep::AfterUpstreamBoard {
                    Some(wait_ready(&mut upstream_rx).await)
                } else {
                    None
                };

                let deadline = if role.prompt_dep() != PromptDep::None {
                    stage_deadline.max(Instant::now() + FOLLOWUP_MIN_BUDGET)
                } else {
                    stage_deadline
                };
                let cfg = stage_cfg.clone().reply_deadline(deadline);
                let gctx = GroupPromptCtx {
                    bot,
                    roster,
                    ask_cfg: &cfg,
                    deadline,
                    copied_records: &copied_records,
                    shielded,
                };

                let intent = role
                    .prompt_group(active_seats.clone(), &gctx, upstream_for_prompt.as_ref())
                    .await?;
                let mut board = match upstream_for_prompt {
                    Some(b) => b,
                    None => wait_ready(&mut upstream_rx).await,
                };
                let mut rctx = ResolveCtx {
                    roster,
                    logs: &mut logs,
                };
                role.resolve_group(&active_seats, intent, &mut board, &mut rctx);
                if let Some(tx) = next_tx {
                    let _ = tx.send(Some(board));
                }
                Ok((order_key, logs))
            }));
        } else {
            for (offset, seat) in roster.initial_holders(role).into_iter().enumerate() {
                if feared_seats.contains(&seat) {
                    continue;
                }
                let mut upstream_rx = current_board_rx.clone();
                let mut copied_watch = copied_rx.clone();
                let next_tx = if role.mutates_board() {
                    let (tx, rx) = watch::channel(None);
                    current_board_rx = rx;
                    Some(tx)
                } else {
                    None
                };

                let task_key = order_key + offset as u32;
                tasks.push(Box::pin(async move {
                    if role.prompt_dep() == PromptDep::AfterCopycats {
                        let _ = wait_ready(&mut copied_watch).await;
                    }

                    let mut logs = Vec::new();
                    if !role.needs_dm_prompt() {
                        let mut board = wait_ready(&mut upstream_rx).await;
                        let mut rctx = ResolveCtx {
                            roster,
                            logs: &mut logs,
                        };
                        role.resolve_seat(seat, role.name(), Box::new(()), &mut board, &mut rctx);
                        if let Some(tx) = next_tx {
                            let _ = tx.send(Some(board));
                        }
                        return Ok((task_key, logs));
                    }

                    let user = &roster.seats[seat].user;
                    let upstream_for_prompt = if role.prompt_dep() == PromptDep::AfterUpstreamBoard {
                        Some(wait_ready(&mut upstream_rx).await)
                    } else {
                        None
                    };

                    let deadline = if upstream_for_prompt.is_some() {
                        stage_deadline.max(Instant::now() + FOLLOWUP_MIN_BUDGET)
                    } else {
                        stage_deadline
                    };
                    let cfg = stage_cfg.clone().reply_deadline(deadline);
                    let active_shields = upstream_for_prompt
                        .as_ref()
                        .map(|b| &b.shielded)
                        .unwrap_or(shielded);
                    let pctx = PromptCtx {
                        roster,
                        ask_cfg: &cfg,
                        deadline,
                        shielded: active_shields,
                    };

                    match user.exclusive().await {
                        Ok(dm) => {
                            let mut dm = dm.with_ask_config(&cfg).with_ignore_commands();
                            let intent = role
                                .prompt_seat(&mut dm, seat, &pctx, upstream_for_prompt.as_ref())
                                .await?;
                            drop(dm);

                            let mut board = match upstream_for_prompt {
                                Some(b) => b,
                                None => wait_ready(&mut upstream_rx).await,
                            };
                            let mut rctx = ResolveCtx {
                                roster,
                                logs: &mut logs,
                            };
                            role.resolve_seat(seat, role.name(), intent, &mut board, &mut rctx);
                            if let Some(tx) = next_tx {
                                let _ = tx.send(Some(board));
                            }
                        }
                        Err(e) if !e.is_fatal() => {
                            let mut board = match upstream_for_prompt {
                                Some(b) => b,
                                None => wait_ready(&mut upstream_rx).await,
                            };
                            let mut rctx = ResolveCtx {
                                roster,
                                logs: &mut logs,
                            };
                            role.resolve_unavailable_seat(seat, role.name(), &mut board, &mut rctx);
                            if let Some(tx) = next_tx {
                                let _ = tx.send(Some(board));
                            }
                        }
                        Err(fatal) => return Err(fatal),
                    }
                    Ok((task_key, logs))
                }));
            }
        }
    }

    current_board_rx
}
