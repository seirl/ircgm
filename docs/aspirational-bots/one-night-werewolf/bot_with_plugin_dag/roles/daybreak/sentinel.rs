use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, PromptCtx, ResolveCtx, RolePlugin, Team,
};

pub struct Sentinel;
pub static SENTINEL: Sentinel = Sentinel;

impl RolePlugin for Sentinel {
    fn id(&self) -> &'static str {
        "sentinel"
    }

    fn name(&self) -> &'static str {
        "Sentinel"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Wake before Night (`Dusk`) to optionally place a Shield token on another player's card, preventing it from being viewed or moved."
    }

    /// Runs in Stage A (`wake_order < 100`) so all Night prompts (`>= 100`) immediately know which seat is shielded!
    fn wake_order(&self) -> Option<u16> {
        Some(15)
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
                "🛡️ Sentinel: Reply with `<seat|nick>` to place a Shield token on another player's card, or `pass`. Seats: {}",
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
                            "You cannot shield your own card! Pick another player or `pass`.".yellow(),
                        )),
                        Some(idx) => Match::Accept(Some(idx)),
                        None => Match::Reject(Text::from("Unknown player (or `pass`).".yellow())),
                    }
                })
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left to place your Shield!")
                .settled()
                .await?;

            if let Turn::Reply(Some(idx)) = turn {
                let _ = dm.say(
                    "🛡️ Placed a Shield token on "
                        + ctx.roster.seats[idx].user.nick().bold()
                        + "'s card!",
                );
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
        let Ok(turn) = intent.downcast::<Turn<Option<usize>>>() else {
            return;
        };
        let actor_nick = ctx.roster.seats[seat].user.nick();
        if let Turn::Reply(Some(target)) = *turn {
            board.shielded.insert(target);
            ctx.logs.push(format!(
                "{actor_label} ({actor_nick}) shielded {}.",
                ctx.roster.seats[target].user.nick()
            ));
        } else {
            ctx.logs
                .push(format!("{actor_label} ({actor_nick}) placed no shield."));
        }
    }
}
