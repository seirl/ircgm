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

/// Catalog of `(command_slug, one_line_summary)` for all 8 playable minigames.
pub const GAMES: &[(&str, &str)] = &[
    ("blackjack", "1-4p turn-based channel Blackjack"),
    ("heist", "2p co-op vault breach + live-scaled laser alarm"),
    ("cold_war", "2p two-breakout hotline Prisoner's Dilemma"),
    ("mafia", "5p Werewolf DM huddle + Bodyguard + secret day vote"),
    ("quiplash", "3p simultaneous DM quips + channel vote + buzz-in"),
    ("coup", "2p Ambassador DM exchange + serialized out-of-turn steal"),
    ("auction", "3p mixed channel bid + secret DM proxy/appraise auction"),
    ("simon_says", "3p PRIVMSG + /me + NOTICE + /nick + Bunker Rush"),
];

/// Builds a lobby-namespaced breakout room prefix (`"{lobby}-{kind}"`, e.g.
/// `#ircgm-minigames` -> `#ircgm-minigames-vault`, `##ircgm-minigames` -> `##ircgm-minigames-vault`).
pub(crate) fn breakout_slug(lobby: &Room, kind: &str) -> String {
    format!("{}-{kind}", lobby.name())
}

/// Shared lobby recruitment helper: collects `exact_count` distinct players via `!join`
/// within `join_window` (deduplicating via [`UserId`] even across `/nick` changes).
///
/// Returns `Ok(None)` if fewer than `exact_count` players joined in time.
pub async fn recruit_players(
    room: &Room,
    game_title: &str,
    exact_count: usize,
    join_window: Duration,
) -> Result<Option<Vec<User>>> {
    let mut inbox = room.inbox();
    room.say(
        "🎲 ".bold()
            + game_title.bold().cyan()
            + format!(" — need {exact_count} players! Type ")
            + "!join".bold().green()
            + format!(" ({}s):", join_window.as_secs()),
    )?;

    let collect = async {
        let mut seen = HashSet::with_capacity(exact_count);
        let mut players = Vec::with_capacity(exact_count);
        while players.len() < exact_count {
            let msg = inbox.wait_cmd("!join").await?;
            if seen.insert(msg.sender.id()) {
                players.push(msg.sender.clone());
                room.say(
                    msg.sender.nick().bold()
                        + format!(" joined! ({}/{exact_count})", players.len()),
                )?;
            }
        }
        Ok::<_, Error>(players)
    };

    match collect.with_timeout(join_window).optional().await? {
        Some(players) => Ok(Some(players)),
        None => {
            room.say(format!("Not enough players joined for {game_title}.").yellow())?;
            Ok(None)
        }
    }
}

/// Dispatches a minigame by slug (see [`GAMES`]). Returns `Ok(false)` if `name` is unknown.
pub async fn play_by_name(name: &str, bot: &Bot, lobby: &Room) -> Result<bool> {
    match name {
        "blackjack" => blackjack::play(lobby).await?,
        "heist" => {
            let _ = heist::play(bot, lobby).await?;
        }
        "cold_war" => {
            let _ = cold_war::play(bot, lobby).await?;
        }
        "mafia" => {
            let _ = mafia::play(bot, lobby).await?;
        }
        "quiplash" => {
            let _ = quiplash::play(bot, lobby).await?;
        }
        "coup" => {
            let _ = coup::play(lobby).await?;
        }
        "auction" => {
            let _ = auction::play(bot, lobby).await?;
        }
        "simon_says" => {
            let _ = simon_says::play(bot, lobby).await?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

#[cfg(test)]
#[path = "lib_test.rs"]
mod test;
