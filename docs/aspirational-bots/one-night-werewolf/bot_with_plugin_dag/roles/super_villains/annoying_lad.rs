use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, PromptCtx, ResolveCtx, RolePlugin, Team,
};

pub struct AnnoyingLad;
pub static ANNOYING_LAD: AnnoyingLad = AnnoyingLad;

impl RolePlugin for AnnoyingLad {
    fn id(&self) -> &'static str {
        "annoying_lad"
    }

    fn name(&self) -> &'static str {
        "Annoying Lad"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Nudge either your immediate `left` or `right` neighbor in DM so they know Annoying Lad sits beside them!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(198)
    }

    fn copy_behavior(&self) -> CopyBehavior {
        CopyBehavior::ActImmediately
    }

    fn prompt_seat<'a>(
        &'a self,
        dm: &'a mut ExclusiveDm,
        seat: usize,
        ctx: &'a PromptCtx<'a>,
        _upstream_board: Option<&'a BoardCards>,
    ) -> BoxFuture<'a, ircgm::Result<ErasedIntent>> {
        Box::pin(async move {
            let (left, right) = ctx.roster.neighbors(seat);
            let prompt = format!(
                "😜 Annoying Lad: Reply `left` ([{}] {}) or `right` ([{}] {}) to poke a neighbor!",
                left + 1,
                ctx.roster.seats[left].user.nick(),
                right + 1,
                ctx.roster.seats[right].user.nick()
            );
            let turn = dm
                .ask(prompt, move |m| match m.text.trim().to_ascii_lowercase().as_str() {
                    "left" | "l" => Match::Accept(left),
                    "right" | "r" => Match::Accept(right),
                    other => match ctx.roster.resolve_token(other) {
                        Some(idx) if idx == left || idx == right => Match::Accept(idx),
                        _ => Match::Reject(Text::from("Pick `left` or `right`!".yellow())),
                    },
                })
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left to poke a neighbor!")
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
        _board: &mut BoardCards,
        ctx: &mut ResolveCtx<'_>,
    ) {
        let Ok(turn) = intent.downcast::<Turn<usize>>() else {
            return;
        };
        let (left, _) = ctx.roster.neighbors(seat);
        let target = match *turn {
            Turn::Reply(idx) => idx,
            Turn::TimedOut | Turn::Left { .. } => left,
        };
        let _ = ctx.roster.seats[target].user.say(
            "👉 *POKE!* ".bold().yellow()
                + "One of your immediate neighbors is the Annoying Lad!",
        );
        ctx.logs.push(format!(
            "{actor_label} ({}) poked {}.",
            ctx.roster.seats[seat].user.nick(),
            ctx.roster.seats[target].user.nick()
        ));
    }
}
