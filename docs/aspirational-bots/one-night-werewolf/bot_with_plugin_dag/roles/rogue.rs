//! Showcase Expansion Plugin: **Rogue** (`PromptDep::AfterUpstreamBoard` Sequence Point!)
//!
//! Included to demonstrate the exact mid-night sequence point discussed earlier:
//! - `Robber (60)` and `Troublemaker (70)` prompt in parallel at `t = 0` and swap cards.
//! - Because `Rogue` declares `wake_order = Some(75)` + `prompt_dep = PromptDep::AfterUpstreamBoard`,
//!   `dag.rs` **automatically inserts a sequence barrier** after `Troublemaker (70)`:
//!   `Rogue` sees a random player's *post-swap* card and THEN chooses whether to swap it with Center #1!
//! - Meanwhile, `Drunk (80)` STILL received its prompt at `t = 0` in parallel, and commits its
//!   center swap right after `Rogue` finishes!

use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, PromptCtx, PromptDep, ResolveCtx,
    RolePlugin, Team,
};

pub struct Rogue;
pub static ROGUE: Rogue = Rogue;

impl RolePlugin for Rogue {
    fn id(&self) -> &'static str {
        "rogue"
    }

    fn name(&self) -> &'static str {
        "Rogue"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Wake AFTER Robber & Troublemaker swaps: peek 1 player's current card, then choose whether to swap it with Center #1!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(175)
    }

    fn copy_behavior(&self) -> CopyBehavior {
        CopyBehavior::ActImmediately
    }

    /// Declares a true mid-night sequence point: waits for `Robber (160)` & `Troublemaker (170)`
    /// to mutate `BoardCards` BEFORE prompting this player!
    fn prompt_dep(&self) -> PromptDep {
        PromptDep::AfterUpstreamBoard
    }

    fn mutates_board(&self) -> bool {
        true
    }

    fn prompt_seat<'a>(
        &'a self,
        dm: &'a mut ExclusiveDm,
        seat: usize,
        ctx: &'a PromptCtx<'a>,
        upstream_board: Option<&'a BoardCards>,
    ) -> BoxFuture<'a, ircgm::Result<ErasedIntent>> {
        Box::pin(async move {
            let board = upstream_board.expect("`AfterUpstreamBoard` guarantees resolved board");
            let others: Vec<usize> = (0..ctx.roster.len())
                .filter(|&i| i != seat && !ctx.shielded.contains(&i))
                .collect();
            if others.is_empty() {
                let _ = dm.say("🛡️ All other players are shielded tonight!");
                return Ok(Box::new(()) as ErasedIntent);
            }
            let peek_seat = others[fastrand::usize(..others.len())];
            let peek_card = board.players[peek_seat];
            let peek_nick = ctx.roster.seats[peek_seat].user.nick();

            let prompt = "🗡️ Rogue: After earlier swaps, "
                + format!("[{}] {peek_nick}", peek_seat + 1).bold()
                + " currently holds "
                + peek_card.display_for_peek()
                + "! Swap their card with Center #1? (`yes` or `no`):";

            let turn = dm
                .ask(prompt, |m| match m.text.trim().to_ascii_lowercase().as_str() {
                    "yes" | "y" => Match::Accept(true),
                    "no" | "n" | "pass" => Match::Accept(false),
                    _ => Match::Reject(Text::from("Reply `yes` or `no`.".yellow())),
                })
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left for Rogue decision!")
                .settled()
                .await?;

            Ok(Box::new((peek_seat, turn)) as ErasedIntent)
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
        let Ok(boxed) = intent.downcast::<(usize, Turn<bool>)>() else {
            return;
        };
        let (peek_seat, turn) = *boxed;
        let actor_nick = ctx.roster.seats[seat].user.nick();
        let peek_nick = ctx.roster.seats[peek_seat].user.nick();

        board.record_card_interaction(seat);
        if matches!(turn, Turn::Reply(true)) && !board.is_shielded(peek_seat) {
            std::mem::swap(&mut board.players[peek_seat], &mut board.center[0]);
            ctx.logs.push(format!(
                "{actor_label} ({actor_nick}) peeked {peek_nick} and swapped them with Center #1."
            ));
        } else {
            ctx.logs.push(format!(
                "{actor_label} ({actor_nick}) peeked {peek_nick} and left their card."
            ));
        }
    }
}
