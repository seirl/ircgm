use crate::bot_with_plugin_dag::plugin::{RolePlugin, Team};

pub struct Villager;
pub static VILLAGER: Villager = Villager;

impl RolePlugin for Villager {
    fn id(&self) -> &'static str {
        "villager"
    }

    fn name(&self) -> &'static str {
        "Villager"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Sleep at night. Deduce who holds a Werewolf card and vote them out!"
    }
}
