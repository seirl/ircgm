//! # Quiplash & Speed Tiebreaker (3 Players)
//!
//! ## How to Play
//! 1. Three players join in `#lobby` (or pass `(User, prompt)` assignments to [`collect_quips`]).
//! 2. **Quip Phase (`45s`):** The bot simultaneously DMs each player a fill-in-the-blank prompt
//!    ([`FuturesUnordered`] over `player.ask(...)`):
//!    - Empty or `> 80`-char answers receive validation feedback via [`Verdict::reject`] without
//!      resetting the `45s` timer.
//!    - Players who haven't answered yet get a `10s` countdown warning (`warn_remaining`).
//!    - If a player blocks DMs (`+g` `716` / `+R` `486`), [`Turn::Unreachable`] isolates that
//!      bounce and posts a helpful hint in `#lobby` without aborting everyone else!
//! 3. **Audience Vote Phase (`20s`):** Submitted quips are posted anonymously in `#lobby`
//!    (`#1`, `#2`, ...), and everyone votes with `!pick <number>` (deduplicated per [`UserId`],
//!    preventing authors from voting for their own quip).
//! 4. **Lightning Tiebreaker (`20s`):** Whenever two or more quips tie for first place, the tied
//!    authors enter a sudden-death DM math buzz-in (`What is 17 * 6?`) where the first correct
//!    answer on the wire (`ChatMessage::seq`) breaks the tie!
//!
//! ## Library Mechanics Showcased
//! - **Concurrent per-player DM [`ircgm::Ask`]s (`FuturesUnordered`):** Inline [`Verdict::reject`]
//!   validation (preserving remaining timeout), [`Verdict::accept_ack`] in-DM confirmation, and
//!   `warn_remaining` countdown warnings.
//! - **PM-block bounce isolation (`+g` `716` / `+R` `486`):** [`ircgm::GameFutureExt::settled`] maps
//!   server DM rejection numerics to [`Turn::Unreachable`].
//! - **Identity-safe channel ballot collection:** [`Inbox::wait_map`] + [`UserId`] keys block
//!   self-votes and double-votes even across mid-round `/nick` changes.
//! - **Drop-out tolerant [`Bot::huddle_available`] + [`Huddle::ask`] tiebreaker:** Locks online
//!   finalists in canonical order and adjudicates packet-burst ties strictly by wire arrival (`seq`).

use std::collections::HashMap;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use ircgm::prelude::*;

const DEFAULT_PROMPTS: &[&str] = &[
    "Worst thing to say at a wedding",
    "Rejected superhero name",
    "Secret ingredient in mystery meat",
];

/// Lobby entrypoint: recruits 3 players, collects DM quips, votes in `lobby`, and breaks ties via DM buzz-in!
pub async fn play(bot: &Bot, lobby: &Room) -> Result<crate::PlayOutcome> {
    let Some(players) =
        crate::recruit_players(lobby, "Quiplash", 3..=3, Duration::from_secs(30)).await?
    else {
        return Ok(crate::PlayOutcome::NotEnoughPlayers);
    };

    let assignments: Vec<(User, &str)> = players
        .into_iter()
        .enumerate()
        .map(|(idx, u)| (u, DEFAULT_PROMPTS[idx % DEFAULT_PROMPTS.len()]))
        .collect();

    let submissions = collect_quips(lobby, &assignments).await?;
    let _ = vote_on_quips(bot, lobby, &submissions).await?;
    Ok(crate::PlayOutcome::Completed)
}

/// Collects simultaneous secret DM quips from `assignments` (`(player, prompt)`), announcing
/// progress in `room` as each player submits, times out, disconnects, or bounces (`+g` / `+R`).
pub async fn collect_quips(
    room: &Room,
    assignments: &[(User, &str)],
) -> Result<Vec<(User, String)>> {
    let total = assignments.len();
    let cfg = AskConfig::new()
        .reply_timeout(Duration::from_secs(45))
        .timeout_message("⏳ Pencils down—time's up!".yellow())
        .ignore_commands();

    let mut tasks: FuturesUnordered<_> = assignments
        .iter()
        .map(|(player, secret_prompt)| {
            let cfg = &cfg;
            async move {
                let turn = player
                    .ask(
                        irc_format!("{} {secret_prompt} (45s, max 80 chars)", "Prompt:".bold().cyan()),
                        |m| {
                            let quip = m.text.trim();
                            if quip.is_empty() || quip.len() > 80 {
                                Verdict::reject(
                                    "Keep your answer between 1 and 80 characters!".yellow(),
                                )
                            } else {
                                Verdict::accept_ack(
                                    quip.to_owned(),
                                    "✔ Quip locked in! Watch the channel for voting.".green(),
                                )
                            }
                        },
                    )
                    .config(cfg)
                    .warn_remaining(
                        Duration::from_secs(10),
                        "⏳ 10 seconds left to submit!".yellow(),
                    )
                    .settled()
                    .await?;
                Ok::<_, Error>((player.clone(), turn))
            }
        })
        .collect();

    let mut submissions = Vec::with_capacity(total);
    while let Some(res) = tasks.next().await {
        let (player, turn) = res?;
        match turn {
            Turn::Reply(quip) => {
                submissions.push((player.clone(), quip));
                room.say(irc_format!(
                    "{} {} locked in! ({}/{total})",
                    "✔".green().bold(),
                    player.nick().bold(),
                    submissions.len(),
                ))?;
            }
            Turn::TimedOut => {
                room.say(irc_format!("{} ran out of time!", player.nick().bold().yellow()))?;
            }
            Turn::Left { nick, .. } => {
                room.say(irc_format!("{} disconnected.", nick.bold()))?;
            }
            Turn::Unreachable { code, .. } => {
                room.say(irc_format!(
                    "{} couldn't receive DMs ({code:?} — check `/umode -g` / NickServ)!",
                    player.nick().bold().red(),
                ))?;
            }
        }
    }
    Ok(submissions)
}

/// Wire-fair sudden-death DM speed math race (`17 * 6 = 102`) across `finalists`.
pub async fn tiebreaker_buzz_in(
    bot: &Bot,
    room: &Room,
    finalists: &[User],
) -> Result<Option<User>> {
    let mut huddle = bot.huddle_available(finalists).await?;
    if huddle.is_empty() {
        return Ok(None);
    }

    room.say(
        "⚡ TIEBREAKER! Finalists, check your DMs for a sudden-death math buzz-in!"
            .yellow()
            .bold(),
    )?;
    huddle
        .ask("⚡ Tiebreaker! What is 17 * 6? (20s)", |m| {
            if m.text.trim() == "102" {
                Verdict::accept_ack(
                    m.sender.clone(),
                    irc_format!(
                        "{} {} buzzed in first with 102!",
                        "⚡".yellow().bold(),
                        m.sender.nick().bold().green(),
                    ),
                )
            } else {
                Verdict::reject("Nope — try again! (17 * 6)".yellow())
            }
        })
        .reply_timeout(Duration::from_secs(20))
        .no_timeout_message()
        .settled()
        .await
        .map(Turn::into_reply)
}

/// Posts anonymous quips in `room`, collects `!pick <1..=N>` votes for `20s`, and breaks ties via DM buzz-in.
pub async fn vote_on_quips(
    bot: &Bot,
    room: &Room,
    submissions: &[(User, String)],
) -> Result<Option<(User, String)>> {
    if submissions.is_empty() {
        room.say("No quips were submitted!".yellow())?;
        return Ok(None);
    }
    if submissions.len() == 1 {
        let winner = submissions[0].clone();
        room.say(irc_format!(
            "{} {} wins by default with: \"{}\"!",
            "🏆".bold(),
            winner.0.nick().bold().green(),
            winner.1.as_str().italic(),
        ))?;
        return Ok(Some(winner));
    }

    room.say("🗳️ Vote for the funniest answer with `!pick <number>` (20s):".bold())?;
    for (idx, (_author, quip)) in submissions.iter().enumerate() {
        room.say(format_args!("  #{}: \"{quip}\"", idx + 1))?;
    }

    let mut inbox = room.inbox();
    let mut ballots: HashMap<UserId, usize> = HashMap::new();
    let collect = async {
        loop {
            let msg = inbox.wait_cmd("!pick").await?;
            match msg
                .cmd_arg("!pick")
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|&n| (1..=submissions.len()).contains(&n))
            {
                Some(choice) => {
                    let idx = choice - 1;
                    if submissions[idx].0 != msg.sender {
                        ballots.insert(msg.sender.id(), idx);
                        msg.sender
                            .notice(format_args!("✔ Vote recorded for quip #{choice}!").green())?;
                    } else {
                        msg.sender
                            .notice("You can't vote for your own quip!".yellow())?;
                    }
                }
                None => {
                    msg.sender.notice(
                        format_args!("Pick a quip number from 1 to {}!", submissions.len())
                            .yellow(),
                    )?;
                }
            }
        }
    };
    collect.run_for(Duration::from_secs(20)).await?;

    let mut counts = vec![0usize; submissions.len()];
    for &idx in ballots.values() {
        counts[idx] += 1;
    }
    let top_votes = counts.iter().copied().max().unwrap_or(0);
    let tied_indices: Vec<usize> = counts
        .iter()
        .enumerate()
        .filter_map(|(idx, &c)| (c == top_votes).then_some(idx))
        .collect();

    let best_idx = if tied_indices.len() > 1 {
        let mut unique_finalists = Vec::new();
        for &idx in &tied_indices {
            let author = &submissions[idx].0;
            if !unique_finalists.contains(author) {
                unique_finalists.push(author.clone());
            }
        }
        if unique_finalists.len() > 1 {
            match tiebreaker_buzz_in(bot, room, &unique_finalists).await? {
                Some(buzz_winner) => tied_indices
                    .iter()
                    .copied()
                    .find(|&idx| submissions[idx].0 == buzz_winner)
                    .unwrap_or(tied_indices[0]),
                None => tied_indices[0],
            }
        } else {
            tied_indices[0]
        }
    } else {
        tied_indices[0]
    };

    let winner = submissions[best_idx].clone();
    room.say(irc_format!(
        "{} {} wins Quiplash ({top_votes} votes) with: \"{}\"!",
        "🏆".bold(),
        winner.0.nick().bold().green(),
        winner.1,
    ))?;
    Ok(Some(winner))
}

#[cfg(test)]
#[path = "quiplash_test.rs"]
mod test;
