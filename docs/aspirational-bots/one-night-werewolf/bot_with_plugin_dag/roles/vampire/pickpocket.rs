use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, PromptCtx, ResolveCtx, RolePlugin, Team,
};

pub struct Pickpocket;
pub static PICKPOCKET: Pickpocket = Pickpocket;

impl RolePlugin for Pickpocket {
    fn id(&self) -> &'static str {
        "pickpocket"
    }

    fn name(&self) -> &'static str {
        "Pickpocket"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Swap your Mark with another player's Mark at the end of Dusk and view your new Mark!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(60)
    }

    fn copy_behavior(&self) -> CopyBehavior {
        CopyBehavior::ActImmediately
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
                "🤏 Pickpocket: Reply `<seat|nick>` to swap Marks and view your new Mark, or `pass`. Seats: {}",
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
                            Match::Reject(Text::from("Pick ANOTHER player!".yellow()))
                        }
                        Some(idx) => Match::Accept(Some(idx)),
                        None => Match::Reject(Text::from("Unknown player (or `pass`).".yellow())),
                    }
                })
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left for Pickpocket swap!")
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
        let actor = &ctx.roster.seats[seat].user;
        if let Turn::Reply(Some(target)) = *turn {
            board.marks.swap(seat, target);
            let new_mark = board.marks[seat];
            let _ = actor.say(
                format!(
                    "🤏 Swapped Marks with [{}] {} — your new Mark is ",
                    target + 1,
                    ctx.roster.seats[target].user.nick()
                ) + new_mark.styled()
                    + "!",
            );
            ctx.logs.push(format!(
                "{actor_label} ({}) swapped Marks with {} (got {}).",
                actor.nick(),
                ctx.roster.seats[target].user.nick(),
                new_mark.name()
            ));
        }
    }
}
