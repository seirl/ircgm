//! Alien Expansion **Cow** (also covers Super Villains **Evilometer**!)
//!
//! Puts out their fist at night: if either immediate neighbor (`left` or `right`) holds an
//! `AlienPack` card, the Cow's fist is tapped!

use ircgm::Colorize;

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, CopyBehavior, ErasedIntent, ResolveCtx, RolePlugin, RoleTag, Team,
};

pub struct Cow;
pub static COW: Cow = Cow;

impl RolePlugin for Cow {
    fn id(&self) -> &'static str {
        "cow"
    }

    fn name(&self) -> &'static str {
        "Cow"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Stick out your hoof: you feel a tap if your immediate left OR right neighbor is an Alien!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(197)
    }

    fn copy_behavior(&self) -> CopyBehavior {
        CopyBehavior::WakeAfterAllMutations
    }

    fn needs_dm_prompt(&self) -> bool {
        false
    }

    fn resolve_seat(
        &self,
        seat: usize,
        actor_label: &str,
        _intent: ErasedIntent,
        board: &mut BoardCards,
        ctx: &mut ResolveCtx<'_>,
    ) {
        let (left, right) = ctx.roster.neighbors(seat);
        let neighbor_alien = [left, right].into_iter().any(|n| {
            board.players[n]
                .effective_role()
                .tags()
                .contains(&RoleTag::AlienPack)
        });

        let user = &ctx.roster.seats[seat].user;
        if neighbor_alien {
            let _ = user.say(
                "🐄🛸 *TAP!* ".bold().yellow()
                    + "An Alien sits immediately to your left or right! Moo!",
            );
        } else {
            let _ = user.say("🐄 Nobody tapped your hoof tonight (neither neighbor is an Alien).");
        }
        ctx.logs.push(format!(
            "{actor_label} ({}) hoof tap = {neighbor_alien}.",
            user.nick()
        ));
    }
}
