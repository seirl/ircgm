use ircgm::Colorize;

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, ErasedIntent, GroupPromptCtx, PromptDep, RolePlugin, RoleTag, Team,
    VictoryCtx,
};

pub struct Groob;
pub static GROOB: Groob = Groob;

pub struct Zerb;
pub static ZERB: Zerb = Zerb;

impl RolePlugin for Groob {
    fn id(&self) -> &'static str {
        "groob"
    }

    fn name(&self) -> &'static str {
        "Groob"
    }

    fn team(&self) -> Team {
        Team::Independent
    }

    fn tags(&self) -> &'static [RoleTag] {
        &[RoleTag::AlienPack]
    }

    fn night_summary(&self) -> &'static str {
        "Rival Alien: you wake with the Aliens, know who Zerb is, and win ONLY if Zerb dies!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(126)
    }

    fn is_group_wake(&self) -> bool {
        true
    }

    fn prompt_dep(&self) -> PromptDep {
        PromptDep::AfterCopycats
    }

    fn prompt_group<'a>(
        &'a self,
        active_seats: Vec<usize>,
        ctx: &'a GroupPromptCtx<'a>,
        _upstream_board: Option<&'a BoardCards>,
    ) -> BoxFuture<'a, ircgm::Result<ErasedIntent>> {
        Box::pin(async move {
            let zerb_seats = ctx.roster.initial_holders(&ZERB);
            let zerb_info = if zerb_seats.is_empty() {
                "Zerb is in the center!".to_owned()
            } else {
                zerb_seats
                    .iter()
                    .map(|&z| format!("[{}] {}", z + 1, ctx.roster.seats[z].user.nick()))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            for &s in &active_seats {
                let _ = ctx.roster.seats[s].user.say(
                    "👾 Groob: Your mortal nemesis Zerb is "
                        + zerb_info.bold().red()
                        + " — get Zerb killed to win!",
                );
            }
            Ok(Box::new(()) as ErasedIntent)
        })
    }

    fn check_independent_win(&self, _seat: usize, ctx: &VictoryCtx<'_>) -> bool {
        let zerbs = ctx.find_seats_by_role_id("zerb");
        !zerbs.is_empty() && zerbs.into_iter().any(|z| ctx.is_dead(z))
    }
}

impl RolePlugin for Zerb {
    fn id(&self) -> &'static str {
        "zerb"
    }

    fn name(&self) -> &'static str {
        "Zerb"
    }

    fn team(&self) -> Team {
        Team::Independent
    }

    fn tags(&self) -> &'static [RoleTag] {
        &[RoleTag::AlienPack]
    }

    fn night_summary(&self) -> &'static str {
        "Rival Alien: you wake with the Aliens, know who Groob is, and win ONLY if Groob dies!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(127)
    }

    fn is_group_wake(&self) -> bool {
        true
    }

    fn prompt_dep(&self) -> PromptDep {
        PromptDep::AfterCopycats
    }

    fn prompt_group<'a>(
        &'a self,
        active_seats: Vec<usize>,
        ctx: &'a GroupPromptCtx<'a>,
        _upstream_board: Option<&'a BoardCards>,
    ) -> BoxFuture<'a, ircgm::Result<ErasedIntent>> {
        Box::pin(async move {
            let groob_seats = ctx.roster.initial_holders(&GROOB);
            let groob_info = if groob_seats.is_empty() {
                "Groob is in the center!".to_owned()
            } else {
                groob_seats
                    .iter()
                    .map(|&g| format!("[{}] {}", g + 1, ctx.roster.seats[g].user.nick()))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            for &s in &active_seats {
                let _ = ctx.roster.seats[s].user.say(
                    "👾 Zerb: Your mortal nemesis Groob is "
                        + groob_info.bold().red()
                        + " — get Groob killed to win!",
                );
            }
            Ok(Box::new(()) as ErasedIntent)
        })
    }

    fn check_independent_win(&self, _seat: usize, ctx: &VictoryCtx<'_>) -> bool {
        let groobs = ctx.find_seats_by_role_id("groob");
        !groobs.is_empty() && groobs.into_iter().any(|g| ctx.is_dead(g))
    }
}
