use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    Artifact, BoardCards, BoxFuture, CopyBehavior, ErasedIntent, PromptCtx, ResolveCtx, RolePlugin,
    Team,
};

pub struct Curator;
pub static CURATOR: Curator = Curator;

impl RolePlugin for Curator {
    fn id(&self) -> &'static str {
        "curator"
    }

    fn name(&self) -> &'static str {
        "Curator"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "May place a random face-down Artifact token on any unshielded player's card (secretly altering their team/goal!)."
    }

    fn wake_order(&self) -> Option<u16> {
        Some(185)
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
            let prompt = format!(
                "🏺 Curator: Reply `<seat|nick>` to bestow a random Artifact token (including yourself), or `pass`. Seats: {}",
                ctx.roster.format_seats_marking_shields(ctx.shielded)
            );
            let turn = dm
                .ask(prompt, move |m| {
                    let t = m.text.trim();
                    if matches!(t.to_ascii_lowercase().as_str(), "pass" | "skip" | "none") {
                        return Match::Accept(None);
                    }
                    match ctx.roster.resolve_token(t) {
                        Some(idx) if ctx.shielded.contains(&idx) => Match::Reject(Text::from(
                            "🛡️ Shielded seats cannot receive Artifacts!".yellow(),
                        )),
                        Some(idx) => Match::Accept(Some(idx)),
                        None => Match::Reject(Text::from("Unknown player (or `pass`).".yellow())),
                    }
                })
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left to place an Artifact!")
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
        let actor_nick = ctx.roster.seats[seat].user.nick();
        if let Turn::Reply(Some(target)) = *turn {
            if !board.is_shielded(target) {
                let art = Artifact::random();
                board.artifacts.insert(target, art);
                let _ = ctx.roster.seats[target].user.say(
                    "🏺 An "
                        + "Artifact token".bold().yellow()
                        + " was placed on your card! You secretly hold the "
                        + art.name().bold().orange()
                        + "!",
                );
                ctx.logs.push(format!(
                    "{actor_label} ({actor_nick}) placed {} on {}.",
                    art.name(),
                    ctx.roster.seats[target].user.nick()
                ));
            }
        }
    }
}
