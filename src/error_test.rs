use std::panic::Location;

use super::*;

#[test]
fn is_fatal_classifies_player_vs_session_errors() {
    assert!(!Error::TimedOut.is_fatal());
    assert!(!Error::UserLeft {
        nick: "alice".into(),
        reason: Some("Quit: bye".into()),
    }
    .is_fatal());
    assert!(!Error::DeliveryFailed {
        target: "alice".into(),
        code: 716,
        message: "is in +g mode".into(),
    }
    .is_fatal());

    assert!(Error::Disconnected(DisconnectReason::Eof).is_fatal());
    assert!(Error::Disconnected(DisconnectReason::Quit).is_fatal());
    assert!(Error::Disconnected(DisconnectReason::BotDropped).is_fatal());
    assert!(Error::Disconnected(DisconnectReason::Transport("reset".into())).is_fatal());
    assert!(Error::Lagged {
        scope: "room #lobby".into(),
        skipped: 42,
        opened_at: Location::caller(),
    }
    .is_fatal());
    assert!(Error::JoinFailed {
        channel: "#secret".into(),
        code: 473,
        message: "Cannot join (+i)".into(),
    }
    .is_fatal());
    assert!(Error::BotLeftRoom {
        channel: "#lobby".into(),
        reason: None,
    }
    .is_fatal());
    assert!(Error::NotChannelOp {
        channel: "#heist-123".into(),
    }
    .is_fatal());
    assert!(Error::HandshakeTimeout {
        operation: "JOIN / NAMES",
    }
    .is_fatal());
    assert!(Error::InvalidInput("empty target").is_fatal());
}

#[test]
fn display_formats_actionable_messages_and_optional_reasons() {
    let lag = Error::Lagged {
        scope: "room #quiz".into(),
        skipped: 12,
        opened_at: Location::caller(),
    };
    let lag_str = lag.to_string();
    assert!(lag_str.contains("room #quiz"));
    assert!(lag_str.contains("by 12 messages"));
    assert!(lag_str.contains(".clear() or .lossy()"));

    let left_no_reason = Error::UserLeft {
        nick: "bob".into(),
        reason: None,
    };
    assert_eq!(left_no_reason.to_string(), "user bob left");

    let left_with_reason = Error::UserLeft {
        nick: "bob".into(),
        reason: Some("Ping timeout".into()),
    };
    assert_eq!(left_with_reason.to_string(), "user bob left (Ping timeout)");

    let bot_left_no_reason = Error::BotLeftRoom {
        channel: "#game".into(),
        reason: None,
    };
    assert_eq!(
        bot_left_no_reason.to_string(),
        "bot is no longer in channel #game"
    );

    let bot_left_with_reason = Error::BotLeftRoom {
        channel: "#game".into(),
        reason: Some("kicked by oper".into()),
    };
    assert_eq!(
        bot_left_with_reason.to_string(),
        "bot is no longer in channel #game (kicked by oper)"
    );
}
