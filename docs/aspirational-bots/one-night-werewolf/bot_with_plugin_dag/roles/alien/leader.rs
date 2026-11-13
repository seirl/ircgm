use ircgm::Colorize;

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, GroupPromptCtx, PromptDep, RolePlugin,
    RoleTag, Team, VictoryCtx,
};

pub struct Leader;
pub static LEADER: Leader = Leader;

impl RolePlugin for Leader {
    fn id(&self) -> &'static str {
        "leader"
    }

    fn name(&self) -> &'static str {
        "Leader"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Knows who the Aliens are! WARNING: if ALL Aliens point at you during the vote, the Aliens win regardless of who dies!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(135)
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
            let aliens = ctx.active_seats_with_tag(RoleTag::AlienPack);
            for &s in &active_seats {
                if aliens.is_empty() {
                    let _ = ctx.roster.seats[s]
                        .user
                        .say("🫡 Leader: No Aliens are held by players tonight!");
                } else {
                    let names: Vec<String> = aliens
                        .iter()
                        .map(|&a| format!("[{}] {}", a + 1, ctx.roster.seats[a].user.nick()))
                        .collect();
                    let _ = ctx.roster.seats[s].user.say(
                        "🫡 Leader: The Aliens are "
                            + names.join(", ").bold().red()
                            + " — stay hidden so they don't all vote for you!",
                    );
                }
            }
            Ok(Box::new(()) as ErasedIntent)
        })
    }

    /// Global team-win override: if every Alien threat (`AlienPack` + `WolfThreat`) points at this
    /// `Leader`, the Alien/Evil team steals victory regardless of who died!
    fn check_team_win_override(&self, seat: usize, ctx: &VictoryCtx<'_>) -> Option<(Team, String)> {
        let alien_threats: Vec<usize> = (0..ctx.roster.len())
            .filter(|&i| {
                let tags = ctx.board.players[i].effective_role().tags();
                tags.contains(&RoleTag::AlienPack) && tags.contains(&RoleTag::WolfThreat)
            })
            .collect();

        if !alien_threats.is_empty() && alien_threats.iter().all(|&a| ctx.votes[a] == Some(seat)) {
            Some((
                Team::Werewolf,
                format!(
                    "🛸 All Aliens pointed at the Leader ({}) — Aliens abduct the Leader and WIN!",
                    ctx.roster.seats[seat].user.nick()
                ),
            ))
        } else {
            None
        }
    }
}
