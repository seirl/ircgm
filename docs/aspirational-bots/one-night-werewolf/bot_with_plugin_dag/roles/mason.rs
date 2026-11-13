use ircgm::Colorize;

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, GroupPromptCtx, PromptDep, ResolveCtx,
    RolePlugin, Team,
};

pub struct Mason;
pub static MASON: Mason = Mason;

impl RolePlugin for Mason {
    fn id(&self) -> &'static str {
        "mason"
    }

    fn name(&self) -> &'static str {
        "Mason"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Wake to see the other Mason. If you are alone, the other Mason card is in the center."
    }

    fn requires_pair_in_deck(&self) -> bool {
        true
    }

    fn wake_order(&self) -> Option<u16> {
        Some(140)
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
            if active_seats.len() == 1 {
                let _ = ctx.roster.seats[active_seats[0]].user.say(
                    "🧱 Mason: You are the "
                        + "ONLY Mason".bold().light_cyan()
                        + " in play (the other Mason card is in the center!).",
                );
            } else if active_seats.len() >= 2 {
                for &seat in &active_seats {
                    let others: Vec<String> = active_seats
                        .iter()
                        .filter(|&&o| o != seat)
                        .map(|&o| format!("[{}] {}", o + 1, ctx.roster.seats[o].user.nick()))
                        .collect();
                    let _ = ctx.roster.seats[seat]
                        .user
                        .say("🧱 Fellow Mason(s): " + others.join(", ").bold().light_cyan());
                }
            }
            Ok(Box::new(()) as ErasedIntent)
        })
    }

    fn resolve_group(
        &self,
        active_seats: &[usize],
        _intent: ErasedIntent,
        _board: &mut BoardCards,
        ctx: &mut ResolveCtx<'_>,
    ) {
        let names: Vec<String> = active_seats
            .iter()
            .map(|&s| ctx.roster.seats[s].user.nick())
            .collect();
        ctx.logs
            .push(format!("Masons ({}) woke up.", names.join(", ")));
    }
}
