//! Daybreak **Paranormal Investigator** — Sequential Multi-Peek + Dynamic Role Transformation!
//!
//! Demonstrates a mid-night sequence point (`PromptDep::AfterUpstreamBoard`) where a player
//! looks at up to TWO other players' *current* cards inside a single `ExclusiveDm` session,
//! stopping immediately and transforming (`Card::Transformed`) if they spot a threat or Tanner!

use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, Card, CopyBehavior, ErasedIntent, PromptCtx, PromptDep, ResolveCtx,
    RolePlugin, Team,
};

pub struct ParanormalInvestigator;
pub static PARANORMAL_INVESTIGATOR: ParanormalInvestigator = ParanormalInvestigator;

struct PiOutcome {
    peeked: Vec<usize>,
    transformed_into: Option<&'static dyn RolePlugin>,
}

impl RolePlugin for ParanormalInvestigator {
    fn id(&self) -> &'static str {
        "paranormal_investigator"
    }

    fn name(&self) -> &'static str {
        "Paranormal Investigator"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Look at up to 2 players' cards. If you see an Evil threat or Independent (e.g. Tanner), you STOP immediately and become that role!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(155)
    }

    fn copy_behavior(&self) -> CopyBehavior {
        CopyBehavior::ActImmediately
    }

    /// Must see the true upstream board (e.g. after `Alpha Wolf` swaps) during the interactive prompt!
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
            let mut peeked: Vec<usize> = Vec::new();
            let mut transformed_into: Option<&'static dyn RolePlugin> = None;

            for step in 1..=2 {
                let already = peeked.clone();
                let prompt = format!(
                    "🕵️ Paranormal Investigator (peek {step}/2): Reply `<seat|nick>` to inspect, or `pass`. Seats: {}",
                    ctx.roster.format_seats_marking_shields(ctx.shielded)
                );
                let turn = dm
                    .ask(prompt, move |m| {
                        let t = m.text.trim();
                        if matches!(t.to_ascii_lowercase().as_str(), "pass" | "skip" | "stop") {
                            return Match::Accept(None);
                        }
                        match ctx.roster.resolve_token(t) {
                            Some(idx) if idx == seat => Match::Reject(Text::from(
                                "You cannot inspect your own card!".yellow(),
                            )),
                            Some(idx) if already.contains(&idx) => Match::Reject(Text::from(
                                "You already inspected that player—pick someone else or `pass`!".yellow(),
                            )),
                            Some(idx) if ctx.shielded.contains(&idx) => Match::Reject(Text::from(
                                "🛡️ That card is shielded by the Sentinel!".yellow(),
                            )),
                            Some(idx) => Match::Accept(Some(idx)),
                            None => Match::Reject(Text::from("Unknown player (or `pass`).".yellow())),
                        }
                    })
                    .with_config(ctx.ask_cfg)
                    .warn_at(Duration::from_secs(8), "⏳ Hurry—PI investigation closing soon!")
                    .settled()
                    .await?;

                let Turn::Reply(Some(target)) = turn else {
                    break;
                };
                peeked.push(target);

                let seen_card = board.players[target];
                let seen_role = seen_card.effective_role();
                let target_nick = ctx.roster.seats[target].user.nick();

                if board.is_primary_threat(target) || seen_role.team() != Team::Village {
                    let _ = dm.say(
                        format!("😱 [{}] {target_nick} is ", target + 1)
                            + seen_card.display_for_peek()
                            + "! You STOP investigating and become "
                            + seen_role.styled_name()
                            + "!",
                    );
                    transformed_into = Some(seen_role);
                    break;
                } else {
                    let _ = dm.say(
                        format!("🕵️ [{}] {target_nick} is ", target + 1)
                            + seen_card.display_for_peek()
                            + " (Village).",
                    );
                }
            }

            Ok(Box::new(PiOutcome {
                peeked,
                transformed_into,
            }) as ErasedIntent)
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
        let Ok(out) = intent.downcast::<PiOutcome>() else {
            return;
        };
        let actor_nick = ctx.roster.seats[seat].user.nick();
        if !out.peeked.is_empty() {
            board.record_card_interaction(seat);
        }
        if let Some(became) = out.transformed_into {
            let orig = board.players[seat].effective_role();
            board.players[seat] = Card::Transformed {
                original: orig,
                became,
            };
            ctx.logs.push(format!(
                "{actor_label} ({actor_nick}) inspected {:?} and transformed into {}!",
                out.peeked
                    .iter()
                    .map(|&i| ctx.roster.seats[i].user.nick())
                    .collect::<Vec<_>>(),
                became.name()
            ));
        } else {
            ctx.logs.push(format!(
                "{actor_label} ({actor_nick}) inspected {} safe seat(s).",
                out.peeked.len()
            ));
        }
    }
}
