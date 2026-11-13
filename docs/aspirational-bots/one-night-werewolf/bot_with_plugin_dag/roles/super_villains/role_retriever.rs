//! Super Villains **Role Retriever** — Cross-Player Victim Notification!
//!
//! Swaps with another unshielded player AND **notifies the victim** of their new card!
//! Notice the `ircgm` concurrency subtlety documented in `FINDINGS.md` §1.7:
//! Because we send `victim.say(...)` inside `resolve_seat` (rather than inside `prompt_seat`),
//! we avoid deadlocking on `dm_mutex` and minimize mid-prompt interleaving!

use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, PromptCtx, ResolveCtx, RolePlugin, Team,
};

pub struct RoleRetriever;
pub static ROLE_RETRIEVER: RoleRetriever = RoleRetriever;

impl RolePlugin for RoleRetriever {
    fn id(&self) -> &'static str {
        "role_retriever"
    }

    fn name(&self) -> &'static str {
        "Role Retriever"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "May swap your card with another player's card—and BOTH of you see the card that lands in front of that player!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(162)
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
                "🐕 Role Retriever: Reply `<seat|nick>` to swap cards with another player (they will see their new card!), or `pass`. Seats: {}",
                ctx.roster.format_seats_marking_shields(ctx.shielded)
            );
            let turn = dm
                .ask(prompt, move |m| {
                    let t = m.text.trim();
                    if matches!(t.to_ascii_lowercase().as_str(), "pass" | "skip" | "none") {
                        return Match::Accept(None);
                    }
                    match ctx.roster.resolve_token(t) {
                        Some(idx) if idx == seat => {
                            Match::Reject(Text::from("Pick ANOTHER player (or `pass`)!".yellow()))
                        }
                        Some(idx) if ctx.shielded.contains(&idx) => {
                            Match::Reject(Text::from("🛡️ Shielded seats cannot be swapped!".yellow()))
                        }
                        Some(idx) => Match::Accept(Some(idx)),
                        None => Match::Reject(Text::from("Unknown player (or `pass`).".yellow())),
                    }
                })
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left for Role Retriever!")
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
            if board.is_shielded(seat) || board.is_shielded(target) {
                return;
            }
            board.record_card_interaction(seat);
            board.players.swap(seat, target);
            let actor_new = board.players[seat];
            let victim_new = board.players[target];
            let victim = &ctx.roster.seats[target].user;

            let _ = actor.say(
                format!("🐕 Swapped with [{}] {} — you fetched ", target + 1, victim.nick())
                    + actor_new.display_for_peek()
                    + "!",
            );
            let _ = victim.say(
                "🐕 The Role Retriever swapped your card! Your card is now "
                    + victim_new.display_for_peek()
                    + "!",
            );
            ctx.logs.push(format!(
                "{actor_label} ({}) swapped with {} and showed them {}.",
                actor.nick(),
                victim.nick(),
                victim_new.effective_role().name()
            ));
        }
    }
}
