use super::*;
use crate::casemap::AtomicCaseMap;
use crate::fmt::Colorize as _;
use irc::proto::Response;

#[tokio::test]
async fn crlf_splitting_ctcp_sanitization_and_396_wire_budget() {
    let mut ws = WorldState::new("GameBot", AtomicCaseMap::default());
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

    // 2. Embedded \x01 stripped on PRIVMSG, and Action wraps in \x01ACTION ...\x01:
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

    let cmds_act = build_chat_commands(
        OutboundChatKind::Action,
        "#room",
        Text::from("waves\x01"),
        &ws,
        450,
    );
    assert_eq!(
        cmds_act,
        vec![Command::PRIVMSG("#room".into(), "\x01ACTION waves\x01".into())]
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
        let parsed = Text::parse(payload);
        for sp in parsed.spans() {
            assert!(sp.style.bold);
            assert_eq!(
                sp.style.fg,
                Some(irc_colors::ColorValue::Palette(irc_colors::Color::Red))
            );
        }
    }

    // 4. Identifier & parameter validation + reason sanitization:
    assert!(validate_target("#ok", "channel").is_ok());
    assert!(validate_target("", "channel").is_err());
    assert!(validate_target("#bad\r\nQUIT", "channel").is_err());
    assert!(validate_target("bad nick", "nick").is_err());
    assert!(validate_param("+m", "mode").is_ok());
    assert!(validate_param(":leading_colon", "mode").is_err());
    assert_eq!(sanitize_reason("bye\r\nQUIT\0"), "byeQUIT");
}

#[test]
fn fifo_outbound_sink_stops_accepting_commands_after_quit_or_abort() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let sink = OutboundSink::new(RawTransport::test_only(tx));

    sink.enqueue_batch(vec![
        Command::PRIVMSG("#lobby".into(), "a".into()),
        Command::PRIVMSG("#lobby".into(), "b".into()),
    ]);
    sink.send_quit(Some("bye"));
    sink.enqueue_one(Command::PRIVMSG("#lobby".into(), "ignored".into()));

    assert_eq!(
        rx.try_recv().unwrap(),
        Command::PRIVMSG("#lobby".into(), "a".into())
    );
    assert_eq!(
        rx.try_recv().unwrap(),
        Command::PRIVMSG("#lobby".into(), "b".into())
    );
    assert_eq!(rx.try_recv().unwrap(), Command::QUIT(Some("bye".into())));
    assert!(rx.try_recv().is_err());
}
