use std::borrow::Borrow;
use std::future::Future;
use std::panic::Location;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use irc::proto::{CapSubCommand, Command};
use tokio::task::JoinHandle;
use tracing::Instrument as _;

use crate::breakout::{Breakout, BreakoutOptions};
use crate::casemap::{AtomicCaseMap, CaseMap};
use crate::core::{BotCore, ConnLifecycle};
use crate::error::{DisconnectReason, Error, Result};
use crate::exclusive_dm::{acquire_dm_leases, DmLease, ExclusiveDmGroup};
use crate::huddle::Huddle;
use crate::inbox::{Filter, Inbox};
use crate::outbound::{validate_param, validate_target, OutboundSink, RawTransport};
use crate::pump::{run_pump, Keepalive, PumpSource, RegistrationTracker};
use crate::room::Room;
use crate::state::JoinOutcome;
use crate::user::{User, UserTrack};

/// Default `ping_time` (in seconds) applied when `config.irc.ping_time` is `None`.
///
/// Chosen shorter than `irc`'s 180s default so residential NAT / stateful IPv6 CPE conntrack
/// entries stay warm across idle channels.
const DEFAULT_PING_TIME_SECS: u32 = 60;

/// Default `ping_timeout` (in seconds) applied when `config.irc.ping_timeout` is `None`.
///
/// Chosen wider than `irc`'s 20s default so transient link stalls (Linux TCP RTO backoff) do not
/// trigger premature client-side disconnects before TCP retransmission recovers.
const DEFAULT_PING_TIMEOUT_SECS: u32 = 60;

/// Effectively disables `irc::client`'s fixed wall-clock `Pinger::send_ping` (~10 years) so
/// `run_pump` can drive idle-aware keepalive probes while keeping `ClientStream`'s automatic
/// inbound `PING` -> `PONG` responder active.
const DISABLED_IRC_PING_SECS: u32 = 10 * 365 * 24 * 60 * 60;

fn apply_default_ping_config(irc: &mut irc::client::data::Config) {
    irc.ping_time.get_or_insert(DEFAULT_PING_TIME_SECS);
    irc.ping_timeout.get_or_insert(DEFAULT_PING_TIMEOUT_SECS);
}

/// Multi-source filter builder for [`Bot::inbox_scope`] (delivers matching channels
/// and/or DMs into one wire-ordered [`Inbox`]).
#[derive(Clone, Debug, Default)]
pub struct InboxScope {
    pub(crate) rooms: Vec<Room>,
    pub(crate) dm_users: Vec<User>,
}

impl InboxScope {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn room(mut self, room: &Room) -> Self {
        let cm = room.casemap();
        if !self
            .rooms
            .iter()
            .any(|existing| {
                existing.belongs_to(room.core()) && cm.str_eq(existing.name(), room.name())
            })
        {
            self.rooms.push(room.clone());
        }
        self
    }

    pub fn rooms<I, R>(mut self, rooms: I) -> Self
    where
        I: IntoIterator<Item = R>,
        R: Borrow<Room>,
    {
        for r in rooms {
            self = self.room(r.borrow());
        }
        self
    }

    pub fn dm_from(mut self, user: &User) -> Self {
        if !self.dm_users.iter().any(|u| u == user) {
            self.dm_users.push(user.clone());
        }
        self
    }

    pub fn dms_from<I, U>(mut self, users: I) -> Self
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        for u in users {
            self = self.dm_from(u.borrow());
        }
        self
    }
}

/// Whether `Bot::join_inner` may reuse a channel the bot already holds (`Bot::join`) or must
/// claim it fresh (`Bot::breakout_with`, whose `Drop` tears the room down).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JoinMode {
    Shared,
    Exclusive,
}

/// Configuration for [`Bot::connect_with`] and [`crate::testing::test_harness_with`].
#[derive(Clone, Debug)]
pub struct BotConfig {
    /// Underlying `irc` client configuration (`ping_time` and `ping_timeout` default to `60s`
    /// when left `None`).
    pub irc: irc::client::data::Config,
    /// Per-inbox bounded event queue size (default: `1024`).
    pub inbox_capacity: usize,
    /// IRCv3 capabilities requested via `CAP REQ` on connect.
    pub request_caps: Vec<String>,
    /// Command prefixes skipped when `.ignore_commands()` is opted into (default: `["!"]`).
    pub command_prefixes: Vec<String>,
    /// Default timeout for protocol handshakes like `JOIN`/`NAMES`, `Breakout`, and `quit` (default: `15s`).
    pub handshake_timeout: Duration,
    /// Upper ceiling on payload bytes per outbound `PRIVMSG`/`NOTICE` passed to `Text::wrap` (default: `450`).
    pub max_payload_bytes: usize,
    /// Automatically set the server-advertised `005 BOT=<mode>` user mode (e.g. `+B`) upon
    /// registration (default: `true`).
    pub announce_bot: bool,
}

impl Default for BotConfig {
    fn default() -> Self {
        Self::new(irc::client::data::Config::default())
    }
}

impl BotConfig {
    pub fn new(mut irc: irc::client::data::Config) -> Self {
        apply_default_ping_config(&mut irc);
        Self {
            irc,
            inbox_capacity: 1024,
            request_caps: vec![
                "account-tag".into(),
                "account-notify".into(),
                "extended-join".into(),
                "multi-prefix".into(),
                "userhost-in-names".into(),
            ],
            command_prefixes: vec!["!".into()],
            handshake_timeout: Duration::from_secs(15),
            max_payload_bytes: 450,
            announce_bot: true,
        }
    }

    pub fn with_command_prefixes<I, S>(mut self, prefixes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.command_prefixes = prefixes.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_announce_bot(mut self, announce: bool) -> Self {
        self.announce_bot = announce;
        self
    }
}

/// Owns the live background tasks for a [`Bot`] session separately from [`BotCore`].
///
/// Cycle-prevention invariant: `pump_task` holds a strong `Arc<BotCore>`, and `WorldState` holds
/// `Arc<UserTrack>`s. Splitting [`Bot`] (`Arc<BotOwner>`) -> `Arc<BotCore>` while [`User`] holds
/// only `Weak<BotCore>` ensures dropping the caller's last `Bot` handle immediately runs
/// [`BotOwner::drop`] (aborting `pump_task` and clearing state) even when `User` or `Member`
/// values remain stored in caller data structures.
pub(crate) struct BotOwner {
    pub(crate) core: Arc<BotCore>,
    pub(crate) pump_task: JoinHandle<()>,
    /// Keeps `irc::client::Client` alive for the lifetime of live TCP/TLS sessions so its
    /// background outgoing writer task does not drop prematurely.
    pub(crate) _live_client: Mutex<Option<irc::client::Client>>,
}

impl std::fmt::Debug for BotOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotOwner")
            .field("core", &Arc::as_ptr(&self.core))
            .finish_non_exhaustive()
    }
}

impl Drop for BotOwner {
    fn drop(&mut self) {
        self.core.mark_disconnected(DisconnectReason::BotDropped);
        self.pump_task.abort();

        self.core.state.write().unwrap().clear();
        self.core.users.clear();
    }
}

/// Primary IRC session handle. Cloning `Bot` shares ownership of the active connection pump;
/// dropping the final `Bot` clone stops the pump and clears session state cleanly.
#[derive(Clone, Debug)]
pub struct Bot {
    owner: Arc<BotOwner>,
}

impl Bot {
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn from_owner(owner: Arc<BotOwner>) -> Self {
        Self { owner }
    }

    /// Connects with default [`BotConfig`] settings.
    pub async fn connect(irc_config: irc::client::data::Config) -> Result<Self> {
        Self::connect_with(BotConfig::new(irc_config)).await
    }

    /// Connects with custom [`BotConfig`] settings, negotiates IRCv3 `request_caps`, awaits
    /// `376 RPL_ENDOFMOTD` / `422 ERR_NOMOTD` (with `handshake_timeout` fallback), and joins
    /// any initial channels configured in `config.irc.channels`.
    pub async fn connect_with(mut config: BotConfig) -> Result<Self> {
        let initial_channels = std::mem::take(&mut config.irc.channels);
        let channel_keys = std::mem::take(&mut config.irc.channel_keys);

        let nick = config
            .irc
            .nickname
            .clone()
            .unwrap_or_else(|| "ircgm".to_owned());
        let username = config.irc.username.clone().unwrap_or_else(|| nick.clone());
        let realname = config.irc.realname.clone().unwrap_or_else(|| nick.clone());
        let password = config.irc.password.clone().filter(|p| !p.is_empty());

        apply_default_ping_config(&mut config.irc);
        let keepalive = Keepalive::new(
            Duration::from_secs(u64::from(config.irc.ping_time().max(1))),
            Duration::from_secs(u64::from(config.irc.ping_timeout().max(1))),
        );

        let mut irc_config = config.irc.clone();
        irc_config.ping_time = Some(DISABLED_IRC_PING_SECS);
        irc_config.ping_timeout = Some(DISABLED_IRC_PING_SECS);
        let mut client = irc::client::Client::from_config(irc_config).await?;
        let stream = client.stream()?;
        let sender = client.sender();

        let casemap = AtomicCaseMap::default();
        let outbound = OutboundSink::new(RawTransport::Irc(sender));
        let core = BotCore::new(nick.clone(), casemap, outbound, &config);

        let has_caps = !config.request_caps.is_empty();
        let reg = RegistrationTracker {
            cap_pending: has_caps,
            motd_done: false,
        };

        let pump_core = Arc::clone(&core);
        let pump_task = tokio::spawn(async move {
            run_pump(pump_core, PumpSource::Irc(Box::new(stream), keepalive), reg).await;
        });

        let bot = Self {
            owner: Arc::new(BotOwner {
                core: Arc::clone(&core),
                pump_task,
                _live_client: Mutex::new(Some(client)),
            }),
        };

        // Send initial registration burst (`CAP REQ`, optional `PASS`, `NICK`, `USER`):
        let mut init_cmds = Vec::with_capacity(4);
        if has_caps {
            let cap_list = config.request_caps.join(" ");
            init_cmds.push(Command::CAP(
                None,
                CapSubCommand::REQ,
                None,
                Some(cap_list),
            ));
        }
        if let Some(pass) = password {
            init_cmds.push(Command::PASS(pass));
        }
        init_cmds.push(Command::NICK(nick));
        init_cmds.push(Command::USER(username, "0".into(), realname));
        core.outbound.enqueue_batch(init_cmds);

        bot.wait_for_registration().await?;

        for ch in initial_channels {
            if let Some(key) = channel_keys.get(&ch) {
                bot.join_with_key(&ch, key).await?;
            } else {
                bot.join(&ch).await?;
            }
        }

        Ok(bot)
    }

    /// Awaits `CAP` completion + `376`/`422`, falling back to `handshake_timeout` only if `001`
    /// (`RPL_WELCOME`) has already been received (propagating `Disconnected` or `HandshakeTimeout`
    /// if registration fails or stalls before `001`).
    pub(crate) async fn wait_for_registration(&self) -> Result<()> {
        let core = &self.owner.core;
        let mut rx = core.lifecycle_tx.subscribe();
        let sleep = tokio::time::sleep(core.handshake_timeout);
        tokio::pin!(sleep);

        loop {
            let state = rx.borrow_and_update().clone();
            match state {
                ConnLifecycle::Active => return Ok(()),
                ConnLifecycle::Disconnected(reason) => {
                    return Err(Error::Disconnected(reason));
                }
                ConnLifecycle::Registering { .. } => {}
            }

            tokio::select! {
                biased;
                res = rx.changed() => {
                    if res.is_err() {
                        return Err(Error::Disconnected(DisconnectReason::BotDropped));
                    }
                }
                _ = &mut sleep => {
                    let state = rx.borrow_and_update().clone();
                    match state {
                        ConnLifecycle::Active => return Ok(()),
                        ConnLifecycle::Disconnected(reason) => {
                            return Err(Error::Disconnected(reason));
                        }
                        ConnLifecycle::Registering { welcomed: true } => {
                            tracing::warn!(
                                "registration barrier timed out waiting for CAP/376/422 after 001 welcome; proceeding"
                            );
                            core.outbound.enqueue_one(Command::CAP(
                                None,
                                CapSubCommand::END,
                                None,
                                None,
                            ));
                            core.mark_registered();
                            return Ok(());
                        }
                        ConnLifecycle::Registering { welcomed: false } => {
                            return Err(Error::HandshakeTimeout {
                                operation: "IRC registration",
                            });
                        }
                    }
                }
            }
        }
    }

    pub fn nick(&self) -> String {
        self.owner.core.state.read().unwrap().current_nick.clone()
    }

    pub fn casemap(&self) -> CaseMap {
        self.owner.core.casemap()
    }

    pub fn room(&self, channel: &str) -> Result<Room> {
        validate_target(channel, "channel name")?;
        Ok(Room::new(
            Arc::clone(&self.owner.core),
            channel.to_owned(),
        ))
    }

    pub fn user(&self, nick: &str) -> Result<User> {
        validate_target(nick, "nickname")?;
        let core = &self.owner.core;
        let track = core.users.get_or_create(nick);
        Ok(User::from_parts(Arc::downgrade(core), track))
    }

    /// Acquires multiple per-user DM prompt locks in canonical [`crate::UserId`] order, returning
    /// an [`ExclusiveDmGroup`] in the caller's iteration order without cross-relaying member chat.
    ///
    /// Use this instead of acquiring individual [`User::exclusive`] guards across concurrent tasks
    /// when locking multiple players for *private* 1-on-1 exchanges (where [`Bot::huddle`] is
    /// unsuitable because `Huddle` relays messages and broadcasts replies): canonical lock ordering
    /// guarantees concurrent callers locking overlapping user sets in different orders can never
    /// ABBA-deadlock.
    #[track_caller]
    pub fn exclusive_many<I, U>(
        &self,
        users: I,
    ) -> impl Future<Output = Result<ExclusiveDmGroup>> + Send + '_
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        let caller = Location::caller();
        let collected: Vec<User> = users.into_iter().map(|u| u.borrow().clone()).collect();
        async move {
            let (leases, _) = acquire_dm_leases(&self.owner.core, collected, true, caller).await?;
            Ok(self.build_exclusive_group(leases, None, caller))
        }
    }

    /// Drop-out tolerant counterpart to [`Bot::exclusive_many`]: acquires per-user DM prompt locks
    /// in canonical [`crate::UserId`] order (preventing ABBA deadlocks across concurrent tasks)
    /// while skipping any user who is offline or quits during lock acquisition.
    #[track_caller]
    pub fn exclusive_available<I, U>(
        &self,
        users: I,
    ) -> impl Future<Output = Result<ExclusiveDmGroup>> + Send + '_
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        let caller = Location::caller();
        let collected: Vec<User> = users.into_iter().map(|u| u.borrow().clone()).collect();
        async move {
            let (leases, initial_offline) =
                acquire_dm_leases(&self.owner.core, collected, false, caller).await?;
            Ok(self.build_exclusive_group(leases, initial_offline, caller))
        }
    }

    fn build_exclusive_group(
        &self,
        leases: Vec<DmLease>,
        initial_offline: Option<(String, Option<String>)>,
        caller: &'static Location<'static>,
    ) -> ExclusiveDmGroup {
        ExclusiveDmGroup::new(
            Arc::clone(&self.owner.core),
            leases,
            initial_offline,
            caller,
        )
    }

    /// Strict channel-free group DM deliberation guard (`[<name>] <<nick>> <msg>`).
    #[track_caller]
    pub fn huddle<I, U>(
        &self,
        name: impl Into<irc_colors::Text>,
        users: I,
    ) -> impl Future<Output = Result<Huddle>> + Send + '_
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        let caller = Location::caller();
        let name = name.into();
        let collected: Vec<User> = users.into_iter().map(|u| u.borrow().clone()).collect();
        async move {
            let (leases, _) = acquire_dm_leases(&self.owner.core, collected, true, caller).await?;
            Ok(self.build_huddle(name, leases, None, caller))
        }
    }

    /// Pack-resilient channel-free group DM deliberation guard (skips offline/quitting users).
    #[track_caller]
    pub fn huddle_available<I, U>(
        &self,
        name: impl Into<irc_colors::Text>,
        users: I,
    ) -> impl Future<Output = Result<Huddle>> + Send + '_
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        let caller = Location::caller();
        let name = name.into();
        let collected: Vec<User> = users.into_iter().map(|u| u.borrow().clone()).collect();
        async move {
            let (leases, initial_offline) =
                acquire_dm_leases(&self.owner.core, collected, false, caller).await?;
            Ok(self.build_huddle(name, leases, initial_offline, caller))
        }
    }

    fn build_huddle(
        &self,
        name: irc_colors::Text,
        leases: Vec<DmLease>,
        initial_offline: Option<(String, Option<String>)>,
        caller: &'static Location<'static>,
    ) -> Huddle {
        Huddle::new(
            Arc::clone(&self.owner.core),
            name,
            leases,
            initial_offline,
            caller,
        )
    }

    pub async fn join(&self, channel: &str) -> Result<Room> {
        self.join_inner(channel, None, self.owner.core.handshake_timeout, JoinMode::Shared)
            .instrument(tracing::info_span!("ircgm::join", channel = channel))
            .await
    }

    pub async fn join_with_key(&self, channel: &str, key: &str) -> Result<Room> {
        validate_param(key, "channel key")?;
        self.join_inner(channel, Some(key), self.owner.core.handshake_timeout, JoinMode::Shared)
            .instrument(tracing::info_span!("ircgm::join", channel = channel))
            .await
    }

    async fn join_inner(
        &self,
        channel: &str,
        key: Option<&str>,
        timeout: Duration,
        mode: JoinMode,
    ) -> Result<Room> {
        validate_target(channel, "channel name")?;
        let core = &self.owner.core;
        core.check_connected()?;

        // Register as a join waiter (coalescing onto any in-flight join of the same channel)
        // under the same lock that checks for an already-synced or claimed channel:
        let (mut outcome_rx, send_join) = {
            let mut ws = core.state.write().unwrap();
            match mode {
                JoinMode::Shared => {
                    if let Some(existing) = ws.channel(channel).filter(|c| c.members().is_some()) {
                        return Ok(Room::new(Arc::clone(core), existing.name.clone()));
                    }
                }
                JoinMode::Exclusive => {
                    if ws.is_claimed(channel) {
                        return Err(Error::InvalidInput(
                            "breakout channel is already joined or being joined",
                        ));
                    }
                }
            }
            ws.await_join(channel, mode == JoinMode::Exclusive)?
        };

        if send_join {
            core.outbound.enqueue_one(Command::JOIN(
                channel.to_owned(),
                key.map(str::to_owned),
                None,
            ));
        }

        let outcome = tokio::select! {
            biased;
            reason = core.wait_disconnected() => return Err(Error::Disconnected(reason)),
            res = tokio::time::timeout(timeout, outcome_rx.wait_for(Option::is_some)) => match res {
                Err(_) => {
                    return Err(Error::HandshakeTimeout {
                        operation: "JOIN / NAMES",
                    });
                }
                // `WorldState::clear` dropped the sender: the session is gone.
                Ok(Err(_)) => {
                    return Err(Error::Disconnected(
                        core.disconnect_reason()
                            .unwrap_or(DisconnectReason::BotDropped),
                    ));
                }
                Ok(Ok(outcome)) => outcome.clone(),
            },
        };

        match outcome {
            Some(JoinOutcome::Joined { name }) => Ok(Room::new(Arc::clone(core), name)),
            Some(JoinOutcome::Failed {
                channel,
                code,
                message,
            }) => Err(Error::JoinFailed {
                channel,
                code,
                message,
            }),
            Some(JoinOutcome::Left { channel, reason }) => {
                Err(Error::BotLeftRoom { channel, reason })
            }
            None => unreachable!("wait_for(Option::is_some) only resolves on Some"),
        }
    }

    pub async fn breakout(&self, prefix: &str) -> Result<Breakout> {
        self.breakout_with(BreakoutOptions::new(prefix)).await
    }

    pub async fn breakout_with(&self, opts: BreakoutOptions) -> Result<Breakout> {
        struct BreakoutSetupGuard<'a> {
            core: &'a Arc<BotCore>,
            channel: Option<String>,
        }

        impl Drop for BreakoutSetupGuard<'_> {
            fn drop(&mut self) {
                if let Some(ch) = self.channel.take() {
                    if self.core.check_connected().is_ok() {
                        let part = Command::PART(ch.clone(), None);
                        self.core.enqueue_part(&ch, vec![part], true);
                    }
                }
            }
        }

        let chan_name = opts.naming.generate_channel_name()?;
        validate_target(&chan_name, "breakout channel")?;
        let core = &self.owner.core;
        core.check_connected()?;
        let span = tracing::info_span!("ircgm::breakout", channel = %chan_name);

        async {
            let mut rollback = BreakoutSetupGuard {
                core,
                channel: Some(chan_name.clone()),
            };

            let room = match self
                .join_inner(&chan_name, None, opts.setup_timeout, JoinMode::Exclusive)
                .await
            {
                Ok(room) => room,
                Err(e) => {
                    // Nothing to roll back if the room belongs to someone else, the server
                    // refused us, kicked us mid-join, or the session is gone:
                    if matches!(
                        e,
                        Error::InvalidInput(_)
                            | Error::JoinFailed { .. }
                            | Error::BotLeftRoom { .. }
                            | Error::Disconnected(_)
                    ) {
                        rollback.channel = None;
                    }
                    return Err(e);
                }
            };
            rollback.channel = Some(room.name().to_owned());

            if opts.require_op && !room.bot_is_op() {
                return Err(Error::NotChannelOp {
                    channel: room.name().to_owned(),
                });
            }

            if let Some(ref mode_flags) = opts.modes {
                room.set_mode(mode_flags, &[])?;
            }

            rollback.channel = None;
            Ok(Breakout::new(room, opts))
        }
        .instrument(span)
        .await
    }

    pub fn send_raw(&self, cmd: impl Into<Command>) -> Result<()> {
        let core = &self.owner.core;
        core.check_connected()?;
        core.outbound.enqueue_one(cmd.into());
        Ok(())
    }

    /// Opens an [`Inbox`] receiving all events across all joined channels and DMs on this session.
    #[track_caller]
    pub fn inbox(&self) -> Inbox {
        let caller = Location::caller();
        let core = &self.owner.core;
        core.inboxes.subscribe(Arc::clone(core), Filter::All, caller)
    }

    /// Opens an [`Inbox`] receiving all DMs sent to the bot from any user.
    #[track_caller]
    pub fn inbox_dms(&self) -> Inbox {
        let caller = Location::caller();
        let core = &self.owner.core;
        core.inboxes
            .subscribe(Arc::clone(core), Filter::AllDms, caller)
    }

    /// Opens an [`Inbox`] receiving DMs sent to the bot from `users` (matching [`InboxScope::dms_from`]).
    #[track_caller]
    pub fn inbox_dms_from<I, U>(&self, users: I) -> Inbox
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        let caller = Location::caller();
        let scope = InboxScope::new().dms_from(users);
        self.inbox_scope_at(scope, caller)
    }

    #[track_caller]
    pub fn inbox_rooms<I, R>(&self, rooms: I) -> Inbox
    where
        I: IntoIterator<Item = R>,
        R: Borrow<Room>,
    {
        let caller = Location::caller();
        let scope = InboxScope::new().rooms(rooms);
        self.inbox_scope_at(scope, caller)
    }

    #[track_caller]
    pub fn inbox_scope(&self, scope: InboxScope) -> Inbox {
        let caller = Location::caller();
        self.inbox_scope_at(scope, caller)
    }

    fn inbox_scope_at(&self, scope: InboxScope, caller: &'static Location<'static>) -> Inbox {
        let core = &self.owner.core;
        let all_rooms_belong = scope.rooms.iter().all(|r| r.belongs_to(core));
        let all_users_belong = scope.dm_users.iter().all(|u| u.belongs_to(core));
        let rooms: Vec<String> = scope.rooms.into_iter().map(Room::into_name).collect();
        let dm_users: Vec<Arc<UserTrack>> = scope.dm_users.iter().map(User::track_arc).collect();
        let filter = Filter::Scoped { rooms, dm_users };
        if !all_rooms_belong || !all_users_belong {
            return Inbox::invalid_session(filter, caller);
        }
        core.inboxes.subscribe(Arc::clone(core), filter, caller)
    }

    pub fn request_shutdown(&self) {
        let _ = self.owner.core.shutdown_tx.send_replace(true);
    }

    pub fn is_shutting_down(&self) -> bool {
        *self.owner.core.shutdown_tx.borrow()
    }

    /// Resolves to `()` as soon as [`Bot::request_shutdown`] or [`Bot::quit`] is called.
    pub fn shutdown_requested(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut rx = self.owner.core.shutdown_tx.subscribe();
        async move {
            while !*rx.borrow_and_update() {
                if rx.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        }
    }

    pub async fn quit(&self, reason: Option<&str>) -> Result<()> {
        let core = &self.owner.core;
        if let Some(DisconnectReason::Quit) = core.disconnect_reason() {
            return Ok(());
        }
        core.check_connected()?;

        // 1. Signal cooperative shutdown and yield so ready tasks enqueue farewell `say` / `Breakout::drop`:
        self.request_shutdown();
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // 2. Send `QUIT` and best-effort wait (within `handshake_timeout`) for the peer to close
        // the connection so live TCP sockets flush `QUIT` cleanly:
        let deadline = crate::future::deadline_after(core.handshake_timeout);
        core.outbound.send_quit(reason);
        let _ = tokio::time::timeout_at(deadline, core.wait_disconnected()).await;
        core.finalize_quit();
        Ok(())
    }
}

#[cfg(test)]
#[path = "bot_test.rs"]
mod test;

