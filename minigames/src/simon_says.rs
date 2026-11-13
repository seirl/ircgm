//! # IRC Simon Says (2–4 Players)
//!
//! ## How to Play
//! 1. Players gather in `#lobby` for rapid-fire IRC-protocol commands (`10s` per round).
//!    Between rounds, each survivor receives an inline HUD status reminder via IRC `NOTICE` and
//!    dramatic stage directions via CTCP `/me` actions.
//! 2. Each round, the bot announces a directive — either prefixed with **"Simon says: ..."**
//!    (genuine!) or **without** `"Simon says"` (a fakeout trap!):
//!    - `say <phrase>` — send a normal channel `PRIVMSG` matching `<phrase>`.
//!    - `/me <verb>` — send a CTCP `/me` action starting with `<verb>`.
//!    - `/notice <channel> <word>` — send an IRC channel `NOTICE` matching `<word>`.
//!    - `/nick <prefix>*` — rename your IRC nickname to start with `<prefix>`!
//!    - **Bunker Rush (`BunkerRush`):** Opens an unlocked `#bunker-xxxxxx` shelter with only
//!      `N - 1` seats (`"Simon says: /join #bunker-xxxxxx!"`) — as soon as `N - 1` survivors are
//!      inside, the blast door seals (`MODE +i`) and stragglers are eliminated!
//! 3. Failing a genuine `"Simon says"` order — or false-starting on a fakeout — eliminates you!
//!
//! ## Library Mechanics Showcased
//! - **Inline HUD [`User::notice`] & stage [`Room::action`]:** Sends private `NOTICE` HUD lines
//!   and outbound `\x01ACTION ...\x01` narration.
//! - **Unified protocol event loop ([`Inbox::wait_event_map`] & [`Inbox::next_event`]):** Matches
//!   [`Event::Chat`] ([`MessageKind::Privmsg`] vs [`MessageKind::Action`] via [`ChatMessage::is_action`]),
//!   [`Event::Notice`], and [`Event::NickChanged`] on one [`Inbox`], tracking survivors across
//!   `/nick` changes via [`UserId`].
//! - **Zero-ceremony timed loops ([`ircgm::GameFutureExt::run_for`] & [`ircgm::GameFutureExt::run_until_deadline`]):**
//!   Runs both early-exiting (`Ok(())`) and infinite (`loop { ... }`) phase blocks without
//!   `Infallible` or `Ok::<_, Error>(())` turbofish annotations.
//! - **Unlocked [`ircgm::BreakoutOptions`] + [`Event::Joined`] / [`Event::Parted`] seat rush:**
//!   Creates `#bunker-xxxxxx` with `modes(None)` so players can self-join, counts live occupancy
//!   from `JOIN` / `PART` / `QUIT` events, locks the door with [`Room::set_mode`] (`"+i"`), and
//!   kicks survivors back to `#lobby` via RAII [`Breakout`] drop.

use std::collections::HashSet;
use std::time::Duration;

use ircgm::prelude::*;
use ircgm::BreakoutOptions;
use tokio::time::Instant;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Medium {
    Privmsg(&'static str),
    Action(&'static str),
    Notice(&'static str),
    NickPrefix(&'static str),
    BunkerRush,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Directive {
    pub simon_says: bool,
    pub medium: Medium,
}

impl Directive {
    fn prompt_text(&self, channel: &str) -> String {
        let body = match self.medium {
            Medium::Privmsg(word) => format!("say `{word}`"),
            Medium::Action(verb) => format!("do `/me {verb}`"),
            Medium::Notice(word) => format!("send `/notice {channel} {word}`"),
            Medium::NickPrefix(pfx) => format!("change `/nick {pfx}<anything>`"),
            Medium::BunkerRush => "sprint into the bunker".to_owned(),
        };
        if self.simon_says {
            format!("Simon says: {body}!")
        } else {
            format!("{body}!")
        }
    }

    fn matches_event(&self, ev: &Event) -> Option<UserId> {
        match (&self.medium, ev) {
            (Medium::Privmsg(word), Event::Chat(m))
                if m.kind == MessageKind::Privmsg && m.text.trim().eq_ignore_ascii_case(word) =>
            {
                Some(m.sender.id())
            }
            (Medium::Action(verb), Event::Chat(m))
                if m.is_action()
                    && m.text
                        .trim()
                        .to_ascii_lowercase()
                        .starts_with(&verb.to_ascii_lowercase()) =>
            {
                Some(m.sender.id())
            }
            (
                Medium::Notice(word),
                Event::Notice {
                    sender: Some(u),
                    text,
                    ..
                },
            ) if text.trim().eq_ignore_ascii_case(word) => Some(u.id()),
            (Medium::NickPrefix(pfx), Event::NickChanged { user, new_nick, .. })
                if new_nick
                    .to_ascii_lowercase()
                    .starts_with(&pfx.to_ascii_lowercase()) =>
            {
                Some(user.id())
            }
            _ => None,
        }
    }
}

fn any_player_activity(ev: &Event) -> Option<UserId> {
    match ev {
        Event::Chat(m) => Some(m.sender.id()),
        Event::Notice {
            sender: Some(u), ..
        } => Some(u.id()),
        Event::NickChanged { user, .. } => Some(user.id()),
        _ => None,
    }
}

const DEFAULT_DIRECTIVES: &[Directive] = &[
    Directive {
        simon_says: true,
        medium: Medium::Privmsg("hello"),
    },
    Directive {
        simon_says: true,
        medium: Medium::Action("waves"),
    },
    Directive {
        simon_says: true,
        medium: Medium::Notice("ping"),
    },
    Directive {
        simon_says: false,
        medium: Medium::Privmsg("jump"),
    },
    Directive {
        simon_says: true,
        medium: Medium::BunkerRush,
    },
];

/// Lobby entrypoint: recruits 3 players and runs IRC Simon Says ending in a Bunker Rush finale!
pub async fn play(bot: &Bot, room: &Room) -> Result<Option<Vec<User>>> {
    let Some(players) =
        crate::recruit_players(room, "IRC Simon Says", 3, Duration::from_secs(30)).await?
    else {
        return Ok(None);
    };
    run_simon_says(
        bot,
        room,
        &players,
        DEFAULT_DIRECTIVES,
        Duration::from_secs(10),
    )
    .await
    .map(Some)
}

async fn run_bunker_rush_round(
    bot: &Bot,
    lobby: &Room,
    living: &mut Vec<User>,
    simon_says: bool,
    window: Duration,
) -> Result<()> {
    let seats = living.len().saturating_sub(1).max(1);
    let bunker = bot
        .breakout_with(
            BreakoutOptions::new(crate::breakout_slug(lobby, "bunker"))
                .modes(None::<&str>)
                .kick_reason("All clear — back to #lobby!"),
        )
        .await?;

    let mut ev_in = bunker.inbox();
    let prefix = if simon_says { "Simon says: " } else { "" };
    lobby.action(format!(
        "cranks the siren! {prefix}`/join {}` ({seats} seat(s))!",
        bunker.name()
    ))?;

    let living_ids: HashSet<UserId> = living.iter().map(User::id).collect();
    let mut inside: HashSet<UserId> = HashSet::new();

    if simon_says {
        let fill = async {
            while inside.len() < seats {
                match ev_in.next_event().await? {
                    Event::Joined { user, .. } if living_ids.contains(&user.id()) => {
                        inside.insert(user.id());
                    }
                    Event::Parted { user, .. }
                    | Event::Kicked { user, .. }
                    | Event::Quit { user, .. } => {
                        inside.remove(&user.id());
                    }
                    _ => {}
                }
            }
            Ok(())
        };
        fill.run_for(window).await?;
        let _ = bunker.set_mode("+i", &[]);
        living.retain(|u| inside.contains(&u.id()));
    } else {
        let trap = async {
            loop {
                let uid = ev_in
                    .wait_event_map(|ev| match ev {
                        Event::Joined { user, .. } => Some(user.id()),
                        _ => None,
                    })
                    .await?;
                if living_ids.contains(&uid) {
                    inside.insert(uid);
                }
            }
        };
        trap.run_for(window).await?;
        living.retain(|u| !inside.contains(&u.id()));
    }
    Ok(())
}

/// Runs `directives` against `players` in `room`, returning the surviving `User`s.
pub async fn run_simon_says(
    bot: &Bot,
    room: &Room,
    players: &[User],
    directives: &[Directive],
    round_window: Duration,
) -> Result<Vec<User>> {
    let mut living: Vec<User> = players.to_vec();
    let mut inbox = room.inbox();

    room.action("steps up to the microphone for IRC Simon Says!")?;
    for survivor in &living {
        let _ = survivor.notice("🎧 [HUD] Listen carefully for 'Simon says:' — false starts eliminate you!");
    }

    for directive in directives {
        if living.len() <= 1 {
            break;
        }

        if directive.medium == Medium::BunkerRush {
            run_bunker_rush_round(bot, room, &mut living, directive.simon_says, round_window)
                .await?;
            continue;
        }

        let living_ids: HashSet<UserId> = living.iter().map(User::id).collect();
        room.say("📣 ".cyan().bold() + directive.prompt_text(room.name()).bold())?;

        let deadline = Instant::now() + round_window;
        if directive.simon_says {
            let mut completed: HashSet<UserId> = HashSet::new();
            let collect = async {
                while completed.len() < living_ids.len() {
                    let uid = inbox
                        .wait_event_map(|ev| directive.matches_event(ev))
                        .await?;
                    if living_ids.contains(&uid) {
                        completed.insert(uid);
                    }
                }
                Ok(())
            };
            collect.run_until_deadline(deadline).await?;
            living.retain(|u| completed.contains(&u.id()));
        } else {
            let mut tricked: HashSet<UserId> = HashSet::new();
            let collect = async {
                loop {
                    let uid = inbox.wait_event_map(any_player_activity).await?;
                    if living_ids.contains(&uid) {
                        tricked.insert(uid);
                    }
                }
            };
            collect.run_until_deadline(deadline).await?;
            living.retain(|u| !tricked.contains(&u.id()));
        }
    }

    if living.is_empty() {
        room.say("💀 Everyone was eliminated in Simon Says!".yellow())?;
    } else {
        let names: Vec<String> = living.iter().map(User::nick).collect();
        room.say("🏆 Simon Says survivors: ".green().bold() + names.join(", ").bold())?;
    }
    Ok(living)
}

#[cfg(test)]
#[path = "simon_says_test.rs"]
mod test;
