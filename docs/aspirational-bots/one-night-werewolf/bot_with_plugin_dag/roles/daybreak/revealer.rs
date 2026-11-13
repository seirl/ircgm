//! Daybreak **Revealer** (also covers Super Villains **Flipper**!)
//!
//! Flips another unshielded player's card face up (`board.face_up_players`), EXCEPT if it is
//! an Evil threat or `Independent` (e.g. `Tanner`), in which case the Revealer sees it and
//! MUST flip it back face down!

use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, PromptCtx, ResolveCtx, RolePlugin, Team,
};

pub struct Revealer;
pub static REVEALER: Revealer = Revealer;

impl RolePlugin for Revealer {
    fn id(&self) -> &'static str {
        "revealer"
    }

    fn name(&self) -> &'static str {
        "Revealer"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Flip another player's card face up for Dawn—unless it is Evil or Tanner, in which case you flip it back face down!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(192)
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
                "🔦 Revealer: Reply `<seat|nick>` to flip their card, or `pass`. Seats: {}",
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
                            Match::Reject(Text::from("Pick ANOTHER player, not yourself!".yellow()))
                        }
                        Some(idx) if ctx.shielded.contains(&idx) => {
                            Match::Reject(Text::from("🛡️ Shielded cards cannot be flipped!".yellow()))
                        }
                        Some(idx) => Match::Accept(Some(idx)),
                        None => Match::Reject(Text::from("Unknown player (or `pass`).".yellow())),
                    }
                })
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left for Revealer flip!")
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
            if board.is_shielded(target) {
                return;
            }
            board.record_card_interaction(seat);
            let card = board.players[target];
            let target_nick = ctx.roster.seats[target].user.nick();
            if board.is_primary_threat(target) || card.effective_role().team() != Team::Village {
                let _ = actor.say(
                    format!("🔦 [{}] {target_nick} is ", target + 1)
                        + card.display_for_peek()
                        + " — you quickly flip it BACK face down!",
                );
                ctx.logs.push(format!(
                    "{actor_label} ({}) peeked {target_nick} ({}) and flipped it back face-down.",
                    actor.nick(),
                    card.effective_role().name()
                ));
            } else {
                board.face_up_players.insert(target);
                let _ = actor.say(
                    format!("🔦 [{}] {target_nick} is ", target + 1)
                        + card.display_for_peek()
                        + " — left FACE UP for everyone to see at Dawn!",
                );
                ctx.logs.push(format!(
                    "{actor_label} ({}) flipped {target_nick} ({}) face-up!",
                    actor.nick(),
                    card.effective_role().name()
                ));
            }
        }
    }
}
