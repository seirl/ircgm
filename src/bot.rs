use std::borrow::Borrow;
use std::future::Future;
use std::panic::Location;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use irc::proto::{CapSubCommand, Command};
use irc_colors::Text;
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;
use tracing::Instrument as _;

use crate::ask::AskConfig;
use crate::breakout::{Breakout, BreakoutOptions};
use crate::casemap::CaseMap;
use crate::error::{DisconnectReason, Error, Result};
use crate::exclusive_dm::{acquire_available_dm_guards, acquire_many_dm_guards, ExclusiveDm};
use crate::huddle::Huddle;
use crate::inbox::{Filter, Inbox, InboxRegistry, InboxScope};
use crate::outbound::{
    build_chat_commands, validate_param, validate_target, OutboundChatKind, OutboundSink, Priority,
    RawTransport, WriteReceipt,
};
use crate::pump::{extract_numeric, run_pump, PumpSource, RegistrationTracker};
use crate::room::Room;
use crate::state::WorldState;
use crate::user::{User, UserRegistry, UserTrack};

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// Configuration for [`Bot::connect_with`] and [`crate::testing::test_harness_with`].
#[derive(Clone, Debug)]
pub struct BotConfig {
    pub irc: irc::client::data::Config,
    /// Per-inbox bounded event queue size (default: `1024`).
    pub inbox_capacity: usize,
    /// IRCv3 capabilities requested via `CAP REQ` on connect.
    pub request_caps: Vec<String>,
    /// Command prefixes skipped when `.ignore_commands()` is opted into (default: `["!"]`).
    pub command_prefixes: Vec<String>,
    /// Default timeout for protocol handshakes like `JOIN`/`NAMES`, `Breakout`, and `quit` drain (default: `15s`).
    pub handshake_timeout: Duration,
    /// Startup bot-wide [`AskConfig`] defaults inherited by `Room`, `Breakout`, `User::ask`, and `ExclusiveDm`.
    pub ask_config: AskConfig,
    /// Upper ceiling on payload bytes per outbound `PRIVMSG`/`NOTICE` passed to `Text::wrap` (default: `450`).
    pub max_payload_bytes: usize,
    /// Outbound scheduler burst allowance (default: `15` lines per `8s` on live connections).
    pub outbound_burst: usize,
    /// Outbound scheduler rolling window (default: `8s`).
    pub outbound_window: Duration,
}

impl Default for BotConfig {
    fn default() -> Self {
        Self::new(irc::client::data::Config::default())
    }
}

impl BotConfig {
    pub fn new(irc: irc::client::data::Config) -> Self {
        Self {
            irc,
            inbox_capacity: 1024,
            request_caps: vec![
                "account-tag".into(),
                "account-notify".into(),
                "extended-join".into(),
                "multi-prefix".into(),
                "server-time".into(),
                "userhost-in-names".into(),
            ],
            command_prefixes: vec!["!".into()],
            handshake_timeout: Duration::from_secs(15),
            ask_config: AskConfig::default(),
            max_payload_bytes: 450,
            outbound_burst: 15,
            outbound_window: Duration::from_secs(8),
        }
    }

    pub fn with_ask_config(mut self, cfg: AskConfig) -> Self {
        self.ask_config = cfg;
        self
    }

    pub fn with_command_prefixes<I, S>(mut self, prefixes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.command_prefixes = prefixes.into_iter().map(Into::into).collect();
        self
    }
}

pub(crate) struct BotCore {
    pub(crate) session_id: u64,
    pub(crate) state: RwLock<WorldState>,
    pub(crate) users: UserRegistry,
    pub(crate) inboxes: InboxRegistry,
    pub(crate) outbound: Arc<OutboundSink>,
    pub(crate) is_test_transport: bool,
    pub(crate) shutdown_tx: watch::Sender<bool>,
    pub(crate) registered_tx: watch::Sender<bool>,
    pub(crate) registered_flag: AtomicBool,
    pub(crate) disconnect_reason: Mutex<Option<DisconnectReason>>,
    pub(crate) disconnect_notify: Notify,
    pub(crate) inbox_capacity: usize,
    pub(crate) command_prefixes: Vec<String>,
    pub(crate) handshake_timeout: Duration,
    pub(crate) ask_config: Arc<AskConfig>,
    pub(crate) max_payload_bytes: usize,
}

impl std::fmt::Debug for BotCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotCore")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl BotCore {
    pub(crate) fn new(
        initial_nick: String,
        outbound: Arc<OutboundSink>,
        is_test_transport: bool,
        config: &BotConfig,
    ) -> Arc<Self> {
        let session_id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        let (shutdown_tx, _) = watch::channel(false);
        let (registered_tx, _) = watch::channel(false);
        Arc::new(Self {
            session_id,
            state: RwLock::new(WorldState::new(initial_nick)),
            users: UserRegistry::new(CaseMap::default()),
            inboxes: InboxRegistry::default(),
            outbound,
            is_test_transport,
            shutdown_tx,
            registered_tx,
            registered_flag: AtomicBool::new(false),
            disconnect_reason: Mutex::new(None),
            disconnect_notify: Notify::new(),
            inbox_capacity: config.inbox_capacity.max(1),
            command_prefixes: config.command_prefixes.clone(),
            handshake_timeout: config.handshake_timeout,
            ask_config: Arc::new(config.ask_config.clone()),
            max_payload_bytes: config.max_payload_bytes.max(1),
        })
    }

    pub(crate) fn casemap(&self) -> CaseMap {
        self.users.casemap()
    }

    pub(crate) fn disconnect_reason(&self) -> Option<DisconnectReason> {
        self.disconnect_reason.lock().unwrap().clone()
    }

    pub(crate) fn check_connected(&self) -> Result<()> {
        if let Some(reason) = self.disconnect_reason() {
            return Err(Error::Disconnected(reason));
        }
        Ok(())
    }

    pub(crate) fn mark_registered(&self) {
        self.registered_flag.store(true, Ordering::Release);
        let _ = self.registered_tx.send_replace(true);
    }

    pub(crate) fn mark_disconnected_if_unset(&self, reason: DisconnectReason) {
        let mut guard = self.disconnect_reason.lock().unwrap();
        if guard.is_none() {
            *guard = Some(reason);
            drop(guard);
            self.disconnect_notify.notify_waiters();
        }
    }

    pub(crate) async fn wait_disconnected(&self) -> DisconnectReason {
        loop {
            let notified = self.disconnect_notify.notified();
            if let Some(reason) = self.disconnect_reason() {
                return reason;
            }
            notified.await;
        }
    }

    pub(crate) fn is_command_text(&self, plain_text: &str) -> bool {
        let trimmed = plain_text.trim_start();
        if trimmed.is_empty() {
            return false;
        }
        self.command_prefixes
            .iter()
            .any(|p| !p.is_empty() && trimmed.starts_with(p.as_str()))
    }

    pub(crate) fn send_chat_with_receipt(
        &self,
        kind: OutboundChatKind,
        target: &str,
        text: Text,
    ) -> Result<WriteReceipt> {
        self.check_connected()?;
        validate_target(target, "message target")?;
        let (cmds, cm) = {
            let ws = self.state.read().unwrap();
            let cmds = build_chat_commands(kind, target, text, &ws, self.max_payload_bytes);
            (cmds, ws.casemap)
        };
        Ok(self.outbound.enqueue_batch(Priority::Interactive, cmds, cm))
    }
}

pub(crate) struct BotOwner {
    pub(crate) core: Arc<BotCore>,
    pub(crate) pump_task: Mutex<Option<JoinHandle<()>>>,
    pub(crate) outbound_task: Mutex<Option<JoinHandle<()>>>,
    /// Keeps `irc::client::Client` alive for the lifetime of live TCP/TLS sessions so its
    /// background outgoing writer task does not drop prematurely.
    pub(crate) _live_client: Mutex<Option<irc::client::Client>>,
}

impl std::fmt::Debug for BotOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotOwner")
            .field("session_id", &self.core.session_id)
            .finish_non_exhaustive()
    }
}

impl Drop for BotOwner {
    fn drop(&mut self) {
        self.core
            .mark_disconnected_if_unset(DisconnectReason::BotDropped);
        self.core.outbound.abort();

        if let Some(handle) = self.pump_task.lock().unwrap().take() {
            handle.abort();
        }
        if let Some(handle) = self.outbound_task.lock().unwrap().take() {
            handle.abort();
        }

        self.core.state.write().unwrap().clear();
        self.core.inboxes.clear();
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
        config.irc.max_messages_in_burst = Some(u32::MAX);
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

        let mut client = irc::client::Client::from_config(config.irc.clone()).await?;
        let stream = client.stream()?;
        let sender = client.sender();

        let (outbound, outbound_task) = OutboundSink::new(
            RawTransport::Irc(sender),
            config.outbound_burst,
            config.outbound_window,
        );
        let core = BotCore::new(nick.clone(), outbound, false, &config);

        let has_caps = !config.request_caps.is_empty();
        let reg = RegistrationTracker {
            cap_pending: has_caps,
            motd_done: false,
        };

        let pump_core = Arc::clone(&core);
        let pump_task = tokio::spawn(async move {
            run_pump(pump_core, PumpSource::Irc(Box::new(stream)), reg).await;
        });

        let bot = Self {
            owner: Arc::new(BotOwner {
                core: Arc::clone(&core),
                pump_task: Mutex::new(Some(pump_task)),
                outbound_task: Mutex::new(outbound_task),
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
        let _ = core
            .outbound
            .enqueue_batch(Priority::Interactive, init_cmds, core.casemap());

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

    /// Awaits `CAP` completion + `376`/`422`, falling back to `handshake_timeout` if a non-standard
    /// server omits them (propagating `Disconnected` if the stream closes early).
    pub(crate) async fn wait_for_registration(&self) -> Result<()> {
        let core = &self.owner.core;
        if core.registered_flag.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut reg_rx = core.registered_tx.subscribe();
        let sleep = tokio::time::sleep(core.handshake_timeout);
        tokio::pin!(sleep);

        loop {
            if *reg_rx.borrow_and_update() {
                return Ok(());
            }
            tokio::select! {
                biased;
                disc = core.wait_disconnected() => {
                    return Err(Error::Disconnected(disc));
                }
                res = reg_rx.changed() => {
                    if res.is_ok() && *reg_rx.borrow_and_update() {
                        return Ok(());
                    }
                }
                _ = &mut sleep => {
                    tracing::warn!("registration barrier timed out waiting for CAP/376/422; proceeding");
                    let _ = core.outbound.enqueue_one(
                        Priority::Interactive,
                        Command::CAP(None, CapSubCommand::END, None, None),
                        core.casemap(),
                    );
                    core.mark_registered();
                    return Ok(());
                }
            }
        }
    }

    /// Monotonic process-unique connection session ID (`1, 2, 3, ...`).
    pub fn session_id(&self) -> u64 {
        self.owner.core.session_id
    }

    pub fn nick(&self) -> String {
        self.owner.core.state.read().unwrap().current_nick.clone()
    }

    pub fn casemap(&self) -> CaseMap {
        self.owner.core.casemap()
    }

    pub fn room(&self, channel: &str) -> Result<Room> {
        validate_target(channel, "channel name")?;
        let core = &self.owner.core;
        Ok(Room::new(
            Arc::clone(core),
            channel.to_owned(),
            Arc::clone(&core.ask_config),
        ))
    }

    pub fn user(&self, nick: &str) -> Result<User> {
        validate_target(nick, "nickname")?;
        let core = &self.owner.core;
        let track = core.users.get_or_create(nick);
        Ok(User::from_parts(Arc::downgrade(core), track))
    }

    /// Strict all-or-nothing multi-user DM lock in canonical `UserTrack::id` order, returning
    /// guards in the caller's iteration order.
    #[track_caller]
    pub fn exclusive_many<I, U>(
        &self,
        users: I,
    ) -> impl Future<Output = Result<Vec<ExclusiveDm>>> + Send + '_
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        let caller = Location::caller();
        let collected: Vec<User> = users.into_iter().map(|u| u.borrow().clone()).collect();
        acquire_many_dm_guards(&self.owner.core, collected, caller)
    }

    /// Partial/team-resilient multi-user DM lock: skips any `User` who is offline or quits
    /// during lock acquisition, returning online guards in caller relative order.
    #[track_caller]
    pub fn exclusive_available<I, U>(
        &self,
        users: I,
    ) -> impl Future<Output = Result<Vec<ExclusiveDm>>> + Send + '_
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        let caller = Location::caller();
        let collected: Vec<User> = users.into_iter().map(|u| u.borrow().clone()).collect();
        async move {
            let (guards, _) =
                acquire_available_dm_guards(&self.owner.core, collected, caller).await?;
            Ok(guards)
        }
    }

    /// Strict channel-free group DM deliberation guard.
    #[track_caller]
    pub fn huddle<I, U>(
        &self,
        users: I,
    ) -> impl Future<Output = Result<Huddle>> + Send + '_
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        let caller = Location::caller();
        let collected: Vec<User> = users.into_iter().map(|u| u.borrow().clone()).collect();
        async move {
            let guards = acquire_many_dm_guards(&self.owner.core, collected, caller).await?;
            Ok(self.build_huddle(guards, None, caller))
        }
    }

    /// Pack-resilient channel-free group DM deliberation guard (skips offline/quitting users).
    #[track_caller]
    pub fn huddle_available<I, U>(
        &self,
        users: I,
    ) -> impl Future<Output = Result<Huddle>> + Send + '_
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        let caller = Location::caller();
        let collected: Vec<User> = users.into_iter().map(|u| u.borrow().clone()).collect();
        async move {
            let (guards, initial_offline) =
                acquire_available_dm_guards(&self.owner.core, collected, caller).await?;
            Ok(self.build_huddle(guards, initial_offline, caller))
        }
    }

    fn build_huddle(
        &self,
        guards: Vec<ExclusiveDm>,
        initial_offline: Option<(String, Option<String>)>,
        caller: &'static Location<'static>,
    ) -> Huddle {
        let core = &self.owner.core;
        let dm_users: Vec<Arc<UserTrack>> = guards.iter().map(|g| g.user().track_arc()).collect();
        let cm = core.casemap();
        let inbox = core.inboxes.subscribe(
            Arc::clone(core),
            Filter::Scoped {
                rooms: Vec::new(),
                dm_users,
            },
            caller,
            cm,
        );
        Huddle::new(
            Arc::clone(core),
            guards,
            initial_offline,
            inbox,
            (*core.ask_config).clone(),
        )
    }

    pub async fn join(&self, channel: &str) -> Result<Room> {
        self.join_inner(channel, None, self.owner.core.handshake_timeout)
            .instrument(tracing::info_span!("ircgm::join", channel = channel))
            .await
    }

    pub async fn join_with_key(&self, channel: &str, key: &str) -> Result<Room> {
        validate_param(key, "channel key")?;
        self.join_inner(channel, Some(key), self.owner.core.handshake_timeout)
            .instrument(tracing::info_span!("ircgm::join", channel = channel))
            .await
    }

    async fn join_inner(
        &self,
        channel: &str,
        key: Option<&str>,
        timeout: Duration,
    ) -> Result<Room> {
        validate_target(channel, "channel name")?;
        let core = &self.owner.core;
        core.check_connected()?;

        {
            let ws = core.state.read().unwrap();
            let ckey = ws.casemap.fold(channel);
            if let Some(existing) = ws.channels.get(&ckey) {
                if existing.names_synced {
                    return Ok(Room::new(
                        Arc::clone(core),
                        existing.name.clone(),
                        Arc::clone(&core.ask_config),
                    ));
                }
            }
        }

        // Subscribe raw channel inbox BEFORE sending JOIN:
        let cm = core.casemap();
        let mut inbox = core.inboxes.subscribe(
            Arc::clone(core),
            Filter::Scoped {
                rooms: vec![channel.to_owned()],
                dm_users: Vec::new(),
            },
            Location::caller(),
            cm,
        );

        let _ = core.outbound.enqueue_one(
            Priority::Interactive,
            Command::JOIN(channel.to_owned(), key.map(str::to_owned), None),
            cm,
        );

        let sleep = tokio::time::sleep(timeout);
        tokio::pin!(sleep);

        loop {
            let raw_res = tokio::select! {
                biased;
                r = inbox.next_raw() => r,
                _ = &mut sleep => {
                    return Err(Error::HandshakeTimeout {
                        operation: "JOIN / NAMES",
                    });
                }
            };

            let raw = match raw_res {
                Ok(m) => m,
                Err(Error::BotLeftRoom { .. }) => {
                    // Could happen if a previous `lost_channels` entry existed before our self `JOIN` arrived:
                    // keep waiting until our new `JOIN` + `366` or timeout!
                    let cm_now = core.casemap();
                    inbox = core.inboxes.subscribe(
                        Arc::clone(core),
                        Filter::Scoped {
                            rooms: vec![channel.to_owned()],
                            dm_users: Vec::new(),
                        },
                        Location::caller(),
                        cm_now,
                    );
                    continue;
                }
                Err(e) => return Err(e),
            };

            // Check if 366 synced the channel or if a join-failure numeric arrived:
            if let Some((code, args)) = extract_numeric(&raw.command) {
                let live_cm = core.casemap();
                match code {
                    366 => {
                        let synced_name = {
                            let ws = core.state.read().unwrap();
                            ws.channels
                                .get(&ws.casemap.fold(channel))
                                .filter(|c| c.names_synced)
                                .map(|c| c.name.clone())
                        };
                        if let Some(canonical_name) = synced_name {
                            return Ok(Room::new(
                                Arc::clone(core),
                                canonical_name,
                                Arc::clone(&core.ask_config),
                            ));
                        }
                    }
                    403 | 405 | 471 | 473 | 474 | 475 | 476 | 477 => {
                        if let Some(target_ch) = args.get(1) {
                            if live_cm.str_eq(target_ch, channel) {
                                let message = args
                                    .last()
                                    .cloned()
                                    .unwrap_or_else(|| format!("join failed ({code})"));
                                return Err(Error::JoinFailed {
                                    channel: target_ch.clone(),
                                    code,
                                    message,
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    pub async fn breakout(&self, prefix: &str) -> Result<Breakout> {
        self.breakout_with(BreakoutOptions::new(prefix)).await
    }

    pub async fn breakout_with(&self, opts: BreakoutOptions) -> Result<Breakout> {
        let chan_name = opts.naming.generate_channel_name()?;
        validate_target(&chan_name, "breakout channel")?;
        let span = tracing::info_span!("ircgm::breakout", channel = %chan_name);

        async {
            let mut room = self
                .join_inner(&chan_name, None, opts.setup_timeout)
                .await?;

            if opts.require_op && !room.bot_is_op() {
                let _ = room.part(None);
                return Err(Error::NotChannelOp {
                    channel: room.name().to_owned(),
                });
            }

            if let Some(ref mode_flags) = opts.modes {
                if let Err(e) = room.set_mode(mode_flags, &[]) {
                    let _ = room.part(None);
                    return Err(e);
                }
            }

            if let Some(ref cfg) = opts.ask_config {
                room = room.with_ask_config(cfg);
            }

            Ok(Breakout::new(room, opts))
        }
        .instrument(span)
        .await
    }

    pub fn send_raw(&self, cmd: impl Into<Command>) -> Result<()> {
        let core = &self.owner.core;
        core.check_connected()?;
        let _ = core
            .outbound
            .enqueue_one(Priority::Interactive, cmd.into(), core.casemap());
        Ok(())
    }

    /// Opens an [`Inbox`] receiving all events across all joined channels and DMs on this session.
    #[track_caller]
    pub fn inbox(&self) -> Inbox {
        let caller = Location::caller();
        let core = &self.owner.core;
        core.inboxes
            .subscribe(Arc::clone(core), Filter::All, caller, core.casemap())
    }

    /// Opens an [`Inbox`] receiving all DMs sent to the bot from any user.
    #[track_caller]
    pub fn inbox_dms(&self) -> Inbox {
        let caller = Location::caller();
        let core = &self.owner.core;
        core.inboxes
            .subscribe(Arc::clone(core), Filter::AllDms, caller, core.casemap())
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
    pub fn inbox_rooms<'a, I: IntoIterator<Item = &'a Room>>(&self, rooms: I) -> Inbox {
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
        let all_rooms_belong = scope
            .rooms
            .iter()
            .all(|(_, sid)| *sid == core.session_id);
        let all_users_belong = scope.dm_users.iter().all(|u| u.belongs_to(core));
        let rooms: Vec<String> = scope.rooms.into_iter().map(|(r, _)| r).collect();
        let dm_users: Vec<Arc<UserTrack>> = scope.dm_users.iter().map(User::track_arc).collect();
        let filter = Filter::Scoped { rooms, dm_users };
        if !all_rooms_belong || !all_users_belong {
            return Inbox::invalid_session(filter, caller);
        }
        core.inboxes
            .subscribe(Arc::clone(core), filter, caller, core.casemap())
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

        // 2. Drain both Interactive and Background queues followed by QUIT:
        let quit_receipt = core.outbound.enqueue_quit(reason, core.casemap());

        let timeout = core.handshake_timeout;
        if core.is_test_transport {
            tokio::time::timeout(timeout, quit_receipt.wait())
                .await
                .map_err(|_| Error::HandshakeTimeout {
                    operation: "quit outbound drain",
                })?;
            core.mark_disconnected_if_unset(DisconnectReason::Quit);
            core.inboxes.clear();
            return Ok(());
        }

        let drain_and_close = async {
            quit_receipt.wait().await;
            core.mark_disconnected_if_unset(DisconnectReason::Quit);
            let _ = core.wait_disconnected().await;
        };
        let _ = tokio::time::timeout(timeout, drain_and_close).await;
        core.mark_disconnected_if_unset(DisconnectReason::Quit);
        core.inboxes.clear();
        Ok(())
    }
}

#[cfg(test)]
#[path = "bot_test.rs"]
mod test;

