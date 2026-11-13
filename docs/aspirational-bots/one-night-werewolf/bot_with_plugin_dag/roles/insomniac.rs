use crate::bot_with_plugin_dag::plugin::{
    BoardCards, CopyBehavior, ErasedIntent, ResolveCtx, RolePlugin, Team,
};

pub struct Insomniac;
pub static INSOMNIAC: Insomniac = Insomniac;

impl RolePlugin for Insomniac {
    fn id(&self) -> &'static str {
        "insomniac"
    }

    fn name(&self) -> &'static str {
        "Insomniac"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Wake at the very end of the night to check your own card and see if it was swapped."
    }

    fn wake_order(&self) -> Option<u16> {
        Some(190)
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
        board.record_card_interaction(seat);
        let seen = board.players[seat];
        let _ = user.say(
            format!("🦉 {actor_label}: You check your card before dawn — it is ")
                + seen.display_for_peek()
                + "!",
        );
        ctx.logs.push(format!(
            "{actor_label} ({}) checked their card ({}).",
            user.nick(),
            seen.effective_role().name()
        ));
    }
}
