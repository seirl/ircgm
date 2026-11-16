use std::sync::{Arc, RwLock};
use std::time::Duration;

use irc::proto::{Command, Prefix};
use irc_colors::Text;
use tokio::sync::watch;

use crate::bot::BotConfig;
use crate::casemap::{AtomicCaseMap, CaseMap};
use crate::error::{DisconnectReason, Error, Result};
use crate::inbox::InboxRegistry;
use crate::message::BounceScope;
use crate::outbound::{build_chat_commands, validate_target, OutboundChatKind, OutboundSink};
use crate::state::WorldState;
use crate::user::{User, UserRegistry};

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
    pub(crate) lifecycle_tx: watch::Sender<ConnLifecycle>,
    pub(crate) shutdown_tx: watch::Sender<bool>,
    pub(crate) inbox_capacity: usize,
    pub(crate) command_prefixes: Vec<String>,
    pub(crate) handshake_timeout: Duration,
    pub(crate) max_payload_bytes: usize,
    pub(crate) announce_bot: bool,
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
            lifecycle_tx,
            shutdown_tx,
            inbox_capacity: config.inbox_capacity.max(1),
            command_prefixes: config.command_prefixes.clone(),
            handshake_timeout: config.handshake_timeout,
            max_payload_bytes: config.max_payload_bytes.max(1),
            announce_bot: config.announce_bot,
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
        let transitioned = self.lifecycle_tx.send_if_modified(|state| match state {
            ConnLifecycle::Registering { .. } => {
                *state = ConnLifecycle::Active;
                true
            }
            _ => false,
        });
        if transitioned {
            self.maybe_announce_bot();
        }
    }

    /// Sends `MODE <nick> +<bot_mode>` once if `announce_bot` is enabled, the session is active,
    /// and the server advertised a `005 BOT=<mode>` character.
    pub(crate) fn maybe_announce_bot(&self) {
        if !self.announce_bot || !matches!(*self.lifecycle_tx.borrow(), ConnLifecycle::Active) {
            return;
        }
        let cmd = {
            let mut ws = self.state.write().unwrap();
            if ws.bot_mode_announced {
                return;
            }
            let Some(mode) = ws.bot_mode else {
                return;
            };
            ws.bot_mode_announced = true;
            Command::Raw(
                "MODE".into(),
                vec![ws.current_nick.clone(), format!("+{mode}")],
            )
        };
        self.outbound.enqueue_one(cmd);
    }

    /// Transitions to `Disconnected` at most once.
    ///
    /// Ordering invariant: `lifecycle_tx` MUST publish `Disconnected` before `outbound.abort()`
    /// and `inboxes.clear()`. `Inbox` does not subscribe to `lifecycle_tx`; instead, dropping
    /// every sender in `inboxes.clear()` wakes parked `Inbox` polls with `Ready(None)`, which
    /// then read `disconnect_reason()`.
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
        cmds: Vec<Command>,
        evict_on_leave: bool,
    ) {
        let mut ws = self.state.write().unwrap();
        ws.mark_parting(channel, evict_on_leave);
        self.outbound.enqueue_batch(cmds);
    }

    /// Enqueues a validated, line-wrapped outbound chat message.
    pub(crate) fn send_chat(
        &self,
        kind: OutboundChatKind,
        target: &str,
        text: Text,
    ) -> Result<()> {
        self.check_connected()?;
        validate_target(target, "message target")?;
        let ws = self.state.read().unwrap();
        let cmds = build_chat_commands(kind, target, text, &ws, self.max_payload_bytes);
        drop(ws);
        self.outbound.enqueue_batch(cmds);
        Ok(())
    }

    /// Passively updates session-wide actor metadata from an inbound frame's prefix and tags
    /// before command-specific dispatch:
    /// - records our own relayed `ident@host` on `WorldState` (taking a write lock only when it
    ///   changes), and
    /// - updates `@account` on any already-tracked `UserTrack` without allocating tracks for
    ///   strangers in unwatched channels.
    pub(crate) fn observe_inbound_actor(
        &self,
        prefix: Option<&Prefix>,
        raw_account: Option<&str>,
    ) {
        if self.state.read().unwrap().needs_self_prefix_update(prefix) {
            self.state.write().unwrap().observe_self_prefix(prefix);
        }
        if let (Some(Prefix::Nickname(nick, _, _)), Some(acct)) = (prefix, raw_account) {
            self.users.update_account(nick, acct);
        }
    }

    /// Resolves `nick` into a live [`User`] handle, applying `raw_account` if present.
    pub(crate) fn resolve_user(
        self: &Arc<Self>,
        nick: &str,
        raw_account: Option<&str>,
    ) -> User {
        let track = self.users.get_or_create_with_account(nick, raw_account);
        User::from_parts(Arc::downgrade(self), track)
    }

    /// Applies `005 RPL_ISUPPORT` tokens across `WorldState`, `UserRegistry`, and `InboxRegistry`
    /// in lockstep, and announces `+B` bot mode if ready.
    pub(crate) fn apply_isupport(&self, args: &[String]) {
        let changed_cm = self.state.write().unwrap().apply_isupport(args);
        if let Some(new_cm) = changed_cm {
            self.users.rekey_casemap(new_cm);
            self.inboxes.rekey_casemap(new_cm);
        }
        self.maybe_announce_bot();
    }

    /// Records a channel `PART` or `KICK` across `UserRegistry` and `WorldState`.
    /// Returns `None` when the bot itself left `channel`, or `Some(departed_user)` when a peer left.
    pub(crate) fn record_channel_departure(
        self: &Arc<Self>,
        channel: &str,
        target_nick: &str,
        target_raw_account: Option<&str>,
        reason: Option<String>,
        seq: u64,
    ) -> Option<User> {
        let track = self
            .users
            .get_or_create_with_account(target_nick, target_raw_account);
        let watched = self.inboxes.has_channel_listener(channel);
        let self_left = self.state.write().unwrap().handle_part_or_kick(
            channel,
            target_nick,
            track.id,
            reason,
            seq,
            watched,
        );
        (!self_left).then(|| User::from_parts(Arc::downgrade(self), track))
    }

    /// Classifies an outbound send bounce (`401`, `403`, `404`, `486`, `716`, …) and synchronizes
    /// `401` departures across `UserRegistry` and `WorldState`.
    pub(crate) fn resolve_bounce(
        &self,
        target: &str,
        code: u16,
        seq: u64,
        message: &str,
    ) -> BounceScope {
        let is_chan = code == 403 || self.state.read().unwrap().is_channel_name(target);
        if is_chan {
            return BounceScope::Channel;
        }
        let (target_user_id, quit_track_id) =
            self.users.resolve_send_error(target, code, seq, message);
        if let Some(qid) = quit_track_id {
            self.state.write().unwrap().handle_quit(qid);
        }
        BounceScope::User(target_user_id)
    }
}
