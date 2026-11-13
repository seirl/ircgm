use std::time::UNIX_EPOCH;

use irc_colors::Colorize;

use super::*;
use crate::state::parse_irc_message;
use crate::testing::test_harness;

#[test]
fn parse_server_time_handles_rfc3339_and_rejects_garbage() {
    let ts = parse_server_time("2026-04-06T12:34:56.789Z").expect("valid server-time");
    let millis = ts.duration_since(UNIX_EPOCH).unwrap().as_millis();
    assert_eq!(millis, 1_775_478_896_789);

    let ts_no_frac = parse_server_time("1970-01-01T00:00:05Z").expect("valid integer seconds");
    assert_eq!(ts_no_frac.duration_since(UNIX_EPOCH).unwrap().as_secs(), 5);

    assert!(parse_server_time("not-a-timestamp").is_none());
    assert!(parse_server_time("").is_none());
}

#[test]
fn parse_privmsg_body_unwraps_ctcp_action_only() {
    assert_eq!(
        parse_privmsg_body("\x01ACTION rolls the dice\x01"),
        (MessageKind::Action, "rolls the dice".to_owned())
    );
    assert_eq!(
        parse_privmsg_body("\x01ACTION\x01"),
        (MessageKind::Action, String::new())
    );
    assert_eq!(
        parse_privmsg_body("\x01ACTION missing trailing delimiter"),
        (MessageKind::Action, "missing trailing delimiter".to_owned())
    );
    // Not a space-separated ACTION (`\x01ACTIONPACKED\x01`) or other CTCP (`\x01VERSION\x01`):
    assert_eq!(
        parse_privmsg_body("\x01ACTIONPACKED\x01"),
        (MessageKind::Privmsg, "\x01ACTIONPACKED\x01".to_owned())
    );
    assert_eq!(
        parse_privmsg_body("\x01VERSION\x01"),
        (MessageKind::Privmsg, "\x01VERSION\x01".to_owned())
    );
    assert_eq!(
        parse_privmsg_body("hello world"),
        (MessageKind::Privmsg, "hello world".to_owned())
    );
}

#[test]
fn target_predicates_distinguish_channel_and_dm() {
    let ch = Target::Channel("#lobby".into());
    assert!(ch.is_channel());
    assert!(!ch.is_dm());

    let dm = Target::Dm;
    assert!(dm.is_dm());
    assert!(!dm.is_channel());
}

#[tokio::test]
async fn chat_message_command_matching_style_preservation_and_room_check() {
    let (bot, server) = test_harness("gm");
    let room = bot.room("#Lobby").unwrap();

    let styled = "  !join   alpha beta  ".bold().red().to_irc_string();
    server.send_line(&format!(":alice!~a@host PRIVMSG #lobby :{styled}\r\n"));
    let mut inbox = room.inbox();
    server.settle().await;

    let msg = inbox.next_chat().await.unwrap();
    assert!(!msg.is_dm());
    assert!(!msg.is_action());
    assert!(!msg.is_notice());
    assert!(msg.is_in(&room));
    assert!(!msg.is_in(&bot.room("#other").unwrap()));

    assert!(msg.is_cmd("!join"));
    assert!(msg.is_cmd("!JOIN"));
    assert!(!msg.is_cmd("!joined"));
    assert!(!msg.is_cmd("!joi"));
    assert!(!msg.is_cmd(""));
    assert_eq!(msg.cmd_args("!join"), Some("alpha beta"));
    assert_eq!(msg.cmd_args("!JOIN"), Some("alpha beta"));
    assert_eq!(msg.cmd_arg("!JOIN"), Some("alpha beta"));

    // Bare command without arguments yields `cmd_args == Some("")` and `cmd_arg == None`:
    server.chat("alice", "#lobby", "!STAND");
    server.settle().await;
    let bare = inbox.next_chat().await.unwrap();
    assert!(bare.is_cmd("!stand"));
    assert_eq!(bare.cmd_args("!stand"), Some(""));
    assert_eq!(bare.cmd_arg("!stand"), None);

    // Style codes were stripped from `msg.text` but preserved in `msg.to_styled()`:
    assert_eq!(msg.text, "  !join   alpha beta  ");
    assert_eq!(msg.to_styled().to_irc_string(), styled);
}

#[tokio::test]
async fn wire_event_to_public_event_filters_internal_variants() {
    let (bot, _server) = test_harness("gm");
    let alice = bot.user("alice").unwrap();
    let raw = Arc::new(parse_irc_message(":srv 401 gm alice :No such nick\r\n").unwrap());

    let internal_bot_left = WireEvent {
        seq: 1,
        raw: Arc::clone(&raw),
        kind: WireKind::BotLeftRoom {
            channel: "#lobby".into(),
            reason: None,
        },
    };
    assert!(internal_bot_left.to_public_event().is_none());

    let internal_raw = WireEvent {
        seq: 2,
        raw: Arc::clone(&raw),
        kind: WireKind::RawOnly,
    };
    assert!(internal_raw.to_public_event().is_none());

    let send_err = WireEvent {
        seq: 3,
        raw: Arc::clone(&raw),
        kind: WireKind::SendError {
            target: "alice".into(),
            target_user_id: Some(alice.id().as_u64()),
            code: 401,
            message: "No such nick".into(),
        },
    };
    match send_err.to_public_event() {
        Some(Event::DeliveryFailed {
            target,
            code,
            message,
            ..
        }) => {
            assert_eq!(target, "alice");
            assert_eq!(code, 401);
            assert_eq!(message, "No such nick");
        }
        other => panic!("expected Event::DeliveryFailed, got {other:?}"),
    }

    let quit = WireEvent {
        seq: 4,
        raw,
        kind: WireKind::Quit {
            user: alice,
            nick: "alice".into(),
            channels: vec!["#lobby".into()],
            reason: Some("a.irc b.irc".into()),
            netsplit: true,
        },
    };
    match quit.to_public_event() {
        Some(Event::Quit {
            nick,
            channels,
            netsplit,
            ..
        }) => {
            assert_eq!(nick, "alice");
            assert_eq!(channels, vec!["#lobby"]);
            assert!(netsplit);
        }
        other => panic!("expected Event::Quit, got {other:?}"),
    }
}
