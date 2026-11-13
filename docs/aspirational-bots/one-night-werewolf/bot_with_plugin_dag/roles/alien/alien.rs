//! Alien Expansion **Alien** & **Synthetic Alien**
//!
//! - `ALIEN`: Wakes with the Alien pack (`AlienPack`, which ALSO includes `Groob`, `Zerb`, and
//!   `Synthetic Alien`!) and receives a randomized companion-app directive on the fly (either
//!   view another player's card OR scramble two seats!).
//! - `SYNTHETIC_ALIEN`: Tagged `AlienPack` (so Aliens, `Leader`, and `Cow` all think they are an
//!   Alien!), yet `Team::Independent`—only wins if killed!

use std::time::Duration;

use ircgm::{Colorize, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, GroupPromptCtx, PromptDep, ResolveCtx,
    RolePlugin, RoleTag, Team, VictoryCtx,
};

pub struct Alien;
pub static ALIEN: Alien = Alien;

enum AlienAppAction {
    None,
    PeekSeat { actor: usize, target: usize },
    RandomSwap(usize, usize),
}

impl RolePlugin for Alien {
    fn id(&self) -> &'static str {
        "alien"
    }

    fn name(&self) -> &'static str {
        "Alien"
    }

    fn team(&self) -> Team {
        Team::Werewolf
    }

    fn tags(&self) -> &'static [RoleTag] {
        &[RoleTag::WolfThreat, RoleTag::AlienPack]
    }

    fn night_summary(&self) -> &'static str {
        "Wake with the Aliens and follow a randomized Mother-Ship directive (e.g. scan a human's card or abduct/swap seats!)."
    }

    fn wake_order(&self) -> Option<u16> {
        Some(124)
    }

    fn copy_behavior(&self) -> CopyBehavior {
        CopyBehavior::JoinGroupWake
    }

    fn is_group_wake(&self) -> bool {
        true
    }

    fn prompt_dep(&self) -> PromptDep {
        PromptDep::AfterCopycats
    }

    fn mutates_board(&self) -> bool {
        true
    }

    fn prompt_group<'a>(
        &'a self,
        active_seats: Vec<usize>,
        ctx: &'a GroupPromptCtx<'a>,
        _upstream_board: Option<&'a BoardCards>,
    ) -> BoxFuture<'a, ircgm::Result<ErasedIntent>> {
        Box::pin(async move {
            let visible_aliens = ctx.active_seats_with_tag(RoleTag::AlienPack);
            let alien_list: Vec<String> = visible_aliens
                .iter()
                .map(|&s| format!("[{}] {}", s + 1, ctx.roster.seats[s].user.nick()))
                .collect();

            for &s in &active_seats {
                let _ = ctx.roster.seats[s].user.say(
                    "👽 Mother Ship Scan — Aliens aboard: "
                        + alien_list.join(", ").bold().green(),
                );
            }

            let non_aliens: Vec<usize> = (0..ctx.roster.len())
                .filter(|s| !visible_aliens.contains(s) && !ctx.shielded.contains(s))
                .collect();
            if non_aliens.is_empty() {
                return Ok(Box::new(AlienAppAction::None) as ErasedIntent);
            }

            // Companion-app style randomized directive!
            let commander = active_seats[0];
            if fastrand::bool() {
                let user = &ctx.roster.seats[commander].user;
                if let Ok(dm) = user.exclusive().await {
                    let mut dm = dm.with_ask_config(ctx.ask_cfg).with_ignore_commands();
                    let prompt = format!(
                        "👽 App Directive [BIO-SCAN]: Reply `<seat|nick>` to scan a non-Alien's card! Seats: {}",
                        ctx.roster.format_seats_marking_shields(ctx.shielded)
                    );
                    let turn = dm
                        .ask(prompt, |m| match ctx.roster.resolve_token(&m.text) {
                            Some(idx) if non_aliens.contains(&idx) => Match::Accept(idx),
                            Some(_) => Match::Reject(Text::from(
                                "Pick an unshielded non-Alien player!".yellow(),
                            )),
                            None => Match::Reject(Text::from("Unknown player.".yellow())),
                        })
                        .warn_at(Duration::from_secs(10), "⏳ 10s left for Alien bio-scan!")
                        .settled()
                        .await?;
                    if let Turn::Reply(target) = turn {
                        return Ok(Box::new(AlienAppAction::PeekSeat {
                            actor: commander,
                            target,
                        }) as ErasedIntent);
                    }
                }
                Ok(Box::new(AlienAppAction::None) as ErasedIntent)
            } else if non_aliens.len() >= 2 {
                let a = non_aliens[0];
                let b = non_aliens[non_aliens.len() - 1];
                for &s in &active_seats {
                    let _ = ctx.roster.seats[s].user.say(format!(
                        "🛸 App Directive [TRACTOR BEAM]: Automatically swapped [{}] {} and [{}] {}!",
                        a + 1,
                        ctx.roster.seats[a].user.nick(),
                        b + 1,
                        ctx.roster.seats[b].user.nick()
                    ));
                }
                Ok(Box::new(AlienAppAction::RandomSwap(a, b)) as ErasedIntent)
            } else {
                Ok(Box::new(AlienAppAction::None) as ErasedIntent)
            }
        })
    }

    fn resolve_group(
        &self,
        _active_seats: &[usize],
        intent: ErasedIntent,
        board: &mut BoardCards,
        ctx: &mut ResolveCtx<'_>,
    ) {
        let Ok(action) = intent.downcast::<AlienAppAction>() else {
            return;
        };
        match *action {
            AlienAppAction::PeekSeat { actor, target } if !board.is_shielded(target) => {
                board.record_card_interaction(actor);
                let seen = board.players[target];
                let _ = ctx.roster.seats[actor].user.say(
                    format!(
                        "👽 Bio-Scan Result: [{}] {} holds ",
                        target + 1,
                        ctx.roster.seats[target].user.nick()
                    ) + seen.display_for_peek()
                        + "!",
                );
                ctx.logs.push(format!(
                    "Aliens bio-scanned {} ({}).",
                    ctx.roster.seats[target].user.nick(),
                    seen.effective_role().name()
                ));
            }
            AlienAppAction::RandomSwap(a, b) if !board.is_shielded(a) && !board.is_shielded(b) => {
                board.players.swap(a, b);
                ctx.logs.push(format!(
                    "Aliens tractor-beamed a swap between {} and {}.",
                    ctx.roster.seats[a].user.nick(),
                    ctx.roster.seats[b].user.nick()
                ));
            }
            _ => {}
        }
    }
}

pub struct SyntheticAlien;
pub static SYNTHETIC_ALIEN: SyntheticAlien = SyntheticAlien;

impl RolePlugin for SyntheticAlien {
    fn id(&self) -> &'static str {
        "synthetic_alien"
    }

    fn name(&self) -> &'static str {
        "Synthetic Alien"
    }

    fn team(&self) -> Team {
        Team::Independent
    }

    /// Tagged `AlienPack` (NOT `WolfThreat`!) so Aliens, `Leader`, and `Cow` see them as an Alien,
    /// while their true win condition is getting themselves killed!
    fn tags(&self) -> &'static [RoleTag] {
        &[RoleTag::AlienPack]
    }

    fn night_summary(&self) -> &'static str {
        "You appear as an Alien to other Aliens, Leader, & Cow, but you ONLY win if YOU are killed!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(125)
    }

    fn prompt_dep(&self) -> PromptDep {
        PromptDep::AfterCopycats
    }

    fn is_group_wake(&self) -> bool {
        true
    }

    fn prompt_group<'a>(
        &'a self,
        active_seats: Vec<usize>,
        ctx: &'a GroupPromptCtx<'a>,
        _upstream_board: Option<&'a BoardCards>,
    ) -> BoxFuture<'a, ircgm::Result<ErasedIntent>> {
        Box::pin(async move {
            let visible_aliens = ctx.active_seats_with_tag(RoleTag::AlienPack);
            let list: Vec<String> = visible_aliens
                .iter()
                .map(|&s| format!("[{}] {}", s + 1, ctx.roster.seats[s].user.nick()))
                .collect();
            for &s in &active_seats {
                let _ = ctx.roster.seats[s].user.say(
                    "🤖👽 Synthetic Alien: You blended into the Alien scan ("
                        + list.join(", ").bold().orange()
                        + ")! Get YOURSELF killed at Dawn to win!",
                );
            }
            Ok(Box::new(()) as ErasedIntent)
        })
    }

    fn check_independent_win(&self, seat: usize, ctx: &VictoryCtx<'_>) -> bool {
        ctx.is_dead(seat)
    }
}
