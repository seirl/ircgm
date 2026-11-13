use std::sync::Arc;
use std::time::SystemTime;

use irc::proto::Message;
use irc_colors::Text;
use tokio::time::Instant;

use crate::room::Room;
use crate::user::User;

/// Destination of an inbound or outbound chat message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Channel(String),
    Dm,
}

impl Target {
    pub fn is_channel(&self) -> bool {
        matches!(self, Target::Channel(_))
    }

    pub fn is_dm(&self) -> bool {
        matches!(self, Target::Dm)
    }
}

/// Kind of chat message represented by [`ChatMessage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MessageKind {
    Privmsg,
    Action,
    Notice,
}

/// Parsed chat message (`PRIVMSG`, CTCP `ACTION`, or opted-in human `NOTICE`).
#[derive(Clone, Debug)]
pub struct ChatMessage {
    pub raw: Arc<Message>,
    /// Strictly monotonic wire-arrival sequence number (`1, 2, 3, ...`) assigned by the pump.
    /// **`seq` (never `server_time`) is the sole ordering and race-fairness authority.**
    pub seq: u64,
    /// Monotonic local timestamp captured when the pump processed this frame.
    pub received_at: Instant,
    /// IRCv3 `server-time` (`time=` tag), when negotiated and supplied by the IRCd (`None` if absent).
    pub server_time: Option<SystemTime>,
    /// Presence-tracked `User` handle stamped at wire-arrival time.
    pub sender: User,
    /// Nickname snapshot of the sender at the exact moment this message arrived on the wire.
    pub nick: String,
    /// IRC username / ident string (`~ident` in `nick!~ident@host`).
    pub ident: Option<String>,
    pub host: Option<String>,
    /// Populated automatically when IRCv3 `account-tag` is present on `raw.tags`.
    pub account: Option<String>,
    pub target: Target,
    /// Formatting-stripped plain text (`Text::parse(&raw_text).plain()`), parsed once in the pump.
    pub text: String,
    /// Unmodified wire payload (with CTCP `\x01ACTION ...\x01` framing unwrapped if `kind == MessageKind::Action`).
    pub raw_text: String,
    pub kind: MessageKind,
}

impl ChatMessage {
    pub fn is_dm(&self) -> bool {
        self.target.is_dm()
    }

    pub fn is_action(&self) -> bool {
        self.kind == MessageKind::Action
    }

    pub fn is_notice(&self) -> bool {
        self.kind == MessageKind::Notice
    }

    /// Case-folded channel match using `room`'s active `CaseMap`.
    pub fn is_in(&self, room: &Room) -> bool {
        match &self.target {
            Target::Channel(ch) => room.casemap().str_eq(ch, room.name()),
            Target::Dm => false,
        }
    }

    /// Re-parses `raw_text` into an `irc_colors::Text` preserving sender colors/styles.
    pub fn to_styled(&self) -> Text {
        Text::parse(&self.raw_text)
    }

    /// Word-boundary command check against plain `self.text` (`"!join"` matches `"!join"`, not `"!joined"`).
    pub fn is_cmd(&self, cmd: &str) -> bool {
        self.cmd_args(cmd).is_some()
    }

    /// Returns trimmed arguments after `cmd` if matched at a word boundary.
    pub fn cmd_args(&self, cmd: &str) -> Option<&str> {
        if cmd.is_empty() {
            return None;
        }
        let trimmed = self.text.trim_start();
        let rest = trimmed.strip_prefix(cmd)?;
        if rest.is_empty() {
            Some("")
        } else if rest.starts_with(char::is_whitespace) {
            Some(rest.trim())
        } else {
            None
        }
    }
}

/// High-level IRC event surfaced by [`crate::Inbox::next_event`].
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum Event {
    Chat(ChatMessage),
    Notice {
        sender: Option<User>,
        nick: Option<String>,
        target: Target,
        text: String,
        raw: Arc<Message>,
    },
    Joined {
        channel: String,
        user: User,
        nick: String,
        ident: Option<String>,
        host: Option<String>,
    },
    Parted {
        channel: String,
        user: User,
        nick: String,
        reason: Option<String>,
    },
    Kicked {
        channel: String,
        user: User,
        nick: String,
        by: String,
        reason: Option<String>,
    },
    Quit {
        user: User,
        nick: String,
        /// Joined channels (tracked by `WorldState`) that this user occupied at the moment of `QUIT`.
        channels: Vec<String>,
        reason: Option<String>,
        /// Heuristic `true` when `reason` looks like an IRC server-to-server netsplit (`"a.b c.d"`).
        netsplit: bool,
    },
    NickChanged {
        user: User,
        old_nick: String,
        new_nick: String,
        /// Joined channels (tracked by `WorldState`) that this user occupied at the moment of `NICK`.
        channels: Vec<String>,
    },
    /// Server rejected or bounced an outbound delivery (`401`, `403`, `404`, `442`, `486` `+R`,
    /// `492`, `531`, `716` `+g` `callerid`).
    DeliveryFailed {
        target: String,
        code: u16,
        message: String,
        raw: Arc<Message>,
    },
}

/// Internal wire frame distributed from the pump to matching [`crate::Inbox`] slots.
#[derive(Clone, Debug)]
pub(crate) struct WireEvent {
    pub(crate) seq: u64,
    pub(crate) raw: Arc<Message>,
    pub(crate) kind: WireKind,
}

#[derive(Clone, Debug)]
pub(crate) enum WireKind {
    Chat(ChatMessage),
    Notice {
        sender: Option<User>,
        nick: Option<String>,
        target: Target,
        text: String,
        /// Populated only when the `NOTICE` came from a human `nick!user@host` without `@bot` tag
        /// or IRC services pseudoclient nickname, allowing `.accept_notices()` to yield it as chat.
        human_chat: Option<ChatMessage>,
    },
    Joined {
        channel: String,
        user: User,
        nick: String,
        ident: Option<String>,
        host: Option<String>,
    },
    Parted {
        channel: String,
        user: User,
        nick: String,
        reason: Option<String>,
    },
    Kicked {
        channel: String,
        user: User,
        nick: String,
        by: String,
        reason: Option<String>,
    },
    Quit {
        user: User,
        nick: String,
        channels: Vec<String>,
        reason: Option<String>,
        netsplit: bool,
    },
    NickChanged {
        user: User,
        old_nick: String,
        new_nick: String,
        channels: Vec<String>,
    },
    SendError {
        target: String,
        target_user_id: Option<u64>,
        code: u16,
        message: String,
    },
    BotLeftRoom {
        channel: String,
        reason: Option<String>,
    },
    RawOnly,
}

impl WireEvent {
    pub(crate) fn to_public_event(&self) -> Option<Event> {
        match &self.kind {
            WireKind::Chat(msg) => Some(Event::Chat(msg.clone())),
            WireKind::Notice {
                sender,
                nick,
                target,
                text,
                ..
            } => Some(Event::Notice {
                sender: sender.clone(),
                nick: nick.clone(),
                target: target.clone(),
                text: text.clone(),
                raw: Arc::clone(&self.raw),
            }),
            WireKind::Joined {
                channel,
                user,
                nick,
                ident,
                host,
            } => Some(Event::Joined {
                channel: channel.clone(),
                user: user.clone(),
                nick: nick.clone(),
                ident: ident.clone(),
                host: host.clone(),
            }),
            WireKind::Parted {
                channel,
                user,
                nick,
                reason,
            } => Some(Event::Parted {
                channel: channel.clone(),
                user: user.clone(),
                nick: nick.clone(),
                reason: reason.clone(),
            }),
            WireKind::Kicked {
                channel,
                user,
                nick,
                by,
                reason,
            } => Some(Event::Kicked {
                channel: channel.clone(),
                user: user.clone(),
                nick: nick.clone(),
                by: by.clone(),
                reason: reason.clone(),
            }),
            WireKind::Quit {
                user,
                nick,
                channels,
                reason,
                netsplit,
            } => Some(Event::Quit {
                user: user.clone(),
                nick: nick.clone(),
                channels: channels.clone(),
                reason: reason.clone(),
                netsplit: *netsplit,
            }),
            WireKind::NickChanged {
                user,
                old_nick,
                new_nick,
                channels,
            } => Some(Event::NickChanged {
                user: user.clone(),
                old_nick: old_nick.clone(),
                new_nick: new_nick.clone(),
                channels: channels.clone(),
            }),
            WireKind::SendError {
                target,
                code,
                message,
                ..
            } => Some(Event::DeliveryFailed {
                target: target.clone(),
                code: *code,
                message: message.clone(),
                raw: Arc::clone(&self.raw),
            }),
            WireKind::BotLeftRoom { .. } | WireKind::RawOnly => None,
        }
    }
}

/// Parses IRCv3 `server-time` ISO-8601 / RFC 3339 timestamps (`YYYY-MM-DDThh:mm:ss[.sss]Z`).
pub(crate) fn parse_server_time(val: &str) -> Option<SystemTime> {
    chrono::DateTime::parse_from_rfc3339(val)
        .ok()
        .map(SystemTime::from)
}

/// Unwraps CTCP `\x01ACTION <text>\x01` if `body` is a CTCP ACTION.
pub(crate) fn parse_privmsg_body(body: &str) -> (MessageKind, String) {
    if let Some(rest) = body.strip_prefix("\x01ACTION") {
        let trimmed = rest.strip_suffix('\x01').unwrap_or(rest);
        if trimmed.is_empty() {
            return (MessageKind::Action, String::new());
        }
        if let Some(action_text) = trimmed.strip_prefix(' ') {
            return (MessageKind::Action, action_text.to_owned());
        }
    }
    (MessageKind::Privmsg, body.to_owned())
}

#[cfg(test)]
#[path = "message_test.rs"]
mod test;

