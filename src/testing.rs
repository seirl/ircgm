use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};

use irc::proto::Command;
use irc_colors::Text;
use tokio::sync::{mpsc, oneshot};

use crate::bot::{Bot, BotConfig, BotCore, BotOwner};
use crate::casemap::CaseMap;
use crate::outbound::{OutboundSink, RawTransport};
use crate::pump::{run_pump, PumpInput, PumpSource, RegistrationTracker};
use crate::state::parse_irc_message;

/// Creates an in-memory `(Bot, TestServer)` pair with zero sockets and unthrottled outbound writes
/// (`outbound_burst = usize::MAX`) for fast, deterministic unit tests.
pub fn test_harness(initial_nick: impl Into<String>) -> (Bot, TestServer) {
    let cfg = BotConfig {
        outbound_burst: usize::MAX,
        ..BotConfig::default()
    };
    test_harness_with(initial_nick, cfg)
}

/// Creates an in-memory `(Bot, TestServer)` pair honoring custom [`BotConfig`] settings
/// (including `outbound_burst` / `outbound_window` pacing tests).
pub fn test_harness_with(initial_nick: impl Into<String>, config: BotConfig) -> (Bot, TestServer) {
    let initial_nick = initial_nick.into();
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
    let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();

    let (outbound, outbound_task) = OutboundSink::new(
        RawTransport::Test(outbound_tx),
        config.outbound_burst,
        config.outbound_window,
    );
    let core = BotCore::new(initial_nick.clone(), outbound, true, &config);
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
        pump_task: Mutex::new(Some(pump_task)),
        outbound_task: Mutex::new(outbound_task),
        _live_client: Mutex::new(None),
    });

    let bot = Bot::from_owner(owner);
    let server = TestServer {
        inbound_tx,
        outbound_rx,
        pending_cmds: VecDeque::new(),
        core: weak_core,
        fallback_nick: initial_nick,
    };
    (bot, server)
}

/// Deterministic in-memory test harness peer for driving [`Bot`] without sockets or an IRCd.
pub struct TestServer {
    inbound_tx: mpsc::UnboundedSender<PumpInput>,
    outbound_rx: mpsc::UnboundedReceiver<Command>,
    pending_cmds: VecDeque<Command>,
    core: Weak<BotCore>,
    fallback_nick: String,
}

impl TestServer {
    fn current_nick_and_casemap(&self) -> (String, CaseMap) {
        if let Some(core) = self.core.upgrade() {
            let ws = core.state.read().unwrap();
            (ws.current_nick.clone(), ws.casemap)
        } else {
            (self.fallback_nick.clone(), CaseMap::default())
        }
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

    /// Convenience helper: injects `:<nick>!~<nick>@test.host PRIVMSG <channel> :<text>\r\n`.
    pub fn chat(&self, nick: &str, channel: &str, text: &str) {
        self.send_line(&format!(":{nick}!~{nick}@test.host PRIVMSG {channel} :{text}\r\n"));
    }

    /// Convenience helper: injects a CTCP `/me` action (`\x01ACTION <action_text>\x01`) to `target`.
    pub fn action(&self, nick: &str, target: &str, action_text: &str) {
        self.send_line(&format!(
            ":{nick}!~{nick}@test.host PRIVMSG {target} :\x01ACTION {action_text}\x01\r\n"
        ));
    }

    /// Convenience helper: injects `:<nick>!~<nick>@test.host NOTICE <target> :<text>\r\n`.
    pub fn notice(&self, nick: &str, target: &str, text: &str) {
        self.send_line(&format!(
            ":{nick}!~{nick}@test.host NOTICE {target} :{text}\r\n"
        ));
    }

    /// Convenience helper: injects `:<nick>!~<nick>@test.host PRIVMSG <bot_nick> :<text>\r\n`.
    pub fn dm(&self, nick: &str, text: &str) {
        let (bot_nick, _) = self.current_nick_and_casemap();
        self.send_line(&format!(
            ":{nick}!~{nick}@test.host PRIVMSG {bot_nick} :{text}\r\n"
        ));
    }

    /// Convenience helper: injects `:<nick>!~<nick>@test.host NOTICE <bot_nick> :<text>\r\n`.
    pub fn dm_notice(&self, nick: &str, text: &str) {
        let (bot_nick, _) = self.current_nick_and_casemap();
        self.send_line(&format!(
            ":{nick}!~{nick}@test.host NOTICE {bot_nick} :{text}\r\n"
        ));
    }

    /// Convenience helper: injects `:<nick>!~<nick>@test.host JOIN <channel>\r\n`.
    pub fn join_user(&self, nick: &str, channel: &str) {
        self.send_line(&format!(":{nick}!~{nick}@test.host JOIN {channel}\r\n"));
    }

    /// Convenience helper: injects `:<nick>!~<nick>@test.host PART <channel> [:<reason>]\r\n`.
    pub fn part_user(&self, nick: &str, channel: &str, reason: Option<&str>) {
        match reason {
            Some(r) => self.send_line(&format!(
                ":{nick}!~{nick}@test.host PART {channel} :{r}\r\n"
            )),
            None => self.send_line(&format!(":{nick}!~{nick}@test.host PART {channel}\r\n")),
        }
    }

    /// Convenience helper: injects `:<old_nick>!~<old_nick>@test.host NICK <new_nick>\r\n`.
    pub fn rename_user(&self, old_nick: &str, new_nick: &str) {
        self.send_line(&format!(
            ":{old_nick}!~{old_nick}@test.host NICK {new_nick}\r\n"
        ));
    }

    /// Convenience helper: injects self `JOIN`, `353 RPL_NAMREPLY` (`@<bot_nick>` + `names`),
    /// and `366 RPL_ENDOFNAMES`.
    pub fn complete_join(&self, channel: &str, names: &[&str]) {
        let (bot_nick, user_host, cm) = if let Some(core) = self.core.upgrade() {
            let ws = core.state.read().unwrap();
            let bn = ws.current_nick.clone();
            let uh = ws
                .self_user_host()
                .unwrap_or_else(|| format!("~{bn}@test.host"));
            (bn, uh, ws.casemap)
        } else {
            let bn = self.fallback_nick.clone();
            let uh = format!("~{bn}@test.host");
            (bn, uh, CaseMap::default())
        };
        self.send_line(&format!(":{bot_nick}!{user_host} JOIN {channel}\r\n"));

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

    /// Convenience helper: injects `:<nick>!~<nick>@test.host QUIT [:<reason>]\r\n`.
    pub fn quit_user(&self, nick: &str, reason: Option<&str>) {
        match reason {
            Some(r) => self.send_line(&format!(":{nick}!~{nick}@test.host QUIT :{r}\r\n")),
            None => self.send_line(&format!(":{nick}!~{nick}@test.host QUIT\r\n")),
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
    /// this call, then flushes ready task reactions and `OutboundSink` writes.
    pub async fn settle(&self) {
        let (tx, rx) = oneshot::channel();
        if self.inbound_tx.send(PumpInput::Settle(tx)).is_ok() {
            let _ = rx.await;
        }
        self.flush().await;
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
        let expected_prefix = prefix;

        let ch = loop {
            while let Ok(cmd) = self.outbound_rx.try_recv() {
                self.pending_cmds.push_back(cmd);
            }
            let (_, cm) = self.current_nick_and_casemap();
            if let Some(idx) = self.pending_cmds.iter().position(|cmd| {
                matches!(
                    cmd,
                    Command::JOIN(chan, _, _)
                        if chan.len() >= expected_prefix.len()
                            && chan.is_char_boundary(expected_prefix.len())
                            && cm.str_eq(&chan[..expected_prefix.len()], expected_prefix)
                )
            }) {
                let Some(Command::JOIN(chan, _, _)) = self.pending_cmds.remove(idx) else {
                    unreachable!()
                };
                break chan;
            }

            let next = tokio::time::timeout(std::time::Duration::from_secs(5), self.outbound_rx.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!("TestServer::accept_breakout timed out waiting for JOIN {expected_prefix}*")
                })
                .unwrap_or_else(|| {
                    panic!("TestServer::accept_breakout saw outbound channel close before JOIN {expected_prefix}*")
                });
            self.pending_cmds.push_back(next);
        };

        self.complete_join(&ch, names);
        self.settle().await;
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
        while let Ok(cmd) = self.outbound_rx.try_recv() {
            self.pending_cmds.push_back(cmd);
        }

        let (_, cm) = self.current_nick_and_casemap();
        let mut matched = Vec::new();
        let mut kept = VecDeque::with_capacity(self.pending_cmds.len());

        while let Some(cmd) = self.pending_cmds.pop_front() {
            match &cmd {
                Command::PRIVMSG(dest, payload) | Command::NOTICE(dest, payload)
                    if cm.str_eq(dest, target) =>
                {
                    matched.push(Text::parse(payload).plain());
                }
                _ => kept.push_back(cmd),
            }
        }

        self.pending_cmds = kept;
        matched
    }
}

#[cfg(test)]
#[path = "testing_test.rs"]
mod test;

