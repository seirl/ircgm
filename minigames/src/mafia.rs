//! # One-Night Werewolf & Bodyguard (4–5 Players)
//!
//! ## How to Play
//! 1. Players join in `#lobby` and receive secret roles via DM: **Werewolves**, **Bodyguards**
//!    (`0..=N`), and **Villagers**.
//! 2. **Night Phase (`30s`):**
//!    - If the bot holds `@` chanop, it updates the topic, sets `+m` (moderated), devoices dead
//!      players, and voices living players ([`start_mafia_night`]).
//!    - Online Bodyguards receive a radio check ([`Bot::exclusive_available`]) while Werewolves
//!      enter a private pack DM [`Huddle`] ([`Bot::huddle_available`]) with `[Wolves]` relay,
//!      command suppression, and [`Verdict::Reject`] target validation (`!kill <nick>`).
//!    - Once the pack locks in a target, all `0..=N` online Bodyguards have a `5s` midnight reflex
//!      window ([`ircgm::race_all`]) to DM `!protect <nick>` — saving the victim if they match!
//! 3. **Dawn, `#graveyard` & Secret Day Vote (`45s`):**
//!    - Any unguarded victim is eliminated (`-v` devoiced) and non-blockingly invited to a dead-chat
//!      `#graveyard-xxxxxx` [`Breakout`] via [`Breakout::invite_available`].
//!    - All surviving players cast a simultaneous secret DM ballot (`!vote <nick>`, `.lossy()` +
//!      `with_deadline`). Exiling a Werewolf wins for **Town**; otherwise **Werewolves Win**!
//!
//! ## Library Mechanics Showcased
//! - **`005 MODES`-batched channel moderation:** [`Room::bot_is_op`], [`Room::set_topic`],
//!   [`Room::set_mode`], [`Room::devoice_many`], and [`Room::voice_many`].
//! - **Drop-out resilient party primitives:** [`Bot::huddle_available`] (`with_relay`),
//!   [`Bot::exclusive_available`], and [`Breakout::invite_available`] skip offline/AFK players
//!   cleanly without stalling or aborting the session.
//! - **Dynamic `0..=N` interrupt racing ([`ircgm::race_all`]):** Races bodyguard `!protect` DMs and
//!   stays safely `Pending` (never panicking!) when `0` guards are online.
//! - **Flood-tolerant shared-deadline voting:** Combines `.lossy()` [`Inbox`]es with
//!   [`ircgm::GameFutureExt::with_deadline`].

use std::collections::HashMap;
use std::time::Duration;

use futures::future::try_join_all;
use ircgm::prelude::*;
use ircgm::race_all;
use tokio::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MafiaWinner {
    Town,
    Werewolves,
}

/// Lobby entrypoint: recruits 5 players (`2 Werewolves`, `1 Bodyguard`, `2 Villagers`) and runs a round.
pub async fn play(bot: &Bot, room: &Room) -> Result<Option<MafiaWinner>> {
    let Some(mut players) =
        crate::recruit_players(room, "One-Night Werewolf & Bodyguard", 5, Duration::from_secs(30))
            .await?
    else {
        return Ok(None);
    };
    fastrand::shuffle(&mut players);
    let wolves = players[..2].to_vec();
    let guards = vec![players[2].clone()];
    let villagers = players[3..].to_vec();

    for w in &wolves {
        let _ = w.say("🐺 You are a WEREWOLF!".red().bold());
    }
    for g in &guards {
        let _ = g.say("🛡️ You are a BODYGUARD!".cyan().bold());
    }
    for v in &villagers {
        let _ = v.say("🏡 You are a VILLAGER!".green().bold());
    }

    run_mafia_round(bot, room, &wolves, &guards, &villagers)
        .await
        .map(Some)
}

/// Sets channel topic, `+m`, devoices `dead`, and voices `alive` at nightfall.
pub fn start_mafia_night(room: &Room, alive: &[User], dead: &[User], night: u32) -> Result<()> {
    room.set_topic(format!("🌙 Mafia — Night {night} ({} alive)", alive.len()))?;
    room.set_mode("+m", &[])?;
    room.devoice_many(dead)?;
    room.voice_many(alive)?;
    Ok(())
}

/// Runs a private channel-free Werewolf pack deliberation via [`Bot::huddle_available`].
pub async fn werewolf_pack_kill(
    bot: &Bot,
    wolves: &[User],
    townsfolk: &[User],
) -> Result<Option<User>> {
    let mut pack = bot
        .huddle_available(wolves)
        .await?
        .with_relay("[Wolves] ".red().bold())
        .with_ask_config(
            &AskConfig::new()
                .reply_timeout(Duration::from_secs(30))
                .timeout_message("⏳ Dawn breaks—the pack missed its chance!".yellow()),
        );

    if pack.users().is_empty() {
        return Ok(None);
    }

    let pick = pack
        .ask(
            "🐺 Whisper here to deliberate, or `!kill <nick>` to strike (30s):",
            |m| {
                let Some(target) = m.cmd_args("!kill") else {
                    return Verdict::Ignore;
                };
                match townsfolk.iter().find(|u| u.eq_nick(target)) {
                    Some(victim) => Verdict::Accept((m.sender.clone(), victim.clone())),
                    None => Verdict::reject("Pick a living non-Werewolf townsfolk!".yellow()),
                }
            },
        )
        .settled()
        .await?;

    match pick {
        Turn::Reply((killer, victim)) => {
            pack.say(
                "🐺 ".bold()
                    + killer.nick().bold()
                    + " locked in "
                    + victim.nick().bold().red()
                    + "!",
            )?;
            Ok(Some(victim))
        }
        Turn::TimedOut | Turn::Left { .. } | Turn::Unreachable { .. } => Ok(None),
    }
}

/// Collects simultaneous secret `!vote <nick>` DMs from all `alive` players before a shared deadline.
pub async fn collect_secret_votes(
    room: &Room,
    alive: &[User],
) -> Result<Vec<(User, Option<String>)>> {
    let mut dms: Vec<_> = alive
        .iter()
        .map(|u| (u.clone(), u.inbox().lossy()))
        .collect();
    room.say("🗳️ Day vote! Survivors, DM me `!vote <nick>` within 45 seconds.")?;
    let deadline = Instant::now() + Duration::from_secs(45);

    try_join_all(dms.iter_mut().map(|(voter, inbox)| async move {
        let pick = inbox
            .wait_map(|m| {
                m.cmd_args("!vote")
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            })
            .with_deadline(deadline)
            .settled()
            .await?
            .into_reply();
        Ok::<_, Error>((voter.clone(), pick))
    }))
    .await
}

/// Orchestrates Night (`+m`, wolf huddle, bodyguard `race_all`), `#graveyard`, and secret Day Vote.
pub async fn run_mafia_round(
    bot: &Bot,
    room: &Room,
    wolves: &[User],
    guards: &[User],
    villagers: &[User],
) -> Result<MafiaWinner> {
    let townsfolk: Vec<User> = guards.iter().chain(villagers.iter()).cloned().collect();
    let mut alive: Vec<User> = wolves.iter().chain(townsfolk.iter()).cloned().collect();
    let mut dead: Vec<User> = Vec::new();
    let graveyard = bot.breakout(&crate::breakout_slug(room, "graveyard")).await?;

    if room.bot_is_op() {
        start_mafia_night(room, &alive, &dead, 1)?;
    }

    // Brief online Bodyguards without failing if one disconnected (`exclusive_available`):
    {
        let online_guards = bot.exclusive_available(guards).await?;
        for dm in &online_guards {
            let _ = dm.say("🛡️ Night watch: when the wolves prowl, DM `!protect <nick>` (5s)!");
        }
    }

    if let Some(victim) = werewolf_pack_kill(bot, wolves, &townsfolk).await? {
        // 5s midnight reflex window across `0..=N` online Bodyguards (`race_all` stays Pending if 0):
        let protectors = guards.iter().filter(|g| g.is_online()).map(|g| {
            let guard = g.clone();
            let townsfolk = townsfolk.clone();
            async move {
                let saved = guard
                    .inbox()
                    .into_wait_map(|m| {
                        let target = m.cmd_args("!protect")?;
                        townsfolk.iter().find(|u| u.eq_nick(target)).cloned()
                    })
                    .ok_or_pending()
                    .await;
                Ok::<(User, User), Error>((guard, saved))
            }
        });

        let saved_victim = match race_all(protectors)
            .with_timeout(Duration::from_secs(5))
            .optional()
            .await?
        {
            Some((_hero, protected)) => protected.id() == victim.id(),
            None => false,
        };

        if saved_victim {
            room.say("☀️ Dawn breaks! 🛡️ A Bodyguard thwarted the Werewolves' attack!".cyan().bold())?;
        } else {
            room.say(
                "☀️ Dawn breaks! ".yellow()
                    + victim.nick().bold().red()
                    + " was devoured in the night!",
            )?;
            alive.retain(|u| u.id() != victim.id());
            if room.bot_is_op() {
                let _ = room.devoice(&victim);
            }
            dead.push(victim);
            // Non-blocking dead-chat invite (`invite_available` won't fail if victim is AFK/offline):
            let _ = graveyard
                .invite_available(&dead, Duration::from_secs(3))
                .await?;
        }
    } else {
        room.say("☀️ Dawn breaks! Everyone survived the night!".green())?;
    }

    let ballots = collect_secret_votes(room, &alive).await?;
    let mut tally: HashMap<UserId, usize> = HashMap::new();
    for (_voter, pick_opt) in ballots {
        if let Some(target_nick) = pick_opt {
            if let Some(target_user) = alive.iter().find(|u| u.eq_nick(&target_nick)) {
                *tally.entry(target_user.id()).or_default() += 1;
            }
        }
    }

    let max_votes = tally.values().copied().max().unwrap_or(0);
    let leaders: Vec<&User> = alive
        .iter()
        .filter(|u| max_votes > 0 && tally.get(&u.id()) == Some(&max_votes))
        .collect();

    let winner = if let [exiled] = leaders.as_slice() {
        let was_wolf = wolves.iter().any(|w| w.id() == exiled.id());
        if was_wolf {
            room.say(
                "🎉 The town exiled ".green().bold()
                    + exiled.nick().bold()
                    + " (a Werewolf)! TOWN WINS!",
            )?;
            MafiaWinner::Town
        } else {
            room.say(
                "🐺 The town exiled ".red().bold()
                    + exiled.nick().bold()
                    + " (an innocent Townsfolk)! WEREWOLVES WIN!",
            )?;
            MafiaWinner::Werewolves
        }
    } else {
        room.say("🐺 The vote deadlocked—no one was exiled! WEREWOLVES WIN!".red().bold())?;
        MafiaWinner::Werewolves
    };

    if room.bot_is_op() {
        let _ = room.set_mode("-m", &[]);
    }
    Ok(winner)
}

#[cfg(test)]
#[path = "mafia_test.rs"]
mod test;
