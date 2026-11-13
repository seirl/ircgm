use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
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

/// Validates a non-empty parameter (such as a mode flag string or channel key) against CR/LF/NUL.
pub(crate) fn validate_param(s: &str, what: &'static str) -> Result<()> {
    if s.is_empty() || s.contains(['\r', '\n', '\0']) {
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
    emitting_front: VecDeque<QueuedCmd>,
    interactive: VecDeque<QueuedCmd>,
    background: VecDeque<QueuedCmd>,
    quit_pending: Option<(Option<String>, oneshot::Sender<()>)>,
    last_casemap: CaseMap,
    pacing_warned: bool,
    closed: bool,
    aborted: bool,
}

impl SchedulerState {
    fn has_pending(&self) -> bool {
        !self.emitting_front.is_empty()
            || !self.interactive.is_empty()
            || !self.background.is_empty()
            || self.quit_pending.is_some()
    }

    fn pop_next_item(&mut self, casemap: CaseMap) -> Option<QueuedCmd> {
        if let Some(front) = self.emitting_front.pop_front() {
            return Some(front);
        }

        if let Some(item) = self.interactive.pop_front() {
            // Before emitting an Interactive `JOIN <chan>`, flush any pending Background
            // `KICK`/`PART` items targeting the same channel so `JOIN #foo` never precedes `PART #foo`.
            if let Command::JOIN(ref chan_list, _, _) = item.cmd {
                let mut flushed = VecDeque::new();
                for raw_chan in chan_list.split(',') {
                    let target_key = casemap.fold(raw_chan.trim());
                    let mut remaining = VecDeque::with_capacity(self.background.len());
                    while let Some(bg_item) = self.background.pop_front() {
                        if bg_item.bg_channel.as_ref() == Some(&target_key) {
                            flushed.push_back(bg_item);
                        } else {
                            remaining.push_back(bg_item);
                        }
                    }
                    self.background = remaining;
                }
                if !flushed.is_empty() {
                    self.emitting_front.extend(flushed);
                    self.emitting_front.push_back(item);
                    return self.emitting_front.pop_front();
                }
            }
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
    fn try_flush_ready(&mut self, now: Instant, casemap: CaseMap) -> Option<Instant> {
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
                        queued_interactive = self.interactive.len() + self.emitting_front.len(),
                        queued_background = self.background.len(),
                        "outbound IRC rate limit reached; pacing queued commands"
                    );
                }
                let oldest = *self.sent_timestamps.front().expect("burst >= 1");
                return Some(oldest + self.window);
            }

            let Some(item) = self.pop_next_item(casemap) else {
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
                emitting_front: VecDeque::new(),
                interactive: VecDeque::new(),
                background: VecDeque::new(),
                quit_pending: None,
                last_casemap: CaseMap::default(),
                pacing_warned: false,
                closed: false,
                aborted: false,
            }),
            notify: Notify::new(),
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

    async fn run_pacer(sink: Arc<Self>) {
        loop {
            let next_wake = {
                let mut st = sink.state.lock().unwrap();
                if st.aborted {
                    break;
                }
                let cm = st.last_casemap;
                st.try_flush_ready(Instant::now(), cm)
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
            st.last_casemap = casemap;

            for cmd in cmds {
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

            st.try_flush_ready(Instant::now(), casemap).is_some()
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
    pub(crate) fn enqueue_quit(&self, reason: Option<&str>, casemap: CaseMap) -> WriteReceipt {
        let clean_reason = reason.map(sanitize_reason);
        let (tx, rx) = oneshot::channel();
        let needs_wake = {
            let mut st = self.state.lock().unwrap();
            if st.closed || st.aborted {
                return WriteReceipt::immediate();
            }
            st.last_casemap = casemap;
            st.quit_pending = Some((clean_reason, tx));
            st.try_flush_ready(Instant::now(), casemap).is_some()
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
            st.emitting_front.clear();
            st.interactive.clear();
            st.background.clear();
            st.quit_pending = None;
        }
        self.notify.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fmt::Colorize as _;
    use irc::proto::Response;

    #[tokio::test]
    async fn crlf_splitting_ctcp_sanitization_and_396_wire_budget() {
        let mut ws = WorldState::new("GameBot");
        // Simulate 001 + 396 60-byte cloak:
        ws.self_ident = Some("~gamebot".into());
        let cloak_msg: irc::proto::Message = format!(
            ":srv 396 GameBot {} :is now your displayed host\r\n",
            "v".repeat(60)
        )
        .parse()
        .unwrap();
        if let Command::Response(Response::RPL_HOSTHIDDEN, args) = &cloak_msg.command {
            ws.handle_host_hidden(args);
        }

        // 1. CRLF injection splits into 2 separate PRIVMSG lines:
        let cmds = build_chat_commands(
            OutboundChatKind::Privmsg,
            "#room",
            Text::from("hello\r\nQUIT :injected"),
            &ws,
            450,
        );
        assert_eq!(
            cmds,
            vec![
                Command::PRIVMSG("#room".into(), "hello".into()),
                Command::PRIVMSG("#room".into(), "QUIT :injected".into()),
            ]
        );

        // 2. Embedded \x01 stripped:
        let cmds_ctcp = build_chat_commands(
            OutboundChatKind::Privmsg,
            "#room",
            Text::from("hi\x01ACTION spoof\x01"),
            &ws,
            450,
        );
        assert_eq!(
            cmds_ctcp,
            vec![Command::PRIVMSG("#room".into(), "hiACTION spoof".into())]
        );

        // 3. Long styled message wrapped within <= 512 relayed wire bytes while preserving style:
        let long_styled = ("word ".repeat(120)).bold().red();
        let wrapped = build_chat_commands(
            OutboundChatKind::Privmsg,
            "#heist-channel",
            long_styled,
            &ws,
            450,
        );
        assert!(wrapped.len() >= 2);
        for cmd in &wrapped {
            let Command::PRIVMSG(target, payload) = cmd else {
                panic!("expected PRIVMSG");
            };
            let relayed = format!(
                ":{}!{} PRIVMSG {} :{}\r\n",
                ws.current_nick,
                ws.self_user_host().unwrap(),
                target,
                payload
            );
            assert!(
                relayed.len() <= 512,
                "relayed frame was {} bytes (> 512)",
                relayed.len()
            );
            // Each continuation chunk re-establishes bold + red:
            let parsed = Text::parse(payload);
            for sp in parsed.spans() {
                assert!(sp.style.bold);
                assert_eq!(
                    sp.style.fg,
                    Some(irc_colors::ColorValue::Palette(irc_colors::Color::Red))
                );
            }
        }

        // 4. Identifier validation:
        assert!(validate_target("#ok", "channel").is_ok());
        assert!(validate_target("", "channel").is_err());
        assert!(validate_target("#bad\r\nQUIT", "channel").is_err());
        assert!(validate_target("bad nick", "nick").is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn priority_scheduling_same_channel_flush_and_write_receipts() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (sink, _task) =
            OutboundSink::new(RawTransport::Test(tx), 2, Duration::from_secs(8));
        let cm = CaseMap::Rfc1459;

        // Consume initial 2-message burst at t = 0:
        let r1 = sink.enqueue_one(
            Priority::Interactive,
            Command::PRIVMSG("#lobby".into(), "m1".into()),
            cm,
        );
        let r2 = sink.enqueue_one(
            Priority::Interactive,
            Command::PRIVMSG("#lobby".into(), "m2".into()),
            cm,
        );
        r1.wait().await;
        r2.wait().await;
        assert_eq!(
            rx.try_recv().unwrap(),
            Command::PRIVMSG("#lobby".into(), "m1".into())
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            Command::PRIVMSG("#lobby".into(), "m2".into())
        );

        // Now burst window is full! Enqueue Background KICK + PART on #vault and #other,
        // followed by Interactive PRIVMSG and Interactive JOIN #vault:
        let _ = sink.enqueue_batch(
            Priority::Background,
            vec![
                Command::KICK("#other".into(), "Bob".into(), None),
                Command::PART("#other".into(), None),
                Command::KICK("#vault".into(), "Alice".into(), None),
                Command::PART("#vault".into(), None),
            ],
            cm,
        );
        let r_prompt = sink.enqueue_one(
            Priority::Interactive,
            Command::PRIVMSG("#lobby".into(), "urgent_prompt".into()),
            cm,
        );
        let r_rejoin = sink.enqueue_one(
            Priority::Interactive,
            Command::JOIN("#VAULT".into(), None, None),
            cm,
        );
        let r_quit = sink.enqueue_quit(Some("bye"), cm);

        // Nothing else sent yet at t = 0:
        assert!(rx.try_recv().is_err());

        // Track exact timestamp when `r_prompt` resolves:
        let start = Instant::now();
        let prompt_task = tokio::spawn(async move {
            r_prompt.wait().await;
            start.elapsed()
        });

        // Advance to t = 8s -> 2 slots open:
        // Slot 1: `urgent_prompt` (Interactive)
        // Slot 2: `JOIN #VAULT` pulls same-channel Background `KICK #vault Alice` first!
        tokio::time::advance(Duration::from_secs(8)).await;
        tokio::task::yield_now().await;

        assert_eq!(prompt_task.await.unwrap(), Duration::from_secs(8));
        assert_eq!(
            rx.try_recv().unwrap(),
            Command::PRIVMSG("#lobby".into(), "urgent_prompt".into())
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            Command::KICK("#vault".into(), "Alice".into(), None)
        );
        assert!(rx.try_recv().is_err());

        // Advance to t = 16s -> next 2 slots:
        // `PART #vault` (flushed ahead of `JOIN #VAULT`), then `JOIN #VAULT`!
        tokio::time::advance(Duration::from_secs(8)).await;
        r_rejoin.wait().await;
        assert_eq!(
            rx.try_recv().unwrap(),
            Command::PART("#vault".into(), None)
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            Command::JOIN("#VAULT".into(), None, None)
        );

        // Advance to t = 24s -> remaining Background `#other` KICK + PART:
        tokio::time::advance(Duration::from_secs(8)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            rx.try_recv().unwrap(),
            Command::KICK("#other".into(), "Bob".into(), None)
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            Command::PART("#other".into(), None)
        );

        // Advance to t = 32s -> QUIT drains last!
        tokio::time::advance(Duration::from_secs(8)).await;
        r_quit.wait().await;
        assert_eq!(rx.try_recv().unwrap(), Command::QUIT(Some("bye".into())));
    }
}
