//! Playable IRC minigames built on [`ircgm`].
//!
//! Every module in this crate is a pure, self-contained `async` game with **zero** hardcoded
//! admin or bot-shutdown logic:
//! - Tested deterministically in-memory via [`ircgm::testing::test_harness`], or
//! - Launched live from a multi-game bot via [`play_by_name`] and supervised **externally**
//!   with `.until(bot.shutdown_requested()).or_until(admin_stop)` (relying on RAII `Drop` of
//!   [`Breakout`], [`Huddle`], [`ExclusiveDm`], and [`Inbox`] for automatic cleanup).

#![forbid(unsafe_code)]

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

/// Result of launching a lobby minigame via [`play_by_name`] or `<game>::play`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayOutcome {
    /// Enough players joined and the game ran to completion.
    Completed,
    /// Fewer than the minimum required players joined before the lobby window expired.
    NotEnoughPlayers,
}

/// Catalog entry for one playable minigame.
#[derive(Clone, Copy, Debug)]
pub struct Game {
    /// Canonical command name (`!play <slug>`).
    pub slug: &'static str,
    /// Alternate command names accepted by [`find_game`] (e.g. `bj` for `blackjack`).
    pub aliases: &'static [&'static str],
    /// Short one-line player-count and rules summary.
    pub summary: &'static str,
}

/// Catalog of all 8 playable minigames.
pub const GAMES: &[Game] = &[
    Game {
        slug: "blackjack",
        aliases: &["bj"],
        summary: "1-4p turn-based channel Blackjack",
    },
    Game {
        slug: "heist",
        aliases: &[],
        summary: "2p co-op vault breach + live-scaled laser alarm",
    },
    Game {
        slug: "cold_war",
        aliases: &["coldwar"],
        summary: "2p two-breakout hotline Prisoner's Dilemma",
    },
    Game {
        slug: "mafia",
        aliases: &["werewolf", "wolves"],
        summary: "4-5p Werewolf DM huddle + Bodyguard + secret day vote",
    },
    Game {
        slug: "quiplash",
        aliases: &[],
        summary: "3p simultaneous DM quips + channel vote + buzz-in",
    },
    Game {
        slug: "coup",
        aliases: &[],
        summary: "2p Ambassador DM exchange + serialized out-of-turn steal",
    },
    Game {
        slug: "auction",
        aliases: &[],
        summary: "2-5p mixed channel bid + secret DM proxy/appraise auction",
    },
    Game {
        slug: "simon_says",
        aliases: &["simon", "simonsays"],
        summary: "2-4p PRIVMSG + /me + NOTICE + /nick + Bunker Rush",
    },
];

/// Looks up a game by slug or alias, ignoring case, a leading `!`, `-` vs `_`, and anything after
/// the first word (`"!Cold-War now"` finds `cold_war`).
pub fn find_game(name: &str) -> Option<&'static Game> {
    let word = name.split_whitespace().next()?;
    let norm = word
        .trim_start_matches('!')
        .to_ascii_lowercase()
        .replace('-', "_");
    GAMES
        .iter()
        .find(|g| g.slug == norm || g.aliases.contains(&norm.as_str()))
}

/// Builds a lobby-namespaced breakout room prefix (`"{lobby}-{kind}"`, e.g.
/// `#ircgm-minigames` -> `#ircgm-minigames-vault`, `##ircgm-minigames` -> `##ircgm-minigames-vault`).
pub(crate) fn breakout_slug(lobby: &Room, kind: &str) -> String {
    format!("{}-{kind}", lobby.name())
}

/// Shared lobby recruitment helper: collects `count` (`min..=max`) distinct players via `!join`
/// (with `!leave` to step back out) within `join_window`, deduplicating via [`UserId`] across
/// `/nick` changes and evicting recruits who `PART`, are `KICK`ed, or `QUIT` before launch.
///
/// Finishes immediately as soon as `max` players are in, or when `join_window` expires if at least
/// `min` players are in. Returns `Ok(None)` if fewer than `min` players joined in time.
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

    let signups = room
        .ask_many(
            irc_format!(
                "{} {} — {count_desc}! Type {} ({}s):",
                "🎲".bold(),
                game_title.bold().cyan(),
                "!join".bold().green(),
                join_window.as_secs(),
            ),
            |m| {
                if m.is_cmd("!join") {
                    AskMatch::accept_say(true, irc_format!("{} joined!", m.nick.as_str().bold()))
                } else if m.is_cmd("!leave") {
                    AskMatch::accept_say(
                        false,
                        irc_format!("{} left the lobby.", m.nick.as_str().bold()),
                    )
                } else {
                    AskMatch::ignore()
                }
            },
        )
        .allow_change(true)
        .config(AskConfig::new().reply_timeout(join_window))
        .stop_when(|a| a.values().filter(|&&joined| joined).count() >= max_players)
        .await?;

    let players: Vec<User> = signups
        .into_iter()
        .filter_map(|(u, joined)| joined.then_some(u))
        .collect();

    if players.len() >= min_players {
        Ok(Some(players))
    } else {
        room.say(format_args!("Not enough players joined for {game_title}.").yellow())?;
        Ok(None)
    }
}

/// Dispatches a minigame by slug or alias (see [`GAMES`] and [`find_game`]). Returns `Ok(None)`
/// if `name` does not match any registered game.
pub async fn play_by_name(name: &str, bot: &Bot, lobby: &Room) -> Result<Option<PlayOutcome>> {
    let Some(game) = find_game(name) else {
        return Ok(None);
    };
    let outcome = match game.slug {
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
