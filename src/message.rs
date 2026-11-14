use std::sync::Arc;

use irc::proto::Message;
use irc_colors::Text;
use tokio::time::Instant;

use crate::error::Result;
use crate::outbound::OutboundChatKind;
use crate::state::BotPresence;
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
    /// Monotonic local timestamp captured when the pump processed this frame.
    pub received_at: Instant,
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

    /// Re-parses `raw_text` into an `irc_colors::Text` preserving sender colors/styles.
    pub fn to_styled(&self) -> Text {
        Text::parse(&self.raw_text)
    }

    /// Word-boundary, ASCII case-insensitive command check against plain `self.text`
    /// (`"!join"` matches `"!join"` and `"!JOIN"`, not `"!joined"`).
    pub fn is_cmd(&self, cmd: &str) -> bool {
        self.cmd_args(cmd).is_some()
    }

    /// Returns trimmed arguments after `cmd` (matched ASCII case-insensitively at a word boundary),
    /// including `Some("")` when invoked without arguments.
    pub fn cmd_args(&self, cmd: &str) -> Option<&str> {
        if cmd.is_empty() {
            return None;
        }
        let trimmed = self.text.trim_start();
        if trimmed.len() < cmd.len() || !trimmed.is_char_boundary(cmd.len()) {
            return None;
        }
        if !trimmed[..cmd.len()].eq_ignore_ascii_case(cmd) {
            return None;
        }
        let rest = &trimmed[cmd.len()..];
        if rest.is_empty() {
            Some("")
        } else if rest.starts_with(char::is_whitespace) {
            Some(rest.trim())
        } else {
            None
        }
    }

    /// Returns the non-empty trimmed argument string after `cmd` (`None` if `cmd` did not match
    /// or was invoked without arguments).
    pub fn cmd_arg(&self, cmd: &str) -> Option<&str> {
        self.cmd_args(cmd).filter(|s| !s.is_empty())
    }

    /// Outbound kind for any automatic answer to this message: `NOTICE` for a `NOTICE` (which
    /// must never trigger an automatic `PRIVMSG`, RFC 2812 §3.3.2), else `PRIVMSG`.
    fn reply_kind(&self) -> OutboundChatKind {
        match self.kind {
            MessageKind::Notice => OutboundChatKind::Notice,
            MessageKind::Privmsg | MessageKind::Action => OutboundChatKind::Privmsg,
        }
    }

    /// Replies to the channel if this message arrived in a channel (failing with
    /// [`Error::BotLeftRoom`] if the bot has since left that channel), or directly to
    /// `self.sender`'s live nickname if this message was a DM (failing with [`Error::UserLeft`]
    /// if `self.sender` has quit), mirroring `self.kind` (`MessageKind::Notice` sends `NOTICE`;
    /// `Privmsg | Action` sends `PRIVMSG`).
    pub fn reply(&self, text: impl Into<Text>) -> Result<()> {
        let out_kind = self.reply_kind();
        match &self.target {
            Target::Channel(ch) => {
                let core = self.sender.upgrade_core()?;
                core.state
                    .read()
                    .unwrap()
                    .check_bot_in_channel(ch, BotPresence::Tracked)?;
                core.send_chat(out_kind, ch, text.into())?;
            }
            Target::Dm => {
                self.sender.send_chat(out_kind, text.into())?;
            }
        }
        Ok(())
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

/// Internal wire metadata paired with [`WireEvent`] when an [`Event`] carries internal-only
/// routing or prompt context alongside its public payload.
#[derive(Clone, Debug, Default)]
pub(crate) enum WireMeta {
    #[default]
    None,
    /// Populated on [`Event::Notice`] when the `NOTICE` came from a human `nick!user@host`
    /// without `@bot` tag or IRC services pseudoclient nickname, allowing `.accept_notices()` to
    /// yield it as a [`ChatMessage`].
    HumanNotice(Box<ChatMessage>),
    /// Resolved `UserId` (`UserTrack::id`) on non-channel [`Event::DeliveryFailed`] bounces
    /// (`401`, `486`, `716`, etc.).
    BounceUser(u64),
}

/// Internal wire frame distributed from the pump to matching [`crate::Inbox`] slots.
#[derive(Clone, Debug)]
pub(crate) struct WireEvent {
    pub(crate) seq: u64,
    pub(crate) raw: Arc<Message>,
    pub(crate) kind: WireKind,
    pub(crate) meta: WireMeta,
}

#[derive(Clone, Debug)]
pub(crate) enum WireKind {
    Event(Event),
    BotLeftRoom {
        channel: String,
        reason: Option<String>,
    },
    RawOnly,
}

impl WireEvent {
    #[cfg(test)]
    pub(crate) fn new(seq: u64, raw: Arc<Message>, kind: WireKind) -> Self {
        Self {
            seq,
            raw,
            kind,
            meta: WireMeta::None,
        }
    }

    pub(crate) fn bounce_user_id(&self) -> Option<u64> {
        match &self.meta {
            WireMeta::BounceUser(uid) => Some(*uid),
            WireMeta::None | WireMeta::HumanNotice(_) => None,
        }
    }

    pub(crate) fn human_notice(&self) -> Option<&ChatMessage> {
        match &self.meta {
            WireMeta::HumanNotice(msg) => Some(msg),
            WireMeta::None | WireMeta::BounceUser(_) => None,
        }
    }

    pub(crate) fn to_public_event(&self) -> Option<Event> {
        match &self.kind {
            WireKind::Event(ev) => Some(ev.clone()),
            WireKind::BotLeftRoom { .. } | WireKind::RawOnly => None,
        }
    }
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

