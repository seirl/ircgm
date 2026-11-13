use ircgm::Colorize;

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, GroupPromptCtx, PromptDep, ResolveCtx,
    RolePlugin, RoleTag, Team,
};

pub struct Minion;
pub static MINION: Minion = Minion;

impl RolePlugin for Minion {
    fn id(&self) -> &'static str {
        "minion"
    }

    fn name(&self) -> &'static str {
        "Minion"
    }

    fn team(&self) -> Team {
        Team::Werewolf
    }

    fn tags(&self) -> &'static [RoleTag] {
        &[RoleTag::SoloThreatFallback]
    }

    fn night_summary(&self) -> &'static str {
        "Wake to learn who the Werewolves are (they do not know who you are). Win if Werewolves survive!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(130)
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
            // Discovers ALL werewolf-pack roles (`WolfPack` + sleeping `DreamWolfThumb`) via tags!
            let mut wolves = ctx.active_seats_with_tag(RoleTag::WolfPack);
            for dw in ctx.active_seats_with_tag(RoleTag::DreamWolfThumb) {
                if !wolves.contains(&dw) {
                    wolves.push(dw);
                }
            }
            wolves.sort_unstable();
            for &m_seat in &active_seats {
                if wolves.is_empty() {
                    let _ = ctx.roster.seats[m_seat].user.say(
                        "🗡️ Minion: "
                            + "NO players hold Werewolf cards".bold().yellow()
                            + "! Survive and get another non-Tanner player killed to win!",
                    );
                } else {
                    let wolf_list: Vec<String> = wolves
                        .iter()
                        .map(|&w| format!("[{}] {}", w + 1, ctx.roster.seats[w].user.nick()))
                        .collect();
                    let _ = ctx.roster.seats[m_seat].user.say(
                        "🗡️ Minion: The Werewolves are "
                            + wolf_list.join(", ").bold().red()
                            + " (they do not know who you are!).",
                    );
                }
            }
            Ok(Box::new(wolves) as ErasedIntent)
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
            .push(format!("Minion ({}) saw the Werewolf pack.", names.join(", ")));
    }
}
