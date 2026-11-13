use std::panic::Location;

/// Reason why the underlying IRC connection session ended.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisconnectReason {
    /// Clean shutdown requested via `bot.quit(...)`.
    Quit,
    /// All `Bot` handles were dropped while a `Room` / `User` / `Inbox` survived.
    BotDropped,
    /// Server closed the stream cleanly (`EOF`).
    Eof,
    /// Underlying socket / TLS / protocol transport error.
    Transport(String),
}

/// Errors returned by `ircgm` operations.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("IRC client error: {0}")]
    Irc(#[from] irc::error::Error),

    #[error("IRC connection closed ({0:?})")]
    Disconnected(DisconnectReason),

    #[error(
        "inbox ({scope}, opened at {opened_at}) lagged behind stream by {skipped} messages. \
         Did you leave an Inbox open across an .await? Use .clear() or .lossy() if intended."
    )]
    Lagged {
        scope: String,
        skipped: u64,
        opened_at: &'static Location<'static>,
    },

    #[error("timed out waiting for reply")]
    TimedOut,

    #[error("user {nick} left{}", reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default())]
    UserLeft {
        nick: String,
        reason: Option<String>,
    },

    #[error("failed to join {channel} ({code}): {message}")]
    JoinFailed {
        channel: String,
        code: u16,
        message: String,
    },

    #[error("channel send to {channel} rejected ({code}): {message}")]
    ChannelSendFailed {
        channel: String,
        code: u16,
        message: String,
    },

    #[error("message delivery to {nick} rejected ({code}): {message}")]
    UserUnreachable {
        nick: String,
        code: u16,
        message: String,
    },

    #[error("bot is no longer in channel {channel}{}", reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default())]
    BotLeftRoom {
        channel: String,
        reason: Option<String>,
    },

    #[error("bot does not have channel operator (+o) privileges in {channel}")]
    NotChannelOp { channel: String },

    #[error("timed out waiting for {operation}")]
    HandshakeTimeout { operation: &'static str },

    #[error("invalid IRC parameter: {0}")]
    InvalidInput(&'static str),
}

impl Error {
    /// Returns `true` if the underlying IRC connection session itself has terminated or failed
    /// (`Disconnected` or `Irc`), as opposed to a room- or player-scoped error.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Error::Disconnected(_) | Error::Irc(_))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
#[path = "error_test.rs"]
mod test;

