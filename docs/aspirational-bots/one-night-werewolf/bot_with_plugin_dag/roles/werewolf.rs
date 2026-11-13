use std::time::Duration;

use ircgm::{Colorize, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, GroupPromptCtx, PromptDep, ResolveCtx,
    RolePlugin, RoleTag, Team,
};

pub struct Werewolf;
pub static WEREWOLF: Werewolf = Werewolf;

enum WolfIntent {
    Pack,
    LonePeek(Turn<Option<usize>>),
}

impl RolePlugin for Werewolf {
    fn id(&self) -> &'static str {
        "werewolf"
    }

    fn name(&self) -> &'static str {
        "Werewolf"
    }

    fn team(&self) -> Team {
        Team::Werewolf
    }

    fn tags(&self) -> &'static [RoleTag] {
        &[RoleTag::WolfThreat, RoleTag::WolfPack]
    }

    fn night_summary(&self) -> &'static str {
        "See your fellow Werewolves; if you are the Lone Wolf, you may peek at 1 center card."
    }

    fn wake_order(&self) -> Option<u16> {
        Some(120)
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
            if active_seats.len() >= 2 {
                for &w_seat in &active_seats {
                    let partners: Vec<String> = active_seats
                        .iter()
                        .filter(|&&o| o != w_seat)
                        .map(|&o| format!("[{}] {}", o + 1, ctx.roster.seats[o].user.nick()))
                        .collect();
                    let _ = ctx.roster.seats[w_seat]
                        .user
                        .say("🐺 Fellow Werewolf pack: " + partners.join(", ").bold().red());
                }
                Ok(Box::new(WolfIntent::Pack) as ErasedIntent)
            } else {
                // Lone Wolf rule: may look at 1 center card!
                let wolf_user = &ctx.roster.seats[active_seats[0]].user;
                let choice = wolf_user
                    .ask(
                        "🐺 You are the "
                            + "LONE WOLF".bold().red()
                            + "! Pick ONE center card to peek (`1`, `2`, `3`, or `pass`):",
                        |m| match m.text.trim().to_ascii_lowercase().as_str() {
                            "1" | "c1" | "center 1" => Match::Accept(Some(0usize)),
                            "2" | "c2" | "center 2" => Match::Accept(Some(1usize)),
                            "3" | "c3" | "center 3" => Match::Accept(2usize)),
                            "pass" | "skip" | "none" => Match::Accept(None),
                            _ => Match::Reject(Text::from("Reply `1`, `2`, `3`, or `pass`.".yellow())),
                        },
                    )
                    .with_config(ctx.ask_cfg)
                    .warn_at(Duration::from_secs(10), "⏳ 10s left to peek a center card!")
                    .settled()
                    .await?;
                Ok(Box::new(WolfIntent::LonePeek(choice)) as ErasedIntent)
            }
        })
    }

    fn resolve_group(
        &self,
        active_seats: &[usize],
        intent: ErasedIntent,
        board: &mut BoardCards,
        ctx: &mut ResolveCtx<'_>,
    ) {
        let Ok(intent) = intent.downcast::<WolfIntent>() else {
            return;
        };
        match *intent {
            WolfIntent::Pack => {
                let names: Vec<String> = active_seats
                    .iter()
                    .map(|&s| ctx.roster.seats[s].user.nick())
                    .collect();
                ctx.logs
                    .push(format!("Werewolves ({}) saw each other.", names.join(", ")));
            }
            WolfIntent::LonePeek(Turn::Reply(Some(c_idx))) => {
                let wolf_user = &ctx.roster.seats[active_seats[0]].user;
                let seen = board.center[c_idx];
                let _ = wolf_user.say(
                    format!("🐺 Center #{} is: ", c_idx + 1) + seen.display_for_peek(),
                );
                ctx.logs.push(format!(
                    "Lone Wolf ({}) peeked Center #{} ({}).",
                    wolf_user.nick(),
                    c_idx + 1,
                    seen.effective_role().name()
                ));
            }
            WolfIntent::LonePeek(_) => {
                let wolf_user = &ctx.roster.seats[active_seats[0]].user;
                ctx.logs
                    .push(format!("Lone Wolf ({}) did not peek center.", wolf_user.nick()));
            }
        }
    }
}
