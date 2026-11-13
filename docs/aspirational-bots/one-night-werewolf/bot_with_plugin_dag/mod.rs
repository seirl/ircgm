//! Plugin-Driven Causal-DAG One Night Ultimate Werewolf Bot (Base + All 4 Expansions!)
//!
//! Notice that this top-level orchestrator (`mod.rs`), [`dag.rs`](./dag.rs), and [`plugin.rs`](./plugin.rs)
//! contain **zero hardcoded role logic**—every card across Base ONUW, Daybreak, Vampire, Alien,
//! and Super Villains lives in its own [`roles/./*.rs`](./roles/mod.rs) file declaring its
//! properties, Dusk/Night DAG dependencies, routines, death reactions, and win rules!

pub mod dag;
pub mod plugin;
pub mod roles;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use fastrand::Rng;
use futures::future::try_join_all;
use tokio::time::Instant;

use ircgm::{
    AskConfig, Bot, BotConfig, ChatMessage, Colorize, DisconnectReason, Error, GameFutureExt,
    InboxScope, Match, Outcome, Result, Room, RoomEvent, Text, Turn, User,
};

use dag::run_plugin_night_dag;
use plugin::{
    Artifact, BoardCards, Card, DeathCtx, Mark, RoleTag, Roster, SeatInfo, Team, VictoryCtx,
};
use roles::{DeckPreset, build_deck, werewolf::WEREWOLF};

const MIN_PLAYERS: usize = 3;
const MAX_PLAYERS: usize = 10;
const LOBBY_DURATION: Duration = Duration::from_secs(90);
const VOTE_TIMEOUT: Duration = Duration::from_secs(45);

// ============================================================================
// 1. RAII Channel Moderation Guard
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
// 2. Outer Supervisor, Lobby, & Single-Game Orchestrator
// ============================================================================

pub async fn run_bot(irc_cfg: irc::client::data::Config, channel: &str) -> Result<()> {
    let cfg = BotConfig::new(irc_cfg)
        .with_ask_timeout(Duration::from_secs(40))
        .with_command_prefixes(["!"]);

    loop {
        let bot = Bot::connect_with(cfg.clone()).await?;
        let room = bot.join(channel).await?;

        match lobby_loop(bot.clone(), room).await {
            Ok(()) => {
                bot.quit(Some("Shutting down ONUW bot")).await?;
                return Ok(());
            }
            Err(Error::Disconnected(DisconnectReason::Quit | DisconnectReason::BotDropped)) => {
                return Ok(());
            }
            Err(err) => {
                eprintln!(
                    "Session {} ended ({err}); reconnecting in 5s...",
                    bot.session_id()
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn lobby_loop(bot: Bot, room: Room) -> Result<()> {
    room.say(
        "🐺 ".bold()
            + "One Night Ultimate Werewolf (Plugin-DAG + 4 Expansions)"
                .bold()
                .yellow()
            + " ready! Type "
            + "!onuw [base|daybreak|vampire|alien|villains|chaos]"
                .bold()
                .green()
            + " to open a lobby.",
    )?;

    while !bot.is_shutting_down() {
        let trigger = match room.inbox().wait_cmd("!onuw").until(bot.shutdown_requested()).await {
            Outcome::Completed(res) => res?,
            Outcome::Stopped(()) => break,
        };
        let initial_preset = trigger
            .cmd_args("!onuw")
            .and_then(DeckPreset::parse)
            .unwrap_or_default();

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

        match run_single_game(&bot, &room, trigger.sender, initial_preset)
            .until(stop_signal)
            .await
        {
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

async fn recruit_players(
    room: &Room,
    host: User,
    mut preset: DeckPreset,
) -> Result<Option<(Vec<User>, DeckPreset)>> {
    let mut inbox = room.inbox();
    let mut players: Vec<User> = vec![host.clone()];

    room.say(
        "🌕 ".bold()
            + host.nick().bold().yellow()
            + format!(" opened an ONUW [{}] lobby! Type ", preset.label())
            + "!join".bold().green()
            + ", `!preset <base|daybreak|vampire|alien|villains|chaos>`, or "
            + "!go".bold().cyan()
            + " (90s).",
    )?;

    let deadline = Instant::now() + LOBBY_DURATION;
    loop {
        if players.len() >= MAX_PLAYERS {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            ev = inbox.next_event() => {
                match ev? {
                    RoomEvent::Chat(msg) if msg.is_cmd("!join") => {
                        if !players.contains(&msg.sender) {
                            players.push(msg.sender.clone());
                            room.say(msg.sender.nick().bold().green() + format!(" joined! ({}/{MAX_PLAYERS})", players.len()))?;
                        }
                    }
                    RoomEvent::Chat(msg) if msg.is_cmd("!leave") => {
                        players.retain(|u| *u != msg.sender);
                        if players.is_empty() {
                            return Ok(None);
                        }
                    }
                    RoomEvent::Chat(msg) if msg.is_cmd("!preset") && players.first() == Some(&msg.sender) => {
                        if let Some(p) = msg.cmd_args("!preset").and_then(DeckPreset::parse) {
                            preset = p;
                            room.say("📦 Deck preset switched to ".bold() + preset.label().bold().yellow() + "!")?;
                        }
                    }
                    RoomEvent::Chat(msg) if msg.is_cmd("!go") && players.first() == Some(&msg.sender) => {
                        if players.len() >= MIN_PLAYERS {
                            break;
                        }
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

    let check_deadline = Instant::now() + Duration::from_secs(25);
    room.say("📩 DM check: reply `ok` in DM within 25s to confirm you can receive secret cards!")?;
    let checks = try_join_all(players.iter().map(|p| async move {
        let res = p
            .ask("👋 Reply `ok` to confirm DMs for ONUW!", |m| {
                (m.text.trim().eq_ignore_ascii_case("ok") || m.is_cmd("!ok")).then_some(())
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

    Ok((verified.len() >= MIN_PLAYERS).then_some((verified, preset)))
}

async fn run_single_game(
    bot: &Bot,
    room: &Room,
    host: User,
    initial_preset: DeckPreset,
) -> Result<()> {
    let Some((players, preset)) = recruit_players(room, host, initial_preset).await? else {
        return Ok(());
    };

    let mut rng = Rng::new();
    let deck = build_deck(preset, players.len());
    let mut dealt_roles = deck.clone();
    rng.shuffle(&mut dealt_roles);

    let player_roles: Vec<_> = dealt_roles.drain(..players.len()).collect();
    let center: [Card; 3] = [
        dealt_roles[0].make_card(),
        dealt_roles[1].make_card(),
        dealt_roles[2].make_card(),
    ];
    let player_cards: Vec<Card> = player_roles.iter().map(|r| r.make_card()).collect();

    let seats: Vec<SeatInfo> = players
        .iter()
        .zip(player_roles)
        .map(|(u, role)| SeatInfo {
            user: u.clone(),
            initial_role: role,
        })
        .collect();

    let roster = Roster {
        casemap: bot.casemap(),
        seats,
        deck,
    };
    let initial_board = BoardCards::new(player_cards, center, Card::Standard(&WEREWOLF));

    let mut mod_guard = ChannelModerationGuard::new(room.clone());

    // Phase 1: Deal initial cards using plugin metadata
    room.say(
        format!("🃏 [{}] Shuffling ", preset.label()).bold()
            + format!("{} cards", roster.deck.len()).bold().yellow()
            + ": "
            + roster.format_deck(),
    )?;
    room.say("👥 Seats: ".bold() + roster.format_seats())?;
    for (idx, seat) in roster.seats.iter().enumerate() {
        seat.user.say(
            format!("🔒 [Seat #{}] Your initial card is: ", idx + 1)
                + seat.initial_role.styled_name()
                + " — "
                + seat.initial_role.night_summary(),
        )?;
    }

    let deck_snapshot = roster.format_deck();
    let deck_len = roster.deck.len();
    let info_scope = InboxScope::new().room(room).dms_from(&players);
    let mut info_inbox = bot.inbox_for(info_scope).lossy();
    let info_responder = async {
        loop {
            let msg = info_inbox.next_chat().await?;
            if msg.is_cmd("!deck") || msg.is_cmd("!roles") {
                bot.reply(&msg, format!("🃏 Deck ({deck_len} cards): {deck_snapshot}"))?;
            } else if msg.is_cmd("!players") || msg.is_cmd("!seats") {
                bot.reply(&msg, format!("👥 Seats: {}", roster.format_seats()))?;
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), Error>(())
    };

    let game_flow = async {
        // Phase 2: Generic Plugin-Driven Dusk + Night DAG!
        mod_guard.enter_quiet_phase("ONUW — 🌙 Dusk & Night Phase");
        let (final_board, night_log) =
            run_plugin_night_dag(bot, room, &roster, initial_board).await?;

        // Phase 3: Timed Day Discussion (announcing any shields / face-up cards first!)
        mod_guard.enter_day_discussion(&players, "ONUW — ☀️ Day Phase");
        run_day_phase(room, &roster, &final_board).await?;

        // Phase 4: Simultaneous Blind Voting
        mod_guard.enter_quiet_phase("ONUW — 🗳️ Voting Phase");
        let votes = run_voting_phase(room, &roster).await?;

        // Phase 5: Generic Plugin-Driven Death Chain & Win Resolution!
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
// 3. Day Phase, Simultaneous Voting, & Generic Plugin Win Engine
// ============================================================================

async fn run_day_phase(room: &Room, roster: &Roster, board: &BoardCards) -> Result<()> {
    let day_mins = (roster.len() + 2).clamp(5, 8) as u64;
    let deadline = Instant::now() + Duration::from_secs(day_mins * 60);
    room.say(
        "☀️ EVERYONE WAKE UP! ".bold().yellow()
            + format!("You have {day_mins} minutes to debate! Type `!ready` to vote early."),
    )?;

    if !board.shielded.is_empty() {
        let mut s: Vec<usize> = board.shielded.iter().copied().collect();
        s.sort_unstable();
        let names: Vec<String> = s
            .into_iter()
            .map(|i| format!("[{}] {}", i + 1, roster.seats[i].user.nick()))
            .collect();
        room.say("🛡️ Shield Token rests on: ".bold() + names.join(", ").bold().light_cyan())?;
    }

    for &seat in &board.face_up_players {
        room.say(
            "🔦 FACE-UP CARD: ".bold().yellow()
                + format!("[{}] {}'s card is publicly ", seat + 1, roster.seats[seat].user.nick())
                + board.players[seat].display_for_peek()
                + "!",
        )?;
    }
    for &c_idx in &board.face_up_center {
        room.say(
            format!("🔦 FACE-UP CENTER #{}: ", c_idx + 1).bold().yellow()
                + board.center[c_idx].display_for_peek(),
        )?;
    }

    let mut inbox = room.inbox();
    let enrolled: HashSet<User> = roster.seats.iter().map(|s| s.user.clone()).collect();
    let mut ready_voters: HashSet<User> = HashSet::new();

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            ev = inbox.next_event() => {
                match ev? {
                    RoomEvent::Chat(msg) if msg.is_cmd("!time") => {
                        let secs = deadline.saturating_duration_since(Instant::now()).as_secs();
                        room.say(format!("⏳ {:02}:{:02} left.", secs / 60, secs % 60))?;
                    }
                    RoomEvent::Chat(msg) if msg.is_cmd("!ready") => {
                        if enrolled.contains(&msg.sender) && ready_voters.insert(msg.sender.clone()) {
                            let active = enrolled.iter().filter(|u| is_active_in_room(room, u)).count();
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
        "🗳️ 3... 2... 1... POINT! Check your DMs and vote (45s)!"
            .bold()
            .red(),
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

    let max_votes = tally.iter().copied().max().unwrap_or(0);
    let mut dead: HashSet<usize> = HashSet::new();
    if max_votes >= 2 {
        for (idx, &c) in tally.iter().enumerate() {
            if c == max_votes {
                dead.insert(idx);
            }
        }
    }

    // Generic fixpoint loop over `RolePlugin::on_death` (Hunter, Bodyguard-style) AND `Mark::Love`:
    let death_ctx = DeathCtx {
        roster,
        board,
        votes,
    };
    let mut processed_deaths: HashSet<usize> = HashSet::new();
    let mut death_announcements: Vec<Text> = Vec::new();
    loop {
        let mut newly_killed = Vec::new();
        for &d in &dead {
            if processed_deaths.insert(d) {
                if let Some(reaction) = board.players[d].effective_role().on_death(d, &death_ctx) {
                    if !dead.contains(&reaction.kill_seat) {
                        newly_killed.push(reaction.kill_seat);
                        death_announcements.push(reaction.announcement);
                    }
                }
                if board.marks[d] == Mark::Love {
                    for other in 0..n {
                        if other != d && board.marks[other] == Mark::Love && !dead.contains(&other)
                        {
                            newly_killed.push(other);
                            death_announcements.push(
                                "💔 ".bold()
                                    + roster.seats[other].user.nick().bold().red()
                                    + " died of heartbreak alongside their Lover!",
                            );
                        }
                    }
                }
            }
        }
        if newly_killed.is_empty() {
            break;
        }
        dead.extend(newly_killed);
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
        let mut token_suffix = String::new();
        if board.marks[idx] != Mark::Clarity {
            token_suffix.push_str(&format!(" [{}]", board.marks[idx].name()));
        }
        if let Some(art) = board.artifacts.get(&idx) {
            token_suffix.push_str(&format!(" {{{}}}", art.name()));
        }
        room.say(
            format!("[{}] ", idx + 1)
                + roster.seats[idx].user.nick().bold()
                + ": started "
                + roster.seats[idx].initial_role.styled_name()
                + " → ended "
                + board.players[idx].display_reveal()
                + token_suffix
                + format!(
                    " ({} vote{}) — ",
                    tally[idx],
                    if tally[idx] == 1 { "" } else { "s" }
                )
                + status,
        )?;
    }
    for msg in death_announcements {
        room.say(msg)?;
    }
    if !night_log.is_empty() {
        room.say("📜 Night Recap: ".bold() + night_log.join(" -> "))?;
    }

    let vctx = VictoryCtx {
        roster,
        board,
        votes,
        dead_seats: &dead,
    };

    // 1. Check global team overrides (e.g. Alien `Leader` unanimous Alien point!):
    let mut forced_team_winner: Option<Team> = None;
    for seat in 0..n {
        if let Some((team, msg)) = board.players[seat]
            .effective_role()
            .check_team_win_override(seat, &vctx)
        {
            room.say(msg.bold().yellow())?;
            forced_team_winner = Some(team);
            break;
        }
    }

    // 2. Independent winners (including `Artifact::CudgelOfTheTanner`!) & team blocks:
    let mut blocked_teams: HashSet<Team> = HashSet::new();
    let mut winners: HashMap<usize, &'static str> = HashMap::new();
    for seat in 0..n {
        if board.artifacts.get(&seat) == Some(&Artifact::CudgelOfTheTanner) {
            if dead.contains(&seat) {
                winners.insert(seat, "Cudgel of the Tanner");
                blocked_teams.insert(Team::Werewolf);
            }
            continue;
        }
        let role = board.players[seat].effective_role();
        if board.effective_team(seat) == Team::Independent && role.check_independent_win(seat, &vctx)
        {
            winners.insert(seat, role.name());
            for candidate_team in [Team::Village, Team::Werewolf] {
                if role.independent_win_blocks(candidate_team) {
                    blocked_teams.insert(candidate_team);
                }
            }
        }
    }

    // 3. Team victory evaluation using `BoardCards::is_primary_threat` + `effective_team`:
    let primary_threats: Vec<usize> = (0..n).filter(|&i| board.is_primary_threat(i)).collect();
    let active_threats: Vec<usize> = if !primary_threats.is_empty() {
        primary_threats
    } else {
        (0..n)
            .filter(|&i| {
                board.players[i]
                    .effective_role()
                    .tags()
                    .contains(&RoleTag::SoloThreatFallback)
            })
            .collect()
    };

    let any_threat_killed = active_threats.iter().any(|s| dead.contains(s));
    let (village_wins, evil_wins) = if let Some(forced) = forced_team_winner {
        (forced == Team::Village, forced == Team::Werewolf)
    } else {
        let v = !blocked_teams.contains(&Team::Village)
            && if !active_threats.is_empty() {
                any_threat_killed
            } else {
                dead.is_empty()
            };
        let w = !blocked_teams.contains(&Team::Werewolf)
            && !active_threats.is_empty()
            && !any_threat_killed;
        (v, w)
    };

    if village_wins {
        for seat in 0..n {
            if board.effective_team(seat) == Team::Village {
                winners.insert(seat, "Village");
            }
        }
    }
    if evil_wins {
        for seat in 0..n {
            if board.effective_team(seat) == Team::Werewolf {
                winners.insert(seat, "Evil Team");
            }
        }
    }

    // 4. `Mark::Disease` & `Diseased` infection penalty (anyone voting for `Mark::Disease`, or
    //    sitting next to a killed `Diseased`, loses their win!):
    for seat in 0..n {
        if let Some(voted) = votes[seat] {
            if board.marks[voted] == Mark::Disease {
                winners.remove(&seat);
            }
        }
        if dead.contains(&seat) && board.players[seat].effective_role().id() == "diseased" {
            let (l, r) = roster.neighbors(seat);
            winners.remove(&l);
            winners.remove(&r);
        }
    }

    if winners.is_empty() {
        room.say("💀 NOBODY WINS!".bold().red())?;
    } else {
        let mut sorted: Vec<(usize, &'static str)> = winners.into_iter().collect();
        sorted.sort_by_key(|(i, _)| *i);
        let list = sorted
            .into_iter()
            .map(|(i, label)| format!("{} ({label})", roster.seats[i].user.nick()))
            .collect::<Vec<_>>()
            .join(", ");
        room.say("🏆 Winners: ".bold().green() + list.bold())?;
    }
    Ok(())
}
