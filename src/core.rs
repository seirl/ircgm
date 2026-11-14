use std::sync::{Arc, RwLock};
use std::time::Duration;

use irc::proto::Command;
use irc_colors::Text;
use tokio::sync::watch;

use crate::casemap::{AtomicCaseMap, CaseMap};
use crate::error::{DisconnectReason, Error, Result};
use crate::inbox::InboxRegistry;
use crate::outbound::{
    build_chat_commands, validate_target, OutboundChatKind, OutboundSink, Priority,
};
use crate::state::WorldState;
use crate::user::UserRegistry;

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
        inbox_capacity: usize,
        command_prefixes: Vec<String>,
        handshake_timeout: Duration,
        max_payload_bytes: usize,
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
            inbox_capacity: inbox_capacity.max(1),
            command_prefixes,
            handshake_timeout,
            max_payload_bytes: max_payload_bytes.max(1),
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
        priority: Priority,
        cmds: Vec<Command>,
        evict_on_leave: bool,
    ) {
        let mut ws = self.state.write().unwrap();
        ws.mark_parting(channel, evict_on_leave);
        self.outbound.enqueue_batch(priority, cmds);
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
        self.outbound.enqueue_batch(Priority::Interactive, cmds);
        Ok(())
    }
}
