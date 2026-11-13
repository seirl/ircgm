use std::time::Duration;

use ircgm::{Colorize, ExclusiveDm, GameFutureExt, Match, Text, Turn};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, PromptCtx, ResolveCtx, RolePlugin, Team,
};

pub struct VillageIdiot;
pub static VILLAGE_IDIOT: VillageIdiot = VillageIdiot;

#[derive(Clone, Copy)]
enum RotateDir {
    Left,
    Right,
}

impl RolePlugin for VillageIdiot {
    fn id(&self) -> &'static str {
        "village_idiot"
    }

    fn name(&self) -> &'static str {
        "Village Idiot"
    }

    fn team(&self) -> Team {
        Team::Village
    }

    fn night_summary(&self) -> &'static str {
        "May rotate ALL other unshielded players' cards one seat `left` or `right` (your own card stays put!)."
    }

    fn wake_order(&self) -> Option<u16> {
        Some(172)
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
                "🤪 Village Idiot: Reply `left` (`-1`), `right` (`+1`), or `pass` to rotate all other unshielded cards! Seats: {}",
                ctx.roster.format_seats_marking_shields(ctx.shielded)
            );
            let turn = dm
                .ask(prompt, |m| match m.text.trim().to_ascii_lowercase().as_str() {
                    "left" | "l" => Match::Accept(Some(RotateDir::Left)),
                    "right" | "r" => Match::Accept(Some(RotateDir::Right)),
                    "pass" | "skip" | "none" => Match::Accept(None),
                    _ => Match::Reject(Text::from("Reply `left`, `right`, or `pass`.".yellow())),
                })
                .with_config(ctx.ask_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left for Village Idiot rotation!")
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
        let Ok(turn) = intent.downcast::<Turn<Option<RotateDir>>>() else {
            return;
        };
        let actor_nick = ctx.roster.seats[seat].user.nick();
        let Turn::Reply(Some(dir)) = *turn else {
            ctx.logs
                .push(format!("{actor_label} ({actor_nick}) did not rotate cards."));
            return;
        };

        let ring: Vec<usize> = (0..ctx.roster.len())
            .filter(|&i| i != seat && !board.is_shielded(i))
            .collect();
        if ring.len() >= 2 {
            board.record_card_interaction(seat);
            let snapshot: Vec<_> = ring.iter().map(|&i| board.players[i]).collect();
            let k = ring.len();
            for (pos, &dst_seat) in ring.iter().enumerate() {
                let src_pos = match dir {
                    RotateDir::Right => (pos + k - 1) % k,
                    RotateDir::Left => (pos + 1) % k,
                };
                board.players[dst_seat] = snapshot[src_pos];
            }
            let dir_str = match dir {
                RotateDir::Left => "left",
                RotateDir::Right => "right",
            };
            ctx.logs.push(format!(
                "{actor_label} ({actor_nick}) rotated {} cards {dir_str}.",
                ring.len()
            ));
        }
    }
}
