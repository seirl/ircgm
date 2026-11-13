//! # Cold War Hotline — Prisoner's Dilemma (2 Players)
//!
//! ## How to Play
//! 1. Two players represent the **USA** and the **USSR** (recruited via `!join` in `#lobby` or
//!    passed directly to [`run_cold_war`]).
//! 2. The bot opens **two** private invite-only [`Breakout`] war rooms (`#usa-xxxxxx` and
//!    `#ussr-xxxxxx`) and invites each premier into their respective command bunker.
//! 3. **Hotline Phase (`20s`):** A live Moscow–Washington hotline ([`Bot::inbox_rooms`]) relays
//!    every non-command chat message (preserving IRC colors/styles via [`ChatMessage::to_styled`])
//!    between the two war rooms so the premiers can negotiate, bluff, or promise peace. Either side
//!    can type `!ready` to end talks early once both sides are ready.
//! 4. **Launch Key Phase (`15s`):** The hotline goes dark! Both premiers simultaneously issue
//!    their secret order in their private war room (`!stand_down` or `!launch`; hesitating past
//!    `15s` defaults to `!stand_down`):
//!    - Both `!stand_down` → **Peace Prevails** (mutual victory!).
//!    - One `!launch`, one `!stand_down` → **First Strike** (the launcher wins unilateral victory!).
//!    - Both `!launch` → **Mutually Assured Destruction** (both nations are reduced to ash!).
//! 5. When the round finishes, dropping both [`Breakout`] handles automatically kicks both players
//!    and closes the temporary war rooms.
//!
//! ## Library Mechanics Showcased
//! - **Multi-[`Breakout`] orchestration:** Opens and tears down two parallel invite-only bunkers
//!   (`#usa-xxxxxx` and `#ussr-xxxxxx`) in lockstep via `tokio::try_join!`.
//! - **Multi-channel styled relay ([`Bot::inbox_rooms`] + [`ChatMessage::to_styled`]):** Streams
//!   chat from both war rooms in wire order while preserving each player's mIRC colors/bold/italics.
//! - **Simultaneous secret channel prompts:** Runs parallel [`Room::ask_user`] + `.optional()`
//!   decisions in separate private rooms after closing the relay phase.

use std::time::Duration;

use ircgm::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    StandDown,
    Launch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColdWarOutcome {
    Peace,
    UsaFirstStrike,
    UssrFirstStrike,
    MutuallyAssuredDestruction,
}

/// Recruits 2 players in `lobby` and runs a full Cold War Hotline game.
pub async fn play(bot: &Bot, lobby: &Room) -> Result<Option<ColdWarOutcome>> {
    let Some(players) = crate::recruit_players(lobby, "Cold War Hotline", 2, Duration::from_secs(30)).await? else {
        return Ok(None);
    };
    run_cold_war(bot, lobby, &players[0], &players[1])
        .await
        .map(Some)
}

/// Runs a 2-player Cold War Hotline match between `usa_premier` and `ussr_premier`.
pub async fn run_cold_war(
    bot: &Bot,
    lobby: &Room,
    usa_premier: &User,
    ussr_premier: &User,
) -> Result<ColdWarOutcome> {
    let usa = bot.breakout(&crate::breakout_slug(lobby, "usa")).await?;
    let ussr = bot.breakout(&crate::breakout_slug(lobby, "ussr")).await?;

    lobby.say(
        "☎️ ".bold()
            + usa_premier.nick().bold().light_blue()
            + " (USA) and "
            + ussr_premier.nick().bold().red()
            + " (USSR): report to your war rooms!",
    )?;

    tokio::try_join!(
        usa.invite_and_wait([usa_premier], Duration::from_secs(20)),
        ussr.invite_and_wait([ussr_premier], Duration::from_secs(20)),
    )?;

    usa.say("🇺🇸 Hotline connected to Moscow (20s)! Chat freely, or type `!ready` when done.")?;
    ussr.say("🚩 Hotline connected to Washington (20s)! Chat freely, or type `!ready` when done.")?;

    // Phase 1: Bidirectional styled bridge until 20s elapses or both premiers type `!ready`:
    {
        let mut bridge_in = bot.inbox_rooms([&*usa, &*ussr]);
        let (mut usa_ready, mut ussr_ready) = (false, false);

        let negotiate = async {
            while !(usa_ready && ussr_ready) {
                let msg = bridge_in.next_chat().await?;
                if msg.is_cmd("!ready") {
                    if msg.is_in(&usa) {
                        usa_ready = true;
                        ussr.say("[HOTLINE] Washington is ready to issue final orders.".yellow())?;
                    } else {
                        ussr_ready = true;
                        usa.say("[HOTLINE] Moscow is ready to issue final orders.".yellow())?;
                    }
                    continue;
                }
                if msg.is_in(&usa) {
                    ussr.say(
                        "[USA] ".light_blue().bold()
                            + "<"
                            + (&msg.nick).bold()
                            + "> "
                            + msg.to_styled(),
                    )?;
                } else {
                    usa.say(
                        "[USSR] ".red().bold() + "<" + (&msg.nick).bold() + "> " + msg.to_styled(),
                    )?;
                }
            }
            Ok(())
        };
        negotiate.run_for(Duration::from_secs(20)).await?;
    }

    // Phase 2: Simultaneous secret orders in each private war room:
    let parse_order = |m: &ChatMessage| match m.text.trim() {
        "!stand_down" => Some(Order::StandDown),
        "!launch" => Some(Order::Launch),
        _ => None,
    };

    let (usa_turn, ussr_turn) = tokio::try_join!(
        usa.ask_user(
            usa_premier,
            "🔒 Hotline severed! Issue final order (`!stand_down` or `!launch`, 15s):",
            parse_order,
        )
        .reply_timeout(Duration::from_secs(15))
        .settled(),
        ussr.ask_user(
            ussr_premier,
            "🔒 Hotline severed! Issue final order (`!stand_down` or `!launch`, 15s):",
            parse_order,
        )
        .reply_timeout(Duration::from_secs(15))
        .settled(),
    )?;

    let usa_order = usa_turn.into_reply().unwrap_or(Order::StandDown);
    let ussr_order = ussr_turn.into_reply().unwrap_or(Order::StandDown);

    let outcome = match (usa_order, ussr_order) {
        (Order::StandDown, Order::StandDown) => {
            lobby.say(
                "🕊️ PEACE PREVAILS! Both ".green().bold()
                    + usa_premier.nick().bold()
                    + " and "
                    + ussr_premier.nick().bold()
                    + " stood down!",
            )?;
            ColdWarOutcome::Peace
        }
        (Order::Launch, Order::StandDown) => {
            lobby.say(
                "🚀 FIRST STRIKE! "
                    + usa_premier.nick().bold().light_blue()
                    + " launched while "
                    + ussr_premier.nick().bold()
                    + " stood down!",
            )?;
            ColdWarOutcome::UsaFirstStrike
        }
        (Order::StandDown, Order::Launch) => {
            lobby.say(
                "🚀 FIRST STRIKE! "
                    + ussr_premier.nick().bold().red()
                    + " launched while "
                    + usa_premier.nick().bold()
                    + " stood down!",
            )?;
            ColdWarOutcome::UssrFirstStrike
        }
        (Order::Launch, Order::Launch) => {
            lobby.say("☢️ MUTUALLY ASSURED DESTRUCTION! Both superpowers launched!".bold().white().on_red())?;
            ColdWarOutcome::MutuallyAssuredDestruction
        }
    };

    Ok(outcome)
}

#[cfg(test)]
#[path = "cold_war_test.rs"]
mod test;
