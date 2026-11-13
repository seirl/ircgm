//! Playable IRC minigames built on [`ircgm`].
//!
//! Every module in this crate is a pure, self-contained `async` game with **zero** hardcoded
//! admin or bot-shutdown logic:
//! - Tested deterministically in-memory via [`ircgm::testing::test_harness`], or
//! - Launched live from a multi-game bot via [`play_by_name`] and supervised **externally**
//!   with `.until(bot.shutdown_requested()).or_until(admin_stop)` (relying on RAII `Drop` of
//!   [`Breakout`], [`Huddle`], [`ExclusiveDm`], and [`Inbox`] for automatic cleanup).

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::ops::RangeInclusive;
use std::time::Duration;

use ircgm::prelude::*;

pub mod auction;
pub mod blackjack;
pub mod cold_war;
pub mod coup;
pub mod heist;
pub mod mafia;
pub mod quiplash;
pub mod simon_says;

/// Result of launching a lobby minigame via `play` or [`play_by_name`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayOutcome {
    /// Enough players joined and the game ran to completion.
    Completed,
    /// Fewer than the minimum required players joined before the lobby window expired.
    NotEnoughPlayers,
}

/// Catalog of `(command_slug, one_line_summary)` for all 8 playable minigames.
pub const GAMES: &[(&str, &str)] = &[
    ("blackjack", "1-4p turn-based channel Blackjack"),
    ("heist", "2p co-op vault breach + live-scaled laser alarm"),
    ("cold_war", "2p two-breakout hotline Prisoner's Dilemma"),
    ("mafia", "4-5p Werewolf DM huddle + Bodyguard + secret day vote"),
    ("quiplash", "3p simultaneous DM quips + channel vote + buzz-in"),
    ("coup", "2p Ambassador DM exchange + serialized out-of-turn steal"),
    ("auction", "2-5p mixed channel bid + secret DM proxy/appraise auction"),
    ("simon_says", "2-4p PRIVMSG + /me + NOTICE + /nick + Bunker Rush"),
];

/// Builds a lobby-namespaced breakout room prefix (`"{lobby}-{kind}"`, e.g.
/// `#ircgm-minigames` -> `#ircgm-minigames-vault`, `##ircgm-minigames` -> `##ircgm-minigames-vault`).
pub(crate) fn breakout_slug(lobby: &Room, kind: &str) -> String {
    format!("{}-{kind}", lobby.name())
}

/// Shared lobby recruitment helper: collects `count` (`min..=max`) distinct players via `!join`
/// within `join_window` (deduplicating via [`UserId`] even across `/nick` changes).
///
/// Finishes immediately as soon as `max` players join, or when `join_window` expires if at least
/// `min` players have joined. Returns `Ok(None)` if fewer than `min` players joined in time.
pub async fn recruit_players(
    room: &Room,
    game_title: &str,
    count: RangeInclusive<usize>,
    join_window: Duration,
) -> Result<Option<Vec<User>>> {
    let min_players = (*count.start()).max(1);
    let max_players = (*count.end()).max(min_players);

    let count_desc = if min_players == max_players {
        if min_players == 1 {
            "need 1 player".to_owned()
        } else {
            format!("need {min_players} players")
        }
    } else {
        format!("{min_players}–{max_players} players")
    };

    let mut inbox = room.inbox();
    room.say(
        "🎲 ".bold()
            + game_title.bold().cyan()
            + format!(" — {count_desc}! Type ")
            + "!join".bold().green()
            + format!(" ({}s):", join_window.as_secs()),
    )?;

    let mut seen = HashSet::with_capacity(max_players);
    let mut players = Vec::with_capacity(max_players);
    let collect = async {
        while players.len() < max_players {
            let msg = inbox.wait_cmd("!join").await?;
            if seen.insert(msg.sender.id()) {
                players.push(msg.sender.clone());
                room.say(
                    msg.sender.nick().bold()
                        + format!(" joined! ({}/{max_players})", players.len()),
                )?;
            }
        }
        Ok(())
    };
    collect.run_for(join_window).await?;

    if players.len() >= min_players {
        Ok(Some(players))
    } else {
        room.say(format!("Not enough players joined for {game_title}.").yellow())?;
        Ok(None)
    }
}

/// Dispatches a minigame by slug (see [`GAMES`]). Returns `Ok(None)` if `name` is unknown.
pub async fn play_by_name(name: &str, bot: &Bot, lobby: &Room) -> Result<Option<PlayOutcome>> {
    let outcome = match name.trim().to_ascii_lowercase().as_str() {
        "blackjack" => blackjack::play(bot, lobby).await?,
        "heist" => heist::play(bot, lobby).await?,
        "cold_war" => cold_war::play(bot, lobby).await?,
        "mafia" => mafia::play(bot, lobby).await?,
        "quiplash" => quiplash::play(bot, lobby).await?,
        "coup" => coup::play(bot, lobby).await?,
        "auction" => auction::play(bot, lobby).await?,
        "simon_says" => simon_says::play(bot, lobby).await?,
        _ => return Ok(None),
    };
    Ok(Some(outcome))
}

#[cfg(test)]
#[path = "lib_test.rs"]
mod test;
