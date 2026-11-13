use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, PromptCtx, ResolveCtx, RolePlugin, Team,
};

pub struct Drunk;
pub static DRUNK: Drunk = Drunk;

impl RolePlugin for Drunk {
    fn id(&self) -> &'static str {
        "drunk"
    }

    fn name(&self) -> &'static str {
        "Drunk"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Wake and MUST swap your card face-down with 1 of the 3 center cards without looking."
    }

    fn wake_order(&self) -> Option<u16> {
        Some(180)
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
        _seat: usize,
        ctx: &'a PromptCtx<'a>,
        _upstream_board: Option<&'a BoardCards>,
    ) -> BoxFuture<'a, ircgm::Result<ErasedIntent>> {
        Box::pin(async move {
            let turn = dm
                .ask(
                    "🍺 Drunk: You MUST swap your card with center `1`, `2`, or `3`!",
                    |m| match m.text.trim().to_ascii_lowercase().as_str() {
                        "1" | "c1" | "center 1" => Match::Accept(0usize),
                        "2" | "c2" | "center 2" => Match::Accept(1usize),
                        "3" | "c3" | "center 3" => Match::Accept(2usize),
                        _ => Match::Reject(Text::from(
                            "Drunk MUST swap! Reply `1`, `2`, or `3`.".yellow(),
                        )),
                    },
                )
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left to pick center `1`, `2`, or `3`!")
                .settled()
                .await?;

            if let Turn::Reply(c_idx) = turn {
                let _ = dm.say(format!(
                    "🍺 Locked in face-down swap with Center #{}!",
                    c_idx + 1
                ));
            }
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
        let actor = &ctx.roster.seats[seat].user;
        let c_idx = match *turn {
            Turn::Reply(c) => c,
            Turn::TimedOut | Turn::Left { .. } => {
                let auto = fastrand::usize(0..3);
                let _ = actor.say(format!(
                    "🍺 Randomly swapped your card with Center #{}.",
                    auto + 1
                ));
                auto
            }
        };
        if board.is_shielded(seat) {
            let _ = actor.say("🛡️ Your card was shielded—Drunk swap prevented!".yellow());
            return;
        }
        board.record_card_interaction(seat);
        std::mem::swap(&mut board.players[seat], &mut board.center[c_idx]);
        ctx.logs.push(format!(
            "{actor_label} ({}) swapped with Center #{}.",
            actor.nick(),
            c_idx + 1
        ));
    }

    fn resolve_unavailable_seat(
        &self,
        seat: usize,
        actor_label: &str,
        board: &mut BoardCards,
        ctx: &mut ResolveCtx<'_>,
    ) {
        if board.is_shielded(seat) {
            return;
        }
        let c_idx = fastrand::usize(0..3);
        board.record_card_interaction(seat);
        std::mem::swap(&mut board.players[seat], &mut board.center[c_idx]);
        ctx.logs.push(format!(
            "{actor_label} ({}) auto-swapped with Center #{}.",
            ctx.roster.seats[seat].user.nick(),
            c_idx + 1
        ));
    }
}
