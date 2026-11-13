//! # One-Night Werewolf & Bodyguard (4–5 Players)
//!
//! ## How to Play
//! 1. Players join in `#lobby` and receive secret roles via DM: **Werewolves**, **Bodyguards**
//!    (`0..=N`), and **Villagers**.
//! 2. **Night Phase (`30s`):**
//!    - If the bot holds `@` chanop, it updates the topic, sets `+m` (moderated), devoices dead
//!      players, and voices living players ([`start_mafia_night`]).
//!    - Werewolves enter a private pack DM [`Huddle`] ([`Bot::huddle_available`]) with `[Wolves]`
//!      relay, command suppression, [`AskMatch::reject`] target validation, and
//!      [`AskMatch::accept_say`] pack-wide lock-in confirmation (`!kill <nick>`).
//!    - Once the pack locks in a target, all `0..=N` online Bodyguards get a `5s` midnight reflex
//!      prompt ([`ircgm::race_all`] over [`User::ask`]) to DM `!protect <nick>` — saving the victim
//!      if they match!
//! 3. **Dawn, `#graveyard` & Secret Day Vote (`45s`):**
//!    - Any unguarded victim is eliminated (`-v` devoiced) and non-blockingly invited to a dead-chat
//!      `#graveyard-xxxxxx` [`Breakout`] via [`Breakout::invite_available`].
//!    - All surviving players cast a simultaneous secret DM ballot ([`User::ask`] +
//!      [`ircgm::Ask::reply_deadline`] + [`AskMatch::accept_say`] / [`AskMatch::reject`]). Exiling a
//!      Werewolf wins for **Town**; otherwise **Werewolves Win**!
//!
//! ## Library Mechanics Showcased
//! - **`005 MODES`-batched channel moderation:** [`Room::bot_is_op`], [`Room::set_topic`],
//!   [`Room::set_mode`], [`Room::devoice_many`], and [`Room::voice_many`].
//! - **Drop-out resilient party primitives:** [`Bot::huddle_available`] (`with_relay`),
//!   [`Bot::exclusive_available`], and [`Breakout::invite_available`] skip offline/AFK players
//!   cleanly without stalling or aborting the session.
//! - **Dynamic `0..=N` interrupt racing ([`ircgm::race_all`]):** Races bodyguard `!protect` DMs and
//!   stays safely `Pending` (never panicking!) when `0` guards are online.
//! - **Shared-deadline DM ballots:** Combines concurrent [`User::ask`] prompts with
//!   [`ircgm::Ask::reply_deadline`], [`AskMatch::accept_say`], and [`AskMatch::reject`].

use std::collections::HashMap;
use std::time::Duration;

use futures::future::try_join_all;
use ircgm::prelude::*;
use ircgm::race_all;
use tokio::time::Instant;

use crate::PlayOutcome;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MafiaWinner {
    Town,
    Werewolves,
}

/// RAII guard restoring `-m`, devoicing players, and restoring the prior channel topic when
/// `run_mafia_round` finishes or is cancelled externally mid-game.
struct MafiaRoomGuard<'a> {
    room: &'a Room,
    players: Vec<User>,
    prev_topic: Option<String>,
    active: bool,
}

impl<'a> MafiaRoomGuard<'a> {
    fn enter_night(room: &'a Room, alive: &[User], dead: &[User], night: u32) -> Result<Self> {
        let prev_topic = room.topic();
        let players: Vec<User> = alive.iter().chain(dead.iter()).cloned().collect();
        if room.bot_is_op() {
            start_mafia_night(room, alive, dead, night)?;
            Ok(Self {
                room,
                players,
                prev_topic,
                active: true,
            })
        } else {
            Ok(Self {
                room,
                players,
                prev_topic,
                active: false,
            })
        }
    }
}

impl Drop for MafiaRoomGuard<'_> {
    fn drop(&mut self) {
        if self.active && self.room.bot_is_op() {
            let _ = self.room.set_mode("-m", &[]);
            let _ = self.room.devoice_many(&self.players);
            let _ = self.room.set_topic(self.prev_topic.as_deref().unwrap_or(""));
        }
    }
}

/// Lobby entrypoint: recruits 4–5 players (`1–2 Werewolves`, `1 Bodyguard`, `2 Villagers`) and runs a round.
pub async fn play(bot: &Bot, lobby: &Room) -> Result<PlayOutcome> {
    let Some(mut players) = crate::recruit_players(
        lobby,
        "One-Night Werewolf & Bodyguard",
        4..=5,
        Duration::from_secs(30),
    )
    .await?
    else {
        return Ok(PlayOutcome::NotEnoughPlayers);
    };
    fastrand::shuffle(&mut players);
    let wolf_count = if players.len() >= 5 { 2 } else { 1 };
    let wolves = players[..wolf_count].to_vec();
    let guards = vec![players[wolf_count].clone()];
    let villagers = players[wolf_count + 1..].to_vec();

    for w in &wolves {
        let _ = w.say("🐺 You are a WEREWOLF!".red().bold());
    }
    for g in &guards {
        let _ = g.say("🛡️ You are a BODYGUARD!".cyan().bold());
    }
    for v in &villagers {
        let _ = v.say("🏡 You are a VILLAGER!".green().bold());
    }

    let _ = run_mafia_round(bot, lobby, &wolves, &guards, &villagers).await?;
    Ok(PlayOutcome::Completed)
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

    if pack.is_empty() {
        return Ok(None);
    }

    Ok(pack
        .ask(
            "🐺 Whisper here to deliberate, or `!kill <nick>` to strike (30s):",
            |m| {
                let Some(target) = m.cmd_arg("!kill") else {
                    return AskMatch::ignore();
                };
                match townsfolk.iter().find(|u| u.eq_nick(target)) {
                    Some(victim) => AskMatch::accept_say(
                        victim.clone(),
                        irc_format!(
                            "{} {} locked in {}!",
                            "🐺".bold(),
                            m.sender.nick().bold(),
                            victim.nick().bold().red(),
                        ),
                    ),
                    None => AskMatch::reject("Pick a living non-Werewolf townsfolk!".yellow()),
                }
            },
        )
        .await?
        .ok())
}

/// Collects simultaneous secret `!vote <nick>` DMs from all `alive` players before a shared deadline,
/// acknowledging valid ballots in DM and rejecting unknown/dead nicks without wasting the voter's turn.
pub async fn collect_secret_votes(
    room: &Room,
    alive: &[User],
) -> Result<Vec<(User, Option<User>)>> {
    let roster = Text::join(alive.iter().map(|u| u.nick().bold()), ", ");
    room.say("🗳️ Day vote! Survivors, check your DMs to cast `!vote <nick>` (45s).")?;
    let deadline = Instant::now() + Duration::from_secs(45);
    let prompt = irc_format!(
        "🗳️ Secret Day Ballot — reply `!vote <nick>` within 45s (alive: {roster}):"
    );

    try_join_all(alive.iter().map(|voter| {
        let prompt = prompt.clone();
        async move {
            let pick = voter
                .ask(prompt, |m| {
                    let Some(target) = m.cmd_arg("!vote") else {
                        if m.is_cmd("!vote") {
                            return AskMatch::reject(
                                "Usage: `!vote <nick>` (choose a living player).".yellow(),
                            );
                        }
                        return AskMatch::ignore();
                    };
                    match alive.iter().find(|u| u.eq_nick(target)) {
                        Some(target_user) => AskMatch::accept_say(
                            target_user.clone(),
                            irc_format!(
                                "{} {}!",
                                "✔ Secret ballot cast for".green(),
                                target_user.nick().bold(),
                            ),
                        ),
                        None => AskMatch::reject(
                            "Pick a living player's nick with `!vote <nick>`!".yellow(),
                        ),
                    }
                })
                .reply_deadline(deadline)
                .no_timeout_message()
                .await?
                .ok();
            Ok::<_, Error>((voter.clone(), pick))
        }
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
    let _room_guard = MafiaRoomGuard::enter_night(room, &alive, &dead, 1)?;

    // Brief online Bodyguards without failing if one disconnected (`exclusive_available`):
    {
        let online_guards = bot.exclusive_available(guards).await?;
        for dm in &online_guards {
            let _ = dm.say("🛡️ Stand by — you'll be alerted when the Werewolves prowl!");
        }
    }

    if let Some(victim) = werewolf_pack_kill(bot, wolves, &townsfolk).await? {
        // 5s midnight reflex window across `0..=N` online Bodyguards (`race_all` stays Pending if 0):
        let protectors = guards.iter().filter(|g| g.is_online()).map(|g| {
            let guard = g.clone();
            let townsfolk = townsfolk.clone();
            async move {
                let pick = guard
                    .ask(
                        "🛡️ Wolves are prowling! Reply `!protect <nick>` (5s):",
                        |m| {
                            let Some(target) = m.cmd_arg("!protect") else {
                                return AskMatch::ignore();
                            };
                            match townsfolk.iter().find(|u| u.eq_nick(target)) {
                                Some(protected) => AskMatch::accept_say(
                                    protected.clone(),
                                    irc_format!(
                                        "{} {}!",
                                        "🛡️ Moving to protect".cyan(),
                                        protected.nick().bold(),
                                    ),
                                ),
                                None => AskMatch::reject(
                                    "Pick a living townsfolk with `!protect <nick>`!".yellow(),
                                ),
                            }
                        },
                    )
                    .reply_timeout(Duration::from_secs(5))
                    .no_timeout_message()
                    .await?
                    .ok();
                match pick {
                    Some(saved) => Ok::<(User, User), Error>((guard, saved)),
                    None => std::future::pending().await,
                }
            }
        });

        let saved_victim = match race_all(protectors)
            .with_timeout(Duration::from_secs(5))
            .none_on_timeout()
            .await?
        {
            Some((_hero, protected)) => protected == victim,
            None => false,
        };

        if saved_victim {
            room.say("☀️ Dawn breaks! 🛡️ A Bodyguard thwarted the Werewolves' attack!".cyan().bold())?;
        } else {
            room.say(irc_format!(
                "{} {} was devoured in the night!",
                "☀️ Dawn breaks!".yellow(),
                victim.nick().bold().red(),
            ))?;
            alive.retain(|u| u != &victim);
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
        if let Some(target_user) = pick_opt {
            *tally.entry(target_user.id()).or_default() += 1;
        }
    }

    let max_votes = tally.values().copied().max().unwrap_or(0);
    let leaders: Vec<&User> = alive
        .iter()
        .filter(|u| max_votes > 0 && tally.get(&u.id()) == Some(&max_votes))
        .collect();

    let winner = if let [exiled] = leaders.as_slice() {
        let was_wolf = wolves.contains(exiled);
        if was_wolf {
            room.say(irc_format!(
                "{} {} (a Werewolf)! TOWN WINS!",
                "🎉 The town exiled".green().bold(),
                exiled.nick().bold(),
            ))?;
            MafiaWinner::Town
        } else {
            room.say(irc_format!(
                "{} {} (an innocent Townsfolk)! WEREWOLVES WIN!",
                "🐺 The town exiled".red().bold(),
                exiled.nick().bold(),
            ))?;
            MafiaWinner::Werewolves
        }
    } else {
        room.say("🐺 The vote deadlocked—no one was exiled! WEREWOLVES WIN!".red().bold())?;
        MafiaWinner::Werewolves
    };

    Ok(winner)
}

#[cfg(test)]
#[path = "mafia_test.rs"]
mod test;
