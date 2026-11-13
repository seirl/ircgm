use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, OracleOmen, PromptCtx, ResolveCtx,
    RolePlugin, Team,
};

pub struct Oracle;
pub static ORACLE: Oracle = Oracle;

impl RolePlugin for Oracle {
    fn id(&self) -> &'static str {
        "oracle"
    }

    fn name(&self) -> &'static str {
        "Oracle"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "Answer a mysterious Mother-Ship question during Night that alters how downstream Alien-set roles behave!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(142)
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
                    "🔮🛸 Oracle: The Mother Ship asks — tune cosmic frequencies to `amplify` (boosts Psychic visions), `static`, or `normal`?",
                    |m| match m.text.trim().to_ascii_lowercase().as_str() {
                        "amplify" | "a" | "yes" => Match::Accept(OracleOmen::AmplifiedSignals),
                        "static" | "s" => Match::Accept(OracleOmen::QuantumStatic),
                        "normal" | "n" | "pass" => Match::Accept(OracleOmen::Normal),
                        _ => Match::Reject(Text::from(
                            "Reply `amplify`, `static`, or `normal`.".yellow(),
                        )),
                    },
                )
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left to answer the Oracle omen!")
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
        let Ok(turn) = intent.downcast::<Turn<OracleOmen>>() else {
            return;
        };
        if let Turn::Reply(omen) = *turn {
            board.oracle_omen = omen;
            ctx.logs.push(format!(
                "{actor_label} ({}) set Oracle omen to {:?}.",
                ctx.roster.seats[seat].user.nick(),
                omen
            ));
        }
    }
}
