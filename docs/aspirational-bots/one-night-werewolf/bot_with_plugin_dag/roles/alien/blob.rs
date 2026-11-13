use ircgm::Colorize;

use crate::bot_with_plugin_dag::plugin::{BoardCards, RolePlugin, Roster, Team, VictoryCtx};

pub struct Blob;
pub static BLOB: Blob = Blob;

impl RolePlugin for Blob {
    fn id(&self) -> &'static str {
        "blob"
    }

    fn name(&self) -> &'static str {
        "Blob"
    }

    fn team(&self) -> Team {
        Team::Independent
    }

    fn night_summary(&self) -> &'static str {
        "Part of the Blob! You win ONLY if every player absorbed into the Blob survives the vote!"
    }

    fn on_deal(&self, seat: usize, board: &mut BoardCards, roster: &Roster) {
        let (left, right) = roster.neighbors(seat);
        board.blob_seats.insert(seat);
        board.blob_seats.insert(left);
        if roster.len() >= 6 {
            board.blob_seats.insert(right);
        }

        let mut list: Vec<usize> = board.blob_seats.iter().copied().collect();
        list.sort_unstable();
        let names: Vec<String> = list
            .iter()
            .map(|&s| format!("[{}] {}", s + 1, roster.seats[s].user.nick()))
            .collect();
        let _ = roster.seats[seat].user.say(
            "🟢 Blob: You have absorbed "
                + names.join(", ").bold().green()
                + " — keep ALL of them alive to win!",
        );
    }

    fn check_independent_win(&self, seat: usize, ctx: &VictoryCtx<'_>) -> bool {
        if ctx.board.blob_seats.is_empty() {
            !ctx.is_dead(seat)
        } else {
            ctx.board.blob_seats.iter().all(|&s| !ctx.is_dead(s))
        }
    }
}
