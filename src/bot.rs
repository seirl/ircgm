use std::borrow::Borrow;
use std::future::Future;
use std::panic::Location;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use irc::proto::{CapSubCommand, Command};
use irc_colors::Text;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::Instrument as _;

use crate::ask::AskConfig;
use crate::breakout::{Breakout, BreakoutOptions};
use crate::casemap::{AtomicCaseMap, CaseMap};
use crate::error::{DisconnectReason, Error, Result};
use crate::exclusive_dm::{acquire_dm_leases, DmLease, ExclusiveDm};
use crate::huddle::Huddle;
use crate::inbox::{Filter, Inbox, InboxRegistry, InboxScope};
use crate::outbound::{
    build_chat_commands, validate_param, validate_target, OutboundChatKind, OutboundSink, Priority,
    RawTransport, WriteReceipt,
};
use crate::pump::{run_pump, PumpSource, RegistrationTracker};
use crate::room::Room;
use crate::state::{JoinOutcome, WorldState};
use crate::user::{User, UserRegistry, UserTrack};

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConnLifecycle {
    Registering { welcomed: bool },
    Active,
    Disconnected(DisconnectReason),
}

pub(crate) struct BotCore {
    pub(crate) casemap: AtomicCaseMap,
    pub(crate) state: RwLock<WorldState>,
    pub(crate) users: UserRegistry,
    pub(crate) inboxes: InboxRegistry,
    pub(crate) outbound: Arc<OutboundSink>,
    pub(crate) is_test_transport: bool,
    pub(crate) lifecycle_tx: watch::Sender<ConnLifecycle>,
    pub(crate) shutdown_tx: watch::Sender<bool>,
    pub(crate) inbox_capacity: usize,
    pub(crate) command_prefixes: Vec<String>,
    pub(crate) handshake_timeout: Duration,
    pub(crate) ask_config: Arc<AskConfig>,
    pub(crate) max_payload_bytes: usize,
}

impl std::fmt::Debug for BotCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotCore")
            .field("ptr", &std::ptr::from_ref(self))
            .finish_non_exhaustive()
    }
}

impl BotCore {
    pub(crate) fn new(
        initial_nick: String,
        casemap: AtomicCaseMap,
        outbound: Arc<OutboundSink>,
        is_test_transport: bool,
        config: &BotConfig,
    ) -> Arc<Self> {
        let (lifecycle_tx, _) = watch::channel(ConnLifecycle::Registering { welcomed: false });
        let (shutdown_tx, _) = watch::channel(false);
        Arc::new(Self {
            state: RwLock::new(WorldState::new(initial_nick, casemap.clone())),
            users: UserRegistry::new(casemap.clone()),
            inboxes: InboxRegistry::new(casemap.clone()),
            casemap,
            outbound,
            is_test_transport,
            lifecycle_tx,
            shutdown_tx,
            inbox_capacity: config.inbox_capacity.max(1),
            command_prefixes: config.command_prefixes.clone(),
            handshake_timeout: config.handshake_timeout,
            ask_config: Arc::new(config.ask_config.clone()),
            max_payload_bytes: config.max_payload_bytes.max(1),
        })
    }

    pub(crate) fn casemap(&self) -> CaseMap {
        self.casemap.load()
    }

    pub(crate) fn disconnect_reason(&self) -> Option<DisconnectReason> {
        match &*self.lifecycle_tx.borrow() {
            ConnLifecycle::Disconnected(reason) => Some(reason.clone()),
            ConnLifecycle::Registering { .. } | ConnLifecycle::Active => None,
        }
    }

    pub(crate) fn check_connected(&self) -> Result<()> {
        if let Some(reason) = self.disconnect_reason() {
            return Err(Error::Disconnected(reason));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn is_registered(&self) -> bool {
        matches!(*self.lifecycle_tx.borrow(), ConnLifecycle::Active)
    }

    pub(crate) fn mark_welcomed(&self) {
        self.lifecycle_tx.send_if_modified(|state| match state {
            ConnLifecycle::Registering { welcomed } if !*welcomed => {
                *welcomed = true;
                true
            }
            _ => false,
        });
    }

    pub(crate) fn mark_registered(&self) {
        self.lifecycle_tx.send_if_modified(|state| match state {
            ConnLifecycle::Registering { .. } => {
                *state = ConnLifecycle::Active;
                true
            }
            _ => false,
        });
    }

    pub(crate) fn mark_disconnected(&self, reason: DisconnectReason) {
        let transitioned = self.lifecycle_tx.send_if_modified(|state| match state {
            ConnLifecycle::Disconnected(_) => false,
            ConnLifecycle::Registering { .. } | ConnLifecycle::Active => {
                *state = ConnLifecycle::Disconnected(reason);
                true
            }
        });
        if transitioned {
            self.outbound.abort();
            self.inboxes.clear();
        }
    }

    pub(crate) fn finalize_quit(&self) {
        let _ = self.shutdown_tx.send_replace(true);
        let mut was_connected = false;
        self.lifecycle_tx.send_if_modified(|state| match state {
            ConnLifecycle::Disconnected(reason) => {
                if *reason == DisconnectReason::Quit {
                    false
                } else {
                    *reason = DisconnectReason::Quit;
                    true
                }
            }
            ConnLifecycle::Registering { .. } | ConnLifecycle::Active => {
                was_connected = true;
                *state = ConnLifecycle::Disconnected(DisconnectReason::Quit);
                true
            }
        });
        if was_connected {
            self.outbound.abort();
            self.inboxes.clear();
        }
    }

    pub(crate) async fn wait_disconnected(&self) -> DisconnectReason {
        let mut rx = self.lifecycle_tx.subscribe();
        loop {
            if let ConnLifecycle::Disconnected(reason) = rx.borrow_and_update().clone() {
                return reason;
            }
            if rx.changed().await.is_err() {
                return DisconnectReason::BotDropped;
            }
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

    /// Enqueues `cmds` (ending with our own `PART channel`) and marks `channel` `Parting` under
    /// the `WorldState` write lock, so a concurrent `Bot::join` either completes before the
    /// `PART` is queued or sees `Parting` and queues its `JOIN` behind the `PART`.
    /// `evict_on_leave` is `true` only for ephemeral `Breakout` teardown (see
    /// `ChannelPhase::Parting`); `Room::part` keeps a `Left` record.
    pub(crate) fn enqueue_part(
        &self,
        channel: &str,
        priority: Priority,
        cmds: Vec<Command>,
        evict_on_leave: bool,
    ) {
        let mut ws = self.state.write().unwrap();
        ws.mark_parting(channel, evict_on_leave);
        let _ = self.outbound.enqueue_batch(priority, cmds);
    }

    /// Enqueues a validated, line-wrapped outbound chat message and returns its [`WriteReceipt`].
    pub(crate) fn send_chat(
        &self,
        kind: OutboundChatKind,
        target: &str,
        text: Text,
    ) -> Result<WriteReceipt> {
        self.check_connected()?;
        validate_target(target, "message target")?;
        let ws = self.state.read().unwrap();
        let cmds = build_chat_commands(kind, target, text, &ws, self.max_payload_bytes);
        drop(ws);
        Ok(self.outbound.enqueue_batch(Priority::Interactive, cmds))
    }
}

pub(crate) struct BotOwner {
    pub(crate) core: Arc<BotCore>,
    pub(crate) pump_task: JoinHandle<()>,
    pub(crate) outbound_task: Option<JoinHandle<()>>,
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
        if let Some(ref handle) = self.outbound_task {
            handle.abort();
        }

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

        let casemap = AtomicCaseMap::default();
        let (outbound, outbound_task) = OutboundSink::new(
            RawTransport::Irc(sender),
            config.outbound_burst,
            config.outbound_window,
            casemap.clone(),
        );
        let core = BotCore::new(nick.clone(), casemap, outbound, false, &config);

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
                pump_task,
                outbound_task,
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
            .enqueue_batch(Priority::Interactive, init_cmds);

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
                            let _ = core.outbound.enqueue_one(
                                Priority::Interactive,
                                Command::CAP(None, CapSubCommand::END, None, None),
                            );
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

    /// The session-wide [`AskConfig`] (`BotConfig::ask_config`) that new rooms, DMs, and
    /// huddles start from. Clone it to derive a tweaked config for one handle.
    pub fn ask_config(&self) -> &AskConfig {
        &self.owner.core.ask_config
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
        async move {
            let (leases, _) = acquire_dm_leases(&self.owner.core, collected, true, caller).await?;
            Ok(self.wrap_exclusive(leases, caller))
        }
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
            let (leases, _) = acquire_dm_leases(&self.owner.core, collected, false, caller).await?;
            Ok(self.wrap_exclusive(leases, caller))
        }
    }

    fn wrap_exclusive(
        &self,
        leases: Vec<DmLease>,
        caller: &'static Location<'static>,
    ) -> Vec<ExclusiveDm> {
        let core = &self.owner.core;
        leases
            .into_iter()
            .map(|lease| ExclusiveDm::from_lease(Arc::clone(core), lease, caller))
            .collect()
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
            let (leases, _) = acquire_dm_leases(&self.owner.core, collected, true, caller).await?;
            Ok(self.build_huddle(leases, None, caller))
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
            let (leases, initial_offline) =
                acquire_dm_leases(&self.owner.core, collected, false, caller).await?;
            Ok(self.build_huddle(leases, initial_offline, caller))
        }
    }

    fn build_huddle(
        &self,
        leases: Vec<DmLease>,
        initial_offline: Option<(String, Option<String>)>,
        caller: &'static Location<'static>,
    ) -> Huddle {
        let core = &self.owner.core;
        let dm_users: Vec<Arc<UserTrack>> = leases.iter().map(|l| l.user().track_arc()).collect();
        let inbox = core.inboxes.subscribe(
            Arc::clone(core),
            Filter::Scoped {
                rooms: Vec::new(),
                dm_users,
            },
            caller,
        );
        Huddle::new(
            Arc::clone(core),
            leases,
            initial_offline,
            inbox,
            (*core.ask_config).clone(),
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
                        return Ok(Room::new(
                            Arc::clone(core),
                            existing.name.clone(),
                            Arc::clone(&core.ask_config),
                        ));
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
            let _ = core.outbound.enqueue_one(
                Priority::Interactive,
                Command::JOIN(channel.to_owned(), key.map(str::to_owned), None),
            );
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
            Some(JoinOutcome::Joined { name }) => Ok(Room::new(
                Arc::clone(core),
                name,
                Arc::clone(&core.ask_config),
            )),
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
                        self.core
                            .enqueue_part(&ch, Priority::Background, vec![part], true);
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
        let _ = core
            .outbound
            .enqueue_one(Priority::Interactive, cmd.into());
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

        // 2. Drain both Interactive and Background queues followed by QUIT:
        let quit_receipt = core.outbound.enqueue_quit(reason);

        let timeout = core.handshake_timeout;
        if core.is_test_transport {
            tokio::time::timeout(timeout, quit_receipt.wait())
                .await
                .map_err(|_| Error::HandshakeTimeout {
                    operation: "quit outbound drain",
                })?;
            core.finalize_quit();
            return Ok(());
        }

        let drain_and_close = async {
            quit_receipt.wait().await;
            let _ = core.wait_disconnected().await;
        };
        let _ = tokio::time::timeout(timeout, drain_and_close).await;
        core.finalize_quit();
        Ok(())
    }
}

#[cfg(test)]
#[path = "bot_test.rs"]
mod test;

