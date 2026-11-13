use ircgm::Colorize;

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, CopyBehavior, ErasedIntent, ResolveCtx, RolePlugin, Team,
};

pub struct AuraSeer;
pub static AURA_SEER: AuraSeer = AuraSeer;

impl RolePlugin for AuraSeer {
    fn id(&self) -> &'static str {
        "aura_seer"
    }

    fn name(&self) -> &'static str {
        "Aura Seer"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Wake at the end of Night to see every player who viewed or moved a card tonight!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(195)
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
        let user = &ctx.roster.seats[seat].user;
        let mut active: Vec<usize> = board
            .viewed_or_moved_actors
            .iter()
            .copied()
            .filter(|&s| s != seat)
            .collect();
        active.sort_unstable();

        if active.is_empty() {
            let _ = user.say("✨ Aura Seer: Nobody else viewed or moved any cards tonight!");
        } else {
            let names: Vec<String> = active
                .iter()
                .map(|&s| format!("[{}] {}", s + 1, ctx.roster.seats[s].user.nick()))
                .collect();
            let _ = user.say(
                "✨ Aura Seer: Players who viewed or moved a card tonight: "
                    + names.join(", ").bold().light_cyan(),
            );
        }
        ctx.logs.push(format!(
            "{actor_label} ({}) sensed {} active aura(s).",
            user.nick(),
            active.len()
        ));
    }
}
