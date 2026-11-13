use crate::bot_with_plugin_dag::plugin::{RolePlugin, Team, VictoryCtx};

pub struct Tanner;
pub static TANNER: Tanner = Tanner;

impl RolePlugin for Tanner {
    fn id(&self) -> &'static str {
        "tanner"
    }

    fn name(&self) -> &'static str {
        "Tanner"
    }

    fn team(&self) -> Team {
        Team::Independent
    }

    fn night_summary(&self) -> &'static str {
        "You hate your job. You ONLY win if the village kills you!"
    }

    fn check_independent_win(&self, seat: usize, ctx: &VictoryCtx<'_>) -> bool {
        ctx.is_dead(seat)
    }

    fn independent_win_blocks(&self, team: Team) -> bool {
        // ONUW Tanner rule: if Tanner dies and no Werewolf dies, Werewolf Team loses;
        // if a Werewolf ALSO dies, Village still wins alongside Tanner!
        team == Team::Werewolf
    }
}
