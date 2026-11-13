use ircgm::Colorize;

use crate::bot_with_plugin_dag::plugin::{DeathCtx, DeathReaction, RolePlugin, Team};

pub struct Hunter;
pub static HUNTER: Hunter = Hunter;

impl RolePlugin for Hunter {
    fn id(&self) -> &'static str {
        "hunter"
    }

    fn name(&self) -> &'static str {
        "Hunter"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Sleep at night. If you are killed, whoever YOU pointed at also dies immediately!"
    }

    fn on_death(&self, dead_seat: usize, ctx: &DeathCtx<'_>) -> Option<DeathReaction> {
        let target_seat = ctx.votes[dead_seat]?;
        let hunter_nick = ctx.roster.seats[dead_seat].user.nick();
        let target_nick = ctx.roster.seats[target_seat].user.nick();
        Some(DeathReaction {
            kill_seat: target_seat,
            announcement: "🏹 HUNTER SHOT! ".bold().orange()
                + hunter_nick.bold()
                + " died as Hunter and took down "
                + target_nick.bold().red()
                + "!",
        })
    }
}
