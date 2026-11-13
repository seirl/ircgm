//! # Silent-Snipe Auction (2–5 Players)
//!
//! ## How to Play
//! 1. The auctioneer places a mystery relic on the block in `#lobby` (starting bid `10g`, `20s`
//!    gavel timer) with a secret true vault value (`40g`–`150g`).
//! 2. While the gavel clock runs, bidders can act **either openly in `#lobby` OR secretly via DM**:
//!    - **Public `#lobby` (`!bid <gold>`):** Raises the high bid openly (`+5g` minimum increment).
//!    - **Secret DM (`!appraise`, once per player):** Privately reveals the relic's true vault value
//!      so you can spot bargains or bait rivals into overpaying!
//!    - **Secret DM (`!proxy <max_gold>`):** Registers a hidden auto-bid ceiling; whenever someone
//!      bids below your proxy ceiling, the auctioneer automatically outbids them by `+5g` up to your
//!      ceiling (if two proxies collide, whichever arrived earlier on the wire wins ties!).
//! 3. When the gavel falls, the highest bidder wins the relic and scores `true_value - winning_bid`
//!    net profit (negative if they fell victim to the Winner's Curse!).
//!
//! ## Library Mechanics Showcased
//! - **Unified channel + DM [`InboxScope`] ([`Bot::inbox_scope`]):** Subscribes `InboxScope::new().room(room).dms_from(players)`
//!   so public `#lobby` `!bid`s and secret DM `!proxy` / `!appraise` whispers arrive in **one**
//!   strictly monotonic `ChatMessage::seq` stream — eliminating race conditions between simultaneous
//!   channel and DM plays.
//! - **Source discrimination on a merged [`Inbox`]:** Branches cleanly on [`ChatMessage::is_dm`] vs
//!   [`ChatMessage::is_in`] while tracking bidders by rename-proof [`UserId`].

use std::collections::HashSet;
use std::time::Duration;

use ircgm::prelude::*;
use tokio::time::Instant;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuctionWinner {
    pub winner: User,
    pub winning_bid: u32,
    pub true_value: u32,
    pub net_profit: i64,
}

const MIN_INCREMENT: u32 = 5;
const OPENING_MIN: u32 = 10;

/// Lobby entrypoint: recruits 2–5 bidders and auctions a mystery relic.
pub async fn play(bot: &Bot, lobby: &Room) -> Result<crate::PlayOutcome> {
    let Some(players) =
        crate::recruit_players(lobby, "Silent-Snipe Auction", 2..=5, Duration::from_secs(30))
            .await?
    else {
        return Ok(crate::PlayOutcome::NotEnoughPlayers);
    };
    let true_value = 40 + fastrand::u32(0..=22) * 5;
    let _ = run_auction(
        bot,
        lobby,
        &players,
        "Sunken Astrolabe",
        true_value,
        Duration::from_secs(20),
    )
    .await?;
    Ok(crate::PlayOutcome::Completed)
}

fn min_required_bid(high_bid: Option<&(User, u32)>) -> u32 {
    match high_bid {
        Some((_, amt)) => amt.saturating_add(MIN_INCREMENT),
        None => OPENING_MIN,
    }
}

fn apply_proxies(
    room: &Room,
    high_bid: &mut Option<(User, u32)>,
    proxies: &[(User, u32)],
) -> Result<()> {
    loop {
        let needed = min_required_bid(high_bid.as_ref());
        let challenger = proxies.iter().find(|(u, max_amt)| {
            let is_already_leader = high_bid
                .as_ref()
                .is_some_and(|(leader, _)| leader == u);
            !is_already_leader && *max_amt >= needed
        });

        let Some((proxy_user, proxy_max)) = challenger else {
            break;
        };

        // If the current leader ALSO has a proxy ceiling >= `needed`, escalate straight to the contest!
        let next_amt = needed.min(*proxy_max);
        *high_bid = Some((proxy_user.clone(), next_amt));
        room.say(
            "🕵️ Auto-proxy raises the bid to ".cyan()
                + format!("{next_amt}g").bold()
                + " for "
                + proxy_user.nick().bold()
                + "!",
        )?;
    }
    Ok(())
}

/// Runs a single-lot auction merging public `room` bids and private DM `!appraise` / `!proxy` commands.
pub async fn run_auction(
    bot: &Bot,
    room: &Room,
    players: &[User],
    item_name: &str,
    true_value: u32,
    duration: Duration,
) -> Result<Option<AuctionWinner>> {
    let allowed: HashSet<UserId> = players.iter().map(User::id).collect();
    let scope = InboxScope::new().room(room).dms_from(players);
    let mut inbox = bot.inbox_scope(scope);

    room.say(
        "🔨 AUCTION OPEN: ".yellow().bold()
            + item_name.bold()
            + format!(" (min {OPENING_MIN}g, +{MIN_INCREMENT}g increments — `#lobby`: `!bid <g>` | DM: `!appraise`, `!proxy <max>`)!"),
    )?;

    let deadline = Instant::now() + duration;
    let mut high_bid: Option<(User, u32)> = None;
    let mut proxies: Vec<(User, u32)> = Vec::new();
    let mut appraised: HashSet<UserId> = HashSet::new();

    let collect = async {
        loop {
            let msg = inbox.next_chat().await?;
            if !allowed.contains(&msg.sender.id()) {
                continue;
            }

            if msg.is_dm() {
                if msg.is_cmd("!appraise") {
                    if appraised.insert(msg.sender.id()) {
                        msg.sender.say(format!(
                            "🔍 Secret Appraisal: {item_name} is genuinely worth {true_value}g!"
                        ))?;
                    } else {
                        msg.sender.say("You already used your appraisal!".yellow())?;
                    }
                } else if msg.is_cmd("!proxy") {
                    match msg.cmd_arg("!proxy").and_then(|a| a.parse::<u32>().ok()) {
                        Some(max_amt) if max_amt >= min_required_bid(high_bid.as_ref()) => {
                            if let Some(slot) =
                                proxies.iter_mut().find(|(u, _)| u == &msg.sender)
                            {
                                slot.1 = max_amt;
                            } else {
                                proxies.push((msg.sender.clone(), max_amt));
                            }
                            msg.sender
                                .say(format!("✅ Secret proxy ceiling set to {max_amt}g."))?;
                            apply_proxies(room, &mut high_bid, &proxies)?;
                        }
                        _ => {
                            msg.sender.say(
                                format!(
                                    "Usage: `!proxy <max_gold>` (at least {}g).",
                                    min_required_bid(high_bid.as_ref())
                                )
                                .yellow(),
                            )?;
                        }
                    }
                }
            } else if msg.is_in(room) && msg.is_cmd("!bid") {
                let needed = min_required_bid(high_bid.as_ref());
                match msg.cmd_arg("!bid").and_then(|a| a.parse::<u32>().ok()) {
                    Some(amt) if amt >= needed => {
                        high_bid = Some((msg.sender.clone(), amt));
                        room.say(
                            "💰 ".green()
                                + msg.sender.nick().bold()
                                + format!(" bids {amt}g!"),
                        )?;
                        apply_proxies(room, &mut high_bid, &proxies)?;
                    }
                    _ => {
                        room.say(format!("Minimum bid is `!bid {needed}`!").yellow())?;
                    }
                }
            }
        }
    };

    collect.run_until_deadline(deadline).await?;

    match high_bid {
        Some((winner, winning_bid)) => {
            let net_profit = i64::from(true_value) - i64::from(winning_bid);
            room.say(
                "🔨 SOLD to ".green().bold()
                    + winner.nick().bold()
                    + format!(" for {winning_bid}g! True value: {true_value}g (net {net_profit:+}g)."),
            )?;
            Ok(Some(AuctionWinner {
                winner,
                winning_bid,
                true_value,
                net_profit,
            }))
        }
        None => {
            room.say("🔨 Gavel falls with no bids!".yellow())?;
            Ok(None)
        }
    }
}

#[cfg(test)]
#[path = "auction_test.rs"]
mod test;
