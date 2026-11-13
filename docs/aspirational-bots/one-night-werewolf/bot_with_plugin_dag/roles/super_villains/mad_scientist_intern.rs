//! Super Villains **Mad Scientist** & **Intern**
//! (Also covers Vampire's **Apprentice Assassin** and Alien's **Mortician** win-hook patterns!)

use ircgm::Colorize;

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, GroupPromptCtx, PromptDep, RolePlugin,
    RoleTag, Team, VictoryCtx,
};

pub struct MadScientist;
pub static MAD_SCIENTIST: MadScientist = MadScientist;

pub struct Intern;
pub static INTERN: Intern = Intern;

impl RolePlugin for MadScientist {
    fn id(&self) -> &'static str {
        "mad_scientist"
    }

    fn name(&self) -> &'static str {
        "Mad Scientist"
    }

    fn team(&self) -> Team {
        Team::Independent
    }

    fn tags(&self) -> &'static [RoleTag] {
        &[RoleTag::MadScientist]
    }

    fn night_summary(&self) -> &'static str {
        "Your invention requires a jolt—you ONLY win if YOU are killed!"
    }

    fn check_independent_win(&self, seat: usize, ctx: &VictoryCtx<'_>) -> bool {
        ctx.is_dead(seat)
    }
}

impl RolePlugin for Intern {
    fn id(&self) -> &'static str {
        "intern"
    }

    fn name(&self) -> &'static str {
        "Intern"
    }

    fn team(&self) -> Team {
        Team::Independent
    }

    fn night_summary(&self) -> &'static str {
        "Wake to see who the Mad Scientist is. You win if the Mad Scientist dies (or if no Mad Scientist is in play and YOU die)!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(136)
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

    fn prompt_group<'a>(
        &'a self,
        active_seats: Vec<usize>,
        ctx: &'a GroupPromptCtx<'a>,
        _upstream_board: Option<&'a BoardCards>,
    ) -> BoxFuture<'a, ircgm::Result<ErasedIntent>> {
        Box::pin(async move {
            let scientists = ctx.active_seats_with_tag(RoleTag::MadScientist);
            for &s in &active_seats {
                if scientists.is_empty() {
                    let _ = ctx.roster.seats[s].user.say(
                        "🥽 Intern: No Mad Scientist is in play — get YOURSELF killed to take over the lab and win!",
                    );
                } else {
                    let list: Vec<String> = scientists
                        .iter()
                        .map(|&m| format!("[{}] {}", m + 1, ctx.roster.seats[m].user.nick()))
                        .collect();
                    let _ = ctx.roster.seats[s].user.say(
                        "🥽 Intern: The Mad Scientist is "
                            + list.join(", ").bold().orange()
                            + " — help them get killed so you both win!",
                    );
                }
            }
            Ok(Box::new(()) as ErasedIntent)
        })
    }

    fn check_independent_win(&self, seat: usize, ctx: &VictoryCtx<'_>) -> bool {
        let scientists = ctx.find_seats_with_tag(RoleTag::MadScientist);
        if scientists.is_empty() {
            ctx.is_dead(seat)
        } else {
            scientists.into_iter().any(|m| ctx.is_dead(m))
        }
    }
}
