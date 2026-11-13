//! Aspirational `ircgm` Bot: **One Night Ultimate Werewolf — Causal-DAG Parallel Night**
//!
//! Companion to [`bot.rs`](./bot.rs). Everything outside the Night Phase (Lobby, Setup, Day,
//! Simultaneous Voting, Hunter chain, and Win Conditions) plays the exact same base game,
//! but **Phase 2 (`run_night_phase_dag`)** replaces the 9-step sequential tabletop wake-up loop
//! with a **minimal-sequence-point dataflow DAG**:
//!
//! ```text
//! t = 0 (ALL independent role prompts fire concurrently in DM):
//!   ├── Doppelgänger 1a ──► [doppel_1a watch] ──┬──► Minion DM (instant)
//!   │        │                                  ├──► Masons DM (instant)
//!   │        ▼                                  └──► Werewolves: Pack DM OR Lone Wolf ask()
//!   │   Doppelgänger 1b                                               │
//!   │        │                                                        │
//!   │        ▼                                                        ▼
//!   │   [board_1 watch] ────────────────────────┬──────────────► Lone Wolf center reveal DM
//!   │        │                                  │
//!   ├── Seer ask() ─────────────────────────────┼──────────────► Seer vision reveal DM
//!   │                                           │
//!   ├── Robber ask() ───────────────────────────┴──► [board_6] ─► Robber stolen-card reveal DM
//!   │                                                    │
//!   ├── Troublemaker ask() ──────────────────────────────▼
//!   │                                                [board_7]
//!   │                                                    │
//!   └── Drunk ask() ─────────────────────────────────────▼
//!                                                    [board_8] ─► Insomniac + Doppel-Insomniac DMs
//! ```
//!
//! ### Why this is wild on IRC:
//! 1. **Almost every prompt starts at `t = 0`:** `Doppelgänger 1a`, `Seer`, `Robber`, `Troublemaker`,
//!    and `Drunk` (plus `Lone Wolf` whenever Doppelgänger isn't in play or `initial_wolves != 1`)
//!    all receive their DM prompts immediately when Night begins.
//! 2. **Fine-grained early reveals (no global barrier needed for mid-night readers):**
//!    If `Robber` answers in 5s while `Troublemaker` ponders for 35s, `Robber` gets his stolen-card
//!    reveal immediately once `board_1` is ready—because `Troublemaker` and `Drunk` act *after*
//!    `Robber` in canonical wake order!
//! 3. **Zero per-role timing leaks:** Instead of announcing `"Seer, wake up..."` step-by-step and
//!    faking random `sleep`s per center card, `#werewolf` sees a single unified Night window
//!    (padded only to a short 15–20s minimum floor so a 0-action deal doesn't end in `10ms`).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use fastrand::Rng;
use futures::future::try_join_all;
use tokio::sync::{oneshot, watch};
use tokio::time::Instant;

use ircgm::{
    AskConfig, Bot, BotConfig, CaseMap, ChatMessage, Colorize, DisconnectReason, Error,
    GameFutureExt, InboxScope, Match, Outcome, Result, Room, RoomEvent, Text, Turn, User,
};

const MIN_PLAYERS: usize = 3;
const MAX_PLAYERS: usize = 10;
const LOBBY_DURATION: Duration = Duration::from_secs(90);
const NIGHT_STAGE1_BUDGET: Duration = Duration::from_secs(40);
const NIGHT_STAGE2_MIN_BUDGET: Duration = Duration::from_secs(25);
const VOTE_TIMEOUT: Duration = Duration::from_secs(45);

// ============================================================================
// 1. Roles, Physical Cards, & Immutable Night Snapshots
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
                "Look at another player's card and permanently become that role & team."
            }
            Role::Werewolf => {
                "See your packmates; if you are the Lone Wolf, you may peek at 1 center card."
            }
            Role::Minion => {
                "Learn who the Werewolves are (they do not know who you are). Win if Werewolves survive!"
            }
            Role::Mason => "See the other Mason. If you are alone, the other Mason is in the center.",
            Role::Seer => "View EITHER 1 other player's card OR 2 center cards.",
            Role::Robber => "Optionally swap your card with another player's card and view your new card.",
            Role::Troublemaker => "Optionally swap two OTHER players' cards without looking at them.",
            Role::Drunk => "MUST swap your card with 1 of the 3 center cards without looking at it.",
            Role::Insomniac => "Check your own card right before dawn to see if it changed.",
            Role::Tanner => "You ONLY win if you are killed!",
            Role::Hunter => "Sleep at night. If you die, whoever YOU voted for also dies immediately!",
            Role::Villager => "Sleep at night. Find and kill a Werewolf during the day!",
        }
    }
}

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

    fn effective_role(self) -> Role {
        match self {
            Card::Doppelganger { copied: Some(r) } => r,
            Card::Doppelganger { copied: None } => Role::Villager,
            Card::Standard(r) => r,
        }
    }

    fn display_for_peek(self) -> Text {
        match self {
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
// 2. Immutable `Roster` + Value-Semantic `BoardCards` for Lock-Free DAG Edges
// ============================================================================

struct SeatInfo {
    user: User,
    initial_role: Role,
}

/// Read-only Setup metadata shared freely (`&Roster`) across all concurrent DAG branches!
/// Notice how separating immutable `Roster` from value-passed `BoardCards` means ZERO
/// `Arc<Mutex<TableState>>` locks and zero borrow-checker conflicts across `tokio::try_join!`.
struct Roster {
    casemap: CaseMap,
    seats: Vec<SeatInfo>,
    deck_roles: Vec<Role>,
}

impl Roster {
    fn len(&self) -> usize {
        self.seats.len()
    }

    fn initial_holders(&self, role: Role) -> Vec<usize> {
        self.seats
            .iter()
            .enumerate()
            .filter_map(|(i, s)| (s.initial_role == role).then_some(i))
            .collect()
    }

    fn format_seats(&self) -> String {
        self.seats
            .iter()
            .enumerate()
            .map(|(i, s)| format!("[{}] {}", i + 1, s.user.nick()))
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

    fn resolve_token(&self, token: &str) -> Option<usize> {
        let clean = token.trim().trim_end_matches(':').trim_end_matches(',');
        let num_token = clean.strip_prefix('#').unwrap_or(clean);
        if let Ok(seat) = num_token.parse::<usize>() {
            if (1..=self.seats.len()).contains(&seat) {
                return Some(seat - 1);
            }
        }
        self.seats
            .iter()
            .position(|s| self.casemap.eq(&s.user.nick(), clean))
    }
}

/// Lightweight value snapshot of where every physical card sits at a given DAG edge (`<= 13` `Copy` cards).
#[derive(Clone, Debug)]
struct BoardCards {
    players: Vec<Card>,
    center: [Card; 3],
}

/// Waits until a `watch::Receiver<Option<T>>` transitions from `None` (pending) to `Some(val)`.
async fn wait_ready<T: Clone>(rx: &mut watch::Receiver<Option<T>>) -> T {
    loop {
        if let Some(val) = rx.borrow_and_update().clone() {
            return val;
        }
        // Sender lives in a sibling `tokio::try_join!` branch; if cancelled, this future drops too.
        let _ = rx.changed().await;
    }
}

// ============================================================================
// 3. RAII Channel Moderation Guard
// ============================================================================

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

fn is_active_in_room(room: &Room, user: &User) -> bool {
    user.is_online() && room.has_nick(&user.nick())
}

fn is_admin(msg: &ChatMessage) -> bool {
    msg.account.as_deref() == Some("seirl") || msg.host.as_deref() == Some("staff/seirl")
}

// ============================================================================
// 4. Entrypoint, Lobby, & Top-Level Game Flow
// ============================================================================

pub async fn run_bot(irc_cfg: irc::client::data::Config, channel: &str) -> Result<()> {
    let cfg = BotConfig::new(irc_cfg)
        .with_ask_timeout(NIGHT_STAGE1_BUDGET)
        .with_command_prefixes(["!"]);

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
                eprintln!("Session {} ended ({err}); reconnecting in 5s...", bot.session_id());
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn lobby_loop(bot: Bot, room: Room) -> Result<()> {
    room.say(
        "🐺 ".bold()
            + "One Night Ultimate Werewolf (Parallel-DAG Mode)".bold().yellow()
            + " ready! Type "
            + "!onuw".bold().green()
            + " to open a lobby (3–10 players).",
    )?;

    while !bot.is_shutting_down() {
        let trigger = match room.inbox().wait_cmd("!onuw").until(bot.shutdown_requested()).await {
            Outcome::Completed(res) => res?,
            Outcome::Stopped(()) => break,
        };

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
                room.say("🛑 Bot restarting—aborting game, sorry!".yellow())?;
                break;
            }
        }
    }
    Ok(())
}

async fn recruit_players(bot: &Bot, room: &Room, host: User) -> Result<Option<Vec<User>>> {
    let mut inbox = room.inbox();
    let mut players: Vec<User> = vec![host.clone()];

    room.say(
        "🌕 ".bold()
            + host.nick().bold().yellow()
            + " opened an ONUW lobby! Type "
            + "!join".bold().green()
            + ", "
            + "!leave".bold()
            + ", or "
            + "!go".bold().cyan()
            + " once 3+ players are in (90s).",
    )?;

    let deadline = Instant::now() + LOBBY_DURATION;
    let warn_instant = deadline - Duration::from_secs(30);
    let mut warned_30s = false;

    loop {
        if players.len() >= MAX_PLAYERS {
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
                    room.say(format!("⏳ 30s left to `!join`! ({}/{MIN_PLAYERS}+)", players.len()).yellow())?;
                } else {
                    break;
                }
            }
            ev = inbox.next_event() => {
                match ev? {
                    RoomEvent::Chat(msg) if msg.is_cmd("!join") => {
                        if !players.contains(&msg.sender) {
                            players.push(msg.sender.clone());
                            room.say(msg.sender.nick().bold().green() + format!(" joined! ({}/{MAX_PLAYERS})", players.len()))?;
                        }
                    }
                    RoomEvent::Chat(msg) if msg.is_cmd("!leave") => {
                        if let Some(pos) = players.iter().position(|u| *u == msg.sender) {
                            players.remove(pos);
                            if players.is_empty() {
                                room.say("Lobby empty—cancelled.".yellow())?;
                                return Ok(None);
                            }
                        }
                    }
                    RoomEvent::Chat(msg) if msg.is_cmd("!go") && players.first() == Some(&msg.sender) => {
                        if players.len() >= MIN_PLAYERS {
                            break;
                        }
                        room.say(format!("Need at least {MIN_PLAYERS} players.").yellow())?;
                    }
                    RoomEvent::Parted { user, .. }
                    | RoomEvent::Kicked { user, .. }
                    | RoomEvent::Quit { user, .. } => {
                        players.retain(|u| *u != user);
                    }
                    _ => {}
                }
            }
        }
    }

    if players.len() < MIN_PLAYERS {
        room.say("Not enough players—lobby closed.".yellow())?;
        return Ok(None);
    }

    room.say("📩 DM check: reply `ok` in DM within 25s to confirm you can receive secret cards!")?;
    let check_deadline = Instant::now() + Duration::from_secs(25);
    let checks = try_join_all(players.iter().map(|p| async move {
        let res = p
            .ask("👋 Reply `ok` to confirm DMs for ONUW!", |m| {
                let t = m.text.trim();
                (t.eq_ignore_ascii_case("ok") || m.is_cmd("!ok")).then_some(())
            })
            .reply_deadline(check_deadline)
            .settled()
            .await?;
        Ok::<_, Error>((p.clone(), res))
    }))
    .await?;

    let verified: Vec<User> = checks
        .into_iter()
        .filter_map(|(p, out)| matches!(out, Turn::Reply(()) if is_active_in_room(room, &p)).then_some(p))
        .collect();

    if verified.len() < MIN_PLAYERS {
        room.say("Too few players passed the DM check—cancelled.".yellow())?;
        return Ok(None);
    }
    Ok(Some(verified))
}

async fn run_single_game(bot: &Bot, room: &Room, host: User) -> Result<()> {
    let Some(players) = recruit_players(bot, room, host).await? else {
        return Ok(());
    };

    let mut rng = Rng::new();
    let deck_roles = build_base_deck(players.len());
    let mut dealt: Vec<Card> = deck_roles.iter().copied().map(Card::from_role).collect();
    rng.shuffle(&mut dealt);

    let player_cards: Vec<Card> = dealt.drain(..players.len()).collect();
    let center: [Card; 3] = [dealt[0], dealt[1], dealt[2]];

    let seats: Vec<SeatInfo> = players
        .iter()
        .zip(&player_cards)
        .map(|(u, &c)| SeatInfo {
            user: u.clone(),
            initial_role: match c {
                Card::Doppelganger { .. } => Role::Doppelganger,
                Card::Standard(r) => r,
            },
        })
        .collect();

    let roster = Roster {
        casemap: bot.casemap(),
        seats,
        deck_roles,
    };
    let initial_board = BoardCards {
        players: player_cards,
        center,
    };

    let mut mod_guard = ChannelModerationGuard::new(room.clone());

    // Phase 1: Deal initial cards
    room.say(
        "🃏 Shuffling ".bold()
            + format!("{} cards", roster.deck_roles.len()).bold().yellow()
            + ": "
            + roster.format_deck(),
    )?;
    room.say("👥 Seats: ".bold() + roster.format_seats())?;
    for (idx, seat) in roster.seats.iter().enumerate() {
        seat.user.say(
            format!("🔒 [Seat #{}] Your initial card is: ", idx + 1)
                + seat.initial_role.styled()
                + " — "
                + seat.initial_role.night_summary(),
        )?;
    }

    // Out-of-band `!deck` / `!players` / `!help` responder:
    let deck_snapshot = roster.format_deck();
    let deck_len = roster.deck_roles.len();
    let info_scope = InboxScope::new().room(room).dms_from(&players);
    let mut info_inbox = bot.inbox_for(info_scope).lossy();
    let info_responder = async {
        loop {
            let msg = info_inbox.next_chat().await?;
            if msg.is_cmd("!deck") || msg.is_cmd("!roles") {
                bot.reply(&msg, format!("🃏 Deck ({deck_len} cards): {deck_snapshot}"))?;
            } else if msg.is_cmd("!players") || msg.is_cmd("!seats") {
                bot.reply(&msg, format!("👥 Seats: {}", roster.format_seats()))?;
            } else if msg.is_cmd("!help") {
                bot.reply(&msg, "📖 Commands: `!deck`, `!players`, `!time`, `!ready`.")?;
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), Error>(())
    };

    let game_flow = async {
        // Phase 2: Causal-DAG Parallel Night!
        mod_guard.enter_quiet_phase("ONUW — 🌙 Night Phase (All active roles check DMs!)");
        let (final_board, night_log) = run_night_phase_dag(room, &roster, initial_board).await?;

        // Phase 3: Timed Day Discussion
        mod_guard.enter_day_discussion(&players, "ONUW — ☀️ Day Phase (Discuss & deduce!)");
        run_day_phase(room, &roster).await?;

        // Phase 4: Simultaneous Blind Voting
        mod_guard.enter_quiet_phase("ONUW — 🗳️ Voting Phase (Check DMs!)");
        let votes = run_voting_phase(room, &roster).await?;

        // Phase 5: Reveal & Win Conditions
        drop(mod_guard);
        resolve_and_announce_winners(room, &roster, &final_board, &night_log, &votes)?;
        Ok::<(), Error>(())
    };

    tokio::select! {
        res = game_flow => res,
        res = info_responder => res,
    }
}

// ============================================================================
// 5. Phase 2: Causal-DAG Parallel Night (`watch` + `oneshot` + `tokio::try_join!`)
// ============================================================================

#[derive(Clone, Copy, Debug)]
enum SeerChoice {
    Player(usize),
    Center(usize, usize),
    Pass,
}

async fn run_night_phase_dag(
    room: &Room,
    roster: &Roster,
    initial_board: BoardCards,
) -> Result<(BoardCards, Vec<String>)> {
    let night_start = Instant::now();
    let stage1_deadline = night_start + NIGHT_STAGE1_BUDGET;

    // [API NOTE #11 — HIGHER `t = 0` BURST]:
    // Firing ALL Stage-1 prompts simultaneously at `t = 0` is ~4x faster for players and removes
    // per-role timing leaks, but concentrates even more outbound `PRIVMSG`s into millisecond 0
    // right after `deal_initial_cards` + `+m`, making outbound burst-throttle awareness essential.
    room.say(
        "🌙 Night falls! All night roles have been prompted in DM simultaneously (40s window)..."
            .bold()
            .light_blue(),
    )?;

    let stage1_cfg = AskConfig::new()
        .reply_deadline(stage1_deadline)
        .ignore_commands()
        .on_timeout("⏳ Time's up!".yellow());

    // ------------------------------------------------------------------------
    // Wire the DAG edges with zero Mutexes:
    //   1. `doppel_1a`: `Option<(doppel_seat, copied_role)>`
    //      (Immediately `Some(None)` at `t = 0` if no player holds Doppelgänger!)
    //   2. `board_1`: Board state after Doppelgänger finishes Step 1a + any Step 1b swap
    //      (Immediately `Some(initial_board)` at `t = 0` if no player holds Doppelgänger!)
    //   3. `board_6`: Board state after Robber swaps (`oneshot` -> Troublemaker)
    //   4. `board_7`: Board state after Troublemaker swaps (`oneshot` -> Drunk -> Insomniacs)
    // ------------------------------------------------------------------------
    let doppel_seat = roster.initial_holders(Role::Doppelganger).first().copied();

    let (doppel_1a_tx, doppel_1a_rx) = watch::channel::<Option<Option<(usize, Role)>>>(
        if doppel_seat.is_none() {
            Some(None)
        } else {
            None
        },
    );
    let (board1_tx, board1_rx) = watch::channel::<Option<BoardCards>>(
        if doppel_seat.is_none() {
            Some(initial_board.clone())
        } else {
            None
        },
    );
    let (board6_tx, board6_rx) = oneshot::channel::<BoardCards>();
    let (board7_tx, board7_rx) = oneshot::channel::<BoardCards>();

    // Branch 1: Doppelgänger (Step 1a -> publishes `doppel_1a` -> Step 1b -> publishes `board_1`)
    let doppel_fut = run_doppel_dag_branch(
        roster,
        initial_board,
        doppel_seat,
        &stage1_cfg,
        stage1_deadline,
        doppel_1a_tx,
        board1_tx,
    );

    // Branch 2: Pack & Info Roles (Werewolves / Lone Wolf + Minion + Masons)
    // Waits ONLY on `doppel_1a` (which is already ready at `t = 0` if no player is Doppelgänger!),
    // and if Lone Wolf peeks a center card, awaits `board_1` before revealing that center card.
    let pack_fut = run_pack_and_info_dag_branch(
        roster,
        &stage1_cfg,
        stage1_deadline,
        doppel_1a_rx.clone(),
        board1_rx.clone(),
    );

    // Branch 3: Seer — prompts at `t = 0`, awaits `board_1` ONLY to read the peeked card(s)!
    let seer_fut = run_seer_dag_branch(roster, &stage1_cfg, board1_rx.clone());

    // Branch 4: Robber — prompts at `t = 0`, awaits `board_1`, swaps, reveals stolen card IMMEDIATELY,
    // and forwards `board_6` to Troublemaker!
    let robber_fut = run_robber_dag_branch(roster, &stage1_cfg, board1_rx, board6_tx);

    // Branch 5: Troublemaker — prompts at `t = 0`, awaits `board_6`, swaps, forwards `board_7` to Drunk!
    let tm_fut = run_troublemaker_dag_branch(roster, &stage1_cfg, board6_rx, board7_tx);

    // Branch 6: Drunk + Insomniacs — prompts Drunk at `t = 0`, awaits `board_7` (+ `doppel_1a` for 9a),
    // applies mandatory center swap -> `board_8`, and immediately notifies Insomniac & Doppel-Insomniac!
    let drunk_and_insomniac_fut =
        run_drunk_and_insomniac_dag_branch(roster, &stage1_cfg, board7_rx, doppel_1a_rx);

    let (doppel_logs, pack_logs, seer_logs, robber_logs, tm_logs, (final_board, drunk_ins_logs)) =
        tokio::try_join!(
            doppel_fut,
            pack_fut,
            seer_fut,
            robber_fut,
            tm_fut,
            drunk_and_insomniac_fut,
        )?;

    // Pad total Night duration to a randomized 15–20s minimum floor so a deal where all interactive
    // cards landed in the center doesn't jump to Dawn in 5 milliseconds!
    let min_floor = Duration::from_millis(fastrand::u64(15_000..=20_000));
    if let Some(rem) = min_floor.checked_sub(night_start.elapsed()) {
        tokio::time::sleep(rem).await;
    }

    // Assemble Night Recap in canonical rule wake order (Steps 1 -> 2 -> 5 -> 6 -> 7 -> 8 -> 9 -> 9a):
    let mut ordered_log = Vec::new();
    ordered_log.extend(doppel_logs);
    ordered_log.extend(pack_logs);
    ordered_log.extend(seer_logs);
    ordered_log.extend(robber_logs);
    ordered_log.extend(tm_logs);
    ordered_log.extend(drunk_ins_logs);

    Ok((final_board, ordered_log))
}

// ----------------------------------------------------------------------------
// DAG Branch 1: Doppelgänger (Step 1a -> `doppel_1a_tx` -> Step 1b -> `board1_tx`)
// ----------------------------------------------------------------------------

async fn run_doppel_dag_branch(
    roster: &Roster,
    mut board: BoardCards,
    doppel_seat: Option<usize>,
    stage1_cfg: &AskConfig,
    stage1_deadline: Instant,
    doppel_1a_tx: watch::Sender<Option<Option<(usize, Role)>>>,
    board1_tx: watch::Sender<Option<BoardCards>>,
) -> Result<Vec<String>> {
    let Some(d_idx) = doppel_seat else {
        // Already initialized both `watch` channels to `Some(...)` at construction!
        return Ok(Vec::new());
    };

    let mut logs = Vec::new();
    let doppel_user = roster.seats[d_idx].user.clone();
    let seats_str = roster.format_seats();

    let pick_random_other = || {
        let others: Vec<usize> = (0..roster.len()).filter(|&i| i != d_idx).collect();
        others[fastrand::usize(..others.len())]
    };

    // [API NOTE #12 — SUBTLE `ExclusiveDm` DUAL-TIMER BUG FOUND BY THIS DAG!]:
    // Look closely at what happens below between Step 1a (`stage1_deadline`) and Step 1b:
    // If Doppelgänger takes 38s of the 40s `stage1_deadline` on Step 1a, we want to give her
    // at least `NIGHT_STAGE2_MIN_BUDGET` (25s) on Step 1b via `.reply_deadline(step1b_deadline)`.
    // HOWEVER, in `DESIGN.md` §4.6 (lines 809–837), `dm.with_ask_config(stage1_cfg)` ALSO sets
    // `dm.inbox`'s internal `read_limit` to `stage1_deadline`! If `ExclusiveDm::ask` polls
    // `dm.inbox.wait_map(...)` without overriding `dm.inbox`'s `read_limit`, `dm.inbox` will
    // prematurely time out in 2s instead of honoring `.reply_deadline(step1b_deadline)`!
    // (`ExclusiveDm::ask` MUST temporarily suspend `self.inbox`'s `read_limit` and rely solely
    // on `Ask`'s own resolved deadline.)
    let Ok(mut dm) = doppel_user.exclusive().await else {
        let target_idx = pick_random_other();
        let copied = board.players[target_idx].effective_role();
        board.players[d_idx] = Card::Doppelganger {
            copied: Some(copied),
        };
        let _ = doppel_1a_tx.send(Some(Some((d_idx, copied))));
        if copied == Role::Drunk {
            let c = fastrand::usize(0..3);
            std::mem::swap(&mut board.players[d_idx], &mut board.center[c]);
        }
        let _ = board1_tx.send(Some(board));
        return Ok(logs);
    };

    dm = dm.with_ask_config(stage1_cfg).with_ignore_commands();

    let step1a = dm
        .ask(
            format!(
                "🎭 Doppelgänger: Pick ONE other player (`<seat|nick>`) to copy! Seats: {seats_str}"
            ),
            |m| match roster.resolve_token(&m.text) {
                Some(idx) if idx == d_idx => {
                    Match::Reject(Text::from("Pick ANOTHER player, not yourself!".yellow()))
                }
                Some(idx) => Match::Accept(idx),
                None => Match::Reject(Text::from("Unknown seat or nickname.".yellow())),
            },
        )
        .warn_at(Duration::from_secs(10), "⏳ 10s left to pick a player to copy!")
        .settled()
        .await?;

    let target_idx = match step1a {
        Turn::Reply(idx) => idx,
        Turn::TimedOut | Turn::Left { .. } => {
            let auto = pick_random_other();
            let _ = dm.say(format!(
                "🎲 Randomly looked at [{}] {}.",
                auto + 1,
                roster.seats[auto].user.nick()
            ));
            auto
        }
    };

    let copied_role = board.players[target_idx].effective_role();
    board.players[d_idx] = Card::Doppelganger {
        copied: Some(copied_role),
    };

    // SEQUENCE EDGE 1a: Immediately unblock Werewolves / Lone Wolf, Minion, and Masons
    // BEFORE waiting on Doppelgänger's Step 1b follow-up action!
    let _ = doppel_1a_tx.send(Some(Some((d_idx, copied_role))));

    logs.push(format!(
        "Doppelgänger ({}) copied {} ({}).",
        doppel_user.nick(),
        roster.seats[target_idx].user.nick(),
        copied_role.name()
    ));
    let _ = dm.say(
        format!(
            "🎭 [{}] {}'s card is ",
            target_idx + 1,
            roster.seats[target_idx].user.nick()
        ) + copied_role.styled()
            + "! You are now that role.",
    );

    // Guarantee at least 25s for Step 1b even if Step 1a used most of `stage1_deadline`:
    let step1b_deadline = stage1_deadline.max(Instant::now() + NIGHT_STAGE2_MIN_BUDGET);

    match copied_role {
        Role::Seer => {
            let choice = dm
                .ask(
                    format!(
                        "🔮 Doppelgänger-Seer: View `player <seat|nick>` OR `center <1-3> <1-3>`, or `pass`. Seats: {seats_str}"
                    ),
                    |m| parse_seer_choice(roster, d_idx, &m.text),
                )
                .reply_deadline(step1b_deadline)
                .settled()
                .await?;
            reveal_seer_from_board(&board, roster, &doppel_user, "Doppelgänger-Seer", choice, &mut logs);
        }
        Role::Robber => {
            let choice = dm
                .ask(
                    format!(
                        "🦹 Doppelgänger-Robber: Pick a player (`<seat|nick>`) to rob, or `pass`. Seats: {seats_str}"
                    ),
                    |m| parse_other_or_pass(roster, d_idx, &m.text),
                )
                .reply_deadline(step1b_deadline)
                .settled()
                .await?;
            apply_robber_on_board(&mut board, roster, d_idx, &doppel_user, "Doppelgänger-Robber", choice, &mut logs);
        }
        Role::Troublemaker => {
            let choice = dm
                .ask(
                    format!(
                        "😈 Doppelgänger-Troublemaker: Pick TWO other players (`<a> <b>`) to swap, or `pass`. Seats: {seats_str}"
                    ),
                    |m| parse_troublemaker_choice(roster, d_idx, &m.text),
                )
                .reply_deadline(step1b_deadline)
                .settled()
                .await?;
            apply_troublemaker_on_board(&mut board, roster, &doppel_user, "Doppelgänger-Troublemaker", choice, &mut logs);
        }
        Role::Drunk => {
            let choice = dm
                .ask(
                    "🍺 Doppelgänger-Drunk: Swap your card with center `1`, `2`, or `3`!",
                    parse_drunk_slot,
                )
                .reply_deadline(step1b_deadline)
                .settled()
                .await?;
            apply_drunk_on_board(&mut board, d_idx, &doppel_user, "Doppelgänger-Drunk", choice, &mut logs);
        }
        Role::Werewolf | Role::Minion | Role::Mason | Role::Insomniac => {
            let _ = dm.say("🌙 Stand by—your copied role's night info will arrive shortly!");
        }
        Role::Villager | Role::Tanner | Role::Hunter | Role::Doppelganger => {
            let _ = dm.say("🌙 You have no further night actions.");
        }
    }

    // SEQUENCE EDGE 1b: Publish `board_1` -> immediately unblocks Lone Wolf peek, Seer vision, & Robber swap!
    let _ = board1_tx.send(Some(board));
    Ok(logs)
}

// ----------------------------------------------------------------------------
// DAG Branch 2: Werewolves (Pack or Lone Wolf), Minion, & Masons
// ----------------------------------------------------------------------------

async fn run_pack_and_info_dag_branch(
    roster: &Roster,
    stage1_cfg: &AskConfig,
    stage1_deadline: Instant,
    mut doppel_1a_rx: watch::Receiver<Option<Option<(usize, Role)>>>,
    mut board1_rx: watch::Receiver<Option<BoardCards>>,
) -> Result<Vec<String>> {
    let mut logs = Vec::new();

    // Wait ONLY for Doppelgänger Step 1a (already resolved at `t = 0` if no player holds Doppelgänger!):
    let doppel_1a = wait_ready(&mut doppel_1a_rx).await;

    let mut wolves = roster.initial_holders(Role::Werewolf);
    if let Some((d_idx, Role::Werewolf)) = doppel_1a {
        wolves.push(d_idx);
    }

    // Minion info DM (instant!):
    let mut minions = roster.initial_holders(Role::Minion);
    if let Some((d_idx, Role::Minion)) = doppel_1a {
        minions.push(d_idx);
    }
    for m_idx in minions {
        if wolves.is_empty() {
            let _ = roster.seats[m_idx].user.say(
                "🗡️ Minion: "
                    + "NO players are Werewolves".bold().yellow()
                    + " (all Werewolf cards are in the center)!",
            );
        } else {
            let list: Vec<String> = wolves
                .iter()
                .map(|&w| format!("[{}] {}", w + 1, roster.seats[w].user.nick()))
                .collect();
            let _ = roster.seats[m_idx].user.say(
                "🗡️ Minion: The Werewolves are " + list.join(", ").bold().red() + ".",
            );
        }
    }

    // Masons info DM (instant!):
    let mut masons = roster.initial_holders(Role::Mason);
    if let Some((d_idx, Role::Mason)) = doppel_1a {
        masons.push(d_idx);
    }
    if masons.len() == 1 {
        let _ = roster.seats[masons[0]].user.say(
            "🧱 Mason: You are the " + "ONLY Mason".bold().light_cyan() + " (the other is in the center!).",
        );
    } else if masons.len() >= 2 {
        for &m_idx in &masons {
            let others: Vec<String> = masons
                .iter()
                .filter(|&&o| o != m_idx)
                .map(|&o| format!("[{}] {}", o + 1, roster.seats[o].user.nick()))
                .collect();
            let _ = roster.seats[m_idx]
                .user
                .say("🧱 Fellow Mason(s): " + others.join(", ").bold().light_cyan());
        }
    }

    // Werewolves: either 2+ packmates (instant!) OR Lone Wolf center peek:
    if wolves.len() >= 2 {
        for &w_idx in &wolves {
            let others: Vec<String> = wolves
                .iter()
                .filter(|&&o| o != w_idx)
                .map(|&o| format!("[{}] {}", o + 1, roster.seats[o].user.nick()))
                .collect();
            let _ = roster.seats[w_idx]
                .user
                .say("🐺 Fellow Werewolf pack: " + others.join(", ").bold().red());
        }
        let names: Vec<String> = wolves
            .iter()
            .map(|&w| roster.seats[w].user.nick())
            .collect();
        logs.push(format!("Werewolves ({}) saw each other.", names.join(", ")));
    } else if wolves.len() == 1 {
        let w_idx = wolves[0];
        let wolf_user = &roster.seats[w_idx].user;
        let deadline = stage1_deadline.max(Instant::now() + NIGHT_STAGE2_MIN_BUDGET);

        let choice = wolf_user
            .ask(
                "🐺 You are the "
                    + "LONE WOLF".bold().red()
                    + "! Pick ONE center card to peek (`1`, `2`, `3`, or `pass`):",
                |m| match m.text.trim().to_ascii_lowercase().as_str() {
                    "1" | "c1" => Match::Accept(Some(0usize)),
                    "2" | "c2" => Match::Accept(Some(1usize)),
                    "3" | "c3" => Match::Accept(Some(2usize)),
                    "pass" | "skip" | "none" => Match::Accept(None),
                    _ => Match::Reject(Text::from("Reply `1`, `2`, `3`, or `pass`.".yellow())),
                },
            )
            .with_config(stage1_cfg)
            .reply_deadline(deadline)
            .warn_at(Duration::from_secs(10), "⏳ 10s left to peek a center card!")
            .settled()
            .await?;

        if let Turn::Reply(Some(c_idx)) = choice {
            if board1_rx.borrow().is_none() {
                let _ = wolf_user.say("✅ Choice locked in—waiting for Doppelgänger to finish...");
            }
            // Wait for `board_1` in case Doppelgänger-Drunk swapped with a center card in Step 1b!
            let board1 = wait_ready(&mut board1_rx).await;
            let seen = board1.center[c_idx];
            let _ = wolf_user.say(
                format!("🐺 Center #{} is: ", c_idx + 1) + seen.display_for_peek(),
            );
            logs.push(format!(
                "Lone Wolf ({}) peeked Center #{} ({}).",
                wolf_user.nick(),
                c_idx + 1,
                seen.effective_role().name()
            ));
        } else {
            logs.push(format!("Lone Wolf ({}) did not peek center.", wolf_user.nick()));
        }
    }

    Ok(logs)
}

// ----------------------------------------------------------------------------
// DAG Branch 3: Seer (Prompts at `t = 0`, reads `board_1`)
// ----------------------------------------------------------------------------

async fn run_seer_dag_branch(
    roster: &Roster,
    stage1_cfg: &AskConfig,
    mut board1_rx: watch::Receiver<Option<BoardCards>>,
) -> Result<Vec<String>> {
    let mut logs = Vec::new();
    let Some(&s_idx) = roster.initial_holders(Role::Seer).first() else {
        return Ok(logs);
    };

    let seer_user = &roster.seats[s_idx].user;
    let choice = seer_user
        .ask(
            format!(
                "🔮 Seer: Reply `player <seat|nick>` OR `center <1-3> <1-3>`, or `pass`. Seats: {}",
                roster.format_seats()
            ),
            |m| parse_seer_choice(roster, s_idx, &m.text),
        )
        .with_config(stage1_cfg)
        .warn_at(Duration::from_secs(10), "⏳ 10s left for Seer vision!")
        .settled()
        .await?;

    if matches!(choice, Turn::Reply(SeerChoice::Player(_) | SeerChoice::Center(_, _)))
        && board1_rx.borrow().is_none()
    {
        let _ = seer_user.say("✅ Choice locked in—resolving earlier night actions...");
    }

    // Notice: Seer does NOT wait for Robber, Troublemaker, or Drunk!
    // As soon as `board_1` is ready, Seer immediately receives her vision!
    let board1 = wait_ready(&mut board1_rx).await;
    reveal_seer_from_board(&board1, roster, seer_user, "Seer", choice, &mut logs);
    Ok(logs)
}

// ----------------------------------------------------------------------------
// DAG Branch 4: Robber (Prompts at `t = 0`, mutates `board_1 -> board_6`, reveals immediately!)
// ----------------------------------------------------------------------------

async fn run_robber_dag_branch(
    roster: &Roster,
    stage1_cfg: &AskConfig,
    mut board1_rx: watch::Receiver<Option<BoardCards>>,
    board6_tx: oneshot::Sender<BoardCards>,
) -> Result<Vec<String>> {
    let mut logs = Vec::new();
    let Some(&r_idx) = roster.initial_holders(Role::Robber).first() else {
        let board1 = wait_ready(&mut board1_rx).await;
        let _ = board6_tx.send(board1);
        return Ok(logs);
    };

    let robber_user = &roster.seats[r_idx].user;
    let choice = robber_user
        .ask(
            format!(
                "🦹 Robber: Reply with `<seat|nick>` to steal their card, or `pass`. Seats: {}",
                roster.format_seats()
            ),
            |m| parse_other_or_pass(roster, r_idx, &m.text),
        )
        .with_config(stage1_cfg)
        .warn_at(Duration::from_secs(10), "⏳ 10s left for Robber swap!")
        .settled()
        .await?;

    if matches!(choice, Turn::Reply(Some(_))) && board1_rx.borrow().is_none() {
        let _ = robber_user.say("✅ Target locked in—waiting for Doppelgänger to finish...");
    }

    // Notice: Robber does NOT wait on Seer, Troublemaker, or Drunk!
    let mut board = wait_ready(&mut board1_rx).await;
    apply_robber_on_board(&mut board, roster, r_idx, robber_user, "Robber", choice, &mut logs);
    let _ = board6_tx.send(board);
    Ok(logs)
}

// ----------------------------------------------------------------------------
// DAG Branch 5: Troublemaker (Prompts at `t = 0`, mutates `board_6 -> board_7`)
// ----------------------------------------------------------------------------

async fn run_troublemaker_dag_branch(
    roster: &Roster,
    stage1_cfg: &AskConfig,
    board6_rx: oneshot::Receiver<BoardCards>,
    board7_tx: oneshot::Sender<BoardCards>,
) -> Result<Vec<String>> {
    let mut logs = Vec::new();
    let choice = if let Some(&tm_idx) = roster.initial_holders(Role::Troublemaker).first() {
        let tm_user = &roster.seats[tm_idx].user;
        let turn = tm_user
            .ask(
                format!(
                    "😈 Troublemaker: Reply with TWO other players (`<a> <b>`) to swap, or `pass`. Seats: {}",
                    roster.format_seats()
                ),
                |m| parse_troublemaker_choice(roster, tm_idx, &m.text),
            )
            .with_config(stage1_cfg)
            .warn_at(Duration::from_secs(10), "⏳ 10s left for Troublemaker swap!")
            .settled()
            .await?;

        // Troublemaker learns zero card info, so we can confirm her choice IMMEDIATELY at reply time!
        if let Turn::Reply(Some((a, b))) = turn {
            let _ = tm_user.say(
                "😈 Locked in swap between "
                    + roster.seats[a].user.nick().bold()
                    + " and "
                    + roster.seats[b].user.nick().bold()
                    + "!",
            );
        }
        Some((tm_user, turn))
    } else {
        None
    };

    let Ok(mut board) = board6_rx.await else {
        return Ok(logs);
    };
    if let Some((tm_user, turn)) = choice {
        apply_troublemaker_quiet(&mut board, roster, tm_user, "Troublemaker", turn, &mut logs);
    }
    let _ = board7_tx.send(board);
    Ok(logs)
}

// ----------------------------------------------------------------------------
// DAG Branch 6: Drunk (Prompts at `t = 0`, mutates `board_7 -> board_8`) + Insomniacs
// ----------------------------------------------------------------------------

async fn run_drunk_and_insomniac_dag_branch(
    roster: &Roster,
    stage1_cfg: &AskConfig,
    board7_rx: oneshot::Receiver<BoardCards>,
    mut doppel_1a_rx: watch::Receiver<Option<Option<(usize, Role)>>>,
) -> Result<(BoardCards, Vec<String>)> {
    let mut logs = Vec::new();
    let drunk_turn = if let Some(&dr_idx) = roster.initial_holders(Role::Drunk).first() {
        let drunk_user = &roster.seats[dr_idx].user;
        let turn = drunk_user
            .ask(
                "🍺 Drunk: You MUST swap your card with center `1`, `2`, or `3`!",
                parse_drunk_slot,
            )
            .with_config(stage1_cfg)
            .warn_at(Duration::from_secs(10), "⏳ 10s left to pick center `1`, `2`, or `3`!")
            .settled()
            .await?;

        // Drunk also learns zero card info, so confirm immediately on reply!
        if let Turn::Reply(c_idx) = turn {
            let _ = drunk_user.say(format!(
                "🍺 Locked in face-down swap with Center #{}!",
                c_idx + 1
            ));
        }
        Some((dr_idx, drunk_user, turn))
    } else {
        None
    };

    let mut board = board7_rx
        .await
        .expect("upstream `board7_tx` only drops if `try_join!` cancels on error");

    if let Some((dr_idx, drunk_user, turn)) = drunk_turn {
        apply_drunk_quiet(&mut board, dr_idx, drunk_user, "Drunk", turn, &mut logs);
    }

    // Step 9 & 9a: `board` is now `board_8` (final Dawn state!) — notify Insomniac & Doppel-Insomniac:
    for ins_idx in roster.initial_holders(Role::Insomniac) {
        let seen = board.players[ins_idx];
        let _ = roster.seats[ins_idx].user.say(
            "🦉 Insomniac: You check your card before dawn — it is " + seen.display_for_peek() + "!",
        );
        logs.push(format!(
            "Insomniac ({}) checked their card ({}).",
            roster.seats[ins_idx].user.nick(),
            seen.effective_role().name()
        ));
    }

    if let Some((d_idx, Role::Insomniac)) = wait_ready(&mut doppel_1a_rx).await {
        let seen = board.players[d_idx];
        let _ = roster.seats[d_idx].user.say(
            "🦉 Doppelgänger-Insomniac: You check your card before dawn — it is "
                + seen.display_for_peek()
                + "!",
        );
        logs.push(format!(
            "Doppelgänger-Insomniac ({}) checked their card ({}).",
            roster.seats[d_idx].user.nick(),
            seen.effective_role().name()
        ));
    }

    Ok((board, logs))
}

// ============================================================================
// 5a. Pure Synchronous Parsers & Board Reducers
// ============================================================================

fn parse_seer_choice(roster: &Roster, actor_idx: usize, raw: &str) -> Match<SeerChoice> {
    let trimmed = raw.trim();
    if matches!(trimmed.to_ascii_lowercase().as_str(), "pass" | "skip" | "none") {
        return Match::Accept(SeerChoice::Pass);
    }
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.first().is_some_and(|w| w.eq_ignore_ascii_case("center") || w.eq_ignore_ascii_case("c")) {
        if parts.len() != 3 {
            return Match::Reject(Text::from("Usage: `center <1-3> <1-3>`.".yellow()));
        }
        let (Ok(a), Ok(b)) = (parts[1].parse::<usize>(), parts[2].parse::<usize>()) else {
            return Match::Reject(Text::from("Center slots must be `1`, `2`, or `3`.".yellow()));
        };
        if !(1..=3).contains(&a) || !(1..=3).contains(&b) || a == b {
            return Match::Reject(Text::from("Pick two DIFFERENT center slots (1..=3).".yellow()));
        }
        return Match::Accept(SeerChoice::Center(a - 1, b - 1));
    }

    let token = if parts.first().is_some_and(|w| w.eq_ignore_ascii_case("player") || w.eq_ignore_ascii_case("p")) {
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

    match roster.resolve_token(token) {
        Some(idx) if idx == actor_idx => {
            Match::Reject(Text::from("You cannot look at your own card!".yellow()))
        }
        Some(idx) => Match::Accept(SeerChoice::Player(idx)),
        None => Match::Reject(Text::from("Unknown player.".yellow())),
    }
}

fn reveal_seer_from_board(
    board: &BoardCards,
    roster: &Roster,
    actor: &User,
    label: &str,
    turn: Turn<SeerChoice>,
    logs: &mut Vec<String>,
) {
    match turn {
        Turn::Reply(SeerChoice::Player(idx)) => {
            let card = board.players[idx];
            let target_nick = roster.seats[idx].user.nick();
            let _ = actor.say(
                format!("🔮 [{}] {}'s card is: ", idx + 1, target_nick) + card.display_for_peek(),
            );
            logs.push(format!(
                "{label} ({}) viewed {} ({}).",
                actor.nick(),
                target_nick,
                card.effective_role().name()
            ));
        }
        Turn::Reply(SeerChoice::Center(a, b)) => {
            let (ca, cb) = (board.center[a], board.center[b]);
            let _ = actor.say(
                format!("🔮 Center #{} is ", a + 1)
                    + ca.display_for_peek()
                    + format!(", Center #{} is ", b + 1)
                    + cb.display_for_peek()
                    + ".",
            );
            logs.push(format!(
                "{label} ({}) viewed Center #{}/#{}.",
                actor.nick(),
                a + 1,
                b + 1
            ));
        }
        _ => logs.push(format!("{label} ({}) did not view any cards.", actor.nick())),
    }
}

fn parse_other_or_pass(roster: &Roster, actor_idx: usize, raw: &str) -> Match<Option<usize>> {
    let t = raw.trim();
    if matches!(t.to_ascii_lowercase().as_str(), "pass" | "skip" | "none") {
        return Match::Accept(None);
    }
    match roster.resolve_token(t) {
        Some(idx) if idx == actor_idx => {
            Match::Reject(Text::from("Pick ANOTHER player (or `pass`), not yourself!".yellow()))
        }
        Some(idx) => Match::Accept(Some(idx)),
        None => Match::Reject(Text::from("Unknown player (or `pass`).".yellow())),
    }
}

fn apply_robber_on_board(
    board: &mut BoardCards,
    roster: &Roster,
    actor_idx: usize,
    actor: &User,
    label: &str,
    turn: Turn<Option<usize>>,
    logs: &mut Vec<String>,
) {
    if let Turn::Reply(Some(target_idx)) = turn {
        board.players.swap(actor_idx, target_idx);
        let stolen = board.players[actor_idx];
        let victim_nick = roster.seats[target_idx].user.nick();
        let _ = actor.say(
            format!("🦹 You robbed [{}] {} and got: ", target_idx + 1, victim_nick)
                + stolen.display_for_peek()
                + "!",
        );
        logs.push(format!(
            "{label} ({}) robbed {} (got {}).",
            actor.nick(),
            victim_nick,
            stolen.effective_role().name()
        ));
    } else {
        logs.push(format!("{label} ({}) did not rob anyone.", actor.nick()));
    }
}

fn parse_troublemaker_choice(
    roster: &Roster,
    actor_idx: usize,
    raw: &str,
) -> Match<Option<(usize, usize)>> {
    let t = raw.trim();
    if matches!(t.to_ascii_lowercase().as_str(), "pass" | "skip" | "none") {
        return Match::Accept(None);
    }
    let parts: Vec<&str> = t.split_whitespace().collect();
    if parts.len() != 2 {
        return Match::Reject(Text::from("Usage: `<seat|nick> <seat|nick>` or `pass`.".yellow()));
    }
    let Some(a) = roster.resolve_token(parts[0]) else {
        return Match::Reject(Text::from(format!("Unknown player `{}`.", parts[0]).yellow()));
    };
    let Some(b) = roster.resolve_token(parts[1]) else {
        return Match::Reject(Text::from(format!("Unknown player `{}`.", parts[1]).yellow()));
    };
    if a == actor_idx || b == actor_idx || a == b {
        return Match::Reject(Text::from("Pick TWO DIFFERENT players other than yourself!".yellow()));
    }
    Match::Accept(Some((a, b)))
}

fn apply_troublemaker_on_board(
    board: &mut BoardCards,
    roster: &Roster,
    actor: &User,
    label: &str,
    turn: Turn<Option<(usize, usize)>>,
    logs: &mut Vec<String>,
) {
    if let Turn::Reply(Some((a, b))) = turn {
        let _ = actor.say(
            "😈 Swapped "
                + roster.seats[a].user.nick().bold()
                + " and "
                + roster.seats[b].user.nick().bold()
                + "!",
        );
    }
    apply_troublemaker_quiet(board, roster, actor, label, turn, logs);
}

fn apply_troublemaker_quiet(
    board: &mut BoardCards,
    roster: &Roster,
    actor: &User,
    label: &str,
    turn: Turn<Option<(usize, usize)>>,
    logs: &mut Vec<String>,
) {
    if let Turn::Reply(Some((a, b))) = turn {
        board.players.swap(a, b);
        logs.push(format!(
            "{label} ({}) swapped {} and {}.",
            actor.nick(),
            roster.seats[a].user.nick(),
            roster.seats[b].user.nick()
        ));
    } else {
        logs.push(format!("{label} ({}) did not swap anyone.", actor.nick()));
    }
}

fn parse_drunk_slot(m: &ChatMessage) -> Match<usize> {
    match m.text.trim().to_ascii_lowercase().as_str() {
        "1" | "c1" | "center 1" => Match::Accept(0),
        "2" | "c2" | "center 2" => Match::Accept(1),
        "3" | "c3" | "center 3" => Match::Accept(2),
        _ => Match::Reject(Text::from("Drunk MUST swap! Reply `1`, `2`, or `3`.".yellow())),
    }
}

fn apply_drunk_on_board(
    board: &mut BoardCards,
    actor_idx: usize,
    actor: &User,
    label: &str,
    turn: Turn<usize>,
    logs: &mut Vec<String>,
) {
    if let Turn::Reply(c_idx) = turn {
        let _ = actor.say(format!("🍺 Swapped face-down with Center #{}!", c_idx + 1));
    }
    apply_drunk_quiet(board, actor_idx, actor, label, turn, logs);
}

fn apply_drunk_quiet(
    board: &mut BoardCards,
    actor_idx: usize,
    actor: &User,
    label: &str,
    turn: Turn<usize>,
    logs: &mut Vec<String>,
) {
    let c_idx = match turn {
        Turn::Reply(c) => c,
        Turn::TimedOut | Turn::Left { .. } => {
            let auto = fastrand::usize(0..3);
            let _ = actor.say(format!("🍺 Randomly swapped your card with Center #{}.", auto + 1));
            auto
        }
    };
    std::mem::swap(&mut board.players[actor_idx], &mut board.center[c_idx]);
    logs.push(format!("{label} ({}) swapped with Center #{}.", actor.nick(), c_idx + 1));
}

// ============================================================================
// 6. Phases 3, 4, & 5: Day Discussion, Simultaneous Voting, & Win Resolution
// ============================================================================

async fn run_day_phase(room: &Room, roster: &Roster) -> Result<()> {
    let day_mins = (roster.len() + 2).clamp(5, 8) as u64;
    let deadline = Instant::now() + Duration::from_secs(day_mins * 60);

    room.say(
        "☀️ EVERYONE WAKE UP! ".bold().yellow()
            + format!("You have {day_mins} minutes to debate! Type ")
            + "!ready".bold().green()
            + " to vote early or "
            + "!time".bold()
            + ".",
    )?;

    let mut inbox = room.inbox();
    let enrolled: HashSet<User> = roster.seats.iter().map(|s| s.user.clone()).collect();
    let mut ready_voters: HashSet<User> = HashSet::new();
    let mut milestones: Vec<(Duration, &'static str)> = vec![
        (Duration::from_secs(120), "⏳ 2 minutes remaining!"),
        (Duration::from_secs(60), "⏳ 1 minute remaining!"),
        (Duration::from_secs(15), "⏳ 15 seconds remaining!"),
    ];

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
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
                        room.say(format!("⏳ {:02}:{:02} left.", secs / 60, secs % 60))?;
                    }
                    RoomEvent::Chat(msg) if msg.is_cmd("!ready") || msg.is_cmd("!votenow") => {
                        if enrolled.contains(&msg.sender) && ready_voters.insert(msg.sender.clone()) {
                            let active = enrolled.iter().filter(|u| is_active_in_room(room, u)).count();
                            room.say(msg.sender.nick().bold().green() + format!(" is ready ({}/{active})", ready_voters.len()))?;
                            if active > 0 && ready_voters.len() >= active {
                                break;
                            }
                        }
                    }
                    RoomEvent::Parted { user, .. }
                    | RoomEvent::Kicked { user, .. }
                    | RoomEvent::Quit { user, .. } => {
                        if enrolled.contains(&user) {
                            ready_voters.remove(&user);
                            let active = enrolled.iter().filter(|u| is_active_in_room(room, u)).count();
                            if active > 0 && ready_voters.len() >= active {
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

async fn run_voting_phase(room: &Room, roster: &Roster) -> Result<Vec<Option<usize>>> {
    let deadline = Instant::now() + VOTE_TIMEOUT;
    let seats_str = roster.format_seats();

    room.say(
        "🗳️ 3... 2... 1... POINT! ".bold().red()
            + "Check your DMs and vote for ANOTHER player to kill (45s)!",
    )?;

    let vote_cfg = AskConfig::new()
        .reply_deadline(deadline)
        .ignore_commands()
        .on_timeout("⏳ Time's up! You abstained.".yellow());

    try_join_all((0..roster.len()).map(|voter_idx| {
        let voter = &roster.seats[voter_idx].user;
        let prompt = format!("👉 Point at ANOTHER player (`<seat|nick>`). Seats: {seats_str}");
        let cfg_ref = &vote_cfg;
        async move {
            let outcome = voter
                .ask(prompt, move |m| {
                    let raw = m.cmd_args("!vote").unwrap_or(&m.text);
                    match roster.resolve_token(raw) {
                        Some(idx) if idx == voter_idx => {
                            Match::Reject(Text::from("You cannot point at yourself!".yellow()))
                        }
                        Some(idx) => Match::Accept(idx),
                        None => Match::Reject(Text::from("Unknown seat or nickname.".yellow())),
                    }
                })
                .with_config(cfg_ref)
                .warn_at(Duration::from_secs(10), "⏳ 10s left to point!")
                .settled()
                .await?;

            if let Turn::Reply(target_idx) = outcome {
                let _ = voter.say(
                    "✅ Pointing at "
                        + format!("[{}] {}", target_idx + 1, roster.seats[target_idx].user.nick())
                            .bold(),
                );
            }
            Ok::<_, Error>(outcome.into_reply())
        }
    }))
    .await
}

fn resolve_and_announce_winners(
    room: &Room,
    roster: &Roster,
    board: &BoardCards,
    night_log: &[String],
    votes: &[Option<usize>],
) -> Result<()> {
    let n = roster.len();
    let mut tally = vec![0usize; n];
    for &opt in votes {
        if let Some(t) = opt {
            tally[t] += 1;
        }
    }

    let summary: Vec<String> = (0..n)
        .map(|i| match votes[i] {
            Some(t) => format!("{} → {}", roster.seats[i].user.nick(), roster.seats[t].user.nick()),
            None => format!("{} → (abstained)", roster.seats[i].user.nick()),
        })
        .collect();
    room.say("👉 Votes: ".bold() + summary.join(" | "))?;

    let max_votes = tally.iter().copied().max().unwrap_or(0);
    let mut dead: HashSet<usize> = HashSet::new();
    if max_votes >= 2 {
        for (idx, &c) in tally.iter().enumerate() {
            if c == max_votes {
                dead.insert(idx);
            }
        }
    }

    // Hunter / Doppelgänger-Hunter fixpoint chain:
    let mut hunter_shots = Vec::new();
    loop {
        let mut newly_shot = Vec::new();
        for &d in &dead {
            if board.players[d].effective_role() == Role::Hunter {
                if let Some(target) = votes[d] {
                    if !dead.contains(&target) {
                        newly_shot.push((d, target));
                    }
                }
            }
        }
        if newly_shot.is_empty() {
            break;
        }
        for (h, t) in newly_shot {
            dead.insert(t);
            hunter_shots.push((h, t));
        }
    }

    room.say(
        "🌅 DAWN REVEAL — Center: ".bold()
            + "#1 "
            + board.center[0].display_reveal()
            + " | #2 "
            + board.center[1].display_reveal()
            + " | #3 "
            + board.center[2].display_reveal(),
    )?;

    for idx in 0..n {
        let status = if dead.contains(&idx) {
            "💀 KILLED".bold().red()
        } else {
            "💚 SURVIVED".green()
        };
        room.say(
            format!("[{}] ", idx + 1)
                + roster.seats[idx].user.nick().bold()
                + ": started "
                + roster.seats[idx].initial_role.styled()
                + " → ended "
                + board.players[idx].display_reveal()
                + format!(" ({} vote{}) — ", tally[idx], if tally[idx] == 1 { "" } else { "s" })
                + status,
        )?;
    }

    for (h, t) in &hunter_shots {
        room.say(
            "🏹 HUNTER SHOT! ".bold().orange()
                + roster.seats[*h].user.nick().bold()
                + " took down "
                + roster.seats[*t].user.nick().bold().red()
                + "!",
        )?;
    }

    if !night_log.is_empty() {
        room.say("📜 Night Recap: ".bold() + night_log.join(" -> "))?;
    }

    let wolves: Vec<usize> = (0..n)
        .filter(|&i| board.players[i].effective_role() == Role::Werewolf)
        .collect();
    let minions: Vec<usize> = (0..n)
        .filter(|&i| board.players[i].effective_role() == Role::Minion)
        .collect();
    let killed_tanners: Vec<usize> = (0..n)
        .filter(|&i| board.players[i].effective_role() == Role::Tanner && dead.contains(&i))
        .collect();

    let any_wolf = !wolves.is_empty();
    let wolf_died = wolves.iter().any(|i| dead.contains(i));
    let any_minion = !minions.is_empty();
    let minion_died = minions.iter().any(|i| dead.contains(i));
    let tanner_died = !killed_tanners.is_empty();

    let village_wins = if any_wolf {
        wolf_died
    } else if any_minion {
        minion_died
    } else {
        dead.is_empty()
    };

    let werewolf_team_wins = if any_wolf {
        !wolf_died && !tanner_died
    } else if any_minion {
        !minion_died && !tanner_died
    } else {
        false
    };

    if max_votes <= 1 {
        room.say("🔄 Circle Vote! Nobody received more than 1 vote—nobody died!".cyan())?;
    }

    let mut winners: HashMap<usize, &'static str> = HashMap::new();
    for &idx in &killed_tanners {
        winners.insert(idx, "Tanner");
    }
    if village_wins {
        for idx in 0..n {
            if board.players[idx].effective_role().team() == Team::Village {
                winners.insert(idx, "Village");
            }
        }
    }
    if werewolf_team_wins {
        for idx in 0..n {
            if board.players[idx].effective_role().team() == Team::Werewolf {
                winners.insert(idx, "Werewolf Team");
            }
        }
    }

    if winners.is_empty() {
        room.say(
            "💀 NOBODY WINS! No Werewolves or Minions were in play, yet an innocent died!"
                .bold()
                .red(),
        )?;
    } else {
        let mut sorted: Vec<(usize, &'static str)> = winners.into_iter().collect();
        sorted.sort_by_key(|(i, _)| *i);
        let list = sorted
            .into_iter()
            .map(|(i, team)| format!("{} ({team})", roster.seats[i].user.nick()))
            .collect::<Vec<_>>()
            .join(", ");
        room.say("🏆 Winners: ".bold().green() + list.bold())?;
    }
    Ok(())
}
