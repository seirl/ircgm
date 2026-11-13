use crate::bot_with_plugin_dag::plugin::{RolePlugin, RoleTag, Team};

pub struct DreamWolf;
pub static DREAM_WOLF: DreamWolf = DreamWolf;

impl RolePlugin for DreamWolf {
    fn id(&self) -> &'static str {
        "dream_wolf"
    }

    fn name(&self) -> &'static str {
        "Dream Wolf"
    }

    fn team(&self) -> Team {
        Team::Werewolf
    }

    /// Notice how `RoleTag` handles Dream Wolf with zero engine special cases:
    /// - `WolfThreat`: Village must still kill a Werewolf (including Dream Wolf) to win!
    /// - `DreamWolfThumb` (and NOT `WolfPack`): Dream Wolf sleeps through the Werewolf pack wake,
    ///   while `Minion` sees `WolfPack ∪ DreamWolfThumb`!
    fn tags(&self) -> &'static [RoleTag] {
        &[RoleTag::WolfThreat, RoleTag::DreamWolfThumb]
    }

    fn night_summary(&self) -> &'static str {
        "You sleep through the night (do NOT wake with other Werewolves), though the Minion still sees your thumb!"
    }
}
