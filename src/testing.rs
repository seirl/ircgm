//! In-memory [`Bot`] / [`TestServer`] harness for deterministic tests without sockets or an IRCd.
//!
//! Available in this crate's own unit tests, and to other crates (integration tests, downstream
//! game test suites) through the `testing` Cargo feature, typically enabled from
//! `[dev-dependencies]`: `ircgm = { version = "..", features = ["testing"] }`.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use irc::proto::Command;
use irc_colors::Text;
use tokio::sync::{mpsc, oneshot};

use crate::bot::{Bot, BotConfig, BotOwner};
use crate::casemap::{AtomicCaseMap, CaseMap, FoldedStr};
use crate::core::BotCore;
use crate::outbound::{OutboundSink, RawTransport};
use crate::pump::{run_pump, PumpInput, PumpSource, RegistrationTracker};
use crate::state::parse_irc_message;

/// Creates an in-memory `(Bot, TestServer)` pair with zero sockets for fast, deterministic tests.
pub fn test_harness(initial_nick: impl Into<String>) -> (Bot, TestServer) {
    test_harness_with(initial_nick, BotConfig::default())
}

/// Creates an in-memory `(Bot, TestServer)` pair honoring custom [`BotConfig`] settings.
pub fn test_harness_with(initial_nick: impl Into<String>, config: BotConfig) -> (Bot, TestServer) {
    let initial_nick = initial_nick.into();
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
    let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();

    let casemap = AtomicCaseMap::default();
    let eof_tx = inbound_tx.clone();
    let outbound = OutboundSink::new(RawTransport::Test {
        outbound_tx,
        on_quit: Some(Box::new(move || {
            let _ = eof_tx.send(PumpInput::Eof);
        })),
    });
    let core = BotCore::new(initial_nick.clone(), casemap, outbound, &config);
    core.mark_registered();
    let reg = RegistrationTracker {
        cap_pending: false,
        motd_done: true,
    };

    let pump_core = Arc::clone(&core);
    let pump_task = tokio::spawn(async move {
        run_pump(pump_core, PumpSource::Test(inbound_rx), reg).await;
    });

    let weak_core = Arc::downgrade(&core);
    let owner = Arc::new(BotOwner {
        core,
        pump_task,
        _live_client: Mutex::new(None),
    });

    let bot = Bot::from_owner(owner);
    let server = TestServer {
        inbound_tx,
        outbound_rx,
        pending_cmds: VecDeque::new(),
        peers: Mutex::new(PeerTable::default()),
        core: weak_core,
        fallback_nick: initial_nick,
    };
    (bot, server)
}

#[derive(Clone, Debug)]
struct PeerIdentity {
    user_host: String,
    account: Option<String>,
}

#[derive(Default)]
struct PeerTable {
    casemap: CaseMap,
    by_nick: HashMap<FoldedStr, PeerIdentity>,
}

impl PeerTable {
    fn sync_casemap(&mut self, cm: CaseMap) {
        if self.casemap != cm {
            self.casemap = cm;
            let old = std::mem::take(&mut self.by_nick);
            self.by_nick = old
                .into_iter()
                .map(|(k, v)| (cm.fold(k.as_str()), v))
                .collect();
        }
    }

    fn get_or_insert(&mut self, nick: &str, cm: CaseMap) -> &mut PeerIdentity {
        self.sync_casemap(cm);
        self.by_nick
            .entry(cm.fold(nick))
            .or_insert_with(|| PeerIdentity {
                user_host: format!("~{nick}@test.host"),
                account: None,
            })
    }
}

/// Deterministic in-memory test harness peer for driving [`Bot`] without sockets or an IRCd.
///
/// Tracks per-nick `ident@host` masks and optional IRCv3 `account` identities across [`rename_user`](Self::rename_user)
/// and [`quit_user`](Self::quit_user) so multi-step presence, moderation, and message tests never
/// have to hand-craft wire strings unless testing raw parser edge cases via [`send_line`](Self::send_line).
pub struct TestServer {
    inbound_tx: mpsc::UnboundedSender<PumpInput>,
    outbound_rx: mpsc::UnboundedReceiver<Command>,
    pending_cmds: VecDeque<Command>,
    peers: Mutex<PeerTable>,
    core: Weak<BotCore>,
    fallback_nick: String,
}

impl TestServer {
    fn bot_identity(&self) -> (String, String, CaseMap) {
        if let Some(core) = self.core.upgrade() {
            let ws = core.state.read().unwrap();
            let bn = ws.current_nick.clone();
            let uh = ws
                .self_user_host()
                .unwrap_or_else(|| format!("~{bn}@test.host"));
            (bn, uh, core.casemap())
        } else {
            let bn = self.fallback_nick.clone();
            let uh = format!("~{bn}@test.host");
            (bn, uh, CaseMap::default())
        }
    }

    fn current_nick_and_casemap(&self) -> (String, CaseMap) {
        let (bn, _, cm) = self.bot_identity();
        (bn, cm)
    }

    /// Resolves `(optional_account_tag_prefix, "<nick>!<ident>@<host>")` for `nick` (using the
    /// bot's own mask when `nick` matches the bot's current nick).
    fn actor_prefix(&self, nick: &str) -> (String, String) {
        let (bot_nick, bot_uh, cm) = self.bot_identity();
        if cm.str_eq(nick, &bot_nick) {
            return (String::new(), format!("{nick}!{bot_uh}"));
        }
        let mut peers = self.peers.lock().unwrap();
        let peer = peers.get_or_insert(nick, cm);
        let tag = match &peer.account {
            Some(acct) => format!("@account={acct} "),
            None => String::new(),
        };
        (tag, format!("{nick}!{}", peer.user_host))
    }

    #[doc(hidden)]
    pub fn core_strong_count(&self) -> usize {
        self.core.strong_count()
    }

    #[doc(hidden)]
    pub fn tracked_user_count(&self) -> usize {
        self.core
            .upgrade()
            .map(|c| c.users.tracked_map_len())
            .unwrap_or(0)
    }

    /// Parses a raw IRC wire line and injects it into the connection pump.
    pub fn send_line(&self, raw_line: &str) {
        let formatted: Cow<'_, str> = if raw_line.ends_with("\r\n") {
            Cow::Borrowed(raw_line)
        } else {
            Cow::Owned(format!("{}\r\n", raw_line.trim_end_matches(['\r', '\n'])))
        };
        let msg = parse_irc_message(&formatted)
            .unwrap_or_else(|e| panic!("TestServer::send_line failed to parse {formatted:?}: {e}"));
        let _ = self.inbound_tx.send(PumpInput::Line(Box::new(msg)));
    }

    /// Sets the `ident@host` mask used for subsequent lines emitted by `nick` (preserved across
    /// [`rename_user`](Self::rename_user) until [`quit_user`](Self::quit_user)).
    pub fn set_user_host(&self, nick: &str, ident: &str, host: &str) {
        let (_, cm) = self.current_nick_and_casemap();
        let mut peers = self.peers.lock().unwrap();
        peers.get_or_insert(nick, cm).user_host = format!("{ident}@{host}");
    }

    /// Sets (or clears) the IRCv3 `account` attached via `@account=` tags on subsequent messages
    /// and `extended-join` lines from `nick` without emitting an `ACCOUNT` wire frame.
    pub fn set_account(&self, nick: &str, account: Option<&str>) {
        let (_, cm) = self.current_nick_and_casemap();
        let mut peers = self.peers.lock().unwrap();
        peers.get_or_insert(nick, cm).account = account.map(str::to_owned);
    }

    /// Updates `nick`'s tracked account and injects an IRCv3 `account-notify` frame
    /// (`:<nick>!<ident>@<host> ACCOUNT <account|*>\r\n`).
    pub fn account_notify(&self, nick: &str, account: Option<&str>) {
        self.set_account(nick, account);
        let (_, mask) = self.actor_prefix(nick);
        let acct_tok = account.unwrap_or("*");
        self.send_line(&format!(":{mask} ACCOUNT {acct_tok}\r\n"));
    }

    /// Injects `PRIVMSG <channel> :<text>` from `nick` (with `@account=` if set).
    pub fn chat(&self, nick: &str, channel: &str, text: &str) {
        let (tag, mask) = self.actor_prefix(nick);
        self.send_line(&format!("{tag}:{mask} PRIVMSG {channel} :{text}\r\n"));
    }

    /// Injects a CTCP `/me` action (`\x01ACTION <action_text>\x01`) to `target`.
    pub fn action(&self, nick: &str, target: &str, action_text: &str) {
        let (tag, mask) = self.actor_prefix(nick);
        self.send_line(&format!(
            "{tag}:{mask} PRIVMSG {target} :\x01ACTION {action_text}\x01\r\n"
        ));
    }

    /// Injects `NOTICE <target> :<text>` from `nick` (with `@account=` if set).
    pub fn notice(&self, nick: &str, target: &str, text: &str) {
        let (tag, mask) = self.actor_prefix(nick);
        self.send_line(&format!("{tag}:{mask} NOTICE {target} :{text}\r\n"));
    }

    /// Injects a DM `PRIVMSG <bot_nick> :<text>` from `nick` (with `@account=` if set).
    pub fn dm(&self, nick: &str, text: &str) {
        let (bot_nick, _) = self.current_nick_and_casemap();
        self.chat(nick, &bot_nick, text);
    }

    /// Injects a DM `NOTICE <bot_nick> :<text>` from `nick` (with `@account=` if set).
    pub fn dm_notice(&self, nick: &str, text: &str) {
        let (bot_nick, _) = self.current_nick_and_casemap();
        self.notice(nick, &bot_nick, text);
    }

    /// Injects a DM CTCP `/me` action to `<bot_nick>` from `nick`.
    pub fn dm_action(&self, nick: &str, action_text: &str) {
        let (bot_nick, _) = self.current_nick_and_casemap();
        self.action(nick, &bot_nick, action_text);
    }

    /// Injects `JOIN <channel>` from `nick` (using `extended-join` syntax automatically if `nick`
    /// has an active account set via [`set_account`](Self::set_account)).
    pub fn join_user(&self, nick: &str, channel: &str) {
        let (_, cm) = self.current_nick_and_casemap();
        let (mask, acct) = {
            let mut peers = self.peers.lock().unwrap();
            let peer = peers.get_or_insert(nick, cm);
            (format!("{nick}!{}", peer.user_host), peer.account.clone())
        };
        match acct {
            Some(a) => self.send_line(&format!(":{mask} JOIN {channel} {a} :{nick}\r\n")),
            None => self.send_line(&format!(":{mask} JOIN {channel}\r\n")),
        }
    }

    /// Updates `nick`'s tracked account and injects an IRCv3 `extended-join` frame
    /// (`:<nick>!<ident>@<host> JOIN <channel> <account|*> :<nick>\r\n`).
    pub fn join_user_with_account(&self, nick: &str, channel: &str, account: Option<&str>) {
        self.set_account(nick, account);
        let (_, mask) = self.actor_prefix(nick);
        let acct_tok = account.unwrap_or("*");
        self.send_line(&format!(":{mask} JOIN {channel} {acct_tok} :{nick}\r\n"));
    }

    /// Injects `PART <channel> [:<reason>]` from `nick` (preserving `nick`'s `ident@host`).
    pub fn part_user(&self, nick: &str, channel: &str, reason: Option<&str>) {
        let (_, mask) = self.actor_prefix(nick);
        match reason {
            Some(r) => self.send_line(&format!(":{mask} PART {channel} :{r}\r\n")),
            None => self.send_line(&format!(":{mask} PART {channel}\r\n")),
        }
    }

    /// Injects `NICK <new_nick>` from `old_nick` and transfers `old_nick`'s `ident@host` and
    /// `account` identity to `new_nick`.
    pub fn rename_user(&self, old_nick: &str, new_nick: &str) {
        let (_, cm) = self.current_nick_and_casemap();
        let user_host = {
            let mut peers = self.peers.lock().unwrap();
            peers.sync_casemap(cm);
            let moved = peers
                .by_nick
                .remove(&cm.fold(old_nick))
                .unwrap_or_else(|| PeerIdentity {
                    user_host: format!("~{old_nick}@test.host"),
                    account: None,
                });
            let uh = moved.user_host.clone();
            peers.by_nick.insert(cm.fold(new_nick), moved);
            uh
        };
        self.send_line(&format!(":{old_nick}!{user_host} NICK :{new_nick}\r\n"));
    }

    /// Injects `KICK <channel> <victim> [:<reason>]` by `kicker`.
    pub fn kick_user(&self, kicker: &str, channel: &str, victim: &str, reason: Option<&str>) {
        let (_, mask) = self.actor_prefix(kicker);
        match reason {
            Some(r) => self.send_line(&format!(":{mask} KICK {channel} {victim} :{r}\r\n")),
            None => self.send_line(&format!(":{mask} KICK {channel} {victim}\r\n")),
        }
    }

    /// Injects `KICK <channel> <bot_nick> [:<reason>]` kicking the bot from `channel`.
    pub fn kick_bot(&self, kicker: &str, channel: &str, reason: Option<&str>) {
        let (bot_nick, _) = self.current_nick_and_casemap();
        self.kick_user(kicker, channel, &bot_nick, reason);
    }

    /// Injects the server echo of the bot's own `JOIN <channel>` without `353`/`366`.
    pub fn echo_bot_join(&self, channel: &str) {
        let (bot_nick, user_host, _) = self.bot_identity();
        self.send_line(&format!(":{bot_nick}!{user_host} JOIN {channel}\r\n"));
    }

    /// Injects the server echo of the bot's own `PART <channel> [:<reason>]`.
    pub fn echo_bot_part(&self, channel: &str, reason: Option<&str>) {
        let (bot_nick, user_host, _) = self.bot_identity();
        match reason {
            Some(r) => self.send_line(&format!(":{bot_nick}!{user_host} PART {channel} :{r}\r\n")),
            None => self.send_line(&format!(":{bot_nick}!{user_host} PART {channel}\r\n")),
        }
    }

    /// Injects `MODE <channel> <modes_and_args>` by `actor`.
    pub fn mode(&self, actor: &str, channel: &str, modes_and_args: &str) {
        let (_, mask) = self.actor_prefix(actor);
        self.send_line(&format!(":{mask} MODE {channel} {modes_and_args}\r\n"));
    }

    /// Injects `TOPIC <channel> :<text>` by `actor`.
    pub fn topic(&self, actor: &str, channel: &str, text: &str) {
        let (_, mask) = self.actor_prefix(actor);
        self.send_line(&format!(":{mask} TOPIC {channel} :{text}\r\n"));
    }

    /// Injects `332 RPL_TOPIC` (`:test.srv 332 <bot_nick> <channel> :<text>`).
    pub fn initial_topic(&self, channel: &str, text: &str) {
        let (bot_nick, _) = self.current_nick_and_casemap();
        self.send_line(&format!(":test.srv 332 {bot_nick} {channel} :{text}\r\n"));
    }

    /// Injects `001 RPL_WELCOME` (`:test.srv 001 <bot_nick> :<welcome_text>`).
    pub fn welcome(&self, welcome_text: &str) {
        let (bot_nick, _) = self.current_nick_and_casemap();
        self.send_line(&format!(":test.srv 001 {bot_nick} :{welcome_text}\r\n"));
    }

    /// Injects `005 RPL_ISUPPORT` (`:test.srv 005 <bot_nick> <tokens> :are supported by this server`).
    pub fn isupport(&self, tokens: &str) {
        let (bot_nick, _) = self.current_nick_and_casemap();
        self.send_line(&format!(
            ":test.srv 005 {bot_nick} {tokens} :are supported by this server\r\n"
        ));
    }

    /// Injects a server error/bounce numeric (`401`, `404`, `473`, `482`, `486`, `716`, etc.)
    /// formatted as `:test.srv <code:03> <bot_nick> <target> :<reason>\r\n`.
    pub fn bounce(&self, code: u16, target: &str, reason: &str) {
        let (bot_nick, _) = self.current_nick_and_casemap();
        self.send_line(&format!(
            ":test.srv {code:03} {bot_nick} {target} :{reason}\r\n"
        ));
    }

    /// Convenience helper: injects self `JOIN`, `353 RPL_NAMREPLY` (`@<bot_nick>` + `names`),
    /// and `366 RPL_ENDOFNAMES`. Any `nick!ident@host` masks in `names` also seed [`TestServer`]'s
    /// peer hostmask table.
    pub fn complete_join(&self, channel: &str, names: &[&str]) {
        let (bot_nick, user_host, cm) = self.bot_identity();
        self.send_line(&format!(":{bot_nick}!{user_host} JOIN {channel}\r\n"));

        {
            let mut peers = self.peers.lock().unwrap();
            peers.sync_casemap(cm);
            for entry in names {
                let stripped = entry.trim_start_matches(['~', '&', '@', '%', '+']);
                if let Some((bare, uh)) = stripped.split_once('!') {
                    if !cm.str_eq(bare, &bot_nick) && !uh.is_empty() {
                        peers.get_or_insert(bare, cm).user_host = uh.to_owned();
                    }
                }
            }
        }

        let bot_already_listed = names.iter().any(|entry| {
            let stripped = entry.trim_start_matches(['~', '&', '@', '%', '+']);
            let bare = stripped.split('!').next().unwrap_or(stripped);
            cm.str_eq(bare, &bot_nick)
        });

        let mut entries = Vec::with_capacity(names.len() + 1);
        if !bot_already_listed {
            entries.push(format!("@{bot_nick}"));
        }
        for n in names {
            entries.push((*n).to_owned());
        }
        let joined_entries = entries.join(" ");
        self.send_line(&format!(
            ":test.srv 353 {bot_nick} = {channel} :{joined_entries}\r\n"
        ));
        self.send_line(&format!(
            ":test.srv 366 {bot_nick} {channel} :End of /NAMES list.\r\n"
        ));
    }

    /// Injects `QUIT [:<reason>]` from `nick` and removes `nick`'s tracked peer identity so a
    /// subsequent rejoin starts as a fresh user.
    pub fn quit_user(&self, nick: &str, reason: Option<&str>) {
        let (_, cm) = self.current_nick_and_casemap();
        let user_host = {
            let mut peers = self.peers.lock().unwrap();
            peers.sync_casemap(cm);
            peers
                .by_nick
                .remove(&cm.fold(nick))
                .map(|p| p.user_host)
                .unwrap_or_else(|| format!("~{nick}@test.host"))
        };
        match reason {
            Some(r) => self.send_line(&format!(":{nick}!{user_host} QUIT :{r}\r\n")),
            None => self.send_line(&format!(":{nick}!{user_host} QUIT\r\n")),
        }
    }

    /// Yields enough cooperative scheduler turns for woken tasks and `OutboundSink` to flush
    /// ready outbound commands without requiring an inbound wire line.
    pub async fn flush(&self) {
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
    }

    /// Awaits an in-band barrier confirming the pump has processed every line injected prior to
    /// this call, then loops cooperative task yields and pump barriers until woken tasks and
    /// `OutboundSink` enqueues reach full quiescence.
    pub async fn settle(&self) {
        for _ in 0..64 {
            let out_before = self
                .core
                .upgrade()
                .map(|c| c.outbound.enqueue_seq())
                .unwrap_or(0);

            let (tx, rx) = oneshot::channel();
            if self.inbound_tx.send(PumpInput::Settle(tx)).is_ok() {
                let _ = rx.await;
            }
            self.flush().await;

            let out_after = self
                .core
                .upgrade()
                .map(|c| c.outbound.enqueue_seq())
                .unwrap_or(0);
            if out_after == out_before {
                break;
            }
        }
    }

    /// Settles pending inbound lines and ready tasks first (so newly-triggered timers are armed
    /// before time moves), advances Tokio's paused virtual clock by `duration`, and settles woken
    /// timer tasks, pump events, and outbound writes.
    pub async fn advance(&self, duration: Duration) {
        self.settle().await;
        tokio::time::advance(duration).await;
        self.settle().await;
    }

    /// Waits for an outbound `JOIN` whose channel starts with `prefix` (must explicitly start with
    /// `'#'` or `'&'`, e.g. `"#vault"` or `"##lobby-vault"`), preserves all other outbound commands
    /// in `recv_sent` order, completes `JOIN` + `NAMES` with `names`, settles the pump/outbound
    /// tasks, and returns the joined channel name.
    pub async fn accept_breakout(&mut self, prefix: &str, names: &[&str]) -> String {
        assert!(
            prefix.starts_with(['#', '&']),
            "TestServer::accept_breakout prefix {prefix:?} must start with '#' or '&'"
        );
        let ch = self
            .recv_matching(&format!("JOIN {prefix}*"), |cmd, cm| match cmd {
                Command::JOIN(chan, _, _)
                    if chan.len() >= prefix.len()
                        && chan.is_char_boundary(prefix.len())
                        && cm.str_eq(&chan[..prefix.len()], prefix) =>
                {
                    Some(chan.clone())
                }
                _ => None,
            })
            .await;
        self.complete_join(&ch, names);
        self.settle().await;
        ch
    }

    /// Accepts an initially empty breakout room matching `prefix` (`353` contains only `@<bot>`),
    /// settles the pump/outbound tasks, and returns the joined breakout channel name.
    pub async fn accept_empty_breakout(&mut self, prefix: &str) -> String {
        self.accept_breakout(prefix, &[]).await
    }

    /// Accepts an initially empty breakout room matching `prefix` (`353` contains only `@<bot>`),
    /// settles any immediate `INVITE` burst, joins each `invitees` nick via [`join_user`](Self::join_user),
    /// settles again, and returns the breakout channel name.
    pub async fn accept_breakout_invite(&mut self, prefix: &str, invitees: &[&str]) -> String {
        let ch = self.accept_breakout(prefix, &[]).await;
        if !invitees.is_empty() {
            for nick in invitees {
                self.join_user(nick, &ch);
            }
            self.settle().await;
        }
        ch
    }

    /// Simulates socket EOF / server disconnect (`Error::Disconnected(DisconnectReason::Eof)`).
    pub fn disconnect(self) {
        let _ = self.inbound_tx.send(PumpInput::Eof);
    }

    pub async fn recv_sent(&mut self) -> Option<Command> {
        if let Some(cmd) = self.pending_cmds.pop_front() {
            return Some(cmd);
        }
        self.outbound_rx.recv().await
    }

    pub fn try_recv_sent(&mut self) -> Option<Command> {
        if let Some(cmd) = self.pending_cmds.pop_front() {
            return Some(cmd);
        }
        self.outbound_rx.try_recv().ok()
    }

    /// Drains ready outbound `PRIVMSG`/`NOTICE` payloads (plain-stripped) addressed to `target`
    /// (case-folded), while preserving commands addressed to other targets or non-message commands
    /// (`MODE`, `KICK`, `PART`) in `recv_sent` order.
    pub fn take_messages_to(&mut self, target: &str) -> Vec<String> {
        self.drain_ready();
        let (_, cm) = self.current_nick_and_casemap();
        let mut matched = Vec::new();
        let mut kept = VecDeque::with_capacity(self.pending_cmds.len());
        while let Some(cmd) = self.pending_cmds.pop_front() {
            match message_to(&cmd, cm, target) {
                Some(text) => matched.push(text),
                None => kept.push_back(cmd),
            }
        }
        self.pending_cmds = kept;
        matched
    }

    /// Awaits and removes the next outbound `PRIVMSG`/`NOTICE` addressed to `target` (case-folded),
    /// returning its plain-stripped text while preserving all earlier or non-matching commands in
    /// `recv_sent` order.
    pub async fn recv_message_to(&mut self, target: &str) -> String {
        self.recv_matching(&format!("message to {target:?}"), |cmd, cm| {
            message_to(cmd, cm, target)
        })
        .await
    }

    /// Moves every command the bot has already written into `pending_cmds`.
    fn drain_ready(&mut self) {
        while let Ok(cmd) = self.outbound_rx.try_recv() {
            self.pending_cmds.push_back(cmd);
        }
    }

    /// Awaits and removes the first outbound command, in `recv_sent` order, that `pick` maps to
    /// `Some`, keeping all others queued; panics naming `what` after 5 wall-clock seconds or if the
    /// bot is gone.
    ///
    /// Uses a real wall-clock watchdog rather than `tokio::time::timeout` so tests running with
    /// `#[tokio::test(start_paused = true)]` can auto-advance past `>= 5s` virtual game sleeps
    /// without prematurely timing out `recv_matching`.
    async fn recv_matching<T>(
        &mut self,
        what: &str,
        mut pick: impl FnMut(&Command, CaseMap) -> Option<T>,
    ) -> T {
        let mut watchdog: Option<WallWatchdog> = None;
        loop {
            self.drain_ready();
            let (_, cm) = self.current_nick_and_casemap();
            let found = self
                .pending_cmds
                .iter()
                .enumerate()
                .find_map(|(idx, cmd)| Some((idx, pick(cmd, cm)?)));
            if let Some((idx, val)) = found {
                self.pending_cmds.remove(idx);
                return val;
            }
            let wd = watchdog.get_or_insert_with(|| WallWatchdog::start(Duration::from_secs(5)));
            let next = tokio::select! {
                biased;
                cmd = self.outbound_rx.recv() => cmd.unwrap_or_else(|| {
                    panic!("TestServer saw the outbound channel close before {what}")
                }),
                _ = &mut wd.rx => {
                    panic!("TestServer timed out waiting for {what}")
                }
            };
            self.pending_cmds.push_back(next);
        }
    }
}

struct WallWatchdog {
    cancelled: Arc<AtomicBool>,
    thread: Option<std::thread::Thread>,
    rx: oneshot::Receiver<()>,
}

impl WallWatchdog {
    fn start(timeout: Duration) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_bg = Arc::clone(&cancelled);
        let (tx, rx) = oneshot::channel();
        let deadline = std::time::Instant::now() + timeout;
        let handle = std::thread::spawn(move || {
            while !cancelled_bg.load(Ordering::Acquire) {
                let Some(rem) = deadline.checked_duration_since(std::time::Instant::now()) else {
                    let _ = tx.send(());
                    return;
                };
                std::thread::park_timeout(rem);
            }
        });
        Self {
            cancelled,
            thread: Some(handle.thread().clone()),
            rx,
        }
    }
}

impl Drop for WallWatchdog {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.unpark();
        }
    }
}

/// Plain-stripped text of `cmd` if it is a `PRIVMSG`/`NOTICE` to `target` (case-folded).
fn message_to(cmd: &Command, cm: CaseMap, target: &str) -> Option<String> {
    match cmd {
        Command::PRIVMSG(dest, payload) | Command::NOTICE(dest, payload)
            if cm.str_eq(dest, target) =>
        {
            Some(Text::parse(payload).plain())
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "testing_test.rs"]
mod test;

