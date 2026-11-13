use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use irc::proto::Command;
use irc_colors::{Span, Text};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::casemap::{CaseMap, FoldedStr};
use crate::error::{Error, Result};
use crate::state::WorldState;

/// Validates an IRC channel or nickname target, rejecting empty strings, whitespace, NUL, CR, or LF.
pub(crate) fn validate_target(s: &str, what: &'static str) -> Result<()> {
    if s.is_empty() || s.contains(['\r', '\n', '\0', ' ']) {
        return Err(Error::InvalidInput(what));
    }
    Ok(())
}

/// Validates a single non-trailing IRC middle parameter (such as a mode flag/arg or channel key)
/// against empty strings, leading `:`, whitespace, CR, LF, or NUL.
pub(crate) fn validate_param(s: &str, what: &'static str) -> Result<()> {
    if s.is_empty() || s.starts_with(':') || s.contains(['\r', '\n', '\0', ' ']) {
        return Err(Error::InvalidInput(what));
    }
    Ok(())
}

/// Strips CR, LF, and NUL bytes from free-form protocol comment strings (`PART`, `KICK`, `QUIT`).
pub(crate) fn sanitize_reason(s: &str) -> String {
    if !s.contains(['\r', '\n', '\0']) {
        return s.to_owned();
    }
    s.chars()
        .filter(|&c| !matches!(c, '\r' | '\n' | '\0'))
        .collect()
}

/// Strips any embedded `\x01` (CTCP delimiter) or `\0` bytes from a [`Text`]'s spans.
pub(crate) fn sanitize_text(text: Text) -> Text {
    let needs_strip = text
        .spans()
        .iter()
        .any(|sp| sp.text.contains(['\x01', '\0']));
    if !needs_strip {
        return text;
    }
    let mut out = Text::new();
    for span in text.into_spans() {
        let cleaned: String = span
            .text
            .chars()
            .filter(|&c| c != '\x01' && c != '\0')
            .collect();
        if !cleaned.is_empty() {
            out += Span::new(cleaned, span.style);
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutboundChatKind {
    Privmsg,
    Notice,
    Action,
}

/// Wraps `text` to fit within the dynamic 512-byte relayed wire budget and builds outbound
/// `PRIVMSG` / `NOTICE` / CTCP `ACTION` commands, skipping empty chunks.
pub(crate) fn build_chat_commands(
    kind: OutboundChatKind,
    target: &str,
    text: Text,
    ws: &WorldState,
    max_payload_bytes: usize,
) -> Vec<Command> {
    let text = sanitize_text(text);
    let (cmd_name, is_action) = match kind {
        OutboundChatKind::Privmsg => ("PRIVMSG", false),
        OutboundChatKind::Notice => ("NOTICE", false),
        OutboundChatKind::Action => ("PRIVMSG", true),
    };
    let budget = ws.payload_budget(cmd_name, target, max_payload_bytes, is_action);
    let chunks = text.wrap(budget);
    let mut cmds = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        if chunk.is_empty() {
            continue;
        }
        let wire = chunk.to_irc_string();
        let cmd = match kind {
            OutboundChatKind::Privmsg => Command::PRIVMSG(target.to_owned(), wire),
            OutboundChatKind::Notice => Command::NOTICE(target.to_owned(), wire),
            OutboundChatKind::Action => {
                Command::PRIVMSG(target.to_owned(), format!("\x01ACTION {wire}\x01"))
            }
        };
        cmds.push(cmd);
    }
    cmds
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Priority {
    /// High priority (`PRIVMSG`, `NOTICE`, `INVITE`, `MODE`, `TOPIC`, `JOIN`, `send_raw`).
    Interactive,
    /// Low priority (`KICK`, `PART`, such as `Breakout::drop` eviction).
    Background,
}

/// Resolves when the final line of an outbound prompt batch has actually been written by [`OutboundSink`].
pub(crate) struct WriteReceipt {
    rx: Option<oneshot::Receiver<()>>,
}

impl WriteReceipt {
    pub(crate) fn immediate() -> Self {
        Self { rx: None }
    }

    fn from_rx(rx: oneshot::Receiver<()>) -> Self {
        Self { rx: Some(rx) }
    }

    pub(crate) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let Some(rx) = self.rx.as_mut() else {
            return Poll::Ready(());
        };
        match Pin::new(rx).poll(cx) {
            Poll::Ready(_) => {
                self.rx = None;
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }

    pub(crate) async fn wait(mut self) {
        std::future::poll_fn(|cx| self.poll(cx)).await
    }
}

pub(crate) enum RawTransport {
    Irc(irc::client::Sender),
    Test(mpsc::UnboundedSender<Command>),
}

impl RawTransport {
    fn send(&self, cmd: Command) -> bool {
        match self {
            RawTransport::Irc(sender) => sender.send(cmd).is_ok(),
            RawTransport::Test(tx) => tx.send(cmd).is_ok(),
        }
    }
}

struct QueuedCmd {
    cmd: Command,
    bg_channel: Option<FoldedStr>,
    receipt_tx: Option<oneshot::Sender<()>>,
}

struct SchedulerState {
    transport: RawTransport,
    burst: usize,
    window: Duration,
    sent_timestamps: VecDeque<Instant>,
    interactive: VecDeque<QueuedCmd>,
    background: VecDeque<QueuedCmd>,
    quit_pending: Option<(Option<String>, oneshot::Sender<()>)>,
    pacing_warned: bool,
    closed: bool,
    aborted: bool,
}

impl SchedulerState {
    fn has_pending(&self) -> bool {
        !self.interactive.is_empty()
            || !self.background.is_empty()
            || self.quit_pending.is_some()
    }

    fn pop_next_item(&mut self) -> Option<QueuedCmd> {
        if let Some(item) = self.interactive.pop_front() {
            return Some(item);
        }

        if let Some(bg) = self.background.pop_front() {
            return Some(bg);
        }

        if let Some((reason, done_tx)) = self.quit_pending.take() {
            self.closed = true;
            return Some(QueuedCmd {
                cmd: Command::QUIT(reason),
                bg_channel: None,
                receipt_tx: Some(done_tx),
            });
        }

        None
    }

    /// Flushes as many queued items as permitted by the rolling burst window at `now`.
    /// Returns `Some(next_slot_instant)` if items remain throttled, or `None` if drained.
    fn try_flush_ready(&mut self, now: Instant) -> Option<Instant> {
        if self.burst != usize::MAX {
            while let Some(&oldest) = self.sent_timestamps.front() {
                if now.saturating_duration_since(oldest) >= self.window {
                    self.sent_timestamps.pop_front();
                } else {
                    break;
                }
            }
        }

        loop {
            if !self.has_pending() {
                self.pacing_warned = false;
                return None;
            }

            if self.burst != usize::MAX && self.sent_timestamps.len() >= self.burst {
                if !self.pacing_warned {
                    self.pacing_warned = true;
                    tracing::warn!(
                        burst = self.burst,
                        window_ms = self.window.as_millis() as u64,
                        queued_interactive = self.interactive.len(),
                        queued_background = self.background.len(),
                        "outbound IRC rate limit reached; pacing queued commands"
                    );
                }
                let oldest = *self.sent_timestamps.front().expect("burst >= 1");
                return Some(oldest + self.window);
            }

            let Some(item) = self.pop_next_item() else {
                self.pacing_warned = false;
                return None;
            };

            let _ = self.transport.send(item.cmd);
            if self.burst != usize::MAX {
                self.sent_timestamps.push_back(now);
            }
            if let Some(tx) = item.receipt_tx {
                let _ = tx.send(());
            }
        }
    }
}

/// Outbound command priority scheduler and rolling-window flood pacer.
pub(crate) struct OutboundSink {
    state: Mutex<SchedulerState>,
    notify: Notify,
    enqueue_seq: AtomicU64,
}

impl OutboundSink {
    pub(crate) fn new(
        transport: RawTransport,
        burst: usize,
        window: Duration,
    ) -> (Arc<Self>, Option<JoinHandle<()>>) {
        let burst = burst.max(1);
        let sink = Arc::new(Self {
            state: Mutex::new(SchedulerState {
                transport,
                burst,
                window,
                sent_timestamps: VecDeque::new(),
                interactive: VecDeque::new(),
                background: VecDeque::new(),
                quit_pending: None,
                pacing_warned: false,
                closed: false,
                aborted: false,
            }),
            notify: Notify::new(),
            enqueue_seq: AtomicU64::new(0),
        });

        let task = if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let sink_clone = Arc::clone(&sink);
            Some(handle.spawn(async move {
                Self::run_pacer(sink_clone).await;
            }))
        } else {
            None
        };

        (sink, task)
    }

    pub(crate) fn enqueue_seq(&self) -> u64 {
        self.enqueue_seq.load(Ordering::Acquire)
    }

    async fn run_pacer(sink: Arc<Self>) {
        loop {
            let next_wake = {
                let mut st = sink.state.lock().unwrap();
                if st.aborted {
                    break;
                }
                st.try_flush_ready(Instant::now())
            };

            match next_wake {
                Some(deadline) => {
                    tokio::select! {
                        _ = tokio::time::sleep_until(deadline) => {}
                        _ = sink.notify.notified() => {}
                    }
                }
                None => {
                    sink.notify.notified().await;
                }
            }
        }
    }

    /// Enqueues a batch of commands under `priority` and returns a [`WriteReceipt`] resolved
    /// when the final command in `cmds` is written to the underlying sink.
    pub(crate) fn enqueue_batch(
        &self,
        priority: Priority,
        cmds: Vec<Command>,
        casemap: CaseMap,
    ) -> WriteReceipt {
        if cmds.is_empty() {
            return WriteReceipt::immediate();
        }

        let (tx, rx) = oneshot::channel();
        let needs_wake = {
            let mut st = self.state.lock().unwrap();
            if st.closed || st.aborted || st.quit_pending.is_some() {
                return WriteReceipt::immediate();
            }
            self.enqueue_seq.fetch_add(1, Ordering::Release);

            for cmd in cmds {
                if priority == Priority::Interactive {
                    // Promote any earlier Background `KICK`/`PART` on the same channel ahead of
                    // this `JOIN` at enqueue time so `PART #c` -> `JOIN #c` preserves order while
                    // a later `PART #c` enqueued after `JOIN #c` stays behind `JOIN #c`.
                    if let Command::JOIN(ref chan_list, _, _) = cmd {
                        for raw_chan in chan_list.split(',') {
                            let target_key = casemap.fold(raw_chan.trim());
                            let mut remaining = VecDeque::with_capacity(st.background.len());
                            while let Some(bg_item) = st.background.pop_front() {
                                if bg_item.bg_channel.as_ref() == Some(&target_key) {
                                    st.interactive.push_back(bg_item);
                                } else {
                                    remaining.push_back(bg_item);
                                }
                            }
                            st.background = remaining;
                        }
                    }
                }

                let bg_channel = if priority == Priority::Background {
                    match &cmd {
                        Command::KICK(chan, _, _) | Command::PART(chan, _) => {
                            Some(casemap.fold(chan))
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                let q = QueuedCmd {
                    cmd,
                    bg_channel,
                    receipt_tx: None,
                };
                match priority {
                    Priority::Interactive => st.interactive.push_back(q),
                    Priority::Background => st.background.push_back(q),
                }
            }
            // Attach receipt_tx to the last pushed item:
            let queue = match priority {
                Priority::Interactive => &mut st.interactive,
                Priority::Background => &mut st.background,
            };
            if let Some(last) = queue.back_mut() {
                last.receipt_tx = Some(tx);
            }

            st.try_flush_ready(Instant::now()).is_some()
        };

        if needs_wake {
            self.notify.notify_one();
        }
        WriteReceipt::from_rx(rx)
    }

    pub(crate) fn enqueue_one(
        &self,
        priority: Priority,
        cmd: Command,
        casemap: CaseMap,
    ) -> WriteReceipt {
        self.enqueue_batch(priority, vec![cmd], casemap)
    }

    /// Schedules `QUIT` after all currently queued `Interactive` and `Background` commands drain,
    /// returning a [`WriteReceipt`] resolved once `QUIT` has been written.
    pub(crate) fn enqueue_quit(&self, reason: Option<&str>, _casemap: CaseMap) -> WriteReceipt {
        let clean_reason = reason.map(sanitize_reason);
        let (tx, rx) = oneshot::channel();
        let needs_wake = {
            let mut st = self.state.lock().unwrap();
            if st.closed || st.aborted {
                return WriteReceipt::immediate();
            }
            self.enqueue_seq.fetch_add(1, Ordering::Release);
            st.quit_pending = Some((clean_reason, tx));
            st.try_flush_ready(Instant::now()).is_some()
        };
        if needs_wake {
            self.notify.notify_one();
        }
        WriteReceipt::from_rx(rx)
    }

    pub(crate) fn abort(&self) {
        {
            let mut st = self.state.lock().unwrap();
            st.aborted = true;
            st.closed = true;
            st.interactive.clear();
            st.background.clear();
            st.quit_pending = None;
        }
        self.notify.notify_one();
    }
}

#[cfg(test)]
#[path = "outbound_test.rs"]
mod test;
