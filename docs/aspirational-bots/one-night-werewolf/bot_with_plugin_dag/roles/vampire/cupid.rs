use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, Mark, PromptCtx, ResolveCtx, RolePlugin,
    Team,
};

pub struct Cupid;
pub static CUPID: Cupid = Cupid;

impl RolePlugin for Cupid {
    fn id(&self) -> &'static str {
        "cupid"
    }

    fn name(&self) -> &'static str {
        "Cupid"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Wake in Dusk to give 2 players the Mark of Love (if one Lover dies at Dawn, the other dies of heartbreak!)."
    }

    fn wake_order(&self) -> Option<u16> {
        Some(35)
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
                "💘 Cupid: Reply with TWO players (`<a> <b>`) to bestow the Mark of Love, or `pass`. Seats: {}",
                ctx.roster.format_seats()
            );
            let turn = dm
                .ask(prompt, move |m| {
                    let t = m.text.trim();
                    if matches!(t.to_ascii_lowercase().as_str(), "pass" | "skip" | "none") {
                        return Match::Accept(None);
                    }
                    let parts: Vec<&str> = t.split_whitespace().collect();
                    if parts.len() != 2 {
                        return Match::Reject(Text::from("Usage: `<a> <b>` or `pass`.".yellow()));
                    }
                    let (Some(a), Some(b)) = (
                        ctx.roster.resolve_token(parts[0]),
                        ctx.roster.resolve_token(parts[1]),
                    ) else {
                        return Match::Reject(Text::from("Unknown seat or nickname.".yellow()));
                    };
                    if a == b {
                        return Match::Reject(Text::from("Pick two DIFFERENT players!".yellow()));
                    }
                    Match::Accept(Some((a, b)))
                })
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left for Cupid's arrows!")
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
        if let Turn::Reply(Some((a, b))) = *turn {
            board.marks[a] = Mark::Love;
            board.marks[b] = Mark::Love;
            ctx.logs.push(format!(
                "{actor_label} ({}) linked {} and {} with Mark of Love.",
                ctx.roster.seats[seat].user.nick(),
                ctx.roster.seats[a].user.nick(),
                ctx.roster.seats[b].user.nick()
            ));
        }
    }
}
