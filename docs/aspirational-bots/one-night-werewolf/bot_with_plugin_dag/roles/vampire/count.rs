use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, Mark, PromptCtx, PromptDep, ResolveCtx,
    RolePlugin, RoleTag, Team,
};

pub struct Count;
pub static COUNT: Count = Count;

impl RolePlugin for Count {
    fn id(&self) -> &'static str {
        "count"
    }

    fn name(&self) -> &'static str {
        "Count"
    }

    fn team(&self) -> Team {
        Team::Werewolf
    }

    fn tags(&self) -> &'static [RoleTag] {
        &[RoleTag::WolfThreat, RoleTag::VampirePack]
    }

    fn night_summary(&self) -> &'static str {
        "Vampire who places the Mark of Fear on a non-Vampire player in Dusk, preventing them from waking in Night!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(30)
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
            let prompt = format!(
                "🦇 Count: Reply `<seat|nick>` to place the Mark of Fear (suppressing their Night wake!), or `pass`. Seats: {}",
                ctx.roster.format_seats()
            );
            let turn = dm
                .ask(prompt, move |m| {
                    let t = m.text.trim();
                    if matches!(t.to_ascii_lowercase().as_str(), "pass" | "skip" | "none") {
                        return Match::Accept(None);
                    }
                    match ctx.roster.resolve_token(t) {
                        Some(idx) if idx == seat => {
                            Match::Reject(Text::from("Pick a non-Vampire player!".yellow()))
                        }
                        Some(idx)
                            if ctx.roster.seats[idx]
                                .initial_role
                                .tags()
                                .contains(&RoleTag::VampirePack) =>
                        {
                            Match::Reject(Text::from("You cannot terrify a fellow Vampire!".yellow()))
                        }
                        Some(idx) => Match::Accept(Some(idx)),
                        None => Match::Reject(Text::from("Unknown player (or `pass`).".yellow())),
                    }
                })
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left for Count's Mark of Fear!")
                .settled()
                .await?;
            Ok(Box::new(turn) as ErasedIntent)
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
        let Ok(turn) = intent.downcast::<Turn<Option<usize>>>() else {
            return;
        };
        if let Turn::Reply(Some(target)) = *turn {
            board.marks[target] = Mark::Fear;
            ctx.logs.push(format!(
                "{actor_label} ({}) placed Mark of Fear on {}.",
                ctx.roster.seats[seat].user.nick(),
                ctx.roster.seats[target].user.nick()
            ));
        }
    }
}
