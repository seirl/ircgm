//! Aspirational `ircgm` Bot: **One Night Ultimate Werewolf** (Base Game, 3–10 Players)
//!
//! Written against `docs/design/DESIGN.md` (and accepted `REVIEW_03.md` table items)
//! without compiling or running, specifically to pressure-test `ircgm`'s public API
//! against a complex, stateful, hidden-information social deduction game.
//!
//! Look for `// [API NOTE ...]` comments throughout this file for places where the
//! `DESIGN.md` API had missing methods, spec drift, flood-control hazards, or friction.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use fastrand::Rng;
use futures::future::try_join_all;
use tokio::time::Instant;

// [API NOTE #1]: `DESIGN.md` Table 4.1 & `REVIEW_03.md` §6.1.3 accept `Match` (`Accept`,
// `Reject`, `Ignore`) and `DisconnectReason`, and `Bot::casemap()` returns `CaseMap`,
// yet none of `Match`, `CaseMap`, `Member`, or `DisconnectReason` appear in `DESIGN.md`
// §4.2's re-export line or §4.3–4.10's code blocks. We import them assuming root re-export.
use ircgm::{
    AskConfig, Bot, BotConfig, CaseMap, ChatMessage, Colorize, DisconnectReason, Error,
    GameFutureExt, InboxScope, Match, Outcome, Result, Room, RoomEvent, Text, Turn, User,
};

const MIN_PLAYERS: usize = 3;
const MAX_PLAYERS: usize = 10;
const LOBBY_DURATION: Duration = Duration::from_secs(90);
const NIGHT_ACTION_TIMEOUT: Duration = Duration::from_secs(45);
const VOTE_TIMEOUT: Duration = Duration::from_secs(45);

// ============================================================================
// 1. Roles, Physical Cards, & Curated N+3 Decks
// ============================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Role {
    Doppelganger,
    Werewolf,
    Minion,
    Mason,
    Seer,
    Robber,
    Troublemaker,
    Drunk,
    Insomniac,
    Tanner,
    Hunter,
    Villager,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Team {
    Village,
    Werewolf,
    Tanner,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Doppelganger => "Doppelgänger",
            Role::Werewolf => "Werewolf",
            Role::Minion => "Minion",
            Role::Mason => "Mason",
            Role::Seer => "Seer",
            Role::Robber => "Robber",
            Role::Troublemaker => "Troublemaker",
            Role::Drunk => "Drunk",
            Role::Insomniac => "Insomniac",
            Role::Tanner => "Tanner",
            Role::Hunter => "Hunter",
            Role::Villager => "Villager",
        }
    }

    fn styled(self) -> Text {
        match self.team() {
            Team::Werewolf => Text::from(self.name().bold().red()),
            Team::Tanner => Text::from(self.name().bold().orange()),
            Team::Village => Text::from(self.name().bold().light_cyan()),
        }
    }

    fn team(self) -> Team {
        match self {
            Role::Werewolf | Role::Minion => Team::Werewolf,
            Role::Tanner => Team::Tanner,
            Role::Doppelganger
            | Role::Mason
            | Role::Seer
            | Role::Robber
            | Role::Troublemaker
            | Role::Drunk
            | Role::Insomniac
            | Role::Hunter
            | Role::Villager => Team::Village,
        }
    }

    fn night_summary(self) -> &'static str {
        match self {
            Role::Doppelganger => {
                "Wake 1st: look at another player's card and permanently become that role & team."
            }
            Role::Werewolf => {
                "Wake with Werewolves to see your pack. If you are the Lone Wolf, you may peek at 1 center card."
            }
            Role::Minion => {
                "Wake to learn who the Werewolves are (they do not know who you are). Win if Werewolves survive!"
            }
            Role::Mason => "Wake to see the other Mason. If you are alone, the other Mason is in the center.",
            Role::Seer => "Wake to view EITHER 1 other player's card OR 2 center cards.",
            Role::Robber => "Wake to optionally swap your card with another player's card and view your new card.",
            Role::Troublemaker => "Wake to optionally swap two OTHER players' cards without looking at them.",
            Role::Drunk => "Wake and MUST swap your card with 1 of the 3 center cards without looking at it.",
            Role::Insomniac => "Wake last to check your own card and see if it changed during the night.",
            Role::Tanner => "You hate your job. You ONLY win if you are killed!",
            Role::Hunter => "Sleep at night. If you die, whoever YOU voted for also dies immediately!",
            Role::Villager => "Sleep at night. Find and kill a Werewolf during the day!",
        }
    }
}

/// Physical card sitting in front of a player or in one of the 3 center slots.
///
/// Modeling the Doppelgänger card with `copied: Option<Role>` directly on `Card` ensures
/// that if Robber, Troublemaker, or Drunk later moves the physical Doppelgänger card,
/// whoever ends up holding it at Dawn inherits the exact role & team she copied in Step 1
/// (whereas an uncopied Doppelgänger card swapped in from the center by Drunk counts as a Villager).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Card {
    Doppelganger { copied: Option<Role> },
    Standard(Role),
}

impl Card {
    fn from_role(role: Role) -> Self {
        match role {
            Role::Doppelganger => Card::Doppelganger { copied: None },
            other => Card::Standard(other),
        }
    }

    /// True role & team of whoever holds this physical card when votes resolve.
    fn effective_role(self) -> Role {
        match self {
            Card::Doppelganger { copied: Some(r) } => r,
            Card::Doppelganger { copied: None } => Role::Villager,
            Card::Standard(r) => r,
        }
    }

    /// How this card appears when peeked during the night vs revealed at Dawn.
    fn display_for_peek(self) -> Text {
        match self {
            // Note: In Step 1 Doppelgänger can only peek *other* players (never herself);
            // if Seer or Robber peeks/steals a Doppelgänger card later in the night, they see "Doppelgänger".
            Card::Doppelganger { .. } => Role::Doppelganger.styled(),
            Card::Standard(r) => r.styled(),
        }
    }

    fn display_reveal(self) -> Text {
        match self {
            Card::Doppelganger { copied: Some(r) } => {
                "Doppelgänger (".light_cyan() + r.styled() + ")".light_cyan()
            }
            Card::Doppelganger { copied: None } => Role::Doppelganger.styled(),
            Card::Standard(r) => r.styled(),
        }
    }
}

/// Builds a balanced base-game deck of exactly `N + 3` cards (`6..=13` cards for `3..=10` players).
/// Note that Masons are only included when both Mason cards fit together.
fn build_base_deck(player_count: usize) -> Vec<Role> {
    let mut deck = match player_count {
        3 => vec![
            Role::Werewolf,
            Role::Werewolf,
            Role::Seer,
            Role::Robber,
            Role::Troublemaker,
            Role::Villager,
        ],
        4 => vec![
            Role::Doppelganger,
            Role::Werewolf,
            Role::Werewolf,
            Role::Seer,
            Role::Robber,
            Role::Troublemaker,
            Role::Insomniac,
        ],
        5 => vec![
            Role::Doppelganger,
            Role::Werewolf,
            Role::Werewolf,
            Role::Minion,
            Role::Seer,
            Role::Robber,
            Role::Troublemaker,
            Role::Insomniac,
        ],
        6 => vec![
            Role::Doppelganger,
            Role::Werewolf,
            Role::Werewolf,
            Role::Minion,
            Role::Seer,
            Role::Robber,
            Role::Troublemaker,
            Role::Drunk,
            Role::Insomniac,
        ],
        7 => vec![
            Role::Doppelganger,
            Role::Werewolf,
            Role::Werewolf,
            Role::Minion,
            Role::Mason,
            Role::Mason,
            Role::Seer,
            Role::Robber,
            Role::Troublemaker,
            Role::Tanner,
        ],
        8 => vec![
            Role::Doppelganger,
            Role::Werewolf,
            Role::Werewolf,
            Role::Minion,
            Role::Mason,
            Role::Mason,
            Role::Seer,
            Role::Robber,
            Role::Troublemaker,
            Role::Insomniac,
            Role::Tanner,
        ],
        9 => vec![
            Role::Doppelganger,
            Role::Werewolf,
            Role::Werewolf,
            Role::Minion,
            Role::Mason,
            Role::Mason,
            Role::Seer,
            Role::Robber,
            Role::Troublemaker,
            Role::Drunk,
            Role::Insomniac,
            Role::Hunter,
        ],
        _ => vec![
            Role::Doppelganger,
            Role::Werewolf,
            Role::Werewolf,
            Role::Minion,
            Role::Mason,
            Role::Mason,
            Role::Seer,
            Role::Robber,
            Role::Troublemaker,
            Role::Drunk,
            Role::Insomniac,
            Role::Tanner,
            Role::Hunter,
        ],
    };
    while deck.len() < player_count + 3 {
        deck.push(Role::Villager);
    }
    deck
}

// ============================================================================
// 2. RAII Channel Moderation Guard (Workaround for Missing Library Guard)
// ============================================================================

/// [API NOTE #2]: `Breakout` has built-in cancellation-safe `Drop` cleanup, but moderating a
/// persistent public channel (`#werewolf`) with `+m` and `+v`/`-v` has NO RAII guard in `ircgm`.
/// Without this custom `Drop` struct, cancelling `run_game(...).until(stop)` via admin `!stop`,
/// `bot.shutdown_requested()`, or `?` while `#werewolf` is `+m` at Night leaves the channel
/// permanently muted with everyone devoiced!
///
/// [API NOTE #3]: `Room` only exposes singular `voice(&user)` / `devoice(&user)` (Table 4.1).
/// Calling `devoice` in a loop for 10 players enqueues 10 separate `MODE #c -v <nick>` lines
/// instead of batching 4 per line (`MODE #c -vvvv ...`), eating two-thirds of `irc` v1.x's
/// 15-msg / 8s flood-throttle burst budget in one transition!
struct ChannelModerationGuard {
    room: Room,
    original_topic: Option<String>,
    voiced_players: Vec<User>,
    muted: bool,
}

impl ChannelModerationGuard {
    fn new(room: Room) -> Self {
        let original_topic = room.topic();
        Self {
            room,
            original_topic,
            voiced_players: Vec::new(),
            muted: false,
        }
    }

    /// Best-effort `+m` + devoice everyone for quiet phases (Night & simultaneous Voting).
    fn enter_quiet_phase(&mut self, topic_status: &str) {
        if !self.room.bot_is_op() {
            return;
        }
        let _ = self.room.set_mode("+m", &[]);
        self.muted = true;
        for u in self.voiced_players.drain(..) {
            let _ = self.room.devoice(&u);
        }
        let _ = self.room.set_topic(topic_status);
    }

    /// Best-effort `+m` + `+v` on enrolled players so spectators cannot talk during Day discussion.
    fn enter_day_discussion(&mut self, players: &[User], topic_status: &str) {
        if !self.room.bot_is_op() {
            return;
        }
        let _ = self.room.set_mode("+m", &[]);
        self.muted = true;
        for p in players {
            if self.room.voice(p).is_ok() {
                self.voiced_players.push(p.clone());
            }
        }
        let _ = self.room.set_topic(topic_status);
    }
}

impl Drop for ChannelModerationGuard {
    fn drop(&mut self) {
        if !self.room.bot_is_op() {
            return;
        }
        for u in self.voiced_players.drain(..) {
            let _ = self.room.devoice(&u);
        }
        if self.muted {
            let _ = self.room.set_mode("-m", &[]);
            self.muted = false;
        }
        if let Some(ref old_topic) = self.original_topic {
            let _ = self.room.set_topic(old_topic);
        }
    }
}

// ============================================================================
// 3. Game State & Target-Parsing Helpers
// ============================================================================

struct PlayerSlot {
    user: User,
    initial_role: Role,
    card: Card,
}

struct TableState {
    players: Vec<PlayerSlot>,
    center: [Card; 3],
    deck_roles: Vec<Role>,
    /// If a player started as Doppelgänger and peeked a role in Step 1, records `(seat_idx, copied_role)`.
    /// Tracked separately from `Card` because even if Robber/Troublemaker later steals the physical
    /// Doppelgänger card, the *original* Doppelgänger player (who doesn't know she was swapped!)
    /// still wakes up at Step 2/3/4/9a based on what she saw in Step 1.
    doppel_actor: Option<(usize, Role)>,
    night_log: Vec<String>,
}

impl TableState {
    fn deck_contains(&self, role: Role) -> bool {
        self.deck_roles.contains(&role)
    }

    fn initial_holders(&self, role: Role) -> Vec<usize> {
        self.players
            .iter()
            .enumerate()
            .filter_map(|(idx, p)| (p.initial_role == role).then_some(idx))
            .collect()
    }

    /// Formats the numbered player roster (`[1] Alice  [2] Bob  ...`) using live nicknames.
    fn format_roster(&self) -> String {
        self.players
            .iter()
            .enumerate()
            .map(|(i, p)| format!("[{}] {}", i + 1, p.user.nick()))
            .collect::<Vec<_>>()
            .join("  ")
    }

    fn format_deck(&self) -> String {
        let mut counts: Vec<(Role, usize)> = Vec::new();
        for &r in &self.deck_roles {
            if let Some(entry) = counts.iter_mut().find(|(role, _)| *role == r) {
                entry.1 += 1;
            } else {
                counts.push((r, 1));
            }
        }
        counts
            .into_iter()
            .map(|(r, n)| {
                if n > 1 {
                    format!("{n}x {}", r.name())
                } else {
                    r.name().to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Resolves either a 1-based seat number (`"2"` or `"#2"`) or an IRC nickname to a player index.
///
/// [API NOTE #4]: `DESIGN.md` §4 never specifies the methods on `CaseMap`, nor does `User` offer
/// `user.eq_nick(token)`! Comparing `p.user.nick().eq_ignore_ascii_case(token)` is WRONG under
/// RFC 1459 casemapping (`[Alice]` == `{alice}`), while calling `bot.user(token)` on arbitrary
/// chat tokens would allocate transient `UserTrack`s in `UserRegistry` for every typo!
/// Here we assume `CaseMap::eq(&self, a: &str, b: &str) -> bool` exists.
fn resolve_player_token(casemap: CaseMap, players: &[PlayerSlot], token: &str) -> Option<usize> {
    let clean = token.trim().trim_end_matches(':').trim_end_matches(',');
    let num_token = clean.strip_prefix('#').unwrap_or(clean);
    if let Ok(seat) = num_token.parse::<usize>() {
        if (1..=players.len()).contains(&seat) {
            return Some(seat - 1);
        }
    }
    players
        .iter()
        .position(|p| casemap.eq(&p.user.nick(), clean))
}

/// Checks if a `User` is currently present in `room` and online.
///
/// [API NOTE #5]: `Room` has `has_nick(&str)` (line 744) and `members() -> Vec<Member>` (Table 4.1),
/// but no `room.has_user(&User) -> bool`! `room.has_nick(&user.nick())` locks `UserTrack` to clone a
/// `String` just so `Room` can re-fold and hash it, instead of checking `UserTrack::id` directly.
fn is_active_in_room(room: &Room, user: &User) -> bool {
    user.is_online() && room.has_nick(&user.nick())
}

// ============================================================================
// 4. Entrypoint & Outer Connection Supervisor
// ============================================================================

fn is_admin(msg: &ChatMessage) -> bool {
    msg.account.as_deref() == Some("seirl") || msg.host.as_deref() == Some("staff/seirl")
}

pub async fn run_bot(irc_cfg: irc::client::data::Config, channel: &str) -> Result<()> {
    let cfg = BotConfig::new(irc_cfg)
        .with_ask_timeout(NIGHT_ACTION_TIMEOUT)
        .with_command_prefixes(["!"]);

    // Outer reconnect supervisor loop per `DESIGN.md` §1 ("Connection-bounded v1 sessions"):
    loop {
        let bot = Bot::connect_with(cfg.clone()).await?;
        let room = bot.join(channel).await?;

        match lobby_loop(bot.clone(), room).await {
            Ok(()) => {
                bot.quit(Some("Shutting down One Night Werewolf bot")).await?;
                return Ok(());
            }
            Err(Error::Disconnected(DisconnectReason::Quit | DisconnectReason::BotDropped)) => {
                return Ok(());
            }
            Err(err) => {
                eprintln!("Session {} ended with error: {err}; reconnecting in 5s...", bot.session_id());
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn lobby_loop(bot: Bot, room: Room) -> Result<()> {
    room.say(
        "🐺 ".bold()
            + "One Night Ultimate Werewolf".bold().yellow()
            + " ready! Type "
            + "!onuw".bold().green()
            + " to open a lobby (3–10 players).",
    )?;

    while !bot.is_shutting_down() {
        // One-shot trigger wait: opens temporary Inbox and drops it immediately upon matching `!onuw`,
        // ensuring `!onuw` typed mid-game is never buffered for the next round!
        let trigger_fut = room.inbox().wait_cmd("!onuw");
        let trigger = match trigger_fut.until(bot.shutdown_requested()).await {
            Outcome::Completed(res) => res?,
            Outcome::Stopped(()) => break,
        };

        // Admin `!stop` (in DM or channel) OR process graceful shutdown cancels the game immediately,
        // unwinding `ChannelModerationGuard` via Drop to unmute `#werewolf`!
        let admin_stop = bot.inbox().wait_for(|m| {
            is_admin(m) && m.is_cmd("!stop") && (m.is_dm() || m.is_in(&room))
        });
        let stop_signal = async {
            tokio::select! {
                 biased;
                _ = bot.shutdown_requested() => None,
                msg = admin_stop => msg.ok(),
            }
        };

        match run_single_game(&bot, &room, trigger.sender).until(stop_signal).await {
            Outcome::Completed(Ok(())) => {}
            Outcome::Completed(Err(e)) if !e.is_fatal() => {
                room.say("⚠️ Game ended early: ".yellow() + e.to_string())?;
            }
            Outcome::Completed(Err(fatal)) => return Err(fatal),
            Outcome::Stopped(Some(admin_msg)) => {
                bot.reply(&admin_msg, "Game aborted.".yellow())?;
                if admin_msg.is_dm() {
                    room.say("🛑 Game stopped by an admin.".bold().red())?;
                }
            }
            Outcome::Stopped(None) => {
                room.say("🛑 Bot is restarting—aborting current game, sorry!".yellow())?;
                break;
            }
        }
    }
    Ok(())
}

// ============================================================================
// 5. Lobby Recruitment (`3..=10` Players) + Pre-Game DM Reachability Check
// ============================================================================

async fn recruit_players(bot: &Bot, room: &Room, host: User) -> Result<Option<Vec<User>>> {
    let mut inbox = room.inbox();
    let mut players: Vec<User> = vec![host.clone()];

    room.say(
        "🌕 ".bold()
            + host.nick().bold().yellow()
            + " is starting "
            + "One Night Ultimate Werewolf".bold()
            + "! Type "
            + "!join".bold().green()
            + " to play, "
            + "!leave".bold()
            + " to drop out, or "
            + "!go".bold().cyan()
            + " once 3+ players are in (90s).",
    )?;

    // [API NOTE #6]: Notice the ergonomics cliff between single-turn `Ask` (which has `.reply_timeout()`,
    // `.warn_at()`, `.on_timeout()`) and a multi-event bounded phase like Lobby or Day Discussion:
    // we have to manually manage an `Instant` deadline, warning boolean, and `tokio::select!` loop.
    let deadline = Instant::now() + LOBBY_DURATION;
    let warn_instant = deadline - Duration::from_secs(30);
    let mut warned_30s = false;

    loop {
        if players.len() >= MAX_PLAYERS {
            room.say("Lobby is full (10/10)! Starting now...".green().bold())?;
            break;
        }

        let next_timer = if !warned_30s && Instant::now() < warn_instant {
            warn_instant
        } else {
            deadline
        };

        tokio::select! {
            _ = tokio::time::sleep_until(next_timer) => {
                if !warned_30s && Instant::now() < deadline {
                    warned_30s = true;
                    room.say(format!(
                        "⏳ 30 seconds left to `!join`! Currently {}/{MIN_PLAYERS}+ players.",
                        players.len()
                    ).yellow())?;
                } else {
                    break;
                }
            }
            ev = inbox.next_event() => {
                match ev? {
                    RoomEvent::Chat(msg) if msg.is_cmd("!join") => {
                        if !players.contains(&msg.sender) {
                            players.push(msg.sender.clone());
                            room.say(
                                msg.sender.nick().bold().green()
                                    + format!(" joined! ({}/{MAX_PLAYERS})", players.len()),
                            )?;
                        }
                    }
                    RoomEvent::Chat(msg) if msg.is_cmd("!leave") => {
                        if let Some(pos) = players.iter().position(|u| *u == msg.sender) {
                            players.remove(pos);
                            room.say(
                                msg.sender.nick().bold().yellow()
                                    + format!(" left the lobby. ({} remaining)", players.len()),
                            )?;
                            if players.is_empty() {
                                room.say("Lobby is empty—cancelling game.".yellow())?;
                                return Ok(None);
                            }
                        }
                    }
                    RoomEvent::Chat(msg) if msg.is_cmd("!deck") => {
                        let preview_n = players.len().clamp(MIN_PLAYERS, MAX_PLAYERS);
                        let roles = build_base_deck(preview_n)
                            .into_iter()
                            .map(Role::name)
                            .collect::<Vec<_>>()
                            .join(", ");
                        room.say(format!("🃏 Deck for {preview_n} players ({} cards): {roles}", preview_n + 3))?;
                    }
                    RoomEvent::Chat(msg) if msg.is_cmd("!go") => {
                        // Host (or current player #1 if original host left) can launch early once >= 3 players:
                        if players.first() == Some(&msg.sender) {
                            if players.len() >= MIN_PLAYERS {
                                break;
                            } else {
                                room.say(format!("Need at least {MIN_PLAYERS} players (have {}).", players.len()).yellow())?;
                            }
                        }
                    }
                    // [API NOTE #7]: `DESIGN.md` §3.2 stamps `channels` onto `Quit` so `room.inbox()`
                    // can surface departures during recruitment—critical so AFK/disconnected joiners
                    // aren't dealt cards! (`DESIGN.md` §4.4 should explicitly guarantee `Quit` delivery on `room.inbox()`.)
                    RoomEvent::Parted { user, .. }
                    | RoomEvent::Kicked { user, .. }
                    | RoomEvent::Quit { user, .. } => {
                        if let Some(pos) = players.iter().position(|u| *u == user) {
                            let gone = players.remove(pos);
                            room.say(
                                gone.nick().bold().yellow()
                                    + format!(" left the channel and was removed from signups ({} left).", players.len()),
                            )?;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if players.len() < MIN_PLAYERS {
        room.say(
            format!(
                "Not enough players joined ({}/{MIN_PLAYERS} minimum). Lobby closed.",
                players.len()
            )
            .yellow(),
        )?;
        return Ok(None);
    }

    // [API NOTE #8 — CRITICAL REAL-IRC PROTOCOL GAP]:
    // In ONUW, Villagers, Tanners, Hunters, Masons, Minions, and Insomniacs receive secret info
    // via one-way `user.say(...)`, NOT `user.ask(...)`.
    // 1) `user.say(...)` is synchronous fire-and-forget: if a player has `UMODE +g` (`callerid`, numeric 716)
    //    or `+R` (registered nicks only, numeric 486), `user.say()` returns `Ok(())` and they NEVER see their card!
    // 2) Even if we verify DM reachability right here with `user.ask(...)`, `DESIGN.md` §3.2 only maps
    //    `401/403/404/442` to `Error::DeliveryFailed`—ignoring `486 ERR_NONONREG`, `531 ERR_CANTSENDTOUSER`,
    //    and `716 RPL_TARGUMODEG`, so blocked DMs hang for the full 25s timeout instead of failing in 10ms!
    room.say("📩 Sending a quick DM check to all players—reply `!ok` (or `ok`) in DM within 25s!")?;
    let check_deadline = Instant::now() + Duration::from_secs(25);
    let checks = try_join_all(players.iter().map(|p| async move {
        let res = p
            .ask(
                "👋 ONUW DM check! Reply `ok` here to confirm you can receive your secret role card.",
                |m| {
                    let t = m.text.trim();
                    (t.eq_ignore_ascii_case("ok") || m.is_cmd("!ok")).then_some(())
                },
            )
            .reply_deadline(check_deadline)
            .on_timeout("⏳ You didn't confirm DMs in time and were dropped from the lobby.")
            .settled()
            .await?;
        Ok::<_, Error>((p.clone(), res))
    }))
    .await?;

    let mut verified = Vec::with_capacity(players.len());
    for (p, outcome) in checks {
        match outcome {
            Turn::Reply(()) if is_active_in_room(room, &p) => verified.push(p),
            Turn::Reply(()) | Turn::Left { .. } => {
                room.say(p.nick().bold().yellow() + " left before the game started.")?;
            }
            Turn::TimedOut => {
                room.say(
                    p.nick().bold().yellow()
                        + " didn't respond to the bot's DM (check `/accept "
                        + bot.nick()
                        + "` or `+R`/`+g` modes!) and was dropped.",
                )?;
            }
        }
    }

    if verified.len() < MIN_PLAYERS {
        room.say(
            format!(
                "Only {} verified player(s) remain (need {MIN_PLAYERS}). Game cancelled.",
                verified.len()
            )
            .yellow(),
        )?;
        return Ok(None);
    }

    Ok(Some(verified))
}

// ============================================================================
// 6. Top-Level Single Game Orchestrator & Out-of-Band Info Responder
// ============================================================================

async fn run_single_game(bot: &Bot, room: &Room, host: User) -> Result<()> {
    let Some(players) = recruit_players(bot, room, host).await? else {
        return Ok(());
    };

    let mut rng = Rng::new();
    let deck_roles = build_base_deck(players.len());
    let mut shuffled_cards: Vec<Card> = deck_roles.iter().copied().map(Card::from_role).collect();
    rng.shuffle(&mut shuffled_cards);

    let player_slots: Vec<PlayerSlot> = players
        .iter()
        .zip(shuffled_cards.drain(..players.len()))
        .map(|(u, card)| {
            let initial_role = match card {
                Card::Doppelganger { .. } => Role::Doppelganger,
                Card::Standard(r) => r,
            };
            PlayerSlot {
                user: u.clone(),
                initial_role,
                card,
            }
        })
        .collect();

    let center: [Card; 3] = [shuffled_cards[0], shuffled_cards[1], shuffled_cards[2]];
    let mut state = TableState {
        players: player_slots,
        center,
        deck_roles,
        doppel_actor: None,
        night_log: Vec::new(),
    };

    // RAII guard restores `-m`, `-v`, and original channel topic on ANY exit (`Ok`, `?`, `!stop`, shutdown).
    let mut mod_guard = ChannelModerationGuard::new(room.clone());

    // Phase 1: Deal & Secret Initial Peek
    deal_initial_cards(room, &state)?;

    // Out-of-band `!deck` / `!players` / `!help` responder across both `#werewolf` AND player DMs.
    // Because active night/vote `Ask`s opt into `.ignore_commands()`, a player inside `seer.ask(...)`
    // can DM `!deck` or `!players` without triggering `Match::Reject`, and this background listener
    // answers via synchronous `bot.reply(&msg, ...)` without touching `dm_mutex`!
    let enrolled_users: Vec<User> = state.players.iter().map(|p| p.user.clone()).collect();
    let deck_snapshot = state.format_deck();
    let deck_len = state.deck_roles.len();

    let info_scope = InboxScope::new().room(room).dms_from(&enrolled_users);
    let mut info_inbox = bot.inbox_for(info_scope).lossy();
    let info_responder = async {
        loop {
            let msg = info_inbox.next_chat().await?;
            if msg.is_cmd("!deck") || msg.is_cmd("!roles") {
                bot.reply(&msg, format!("🃏 Deck ({deck_len} cards): {deck_snapshot}"))?;
            } else if msg.is_cmd("!players") || msg.is_cmd("!seats") {
                let live_roster = enrolled_users
                    .iter()
                    .enumerate()
                    .map(|(i, u)| format!("[{}] {}", i + 1, u.nick()))
                    .collect::<Vec<_>>()
                    .join("  ");
                bot.reply(&msg, format!("👥 Seats: {live_roster}"))?;
            } else if msg.is_cmd("!rules") || msg.is_cmd("!help") {
                bot.reply(
                    &msg,
                    "📖 Commands: `!deck` (cards in play), `!players` (seat numbers). During Day: `!time`, `!ready` (vote early).",
                )?;
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), Error>(())
    };

    let game_flow = async {
        // Phase 2: The Night Phase (Strict Wake Sequence)
        mod_guard.enter_quiet_phase("ONUW — 🌙 Night Phase (Players asleep)");
        run_night_phase(bot, room, &mut state, &mut rng).await?;

        // Phase 3: The Day Phase (Timed Discussion)
        mod_guard.enter_day_discussion(&enrolled_users, "ONUW — ☀️ Day Phase (Discuss & deduce!)");
        run_day_phase(room, &state).await?;

        // Phase 4: Simultaneous Secret Voting ("3, 2, 1, POINT!") & Hunter Resolution
        mod_guard.enter_quiet_phase("ONUW — 🗳️ Voting Phase (Check DMs!)");
        let votes = run_voting_phase(bot, room, &state).await?;

        // Phase 5: Reveal & Win Conditions (drop moderation lock first so players can react!)
        drop(mod_guard);
        resolve_and_announce_winners(room, &state, &votes)?;
        Ok::<(), Error>(())
    };

    tokio::select! {
        res = game_flow => res,
        res = info_responder => res,
    }
}

// ============================================================================
// 7. Phase 1: Setup & Initial Secret Card Deal
// ============================================================================

fn deal_initial_cards(room: &Room, state: &TableState) -> Result<()> {
    room.say(
        "🃏 Shuffling ".bold()
            + format!("{} cards", state.deck_roles.len()).bold().yellow()
            + format!(" ({} players + 3 center cards): ", state.players.len())
            + state.format_deck(),
    )?;
    room.say("👥 Seats: ".bold() + state.format_roster())?;

    // [API NOTE #9 — OUTBOUND FLOOD THROTTLE HAZARD]:
    // For 10 players, sending 2 channel lines + 10 private role DMs + `+m` + Night announcement
    // immediately approaches/exceeds `irc` v1.x's `max_messages_in_burst = 15` (8s window),
    // yet `DESIGN.md` §3.5 only warns if ONE single `say()` wraps into >15 lines!
    for (idx, slot) in state.players.iter().enumerate() {
        slot.user.say(
            format!("🔒 [Seat #{}] Your secret initial card is: ", idx + 1)
                + slot.initial_role.styled()
                + " — "
                + slot.initial_role.night_summary()
                + " (Put your card face down; remember it may be swapped tonight without your knowledge!)",
        )?;
    }
    Ok(())
}

// ============================================================================
// 8. Phase 2: The Night Phase (Strict Wake Order + Anti-Timing-Leak Padding)
// ============================================================================

/// Shared DM prompt configuration for Night actions.
///
/// [API NOTE #10]: `AskConfig` in `DESIGN.md` §4.7 lacks `.warn_at(...)` (and §4.7 hasn't
/// been updated with `.ignore_commands()` from `REVIEW_03.md` §6.1.3 yet). Because `warn_at`
/// is only on `Ask`, we have to chain `.warn_at(...)` manually on every single `ask` call below!
fn night_dm_config() -> AskConfig {
    AskConfig::new()
        .reply_timeout(NIGHT_ACTION_TIMEOUT)
        .ignore_commands()
        .on_timeout("⏳ Time's up! Night moves on.".yellow())
}

/// Disguises whether an interactive role is in the 3 center cards (or answered instantly)
/// so observant players in `#werewolf` cannot deduce center cards from step durations!
async fn pad_step_duration(started: Instant, rng: &mut Rng) {
    let min_human_ms = rng.u64(6_000..=11_500);
    let target = Duration::from_millis(min_human_ms);
    if let Some(rem) = target.checked_sub(started.elapsed()) {
        tokio::time::sleep(rem).await;
    }
}

async fn run_night_phase(
    bot: &Bot,
    room: &Room,
    state: &mut TableState,
    rng: &mut Rng,
) -> Result<()> {
    let casemap = bot.casemap();
    let dm_cfg = night_dm_config();

    room.say("🌙 Everyone, close your eyes! Night falls over the village...".bold().light_blue())?;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ------------------------------------------------------------------------
    // Step 1: Doppelgänger
    // ------------------------------------------------------------------------
    if state.deck_contains(Role::Doppelganger) {
        room.say("🌙 Doppelgänger, wake up and look at another player's card...".light_blue())?;
        let step_start = Instant::now();

        if let Some(&d_idx) = state.initial_holders(Role::Doppelganger).first() {
            run_doppelganger_step(casemap, state, d_idx, &dm_cfg, rng).await?;
        }

        pad_step_duration(step_start, rng).await;
    }

    // ------------------------------------------------------------------------
    // Step 2: Werewolves (Initial Werewolves + Doppelgänger-Werewolf)
    // ------------------------------------------------------------------------
    if state.deck_contains(Role::Werewolf)
        || matches!(state.doppel_actor, Some((_, Role::Werewolf)))
    {
        room.say("🌙 Werewolves, wake up and look for other Werewolves...".light_blue())?;
        let step_start = Instant::now();

        let mut wolves = state.initial_holders(Role::Werewolf);
        if let Some((d_idx, Role::Werewolf)) = state.doppel_actor {
            if !wolves.contains(&d_idx) {
                wolves.push(d_idx);
            }
        }

        if wolves.len() == 1 {
            // Lone Wolf rule: may look at ONE center card (1, 2, or 3) for an alibi!
            let w_idx = wolves[0];
            let wolf_user = state.players[w_idx].user.clone();
            let choice = wolf_user
                .ask(
                    "🐺 You are the "
                        + "LONE WOLF".bold().red()
                        + "! No other player is a Werewolf. Pick ONE center card to view (`1`, `2`, `3`, or `pass`):",
                    |m| match m.text.trim().to_ascii_lowercase().as_str() {
                        "1" | "c1" | "center 1" => Match::Accept(Some(0usize)),
                        "2" | "c2" | "center 2" => Match::Accept(Some(1usize)),
                        "3" | "c3" | "center 3" => Match::Accept(Some(2usize)),
                        "pass" | "none" | "skip" => Match::Accept(None),
                        _ => Match::Reject(Text::from(
                            "Reply `1`, `2`, `3` to peek a center card, or `pass`.".yellow(),
                        )),
                    },
                )
                .with_config(&dm_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left to peek a center card!")
                .settled()
                .await?;

            match choice {
                Turn::Reply(Some(c_idx)) => {
                    let seen = state.center[c_idx];
                    let _ = wolf_user.say(
                        format!("🐺 Center card #{} is: ", c_idx + 1) + seen.display_for_peek(),
                    );
                    state.night_log.push(format!(
                        "Lone Wolf ({}) peeked Center #{} ({}).",
                        wolf_user.nick(),
                        c_idx + 1,
                        seen.effective_role().name()
                    ));
                }
                Turn::Reply(None) => {
                    let _ = wolf_user.say("🐺 You chose not to look at a center card.");
                    state
                        .night_log
                        .push(format!("Lone Wolf ({}) passed on peeking center.", wolf_user.nick()));
                }
                Turn::TimedOut | Turn::Left { .. } => {
                    state
                        .night_log
                        .push(format!("Lone Wolf ({}) did not peek.", wolf_user.nick()));
                }
            }
        } else if wolves.len() >= 2 {
            for &w_idx in &wolves {
                let partners: Vec<String> = wolves
                    .iter()
                    .filter(|&&other| other != w_idx)
                    .map(|&other| format!("[{}] {}", other + 1, state.players[other].user.nick()))
                    .collect();
                let _ = state.players[w_idx].user.say(
                    "🐺 Your fellow Werewolf pack: " + partners.join(", ").bold().red(),
                );
            }
            let names: Vec<String> = wolves
                .iter()
                .map(|&i| state.players[i].user.nick())
                .collect();
            state
                .night_log
                .push(format!("Werewolves ({}) saw each other.", names.join(", ")));
        }

        pad_step_duration(step_start, rng).await;
    }

    // ------------------------------------------------------------------------
    // Step 3: Minion (Initial Minion + Doppelgänger-Minion)
    // ------------------------------------------------------------------------
    if state.deck_contains(Role::Minion) || matches!(state.doppel_actor, Some((_, Role::Minion))) {
        room.say("🌙 Minion, wake up! Werewolves, stick out your thumbs...".light_blue())?;
        let step_start = Instant::now();

        let mut minions = state.initial_holders(Role::Minion);
        if let Some((d_idx, Role::Minion)) = state.doppel_actor {
            if !minions.contains(&d_idx) {
                minions.push(d_idx);
            }
        }

        let mut wolves = state.initial_holders(Role::Werewolf);
        if let Some((d_idx, Role::Werewolf)) = state.doppel_actor {
            if !wolves.contains(&d_idx) {
                wolves.push(d_idx);
            }
        }

        for m_idx in minions {
            if wolves.is_empty() {
                let _ = state.players[m_idx].user.say(
                    "🗡️ Minion: "
                        + "NO players are Werewolves".bold().yellow()
                        + " (all Werewolf cards are in the center)! Survive and get someone else killed to win!",
                );
            } else {
                let wolf_list: Vec<String> = wolves
                    .iter()
                    .map(|&w| format!("[{}] {}", w + 1, state.players[w].user.nick()))
                    .collect();
                let _ = state.players[m_idx].user.say(
                    "🗡️ Minion: The Werewolves are "
                        + wolf_list.join(", ").bold().red()
                        + " (they do not know who you are—protect them!).",
                );
            }
        }

        pad_step_duration(step_start, rng).await;
    }

    // ------------------------------------------------------------------------
    // Step 4: Masons (Initial Masons + Doppelgänger-Mason)
    // ------------------------------------------------------------------------
    if state.deck_contains(Role::Mason) || matches!(state.doppel_actor, Some((_, Role::Mason))) {
        room.say("🌙 Masons, wake up and look for other Masons...".light_blue())?;
        let step_start = Instant::now();

        let mut masons = state.initial_holders(Role::Mason);
        if let Some((d_idx, Role::Mason)) = state.doppel_actor {
            if !masons.contains(&d_idx) {
                masons.push(d_idx);
            }
        }

        if masons.len() == 1 {
            let m_idx = masons[0];
            let _ = state.players[m_idx].user.say(
                "🧱 Mason: You are the "
                    + "ONLY Mason".bold().light_cyan()
                    + " in play (the other Mason card is in the center!).",
            );
        } else if masons.len() >= 2 {
            for &m_idx in &masons {
                let others: Vec<String> = masons
                    .iter()
                    .filter(|&&o| o != m_idx)
                    .map(|&o| format!("[{}] {}", o + 1, state.players[o].user.nick()))
                    .collect();
                let _ = state.players[m_idx].user.say(
                    "🧱 Fellow Mason(s): " + others.join(", ").bold().light_cyan(),
                );
            }
        }

        pad_step_duration(step_start, rng).await;
    }

    // ------------------------------------------------------------------------
    // Step 5: Seer
    // ------------------------------------------------------------------------
    if state.deck_contains(Role::Seer) {
        room.say("🌙 Seer, wake up! View one player's card or two center cards...".light_blue())?;
        let step_start = Instant::now();

        if let Some(&s_idx) = state.initial_holders(Role::Seer).first() {
            let seer_user = state.players[s_idx].user.clone();
            let roster = state.format_roster();
            let prompt = format!(
                "🔮 Seer: Reply `player <seat|nick>` (not yourself) OR `center <1-3> <1-3>` (two center cards), or `pass`. Seats: {roster}"
            );
            let action = {
                let players_ref = &state.players;
                seer_user
                    .ask(prompt, move |m| parse_seer_choice(casemap, players_ref, s_idx, &m.text))
                    .with_config(&dm_cfg)
                    .warn_at(Duration::from_secs(10), "⏳ 10s left for Seer action!")
                    .settled()
                    .await?
            };
            apply_seer_choice(state, &seer_user, "Seer", action);
        }

        pad_step_duration(step_start, rng).await;
    }

    // ------------------------------------------------------------------------
    // Step 6: Robber
    // ------------------------------------------------------------------------
    if state.deck_contains(Role::Robber) {
        room.say("🌙 Robber, wake up! You may swap your card with another player...".light_blue())?;
        let step_start = Instant::now();

        if let Some(&r_idx) = state.initial_holders(Role::Robber).first() {
            let robber_user = state.players[r_idx].user.clone();
            let roster = state.format_roster();
            let prompt = format!(
                "🦹 Robber: Reply with a seat number or nick to steal their card, or `pass`. Seats: {roster}"
            );
            let action = {
                let players_ref = &state.players;
                robber_user
                    .ask(prompt, move |m| parse_other_player_or_pass(casemap, players_ref, r_idx, &m.text))
                    .with_config(&dm_cfg)
                    .warn_at(Duration::from_secs(10), "⏳ 10s left for Robber swap!")
                    .settled()
                    .await?
            };
            apply_robber_choice(state, r_idx, &robber_user, "Robber", action);
        }

        pad_step_duration(step_start, rng).await;
    }

    // ------------------------------------------------------------------------
    // Step 7: Troublemaker
    // ------------------------------------------------------------------------
    if state.deck_contains(Role::Troublemaker) {
        room.say("🌙 Troublemaker, wake up! You may swap two other players' cards...".light_blue())?;
        let step_start = Instant::now();

        if let Some(&tm_idx) = state.initial_holders(Role::Troublemaker).first() {
            let tm_user = state.players[tm_idx].user.clone();
            let roster = state.format_roster();
            let prompt = format!(
                "😈 Troublemaker: Reply with TWO other players (`<seat|nick> <seat|nick>`) to swap their cards, or `pass`. Seats: {roster}"
            );
            let action = {
                let players_ref = &state.players;
                tm_user
                    .ask(prompt, move |m| parse_troublemaker_choice(casemap, players_ref, tm_idx, &m.text))
                    .with_config(&dm_cfg)
                    .warn_at(Duration::from_secs(10), "⏳ 10s left for Troublemaker swap!")
                    .settled()
                    .await?
            };
            apply_troublemaker_choice(state, &tm_user, "Troublemaker", action);
        }

        pad_step_duration(step_start, rng).await;
    }

    // ------------------------------------------------------------------------
    // Step 8: Drunk (Mandatory swap with Center 1, 2, or 3!)
    // ------------------------------------------------------------------------
    if state.deck_contains(Role::Drunk) {
        room.say("🌙 Drunk, wake up and exchange your card with a center card...".light_blue())?;
        let step_start = Instant::now();

        if let Some(&dr_idx) = state.initial_holders(Role::Drunk).first() {
            let drunk_user = state.players[dr_idx].user.clone();
            let action = drunk_user
                .ask(
                    "🍺 Drunk: You MUST swap your card with a center card! Reply `1`, `2`, or `3`:",
                    |m| match m.text.trim().to_ascii_lowercase().as_str() {
                        "1" | "c1" | "center 1" => Match::Accept(0usize),
                        "2" | "c2" | "center 2" => Match::Accept(1usize),
                        "3" | "c3" | "center 3" => Match::Accept(2usize),
                        _ => Match::Reject(Text::from(
                            "Drunk MUST swap! Reply `1`, `2`, or `3`.".yellow(),
                        )),
                    },
                )
                .with_config(&dm_cfg)
                .warn_at(Duration::from_secs(10), "⏳ 10s left to pick center `1`, `2`, or `3`!")
                .settled()
                .await?;
            apply_drunk_choice(state, dr_idx, &drunk_user, "Drunk", action, rng);
        }

        pad_step_duration(step_start, rng).await;
    }

    // ------------------------------------------------------------------------
    // Step 9: Insomniac & Step 9a: Doppelgänger-Insomniac
    // ------------------------------------------------------------------------
    if state.deck_contains(Role::Insomniac) {
        room.say("🌙 Insomniac, wake up and check your card...".light_blue())?;
        let step_start = Instant::now();

        for ins_idx in state.initial_holders(Role::Insomniac) {
            let current = state.players[ins_idx].card;
            let _ = state.players[ins_idx].user.say(
                "🦉 Insomniac: You check the card in front of you right before dawn — it is "
                    + current.display_for_peek()
                    + "!",
            );
            state.night_log.push(format!(
                "Insomniac ({}) checked their card and saw {}.",
                state.players[ins_idx].user.nick(),
                current.effective_role().name()
            ));
        }

        pad_step_duration(step_start, rng).await;
    }

    if let Some((d_idx, Role::Insomniac)) = state.doppel_actor {
        room.say("🌙 Doppelgänger-Insomniac, wake up and check your card...".light_blue())?;
        let step_start = Instant::now();

        let current = state.players[d_idx].card;
        let _ = state.players[d_idx].user.say(
            "🦉 Doppelgänger-Insomniac: You check the card in front of you right before dawn — it is "
                + current.display_for_peek()
                + "!",
        );
        state.night_log.push(format!(
            "Doppelgänger-Insomniac ({}) checked their card and saw {}.",
            state.players[d_idx].user.nick(),
            current.effective_role().name()
        ));

        pad_step_duration(step_start, rng).await;
    }

    Ok(())
}

// ============================================================================
// 8a. Doppelgänger Multi-Turn `ExclusiveDm` Handler & Action Helpers
// ============================================================================

#[derive(Clone, Copy, Debug)]
enum SeerChoice {
    Player(usize),
    Center(usize, usize),
    Pass,
}

async fn run_doppelganger_step(
    casemap: CaseMap,
    state: &mut TableState,
    d_idx: usize,
    dm_cfg: &AskConfig,
    rng: &mut Rng,
) -> Result<()> {
    let doppel_user = state.players[d_idx].user.clone();

    // `ExclusiveDm` shines here: Doppelgänger may need TWO consecutive DM turns
    // (1. pick who to copy -> 2. immediately perform Seer/Robber/Troublemaker/Drunk action)
    // under one shared 60s deadline without releasing the DM slot in between!
    let step_deadline = Instant::now() + Duration::from_secs(60);
    let Ok(mut dm) = doppel_user.exclusive().await else {
        // Doppelgänger disconnected before waking: pick a random target quietly so the card has a role.
        let others: Vec<usize> = (0..state.players.len()).filter(|&i| i != d_idx).collect();
        let target = others[rng.usize(..others.len())];
        let copied = state.players[target].card.effective_role();
        state.players[d_idx].card = Card::Doppelganger { copied: Some(copied) };
        state.doppel_actor = Some((d_idx, copied));
        if copied == Role::Drunk {
            let c = rng.usize(0..3);
            std::mem::swap(&mut state.players[d_idx].card, &mut state.center[c]);
        }
        return Ok(());
    };

    dm = dm
        .with_ask_config(dm_cfg)
        .with_ask_deadline(step_deadline)
        .with_ignore_commands();

    let roster = state.format_roster();
    let copy_turn = {
        let players_ref = &state.players;
        dm.ask(
            format!(
                "🎭 Doppelgänger: Pick ONE other player (`<seat|nick>`) to view and become their role! Seats: {roster}"
            ),
            move |m| match resolve_player_token(casemap, players_ref, &m.text) {
                Some(idx) if idx == d_idx => {
                    Match::Reject(Text::from("You must pick ANOTHER player, not yourself!".yellow()))
                }
                Some(idx) => Match::Accept(idx),
                None => Match::Reject(Text::from(
                    "Unknown player—reply with a seat number (e.g. `2`) or nickname.".yellow(),
                )),
            },
        )
        .warn_at(Duration::from_secs(15), "⏳ 15s left to pick a player to copy!")
        .settled()
        .await?
    };

    let target_idx = match copy_turn {
        Turn::Reply(idx) => idx,
        Turn::TimedOut | Turn::Left { .. } => {
            let others: Vec<usize> = (0..state.players.len()).filter(|&i| i != d_idx).collect();
            let auto_idx = others[rng.usize(..others.len())];
            let _ = dm.say(format!(
                "🎲 Randomly looked at [{}] {}.",
                auto_idx + 1,
                state.players[auto_idx].user.nick()
            ));
            auto_idx
        }
    };

    let copied_role = state.players[target_idx].card.effective_role();
    state.players[d_idx].card = Card::Doppelganger {
        copied: Some(copied_role),
    };
    state.doppel_actor = Some((d_idx, copied_role));
    state.night_log.push(format!(
        "Doppelgänger ({}) copied {} and became {}.",
        doppel_user.nick(),
        state.players[target_idx].user.nick(),
        copied_role.name()
    ));

    let _ = dm.say(
        format!(
            "🎭 You looked at [{}] {}'s card: ",
            target_idx + 1,
            state.players[target_idx].user.nick()
        ) + copied_role.styled()
            + "! You are now on that role's team.",
    );

    // Immediate vs deferred Doppelgänger follow-up rules:
    match copied_role {
        Role::Seer => {
            let action = {
                let players_ref = &state.players;
                dm.ask(
                    format!(
                        "🔮 Doppelgänger-Seer: Immediately view `player <seat|nick>` OR `center <1-3> <1-3>`, or `pass`. Seats: {roster}"
                    ),
                    move |m| parse_seer_choice(casemap, players_ref, d_idx, &m.text),
                )
                .settled()
                .await?
            };
            apply_seer_choice(state, &doppel_user, "Doppelgänger-Seer", action);
        }
        Role::Robber => {
            let action = {
                let players_ref = &state.players;
                dm.ask(
                    format!(
                        "🦹 Doppelgänger-Robber: Immediately pick a player (`<seat|nick>`) to swap cards with, or `pass`. Seats: {roster}"
                    ),
                    move |m| parse_other_player_or_pass(casemap, players_ref, d_idx, &m.text),
                )
                .settled()
                .await?
            };
            apply_robber_choice(state, d_idx, &doppel_user, "Doppelgänger-Robber", action);
        }
        Role::Troublemaker => {
            let action = {
                let players_ref = &state.players;
                dm.ask(
                    format!(
                        "😈 Doppelgänger-Troublemaker: Immediately pick TWO other players (`<a> <b>`) to swap, or `pass`. Seats: {roster}"
                    ),
                    move |m| parse_troublemaker_choice(casemap, players_ref, d_idx, &m.text),
                )
                .settled()
                .await?
            };
            apply_troublemaker_choice(state, &doppel_user, "Doppelgänger-Troublemaker", action);
        }
        Role::Drunk => {
            let action = dm
                .ask(
                    "🍺 Doppelgänger-Drunk: Immediately swap your card with center `1`, `2`, or `3`!",
                    |m| match m.text.trim().to_ascii_lowercase().as_str() {
                        "1" | "c1" | "center 1" => Match::Accept(0usize),
                        "2" | "c2" | "center 2" => Match::Accept(1usize),
                        "3" | "c3" | "center 3" => Match::Accept(2usize),
                        _ => Match::Reject(Text::from("Reply `1`, `2`, or `3`.".yellow())),
                    },
                )
                .settled()
                .await?;
            apply_drunk_choice(state, d_idx, &doppel_user, "Doppelgänger-Drunk", action, rng);
        }
        Role::Werewolf | Role::Minion | Role::Mason => {
            let _ = dm.say(format!(
                "🌙 Close your eyes for now—you will wake up when {} is called!",
                copied_role.name()
            ));
        }
        Role::Insomniac => {
            let _ = dm.say(
                "🌙 Close your eyes for now—you will wake up at the very end of the night to check your card!",
            );
        }
        Role::Villager | Role::Tanner | Role::Hunter | Role::Doppelganger => {
            let _ = dm.say("🌙 You have no further night actions. Sleep tight!");
        }
    }

    Ok(())
}

fn parse_seer_choice(
    casemap: CaseMap,
    players: &[PlayerSlot],
    actor_idx: usize,
    raw: &str,
) -> Match<SeerChoice> {
    let trimmed = raw.trim();
    if matches!(trimmed.to_ascii_lowercase().as_str(), "pass" | "skip" | "none") {
        return Match::Accept(SeerChoice::Pass);
    }

    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.first().is_some_and(|w| w.eq_ignore_ascii_case("center") || w.eq_ignore_ascii_case("c")) {
        if parts.len() != 3 {
            return Match::Reject(Text::from(
                "Specify TWO distinct center cards: e.g. `center 1 3`.".yellow(),
            ));
        }
        let Ok(a) = parts[1].parse::<usize>() else {
            return Match::Reject(Text::from("Center slots must be `1`, `2`, or `3`.".yellow()));
        };
        let Ok(b) = parts[2].parse::<usize>() else {
            return Match::Reject(Text::from("Center slots must be `1`, `2`, or `3`.".yellow()));
        };
        if !(1..=3).contains(&a) || !(1..=3).contains(&b) || a == b {
            return Match::Reject(Text::from(
                "Pick two DIFFERENT center slots between 1 and 3 (e.g. `center 1 2`).".yellow(),
            ));
        }
        return Match::Accept(SeerChoice::Center(a - 1, b - 1));
    }

    let target_token = if parts.first().is_some_and(|w| w.eq_ignore_ascii_case("player") || w.eq_ignore_ascii_case("p")) {
        if parts.len() != 2 {
            return Match::Reject(Text::from("Usage: `player <seat|nick>`.".yellow()));
        }
        parts[1]
    } else if parts.len() == 1 {
        parts[0]
    } else {
        return Match::Reject(Text::from(
            "Reply `player <seat|nick>`, `center <1-3> <1-3>`, or `pass`.".yellow(),
        ));
    };

    match resolve_player_token(casemap, players, target_token) {
        Some(idx) if idx == actor_idx => {
            Match::Reject(Text::from("You cannot look at your own card!".yellow()))
        }
        Some(idx) => Match::Accept(SeerChoice::Player(idx)),
        None => Match::Reject(Text::from(
            "Unknown player—use a seat number or `center 1 2`.".yellow(),
        )),
    }
}

fn apply_seer_choice(state: &mut TableState, actor: &User, label: &str, turn: Turn<SeerChoice>) {
    match turn {
        Turn::Reply(SeerChoice::Player(idx)) => {
            let card = state.players[idx].card;
            let target_nick = state.players[idx].user.nick();
            let _ = actor.say(
                format!("🔮 [{}] {}'s card is: ", idx + 1, target_nick) + card.display_for_peek(),
            );
            state.night_log.push(format!(
                "{label} ({}) viewed {}'s card ({}).",
                actor.nick(),
                target_nick,
                card.effective_role().name()
            ));
        }
        Turn::Reply(SeerChoice::Center(a, b)) => {
            let ca = state.center[a];
            let cb = state.center[b];
            let _ = actor.say(
                format!("🔮 Center #{} is ", a + 1)
                    + ca.display_for_peek()
                    + format!(", and Center #{} is ", b + 1)
                    + cb.display_for_peek()
                    + ".",
            );
            state.night_log.push(format!(
                "{label} ({}) viewed Center #{} ({}) & Center #{} ({}).",
                actor.nick(),
                a + 1,
                ca.effective_role().name(),
                b + 1,
                cb.effective_role().name()
            ));
        }
        Turn::Reply(SeerChoice::Pass) | Turn::TimedOut | Turn::Left { .. } => {
            state
                .night_log
                .push(format!("{label} ({}) did not view any cards.", actor.nick()));
        }
    }
}

fn parse_other_player_or_pass(
    casemap: CaseMap,
    players: &[PlayerSlot],
    actor_idx: usize,
    raw: &str,
) -> Match<Option<usize>> {
    let t = raw.trim();
    if matches!(t.to_ascii_lowercase().as_str(), "pass" | "skip" | "none") {
        return Match::Accept(None);
    }
    match resolve_player_token(casemap, players, t) {
        Some(idx) if idx == actor_idx => {
            Match::Reject(Text::from("Pick ANOTHER player (or `pass`), not yourself!".yellow()))
        }
        Some(idx) => Match::Accept(Some(idx)),
        None => Match::Reject(Text::from(
            "Unknown player—reply with a seat number, nickname, or `pass`.".yellow(),
        )),
    }
}

fn apply_robber_choice(
    state: &mut TableState,
    actor_idx: usize,
    actor: &User,
    label: &str,
    turn: Turn<Option<usize>>,
) {
    if let Turn::Reply(Some(target_idx)) = turn {
        let victim_nick = state.players[target_idx].user.nick();
        // Swap the physical cards in front of `actor_idx` and `target_idx`:
        let temp = state.players[actor_idx].card;
        state.players[actor_idx].card = state.players[target_idx].card;
        state.players[target_idx].card = temp;

        let new_card = state.players[actor_idx].card;
        let _ = actor.say(
            format!("🦹 You swapped cards with [{}] {}! Your new card is: ", target_idx + 1, victim_nick)
                + new_card.display_for_peek()
                + " (you now belong to that card's team!).",
        );
        state.night_log.push(format!(
            "{label} ({}) stole {}'s card (receiving {}).",
            actor.nick(),
            victim_nick,
            new_card.effective_role().name()
        ));
    } else {
        state
            .night_log
            .push(format!("{label} ({}) did not steal anyone's card.", actor.nick()));
    }
}

fn parse_troublemaker_choice(
    casemap: CaseMap,
    players: &[PlayerSlot],
    actor_idx: usize,
    raw: &str,
) -> Match<Option<(usize, usize)>> {
    let t = raw.trim();
    if matches!(t.to_ascii_lowercase().as_str(), "pass" | "skip" | "none") {
        return Match::Accept(None);
    }
    let parts: Vec<&str> = t.split_whitespace().collect();
    if parts.len() != 2 {
        return Match::Reject(Text::from(
            "Reply with TWO other players separated by a space (e.g. `1 3`), or `pass`.".yellow(),
        ));
    }
    let Some(a) = resolve_player_token(casemap, players, parts[0]) else {
        return Match::Reject(Text::from(format!("Unknown first player `{}`.", parts[0]).yellow()));
    };
    let Some(b) = resolve_player_token(casemap, players, parts[1]) else {
        return Match::Reject(Text::from(format!("Unknown second player `{}`.", parts[1]).yellow()));
    };
    if a == actor_idx || b == actor_idx {
        return Match::Reject(Text::from(
            "Troublemaker cannot swap their OWN card—pick two OTHER players!".yellow(),
        ));
    }
    if a == b {
        return Match::Reject(Text::from("Pick two DIFFERENT players!".yellow()));
    }
    Match::Accept(Some((a, b)))
}

fn apply_troublemaker_choice(
    state: &mut TableState,
    actor: &User,
    label: &str,
    turn: Turn<Option<(usize, usize)>>,
) {
    if let Turn::Reply(Some((a, b))) = turn {
        let nick_a = state.players[a].user.nick();
        let nick_b = state.players[b].user.nick();
        let temp = state.players[a].card;
        state.players[a].card = state.players[b].card;
        state.players[b].card = temp;

        let _ = actor.say(
            "😈 Swapped "
                + nick_a.as_str().bold()
                + " and "
                + nick_b.as_str().bold()
                + "'s cards!",
        );
        state.night_log.push(format!(
            "{label} ({}) swapped {} and {}.",
            actor.nick(),
            nick_a,
            nick_b
        ));
    } else {
        state
            .night_log
            .push(format!("{label} ({}) did not swap anyone.", actor.nick()));
    }
}

fn apply_drunk_choice(
    state: &mut TableState,
    actor_idx: usize,
    actor: &User,
    label: &str,
    turn: Turn<usize>,
    rng: &mut Rng,
) {
    let c_idx = match turn {
        Turn::Reply(idx) => idx,
        // Mandatory swap rule: even if AFK or disconnected, Drunk MUST swap with a center card!
        Turn::TimedOut | Turn::Left { .. } => {
            let auto = rng.usize(0..3);
            let _ = actor.say(format!(
                "🍺 Randomly swapped your card with Center #{} (you do not see what you got).",
                auto + 1
            ));
            auto
        }
    };
    std::mem::swap(&mut state.players[actor_idx].card, &mut state.center[c_idx]);
    if matches!(turn, Turn::Reply(_)) {
        let _ = actor.say(format!(
            "🍺 Swapped your card face-down with Center #{}! You have no idea what role you are now.",
            c_idx + 1
        ));
    }
    state.night_log.push(format!(
        "{label} ({}) swapped with Center #{}.",
        actor.nick(),
        c_idx + 1
    ));
}

// ============================================================================
// 9. Phase 3: The Day Phase (Timed Discussion + Consensus `!ready` Early Vote)
// ============================================================================

async fn run_day_phase(room: &Room, state: &TableState) -> Result<()> {
    // 5 to 8 minutes depending on player count (3 players -> 5m, 8+ players -> 8m):
    let day_mins = (state.players.len() + 2).clamp(5, 8) as u64;
    let day_duration = Duration::from_secs(day_mins * 60);
    let deadline = Instant::now() + day_duration;

    room.say(
        "☀️ EVERYONE WAKE UP! ".bold().yellow()
            + format!(
                "You have {day_mins} minutes to discuss, bluff, and deduce whose card is where! "
            )
            + "(Type "
            + "!ready".bold().green()
            + " when ready to vote immediately, or "
            + "!time".bold()
            + " to check the clock.)",
    )?;

    let mut inbox = room.inbox();
    let enrolled: HashSet<User> = state.players.iter().map(|p| p.user.clone()).collect();
    let mut ready_voters: HashSet<User> = HashSet::new();

    // Remaining-time warning milestones in descending order:
    let mut milestones: Vec<(Duration, &'static str)> = vec![
        (Duration::from_secs(120), "⏳ 2 minutes remaining before the vote!"),
        (Duration::from_secs(60), "⏳ 1 minute remaining!"),
        (Duration::from_secs(15), "⏳ 15 seconds remaining—figure out who you're pointing at!"),
    ];

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }

        // Drop milestones already in the past:
        while milestones
            .first()
            .is_some_and(|(rem, _)| deadline.saturating_duration_since(now) <= *rem)
        {
            milestones.remove(0);
        }

        let next_tick = milestones
            .first()
            .map(|(rem, _)| deadline - *rem)
            .unwrap_or(deadline);

        tokio::select! {
            _ = tokio::time::sleep_until(next_tick) => {
                if let Some((_, text)) = milestones.first().copied() {
                    if Instant::now() < deadline {
                        milestones.remove(0);
                        room.say(text.yellow().bold())?;
                    }
                } else {
                    break;
                }
            }
            ev = inbox.next_event() => {
                match ev? {
                    RoomEvent::Chat(msg) if msg.is_cmd("!time") => {
                        let secs = deadline.saturating_duration_since(Instant::now()).as_secs();
                        room.say(format!("⏳ {:02}:{:02} left in Day Phase.", secs / 60, secs % 60))?;
                    }
                    RoomEvent::Chat(msg) if msg.is_cmd("!ready") || msg.is_cmd("!votenow") => {
                        if enrolled.contains(&msg.sender) && ready_voters.insert(msg.sender.clone()) {
                            let active_total = enrolled.iter().filter(|u| is_active_in_room(room, u)).count();
                            room.say(
                                msg.sender.nick().bold().green()
                                    + format!(" is ready to vote! ({}/{active_total})", ready_voters.len()),
                            )?;
                            if active_total > 0 && ready_voters.len() >= active_total {
                                room.say("🗳️ All active players are ready—moving straight to the vote!".bold().green())?;
                                break;
                            }
                        }
                    }
                    RoomEvent::Parted { user, .. }
                    | RoomEvent::Kicked { user, .. }
                    | RoomEvent::Quit { user, .. } => {
                        if enrolled.contains(&user) {
                            ready_voters.remove(&user);
                            let active_total = enrolled.iter().filter(|u| is_active_in_room(room, u)).count();
                            if active_total > 0 && ready_voters.len() >= active_total {
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

// ============================================================================
// 10. Phase 4: Simultaneous Secret Voting ("3, 2, 1, POINT!")
// ============================================================================

async fn run_voting_phase(
    bot: &Bot,
    room: &Room,
    state: &TableState,
) -> Result<Vec<Option<usize>>> {
    let casemap = bot.casemap();
    let roster = state.format_roster();
    let deadline = Instant::now() + VOTE_TIMEOUT;

    room.say(
        "🗳️ TIME IS UP! ".bold().red()
            + "3... 2... 1... POINT! Check your DMs and reply with the seat number or nick of the player you vote to KILL (45s)!",
    )?;

    let vote_cfg = AskConfig::new()
        .reply_deadline(deadline)
        .ignore_commands()
        .on_timeout("⏳ Time's up! You failed to point in time.".yellow());

    // Simultaneous secret DM vote across all N players:
    // - Uses `Match::Reject` so typos or self-votes get instant private correction without losing their timer.
    // - Uses `.settled()` so an AFK or disconnected voter yields `None` without cancelling other voters.
    // - Resolves IMMEDIATELY once all players lock in their vote!
    let results = try_join_all((0..state.players.len()).map(|voter_idx| {
        let voter = state.players[voter_idx].user.clone();
        let prompt = format!(
            "👉 Point at ANOTHER player to kill! Reply with a seat number or nick (45s). Seats: {roster}"
        );
        let players_ref = &state.players;
        let cfg_ref = &vote_cfg;
        async move {
            let outcome = voter
                .ask(prompt, move |m| {
                    let raw = m.cmd_args("!vote").unwrap_or(&m.text);
                    match resolve_player_token(casemap, players_ref, raw) {
                        Some(idx) if idx == voter_idx => Match::Reject(Text::from(
                            "You must point at ANOTHER player, not yourself!".yellow(),
                        )),
                        Some(idx) => Match::Accept(idx),
                        None => Match::Reject(Text::from(
                            "Unknown player—reply with their seat number (e.g. `2`) or nickname.".yellow(),
                        )),
                    }
                })
                .with_config(cfg_ref)
                .warn_at(Duration::from_secs(10), "⏳ 10 seconds left to point!")
                .settled()
                .await?;

            if let Turn::Reply(target_idx) = outcome {
                let _ = voter.say(
                    "✅ Locked in your point at "
                        + format!(
                            "[{}] {}",
                            target_idx + 1,
                            players_ref[target_idx].user.nick()
                        )
                        .bold(),
                );
            }
            Ok::<_, Error>(outcome.into_reply())
        }
    }))
    .await?;

    Ok(results)
}

// ============================================================================
// 11. Phase 5: Deaths, Hunter Chain, Card Reveal, & Exact Win Conditions
// ============================================================================

fn resolve_and_announce_winners(
    room: &Room,
    state: &TableState,
    votes: &[Option<usize>],
) -> Result<()> {
    let n = state.players.len();
    let mut tally = vec![0usize; n];
    for &opt_target in votes {
        if let Some(target) = opt_target {
            tally[target] += 1;
        }
    }

    // 1. Reveal simultaneous points:
    let point_summary: Vec<String> = state
        .players
        .iter()
        .enumerate()
        .map(|(i, p)| match votes[i] {
            Some(t) => format!("{} → {}", p.user.nick(), state.players[t].user.nick()),
            None => format!("{} → (abstained)", p.user.nick()),
        })
        .collect();
    room.say("👉 Votes: ".bold() + point_summary.join(" | "))?;

    // 2. Calculate initial deaths:
    //    - Circle Vote: if highest vote count <= 1, NOBODY dies!
    //    - Otherwise all players tied for `max_votes` die.
    let max_votes = tally.iter().copied().max().unwrap_or(0);
    let mut dead: HashSet<usize> = HashSet::new();
    if max_votes >= 2 {
        for (idx, &count) in tally.iter().enumerate() {
            if count == max_votes {
                dead.insert(idx);
            }
        }
    }

    // 3. Hunter chain resolution (handles both Hunter and Doppelgänger-Hunter chains!):
    let mut hunter_shots: Vec<(usize, usize)> = Vec::new();
    loop {
        let mut newly_shot = Vec::new();
        for &dead_idx in &dead {
            if state.players[dead_idx].card.effective_role() == Role::Hunter {
                if let Some(shot_target) = votes[dead_idx] {
                    if !dead.contains(&shot_target) {
                        newly_shot.push((dead_idx, shot_target));
                    }
                }
            }
        }
        if newly_shot.is_empty() {
            break;
        }
        for (hunter_idx, target_idx) in newly_shot {
            dead.insert(target_idx);
            hunter_shots.push((hunter_idx, target_idx));
        }
    }

    // 4. Reveal Night Log & Final Table Cards:
    room.say(
        "🌅 DAWN REVEAL — Center Cards: ".bold()
            + "#1 "
            + state.center[0].display_reveal()
            + " | #2 "
            + state.center[1].display_reveal()
            + " | #3 "
            + state.center[2].display_reveal(),
    )?;

    for (idx, p) in state.players.iter().enumerate() {
        let status = if dead.contains(&idx) {
            "💀 KILLED".bold().red()
        } else {
            "💚 SURVIVED".green()
        };
        room.say(
            format!("[{}] ", idx + 1)
                + p.user.nick().bold()
                + ": started "
                + p.initial_role.styled()
                + " → ended "
                + p.card.display_reveal()
                + format!(" ({} vote{}) — ", tally[idx], if tally[idx] == 1 { "" } else { "s" })
                + status,
        )?;
    }

    for (h_idx, t_idx) in &hunter_shots {
        room.say(
            "🏹 HUNTER SHOT! ".bold().orange()
                + state.players[*h_idx].user.nick().bold()
                + " died as Hunter and took down "
                + state.players[*t_idx].user.nick().bold().red()
                + " with them!",
        )?;
    }

    if !state.night_log.is_empty() {
        room.say("📜 Night Recap: ".bold() + state.night_log.join(" -> "))?;
    }

    // 5. Evaluate Win Conditions in strict ONUW order:
    let wolves_in_play: Vec<usize> = (0..n)
        .filter(|&i| state.players[i].card.effective_role() == Role::Werewolf)
        .collect();
    let minions_in_play: Vec<usize> = (0..n)
        .filter(|&i| state.players[i].card.effective_role() == Role::Minion)
        .collect();
    let killed_tanners: Vec<usize> = (0..n)
        .filter(|&i| state.players[i].card.effective_role() == Role::Tanner && dead.contains(&i))
        .collect();

    let any_wolf_in_play = !wolves_in_play.is_empty();
    let any_wolf_killed = wolves_in_play.iter().any(|i| dead.contains(i));
    let any_minion_in_play = !minions_in_play.is_empty();
    let any_minion_killed = minions_in_play.iter().any(|i| dead.contains(i));
    let any_tanner_killed = !killed_tanners.is_empty();

    // (a) Tanner wins whenever killed:
    let tanner_winners: Vec<usize> = killed_tanners;

    // (b) Village Team win rules:
    //     - Werewolves in play: Village wins iff >= 1 Werewolf is killed (even alongside a Tanner or Villager!).
    //     - NO Werewolves in play, Minion in play: Village MUST kill the Minion to win!
    //     - NO Werewolves & NO Minion in play: Village wins iff NOBODY dies (1-vote Circle Vote)!
    let village_wins = if any_wolf_in_play {
        any_wolf_killed
    } else if any_minion_in_play {
        any_minion_killed
    } else {
        dead.is_empty()
    };

    // (c) Werewolf Team (Werewolves + Minion) win rules:
    //     - Werewolves in play: win iff NO Werewolves are killed AND NO Tanner is killed
    //       (if Minion dies while Werewolves live and Tanner lives, Werewolves + Minion still win!).
    //     - NO Werewolves in play, Minion in play: solo Minion wins iff Minion survives AND NO Tanner is killed!
    let werewolf_team_wins = if any_wolf_in_play {
        !any_wolf_killed && !any_tanner_killed
    } else if any_minion_in_play {
        !any_minion_killed && !any_tanner_killed
    } else {
        false
    };

    if max_votes <= 1 {
        room.say("🔄 Circle Vote! No player received more than 1 vote, so nobody was killed!".cyan())?;
    }

    let mut winner_nicks: HashMap<usize, &'static str> = HashMap::new();
    for &idx in &tanner_winners {
        winner_nicks.insert(idx, "Tanner");
    }
    if village_wins {
        for idx in 0..n {
            if state.players[idx].card.effective_role().team() == Team::Village {
                winner_nicks.insert(idx, "Village");
            }
        }
    }
    if werewolf_team_wins {
        for idx in 0..n {
            if state.players[idx].card.effective_role().team() == Team::Werewolf {
                winner_nicks.insert(idx, "Werewolf Team");
            }
        }
    }

    if winner_nicks.is_empty() {
        room.say(
            "💀 NOBODY WINS! There were no Werewolves or Minions, yet the panicked village executed an innocent!"
                .bold()
                .red(),
        )?;
    } else {
        let mut headlines = Vec::new();
        if !tanner_winners.is_empty() {
            headlines.push("🎭 TANNER WINS!".bold().orange());
        }
        if village_wins {
            headlines.push("🏡 VILLAGE TEAM WINS!".bold().green());
        }
        if werewolf_team_wins {
            headlines.push("🐺 WEREWOLF TEAM WINS!".bold().red());
        }

        let mut banner = Text::from("🏆 ");
        for (i, h) in headlines.into_iter().enumerate() {
            if i > 0 {
                banner = banner + " + ";
            }
            banner = banner + h;
        }

        let mut sorted_winners: Vec<(usize, &'static str)> = winner_nicks.into_iter().collect();
        sorted_winners.sort_by_key(|(idx, _)| *idx);
        let roster_str = sorted_winners
            .into_iter()
            .map(|(idx, team)| format!("{} ({team})", state.players[idx].user.nick()))
            .collect::<Vec<_>>()
            .join(", ");

        room.say(banner + " — Winners: " + roster_str.bold())?;
    }

    Ok(())
}
