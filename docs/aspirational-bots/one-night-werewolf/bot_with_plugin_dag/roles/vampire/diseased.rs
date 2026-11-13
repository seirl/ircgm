use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, Mark, PromptCtx, ResolveCtx, RolePlugin,
    Team,
};

pub struct Diseased;
pub static DISEASED: Diseased = Diseased;

impl RolePlugin for Diseased {
    fn id(&self) -> &'static str {
        "diseased"
    }

    fn name(&self) -> &'static str {
        "Diseased"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Give a neighbor (or other player) the Mark of Disease; anyone voting for a Diseased/marked player (or sitting beside a killed Diseased) cannot win!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(38)
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
                "🤢 Diseased: Reply `<seat|nick>` to place the Mark of Disease on another player! Seats: {}",
                ctx.roster.format_seats()
            );
            let turn = dm
                .ask(prompt, move |m| match ctx.roster.resolve_token(&m.text) {
                    Some(idx) if idx == seat => {
                        Match::Reject(Text::from("Pick ANOTHER player!".yellow()))
                    }
                    Some(idx) => Match::Accept(idx),
                    None => Match::Reject(Text::from("Unknown seat or nickname.".yellow())),
                })
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left to place Mark of Disease!")
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
        let Ok(turn) = intent.downcast::<Turn<usize>>() else {
            return;
        };
        let target = match *turn {
            Turn::Reply(idx) => idx,
            Turn::TimedOut | Turn::Left { .. } => ctx.roster.neighbors(seat).0,
        };
        board.marks[target] = Mark::Disease;
        ctx.logs.push(format!(
            "{actor_label} ({}) placed Mark of Disease on {}.",
            ctx.roster.seats[seat].user.nick(),
            ctx.roster.seats[target].user.nick()
        ));
    }
}
