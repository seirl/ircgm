//! # Coup — Ambassador Exchange & Out-of-Turn Steal (2 Players)
//!
//! ## How to Play
//! 1. Each player holds 2 secret Court Influence cards (`Duke`, `Captain`, `Ambassador`, or
//!    `Contessa`) and `2` coins.
//! 2. The active player claims **Ambassador** to draw 2 cards from the Court deck in DM, viewing all
//!    4 cards and sending 2 DM replies naming the 2 cards they want to keep — or typing `!cancel` at
//!    any point to abort the exchange and keep their original hand.
//! 3. Meanwhile in `#lobby`, a rival can declare an out-of-turn **Captain Steal** (`!steal`) against
//!    that exact player! Even while the target is still inside their Ambassador DM wizard, the Steal
//!    defense prompt (`block captain`, `block ambassador`, or `allow`) queues cleanly behind the
//!    exchange and arrives on the wire the instant the Ambassador wizard finishes or is cancelled.
//!
//! ## Library Mechanics Showcased
//! - **Automatic [`ExclusiveDm`] prompt serialization:** Firing a concurrent `target.ask(...)` while
//!   `target.exclusive()` is already held in another branch of `tokio::try_join!` queues the second
//!   prompt on [`ExclusiveDm`] so the player never receives interleaved DM questions or crossed answers.
//! - **Cancelable multi-step free-text DM wizard:** [`ExclusiveDm::with_ignore_commands`] lets free-text
//!   card names flow into `dm.inbox().next_chat()` while `!cancel` bypasses the wizard and trips the
//!   outer `.until(...)` supervisor, immediately dropping the DM lock and unblocking the queued Steal prompt.

use std::time::Duration;

use ircgm::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Duke,
    Captain,
    Ambassador,
    Contessa,
}

impl Role {
    pub fn parse(input: &str) -> Option<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "duke" => Some(Self::Duke),
            "captain" => Some(Self::Captain),
            "ambassador" => Some(Self::Ambassador),
            "contessa" => Some(Self::Contessa),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Duke => "Duke",
            Self::Captain => "Captain",
            Self::Ambassador => "Ambassador",
            Self::Contessa => "Contessa",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StealResolution {
    Blocked(Role),
    Stolen { coins: u8 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoupTurnOutcome {
    pub final_hand: [Role; 2],
    pub exchanged: bool,
    pub steal: Option<StealResolution>,
}

/// Lobby entrypoint: recruits 2 players and runs an Ambassador Exchange + concurrent Steal round.
pub async fn play(room: &Room) -> Result<Option<CoupTurnOutcome>> {
    let Some(players) =
        crate::recruit_players(room, "Coup: Bluff & Steal", 2, Duration::from_secs(30)).await?
    else {
        return Ok(None);
    };

    let active = &players[0];
    let rival = &players[1];
    room.say(
        active.nick().bold()
            + " claims Ambassador in DM! "
            + rival.nick().bold()
            + " may type "
            + "!steal".bold().yellow()
            + " in #lobby to raid their treasury!",
    )?;

    let steal_branch = async {
        let mut inbox = room.inbox();
        let steal_trigger = inbox
            .wait_for(|m| m.sender.id() == rival.id() && m.is_cmd("!steal"))
            .with_timeout(Duration::from_secs(30))
            .optional()
            .await?;
        match steal_trigger {
            Some(_) => resolve_steal_attempt(room, rival, active).await.map(Some),
            None => Ok(None),
        }
    };

    let initial_hand = [Role::Duke, Role::Contessa];
    let court_draw = [Role::Captain, Role::Ambassador];

    let (opt_new_hand, steal) = tokio::try_join!(
        ambassador_exchange_or_cancel(active, initial_hand, court_draw),
        steal_branch,
    )?;

    Ok(Some(CoupTurnOutcome {
        final_hand: opt_new_hand.unwrap_or(initial_hand),
        exchanged: opt_new_hand.is_some(),
        steal,
    }))
}

/// Multi-step free-text DM wizard where `player` picks 2 cards to keep from `hand + drawn`,
/// or aborts mid-wizard with `!cancel`.
pub async fn ambassador_exchange_or_cancel(
    player: &User,
    hand: [Role; 2],
    drawn: [Role; 2],
) -> Result<Option<[Role; 2]>> {
    let wizard = async {
        let mut pool = vec![hand[0], hand[1], drawn[0], drawn[1]];
        let mut dm = player.exclusive().await?.with_ignore_commands();
        let pool_names: Vec<&str> = pool.iter().map(|r| r.label()).collect();
        dm.say(format!(
            "🃏 Ambassador Exchange — pool: [{}]. Send 2 messages naming the 2 cards to keep (or `!cancel`):",
            pool_names.join(", ")
        ))?;

        let mut kept = Vec::with_capacity(2);
        while kept.len() < 2 {
            let reply = dm.inbox().next_chat().await?;
            match Role::parse(&reply.text)
                .and_then(|role| pool.iter().position(|&r| r == role).map(|idx| (idx, role)))
            {
                Some((idx, role)) => {
                    pool.remove(idx);
                    kept.push(role);
                    dm.say(format!("Kept {} ({}/2).", role.label(), kept.len()))?;
                }
                None => {
                    let rem: Vec<&str> = pool.iter().map(|r| r.label()).collect();
                    dm.say(format!("Choose an available card from [{}]!", rem.join(", ")).yellow())?;
                }
            }
        }
        Ok::<_, Error>([kept[0], kept[1]])
    }
    .with_timeout(Duration::from_secs(30));

    match wizard
        .until_ok(player.inbox().into_wait_cmd("!cancel"))
        .await
    {
        Outcome::Completed(res) => res.map(Some),
        Outcome::Stopped(_) => {
            player.say("Ambassador exchange cancelled — keeping original hand.".yellow())?;
            Ok(None)
        }
    }
}

/// Prompts `target` via DM (`ExclusiveDm`-serialized) to block `thief`'s steal or allow it.
pub async fn resolve_steal_attempt(
    room: &Room,
    thief: &User,
    target: &User,
) -> Result<StealResolution> {
    room.say(
        "🪙 ".bold()
            + thief.nick().bold()
            + " attempts to STEAL 2 coins from "
            + target.nick().bold()
            + "!",
    )?;

    let response = target
        .ask(
            format!(
                "{} is stealing 2 coins from you! Reply `block captain`, `block ambassador`, or `allow` (20s):",
                thief.nick()
            ),
            |m| match m.text.trim().to_ascii_lowercase().as_str() {
                "block captain" => Verdict::Accept(StealResolution::Blocked(Role::Captain)),
                "block ambassador" => Verdict::Accept(StealResolution::Blocked(Role::Ambassador)),
                "allow" => Verdict::Accept(StealResolution::Stolen { coins: 2 }),
                _ => Verdict::reject(
                    "Reply `block captain`, `block ambassador`, or `allow`.".yellow(),
                ),
            },
        )
        .reply_timeout(Duration::from_secs(20))
        .ignore_commands()
        .warn_remaining(Duration::from_secs(5), "⏳ 5s left to block!".yellow())
        .optional()
        .await?;

    let resolution = response.unwrap_or(StealResolution::Stolen { coins: 2 });
    match resolution {
        StealResolution::Blocked(role) => {
            room.say(
                target.nick().bold()
                    + format!(" blocks the steal claiming {}!", role.label()).cyan().bold(),
            )?;
        }
        StealResolution::Stolen { coins } => {
            room.say(
                thief.nick().bold()
                    + format!(" steals {coins} coins from ")
                    + target.nick().bold()
                    + "!",
            )?;
        }
    }
    Ok(resolution)
}

#[cfg(test)]
#[path = "coup_test.rs"]
mod test;
