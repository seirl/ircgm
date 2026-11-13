use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, PromptCtx, ResolveCtx, RolePlugin, Team,
};

pub struct Marksman;
pub static MARKSMAN: Marksman = MARKSMAN;

impl RolePlugin for Marksman {
    fn id(&self) -> &'static str {
        "marksman"
    }

    fn name(&self) -> &'static str {
        "Marksman"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Wake in Night to view ONE unshielded player's Card AND a DIFFERENT player's Mark!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(158)
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
            let prompt = format!(
                "🎯 Marksman: Reply `<card_seat> <mark_seat>` (two DIFFERENT players) or `pass`. Seats: {}",
                ctx.roster.format_seats_marking_shields(ctx.shielded)
            );
            let turn = dm
                .ask(prompt, move |m| {
                    let t = m.text.trim();
                    if matches!(t.to_ascii_lowercase().as_str(), "pass" | "skip" | "none") {
                        return Match::Accept(None);
                    }
                    let parts: Vec<&str> = t.split_whitespace().collect();
                    if parts.len() != 2 {
                        return Match::Reject(Text::from(
                            "Usage: `<card_seat> <mark_seat>` or `pass`.".yellow(),
                        ));
                    }
                    let (Some(card_idx), Some(mark_idx)) = (
                        ctx.roster.resolve_token(parts[0]),
                        ctx.roster.resolve_token(parts[1]),
                    ) else {
                        return Match::Reject(Text::from("Unknown seat or nickname.".yellow()));
                    };
                    if card_idx == seat {
                        return Match::Reject(Text::from(
                            "You cannot inspect your own card!".yellow(),
                        ));
                    }
                    if card_idx == mark_idx {
                        return Match::Reject(Text::from(
                            "Card target and Mark target must be DIFFERENT players!".yellow(),
                        ));
                    }
                    if ctx.shielded.contains(&card_idx) {
                        return Match::Reject(Text::from(
                            "🛡️ That card is shielded by the Sentinel!".yellow(),
                        ));
                    }
                    Match::Accept(Some((card_idx, mark_idx)))
                })
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left for Marksman scope!")
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
        let Ok(turn) = intent.downcast::<Turn<Option<(usize, usize)>>>() else {
            return;
        };
        let actor = &ctx.roster.seats[seat].user;
        if let Turn::Reply(Some((card_idx, mark_idx))) = *turn {
            if board.is_shielded(card_idx) {
                return;
            }
            board.record_card_interaction(seat);
            let seen_card = board.players[card_idx];
            let seen_mark = board.marks[mark_idx];
            let _ = actor.say(
                format!(
                    "🎯 Marksman: [{}] {}'s card is ",
                    card_idx + 1,
                    ctx.roster.seats[card_idx].user.nick()
                ) + seen_card.display_for_peek()
                    + format!(
                        ", and [{}] {}'s mark is ",
                        mark_idx + 1,
                        ctx.roster.seats[mark_idx].user.nick()
                    )
                    + seen_mark.styled()
                    + "!",
            );
            ctx.logs.push(format!(
                "{actor_label} ({}) viewed {}'s card ({}) and {}'s mark ({}).",
                actor.nick(),
                ctx.roster.seats[card_idx].user.nick(),
                seen_card.effective_role().name(),
                ctx.roster.seats[mark_idx].user.nick(),
                seen_mark.name()
            ));
        }
    }
}
