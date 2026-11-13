use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, Mark, PromptCtx, ResolveCtx, RolePlugin,
    Team,
};

pub struct Priest;
pub static PRIEST: Priest = Priest;

impl RolePlugin for Priest {
    fn id(&self) -> &'static str {
        "priest"
    }

    fn name(&self) -> &'static str {
        "Priest"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Cleanse yourself AND optionally 1 other player with the Mark of Clarity (wiping any Dusk marks on them!)."
    }

    /// Notice: `Priest` prompts in parallel with `Vampire (25)` / `Count (30)` / `Assassin (40)` at `t = 0`
    /// of Dusk (`PromptDep::None`), and commits its `Mark::Clarity` cleanse at `wake_order = 50`!
    fn wake_order(&self) -> Option<u16> {
        Some(50)
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
                "✝️ Priest: You cleanse yourself; reply `<seat|nick>` to ALSO cleanse another player with Mark of Clarity, or `pass`. Seats: {}",
                ctx.roster.format_seats()
            );
            let turn = dm
                .ask(prompt, move |m| {
                    let t = m.text.trim();
                    if matches!(t.to_ascii_lowercase().as_str(), "pass" | "skip" | "none") {
                        return Match::Accept(None);
                    }
                    match ctx.roster.resolve_token(t) {
                        Some(idx) if idx == seat => Match::Reject(Text::from(
                            "You already cleanse yourself automatically! Pick another player or `pass`.".yellow(),
                        )),
                        Some(idx) => Match::Accept(Some(idx)),
                        None => Match::Reject(Text::from("Unknown player (or `pass`).".yellow())),
                    }
                })
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left for Priest blessing!")
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
        board.marks[seat] = Mark::Clarity;
        let Ok(turn) = intent.downcast::<Turn<Option<usize>>>() else {
            return;
        };
        let actor_nick = ctx.roster.seats[seat].user.nick();
        if let Turn::Reply(Some(other)) = *turn {
            board.marks[other] = Mark::Clarity;
            ctx.logs.push(format!(
                "{actor_label} ({actor_nick}) cleansed self and {} with Mark of Clarity.",
                ctx.roster.seats[other].user.nick()
            ));
        } else {
            ctx.logs.push(format!(
                "{actor_label} ({actor_nick}) cleansed self with Mark of Clarity."
            ));
        }
    }
}
