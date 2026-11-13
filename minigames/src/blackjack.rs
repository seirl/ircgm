//! # Channel Blackjack (1–4 Players)
//!
//! ## How to Play
//! 1. Up to 4 players `!join` at the table (or a pre-selected slice is passed to [`run_round`]),
//!    then anyone in the channel types `!deal` to cut the shoe and deal opening 2-card hands.
//! 2. Going around the table in order, the bot asks each player in the channel to `hit` or
//!    `stand` (`30s` per decision):
//!    - Aces count as `11` or `1` automatically to avoid busting when possible.
//!    - Exceeding `21` busts immediately; taking longer than `30s` auto-stands (calling out the
//!      player's live `/nick` via [`ircgm::TimeoutCtx`]); leaving or getting kicked mid-hand folds cleanly.
//! 3. Once all players finish, the dealer reveals its hole card, hits until `17+`, and settles
//!    every surviving hand (`WIN`, `PUSH`, or `LOSE`).
//!
//! ## Library Mechanics Showcased
//! - **Player-subset & single-player channel turns ([`Room::ask_users`] & [`Room::ask_user`]):**
//!   Restricts `!deal` to seated players (ignoring spectators) and scopes each `hit`/`stand` turn
//!   to one [`User`] inside a busy public channel.
//! - **Timed lobby seat collection ([`ircgm::GameFutureExt::run_for`]):** Runs a `20s` join loop
//!   that exits early at 4 players with zero `Ok::<_, Error>(())` turbofish ceremony.
//! - **Fault-isolated player turns ([`Answer`]):** Default [`ircgm::Ask`] awaiting resolves to
//!   `Result<Answer<Move>>`, distinguishing [`Answer::Ok`], [`Answer::TimedOut`] (auto-stand), and
//!   [`Answer::Left`] (`PART` / `KICK` / `QUIT` mid-hand folds cleanly without aborting the table).
//! - **Live `/nick`-aware timeout messages ([`ircgm::TimeoutCtx`]):** Renders each player's *current*
//!   nickname if they renamed mid-turn before their `30s` clock expired.

use std::collections::VecDeque;
use std::time::Duration;

use ircgm::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    Hit,
    Stand,
}

fn draw_card(shoe: &mut VecDeque<u8>) -> u8 {
    shoe.pop_front().unwrap_or_else(|| {
        const CARDS: &[u8] = &[2, 3, 4, 5, 6, 7, 8, 9, 10, 10, 10, 10, 11];
        CARDS[fastrand::usize(..CARDS.len())]
    })
}

/// Computes the best Blackjack total (`<= 21` if possible) adjusting Aces (`11 -> 1`).
pub fn hand_value(cards: &[u8]) -> u8 {
    let mut sum: u16 = cards.iter().map(|&c| u16::from(c)).sum();
    let mut aces = cards.iter().filter(|&&c| c == 11).count();
    while sum > 21 && aces > 0 {
        sum -= 10;
        aces -= 1;
    }
    sum as u8
}

/// Recruits 1–4 players via `!join` (`20s` window) and runs a Blackjack round.
pub async fn play(_bot: &Bot, lobby: &Room) -> Result<crate::PlayOutcome> {
    let Some(players) =
        crate::recruit_players(lobby, "Blackjack", 1..=4, Duration::from_secs(20)).await?
    else {
        return Ok(crate::PlayOutcome::NotEnoughPlayers);
    };

    run_round(lobby, &players).await?;
    Ok(crate::PlayOutcome::Completed)
}

/// Runs a Blackjack round with random draws.
pub async fn run_round(room: &Room, players: &[User]) -> Result<()> {
    run_round_with_shoe(room, players, VecDeque::new()).await
}

/// Runs a Blackjack round pulling from `shoe` first (falling back to random draws once empty).
pub async fn run_round_with_shoe(
    room: &Room,
    players: &[User],
    mut shoe: VecDeque<u8>,
) -> Result<()> {
    let room = room.with_ask_config(
        &AskConfig::new()
            .reply_timeout(Duration::from_secs(30))
            .timeout_message_fn(|ctx| match ctx.user {
                Some(u) => irc_format!("{}: ⏳ too slow, auto-standing!", u.nick().bold().yellow()),
                None => "⏳ Time's up!".yellow(),
            }),
    );

    room.ask_users(
        players,
        "Seated players: anyone type `!deal` to cut the deck!",
        |m| m.is_cmd("!deal").then_some(()),
    )
    .required()
    .await?;

    let mut dealer = vec![draw_card(&mut shoe), draw_card(&mut shoe)];
    let mut standing_hands: Vec<(User, u8)> = Vec::new();

    room.say(format_args!("Dealer shows [{} , ?].", dealer[0]))?;

    for player in players {
        let mut cards = vec![draw_card(&mut shoe), draw_card(&mut shoe)];
        loop {
            let total = hand_value(&cards);
            if total > 21 {
                room.say(irc_format!("{} BUSTS with {total}!", player.nick().bold().red()))?;
                break;
            }
            if total == 21 {
                room.say(irc_format!("{} hits 21!", player.nick().bold().green()))?;
                standing_hands.push((player.clone(), total));
                break;
            }

            match room
                .ask_user(
                    player,
                    irc_format!("{} (total {total}): `hit` or `stand`?", player.nick().bold()),
                    |m| {
                        let word = m.text.trim();
                        if word.eq_ignore_ascii_case("hit") {
                            AskMatch::accept(Move::Hit)
                        } else if word.eq_ignore_ascii_case("stand") {
                            AskMatch::accept_say(
                                Move::Stand,
                                irc_format!("{} stands on {total}.", player.nick().bold()),
                            )
                        } else {
                            AskMatch::ignore()
                        }
                    },
                )
                .await?
            {
                Answer::Ok(Move::Hit) => {
                    let card = draw_card(&mut shoe);
                    cards.push(card);
                    room.say(irc_format!(
                        "{} draws {card} (total {})!",
                        player.nick().bold(),
                        hand_value(&cards),
                    ))?;
                }
                Answer::Ok(Move::Stand) | Answer::TimedOut => {
                    standing_hands.push((player.clone(), total));
                    break;
                }
                Answer::Left { nick, .. } => {
                    room.say(irc_format!("{} left the table—hand folded.", nick.bold()))?;
                    break;
                }
                Answer::Unreachable { .. } => break,
            }
        }
    }

    if !standing_hands.is_empty() {
        while hand_value(&dealer) < 17 {
            dealer.push(draw_card(&mut shoe));
        }
        let dealer_total = hand_value(&dealer);
        room.say(format_args!("Dealer finishes with {dealer_total}."))?;

        for (player, score) in standing_hands {
            let outcome = if dealer_total > 21 || score > dealer_total {
                "WINS!".green().bold()
            } else if score == dealer_total {
                "PUSH.".yellow().bold()
            } else {
                "loses.".red()
            };
            room.say(irc_format!("{} ({score}) {outcome}", player.nick().bold()))?;
        }
    }

    Ok(())
}

#[cfg(test)]
#[path = "blackjack_test.rs"]
mod test;
