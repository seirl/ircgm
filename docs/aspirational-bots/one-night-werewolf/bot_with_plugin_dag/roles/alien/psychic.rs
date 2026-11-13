use crate::bot_with_plugin_dag::plugin::{
    BoardCards, CopyBehavior, ErasedIntent, OracleOmen, PromptDep, ResolveCtx, RolePlugin, Team,
};

pub struct Psychic;
pub static PSYCHIC: Psychic = Psychic;

impl RolePlugin for Psychic {
    fn id(&self) -> &'static str {
        "psychic"
    }

    fn name(&self) -> &'static str {
        "Psychic"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "View 1 random neighbor's card (or BOTH neighbors' cards if the Oracle amplified signals tonight!)."
    }

    fn wake_order(&self) -> Option<u16> {
        Some(145)
    }

    fn copy_behavior(&self) -> CopyBehavior {
        CopyBehavior::ActImmediately
    }

    fn prompt_dep(&self) -> PromptDep {
        PromptDep::AfterUpstreamBoard
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
        let (left, right) = ctx.roster.neighbors(seat);
        let mut candidates: Vec<usize> = match board.oracle_omen {
            OracleOmen::AmplifiedSignals => vec![left, right],
            OracleOmen::Normal | OracleOmen::QuantumStatic => {
                vec![if fastrand::bool() { left } else { right }]
            }
        };
        candidates.dedup();
        candidates.retain(|&s| !board.is_shielded(s));

        if candidates.is_empty() {
            let _ = user.say("🧠 Psychic: Neighboring minds were shielded tonight!");
            return;
        }

        board.record_card_interaction(seat);
        for &target in &candidates {
            let seen = board.players[target];
            let _ = user.say(
                format!(
                    "🧠 Psychic Vision: [{}] {} holds ",
                    target + 1,
                    ctx.roster.seats[target].user.nick()
                ) + seen.display_for_peek()
                    + "!",
            );
        }
        ctx.logs.push(format!(
            "{actor_label} ({}) psychically read {} card(s).",
            user.nick(),
            candidates.len()
        ));
    }
}
