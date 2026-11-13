use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, Mark, PromptCtx, ResolveCtx, RolePlugin,
    RoleTag, Team, VictoryCtx,
};

pub struct Assassin;
pub static ASSASSIN: Assassin = Assassin;

impl RolePlugin for Assassin {
    fn id(&self) -> &'static str {
        "assassin"
    }

    fn name(&self) -> &'static str {
        "Assassin"
    }

    fn team(&self) -> Team {
        Team::Independent
    }

    fn tags(&self) -> &'static [RoleTag] {
        &[RoleTag::OriginalAssassin]
    }

    fn night_summary(&self) -> &'static str {
        "Place the Mark of the Assassin on a player in Dusk. You win ONLY if the player holding that Mark dies!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(40)
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
                "🗡️ Assassin: Reply `<seat|nick>` to place the Mark of the Assassin! Seats: {}",
                ctx.roster.format_seats()
            );
            let turn = dm
                .ask(prompt, move |m| match ctx.roster.resolve_token(&m.text) {
                    Some(idx) if idx == seat => {
                        Match::Reject(Text::from("Pick ANOTHER player to assassinate!".yellow()))
                    }
                    Some(idx) => Match::Accept(idx),
                    None => Match::Reject(Text::from("Unknown seat or nickname.".yellow())),
                })
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left to mark your target!")
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
            Turn::TimedOut | Turn::Left { .. } => ctx.roster.neighbors(seat).1,
        };
        board.marks[target] = Mark::Assassin;
        ctx.logs.push(format!(
            "{actor_label} ({}) placed Mark of the Assassin on {}.",
            ctx.roster.seats[seat].user.nick(),
            ctx.roster.seats[target].user.nick()
        ));
    }

    fn check_independent_win(&self, _seat: usize, ctx: &VictoryCtx<'_>) -> bool {
        (0..ctx.roster.len()).any(|s| ctx.board.marks[s] == Mark::Assassin && ctx.is_dead(s))
    }
}
