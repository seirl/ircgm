use super::*;
use crate::fmt::Colorize as _;
use irc::proto::Response;

#[tokio::test]
async fn crlf_splitting_ctcp_sanitization_and_396_wire_budget() {
    let mut ws = WorldState::new("GameBot");
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

#[tokio::test(start_paused = true)]
async fn priority_scheduling_same_channel_flush_and_write_receipts() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (sink, _task) = OutboundSink::new(RawTransport::Test(tx), 2, Duration::from_secs(8));
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

    // Enqueue Background KICK + PART on #vault and #other, followed by Interactive PRIVMSG and JOIN #VAULT:
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

    assert!(rx.try_recv().is_err());

    let start = Instant::now();
    let prompt_task = tokio::spawn(async move {
        r_prompt.wait().await;
        start.elapsed()
    });

    // t = 8s -> `urgent_prompt` + same-channel `KICK #vault Alice` pulled ahead of `JOIN #VAULT`:
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

    // t = 16s -> `PART #vault`, then `JOIN #VAULT`:
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

    // t = 24s -> remaining Background `#other` KICK + PART:
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

    // t = 32s -> QUIT drains last!
    tokio::time::advance(Duration::from_secs(8)).await;
    r_quit.wait().await;
    assert_eq!(rx.try_recv().unwrap(), Command::QUIT(Some("bye".into())));
}

#[tokio::test(start_paused = true)]
async fn join_followed_by_later_background_part_preserves_join_before_part() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (sink, _task) = OutboundSink::new(RawTransport::Test(tx), 1, Duration::from_secs(8));
    let cm = CaseMap::Rfc1459;

    // Consume burst = 1 slot at t = 0:
    sink.enqueue_one(
        Priority::Interactive,
        Command::PRIVMSG("#lobby".into(), "warmup".into()),
        cm,
    )
    .wait()
    .await;
    let _ = rx.try_recv().unwrap();

    // Now rate-limited: enqueue `JOIN #c` (Interactive), THEN `PART #c` (Background):
    let r_join = sink.enqueue_one(
        Priority::Interactive,
        Command::JOIN("#c".into(), None, None),
        cm,
    );
    let r_part = sink.enqueue_one(
        Priority::Background,
        Command::PART("#c".into(), None),
        cm,
    );

    tokio::time::advance(Duration::from_secs(8)).await;
    r_join.wait().await;
    assert_eq!(
        rx.try_recv().unwrap(),
        Command::JOIN("#c".into(), None, None)
    );

    tokio::time::advance(Duration::from_secs(8)).await;
    r_part.wait().await;
    assert_eq!(rx.try_recv().unwrap(), Command::PART("#c".into(), None));
}
