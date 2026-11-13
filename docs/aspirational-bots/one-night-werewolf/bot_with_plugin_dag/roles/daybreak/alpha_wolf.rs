use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, PromptCtx, PromptDep, ResolveCtx,
    RolePlugin, RoleTag, Team,
};

pub struct AlphaWolf;
pub static ALPHA_WOLF: AlphaWolf = AlphaWolf;

impl RolePlugin for AlphaWolf {
    fn id(&self) -> &'static str {
        "alpha_wolf"
    }

    fn name(&self) -> &'static str {
        "Alpha Wolf"
    }

    fn team(&self) -> Team {
        Team::Werewolf
    }

    fn tags(&self) -> &'static [RoleTag] {
        &[RoleTag::WolfThreat, RoleTag::WolfPack]
    }

    fn night_summary(&self) -> &'static str {
        "Wake with the Werewolves, then MUST swap the center Wolf card with another non-Werewolf player's card!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(122)
    }

    fn copy_behavior(&self) -> CopyBehavior {
        CopyBehavior::ActImmediately
    }

    fn prompt_dep(&self) -> PromptDep {
        PromptDep::AfterCopycats
    }

    fn mutates_board(&self) -> bool {
        true
    }

    fn prompt_seat<'a>(
        &'a self,
        dm: &'a mut ExclusiveDm,
        seat: usize,
        ctx: &'a PromptCtx<'a>,
        _upstream_board: Option<&'a BoardCards>,
    ) -> BoxFuture<'a, ircgm::Result<ErasedIntent>> {
        Box::pin(async move {
            let valid_targets: Vec<usize> = (0..ctx.roster.len())
                .filter(|&i| {
                    i != seat
                        && !ctx.shielded.contains(&i)
                        && !ctx.roster.seats[i]
                            .initial_role
                            .tags()
                            .contains(&RoleTag::WolfPack)
                })
                .collect();

            if valid_targets.is_empty() {
                let _ = dm.say("🐺 Alpha Wolf: No valid unshielded non-Wolf targets exist tonight!");
                return Ok(Box::new(None::<usize>) as ErasedIntent);
            }

            let prompt = format!(
                "🐺 Alpha Wolf: You MUST swap the center Werewolf card into a non-Wolf player's seat! Seats: {}",
                ctx.roster.format_seats_marking_shields(ctx.shielded)
            );
            let turn = dm
                .ask(prompt, move |m| match ctx.roster.resolve_token(&m.text) {
                    Some(idx) if valid_targets.contains(&idx) => Match::Accept(idx),
                    Some(idx) if ctx.shielded.contains(&idx) => Match::Reject(Text::from(
                        "🛡️ That seat is shielded by the Sentinel!".yellow(),
                    )),
                    Some(_) => Match::Reject(Text::from(
                        "Pick another non-Werewolf player!".yellow(),
                    )),
                    None => Match::Reject(Text::from("Unknown seat or nickname.".yellow())),
                })
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left for Alpha Wolf bite!")
                .settled()
                .await?;

            let chosen = match turn {
                Turn::Reply(idx) => idx,
                Turn::TimedOut | Turn::Left { .. } => {
                    let auto = valid_targets[fastrand::usize(..valid_targets.len())];
                    let _ = dm.say(format!(
                        "🐺 Randomly turned [{}] {} into a Werewolf!",
                        auto + 1,
                        ctx.roster.seats[auto].user.nick()
                    ));
                    auto
                }
            };
            let _ = dm.say(
                "🐺 Turned "
                    + ctx.roster.seats[chosen].user.nick().bold().red()
                    + " into a Werewolf!",
            );
            Ok(Box::new(Some(chosen)) as ErasedIntent)
        })
    }

    fn resolve_seat(
        &self,
        seat: usize,
        actor_label: &str,
        intent: ErasedIntent,
        board: &mut BoardCards,
        ctx: &mut ResolveCtx<'_>,
    ) {
        let Ok(target_opt) = intent.downcast::<Option<usize>>() else {
            return;
        };
        if let Some(target) = *target_opt {
            if !board.is_shielded(target) {
                board.record_card_interaction(seat);
                std::mem::swap(&mut board.players[target], &mut board.extra_evil_card);
                ctx.logs.push(format!(
                    "{actor_label} ({}) swapped the extra Wolf card onto {}.",
                    ctx.roster.seats[seat].user.nick(),
                    ctx.roster.seats[target].user.nick()
                ));
            }
        }
    }

    fn resolve_unavailable_seat(
        &self,
        seat: usize,
        actor_label: &str,
        board: &mut BoardCards,
        ctx: &mut ResolveCtx<'_>,
    ) {
        let valid: Vec<usize> = (0..ctx.roster.len())
            .filter(|&i| i != seat && !board.is_shielded(i))
            .collect();
        if !valid.is_empty() {
            let target = valid[fastrand::usize(..valid.len())];
            board.record_card_interaction(seat);
            std::mem::swap(&mut board.players[target], &mut board.extra_evil_card);
            ctx.logs.push(format!(
                "{actor_label} ({}) auto-swapped the extra Wolf card onto {}.",
                ctx.roster.seats[seat].user.nick(),
                ctx.roster.seats[target].user.nick()
            ));
        }
    }
}
