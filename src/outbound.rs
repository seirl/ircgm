#[cfg(any(test, feature = "testing"))]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use irc::proto::Command;
use irc_colors::{Span, Text};
#[cfg(any(test, feature = "testing"))]
use tokio::sync::mpsc;

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

pub(crate) enum RawTransport {
    Irc(irc::client::Sender),
    #[cfg(any(test, feature = "testing"))]
    Test {
        outbound_tx: mpsc::UnboundedSender<Command>,
        /// Invoked once `QUIT` hits the wire, modeling an RFC 2812 server closing the connection
        /// after receiving `QUIT`.
        on_quit: Option<Box<dyn Fn() + Send + Sync>>,
    },
}

impl RawTransport {
    #[cfg(test)]
    pub(crate) fn test_only(outbound_tx: mpsc::UnboundedSender<Command>) -> Self {
        Self::Test {
            outbound_tx,
            on_quit: None,
        }
    }

    fn send(&self, cmd: Command) -> bool {
        match self {
            RawTransport::Irc(sender) => sender.send(cmd).is_ok(),
            #[cfg(any(test, feature = "testing"))]
            RawTransport::Test {
                outbound_tx,
                on_quit,
            } => {
                let is_quit = matches!(cmd, Command::QUIT(_));
                let ok = outbound_tx.send(cmd).is_ok();
                if is_quit {
                    if let Some(f) = on_quit {
                        f();
                    }
                }
                ok
            }
        }
    }
}

/// Serialized FIFO outbound command sink.
pub(crate) struct OutboundSink {
    transport: RawTransport,
    closed: Mutex<bool>,
    #[cfg(any(test, feature = "testing"))]
    enqueue_seq: AtomicU64,
}

impl OutboundSink {
    pub(crate) fn new(transport: RawTransport) -> Arc<Self> {
        Arc::new(Self {
            transport,
            closed: Mutex::new(false),
            #[cfg(any(test, feature = "testing"))]
            enqueue_seq: AtomicU64::new(0),
        })
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn enqueue_seq(&self) -> u64 {
        self.enqueue_seq.load(Ordering::Acquire)
    }

    /// Sends a batch of commands atomically in FIFO order unless the sink has been closed.
    pub(crate) fn enqueue_batch(&self, cmds: Vec<Command>) {
        if cmds.is_empty() {
            return;
        }
        let closed = self.closed.lock().unwrap();
        if *closed {
            return;
        }
        #[cfg(any(test, feature = "testing"))]
        self.enqueue_seq.fetch_add(1, Ordering::Release);
        for cmd in cmds {
            let _ = self.transport.send(cmd);
        }
    }

    pub(crate) fn enqueue_one(&self, cmd: Command) {
        self.enqueue_batch(vec![cmd]);
    }

    /// Sends `QUIT` and closes the sink so no subsequent commands are emitted after `QUIT`.
    pub(crate) fn send_quit(&self, reason: Option<&str>) {
        let clean_reason = reason.map(sanitize_reason);
        let mut closed = self.closed.lock().unwrap();
        if *closed {
            return;
        }
        *closed = true;
        #[cfg(any(test, feature = "testing"))]
        self.enqueue_seq.fetch_add(1, Ordering::Release);
        let _ = self.transport.send(Command::QUIT(clean_reason));
    }

    pub(crate) fn abort(&self) {
        *self.closed.lock().unwrap() = true;
    }
}

#[cfg(test)]
#[path = "outbound_test.rs"]
mod test;
