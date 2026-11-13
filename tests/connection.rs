mod common;

use std::time::Duration;

use irc::proto::Command;
use ircgm::fmt::{Color, ColorValue};
use ircgm::prelude::*;
use ircgm::testing::{test_harness, test_harness_with};
use ircgm::{race_all, BotConfig, BreakoutOptions, CaseMap, DisconnectReason};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

#[tokio::test]
async fn acyclic_bot_drop_releases_bot_core_with_surviving_user_and_member() {
    let (bot, server) = test_harness("GM");
    server.complete_join("#lobby", &["Alice"]);
    server.settle().await;

    let room = bot.room("#lobby").unwrap();
    let members = room.members();
    assert_eq!(members.len(), 2);
    let alice = bot.user("Alice").unwrap();
    let mut empty_inbox = bot.inbox();
    let buffered_inbox = bot.inbox();

    // Buffer an unread `WireEvent { User }` inside `buffered_inbox`, while `empty_inbox` drains it first:
    server.chat("Alice", "#lobby", "buffered unread line");
    server.settle().await;
    assert_eq!(
        empty_inbox.next_chat().await.unwrap().text,
        "buffered unread line"
    );

    // Cloning `Bot` shares ownership; dropping 1 of 2 clones keeps session alive:
    let bot_clone = bot.clone();
    drop(bot);
    assert!(room.say("still alive").is_ok());

    // Dropping the LAST `Bot` clone aborts pump, clears state/inboxes, and marks `BotDropped`:
    drop(bot_clone);
    tokio::task::yield_now().await;

    assert!(matches!(
        room.say("after drop"),
        Err(Error::Disconnected(DisconnectReason::BotDropped))
    ));
    assert!(matches!(
        alice.say("after drop"),
        Err(Error::Disconnected(DisconnectReason::BotDropped))
    ));
    assert!(matches!(
        empty_inbox.next_chat().await,
        Err(Error::Disconnected(DisconnectReason::BotDropped))
    ));

    // Drop `room`, `empty_inbox`, and `buffered_inbox` (without ever reading `buffered_inbox`'s unread `WireEvent`!)
    // while KEEPING `members` (`Member { User }`) and `alice` (`User`) alive:
    drop(room);
    drop(empty_inbox);
    drop(buffered_inbox);
    tokio::task::yield_now().await;

    // Zero reference cycles: `Arc<BotCore>` strong count MUST be 0!
    assert_eq!(server.core_strong_count(), 0);
    assert_eq!(members[0].nick, "Alice");
    assert_eq!(alice.nick(), "Alice");
}

#[tokio::test]
async fn loopback_tcp_connect_cap_motd_barrier_and_timeout_fallback() {
    // 1. Full `CAP REQ` -> `CAP ACK` -> `CAP END` + multi-line `005` + `422` barrier before initial `JOIN`:
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_task = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let (rd, mut wr) = sock.into_split();
        let mut lines = BufReader::new(rd).lines();

        let mut saw_cap_req = false;
        let mut saw_nick = false;
        let mut saw_user = false;
        while !(saw_cap_req && saw_nick && saw_user) {
            let line = lines.next_line().await.unwrap().unwrap();
            assert!(
                !line.starts_with("JOIN"),
                "Bot sent JOIN before registration barrier: {line}"
            );
            if line.starts_with("CAP REQ") {
                saw_cap_req = true;
            } else if line.starts_with("NICK ") {
                saw_nick = true;
            } else if line.starts_with("USER ") {
                saw_user = true;
            }
        }

        // Reply with CAP ACK -> bot must send CAP END, still NO JOIN before 376/422!
        wr.write_all(b":mock.irc CAP GM ACK :account-tag server-time\r\n")
            .await
            .unwrap();
        let cap_end = lines.next_line().await.unwrap().unwrap();
        assert_eq!(cap_end, "CAP END");

        // Send 001 + multi-line 005s (changing CASEMAPPING to ascii!) + 422 ERR_NOMOTD:
        wr.write_all(
            b":mock.irc 001 GM :Welcome GM!~gm@127.0.0.1\r\n\
              :mock.irc 005 GM CHANTYPES=# PREFIX=(ov)@+ :are supported\r\n\
              :mock.irc 005 GM CASEMAPPING=ascii MODES=4 :are supported\r\n\
              :mock.irc 422 GM :MOTD File is missing\r\n",
        )
        .await
        .unwrap();

        // Now Bot::connect_with unblocks and sends initial channel JOIN!
        let join_line = lines.next_line().await.unwrap().unwrap();
        assert_eq!(join_line, "JOIN #[Init]");
        wr.write_all(
            b":GM!~gm@127.0.0.1 JOIN #[Init]\r\n\
              :mock.irc 353 GM = #[Init] :@GM Alice\r\n\
              :mock.irc 366 GM #[Init] :End of /NAMES list.\r\n",
        )
        .await
        .unwrap();

        // Keep socket open until bot drops/quits:
        let _ = lines.next_line().await;
    });

    let irc_cfg = irc::client::data::Config {
        nickname: Some("GM".into()),
        server: Some("127.0.0.1".into()),
        port: Some(addr.port()),
        use_tls: Some(false),
        channels: vec!["#[Init]".into()],
        ..Default::default()
    };
    let bot = Bot::connect_with(BotConfig::new(irc_cfg)).await.unwrap();
    assert_eq!(bot.casemap(), CaseMap::Ascii);
    assert_eq!(bot.room("#[init]").unwrap().members().len(), 2);
    assert!(bot.room("#{init}").unwrap().members().is_empty());
    drop(bot);
    server_task.await.unwrap();

    // 2. Non-standard server omitting `376`/`422` falls back cleanly after `handshake_timeout`:
    let listener2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr2 = listener2.local_addr().unwrap();
    let server_task2 = tokio::spawn(async move {
        let (sock, _) = listener2.accept().await.unwrap();
        let (rd, _wr) = sock.into_split();
        let mut lines = BufReader::new(rd).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line == "CAP END" {
                break;
            }
        }
    });

    let mut cfg2 = BotConfig::new(irc::client::data::Config {
        nickname: Some("GM2".into()),
        server: Some("127.0.0.1".into()),
        port: Some(addr2.port()),
        use_tls: Some(false),
        ..Default::default()
    });
    cfg2.handshake_timeout = Duration::from_millis(40);
    let bot2 = Bot::connect_with(cfg2).await.unwrap();
    assert_eq!(bot2.nick(), "GM2");
    drop(bot2);
    server_task2.await.unwrap();
}

#[tokio::test]
async fn priority_scheduling_supervision_and_bot_quit_drain() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#lobby", &["Alice", "Bob"]);
    server.settle().await;
    let lobby = bot.room("#lobby").unwrap();

    let dyn_stops: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = &'static str> + Send>>> = vec![
        Box::pin(std::future::pending::<&'static str>()),
        Box::pin(async { "dynamic_stop" }),
    ];
    assert_eq!(race_all(dyn_stops).await, "dynamic_stop");

    let vault_task = tokio::spawn({
        let bot = bot.clone();
        async move {
            bot.breakout_with(BreakoutOptions::exact("#vault").modes(None::<&str>))
                .await
        }
    });
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::JOIN("#vault".into(), None, None)
    );
    server.complete_join("#vault", &["Alice", "Bob"]);
    let vault = vault_task.await.unwrap().unwrap();

    let game_bot = bot.clone();
    let game_lobby = lobby.clone();
    let game_task = tokio::spawn(async move {
        let outcome = async {
            let _keep_vault_alive = vault;
            std::future::pending::<()>().await
        }
        .until(game_bot.shutdown_requested())
        .await;
        assert_eq!(outcome, Outcome::Stopped(()));
        game_lobby.say("Server restarting!").unwrap();
    });
    tokio::task::yield_now().await;

    bot.request_shutdown();
    game_task.await.unwrap();
    bot.quit(Some("bye")).await.unwrap();

    let mut sent = Vec::new();
    while let Some(cmd) = server.try_recv_sent() {
        sent.push(cmd);
    }

    assert!(sent.contains(&Command::PRIVMSG(
        "#lobby".into(),
        "Server restarting!".into()
    )));
    let kick_a = sent
        .iter()
        .position(|c| matches!(c, Command::KICK(ch, u, _) if ch == "#vault" && u == "Alice"))
        .expect("KICK Alice");
    let kick_b = sent
        .iter()
        .position(|c| matches!(c, Command::KICK(ch, u, _) if ch == "#vault" && u == "Bob"))
        .expect("KICK Bob");
    let part_pos = sent
        .iter()
        .position(|c| matches!(c, Command::PART(ch, _) if ch == "#vault"))
        .expect("PART #vault");
    let quit_pos = sent
        .iter()
        .position(|c| c == &Command::QUIT(Some("bye".into())))
        .expect("QUIT :bye");

    assert!(kick_a < part_pos && kick_b < part_pos);
    assert_eq!(
        quit_pos,
        sent.len() - 1,
        "QUIT must be the final command: {sent:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn crlf_split_pacing_warn_396_cloak_wrapping_and_inbound_formatting() {
    let (logs, _guard) = common::capture_warns();
    let cfg = BotConfig {
        outbound_burst: 2,
        outbound_window: Duration::from_secs(8),
        ..BotConfig::default()
    };
    let (bot, mut server) = test_harness_with("GM", cfg);
    server.send_line(":irc.srv 001 GM :Welcome to IRC GM!~gamemaster@127.0.0.1\r\n");
    server.complete_join("#room", &["Alice"]);
    server.settle().await;

    let room = bot.room("#room").unwrap();

    room.say("hello\r\nQUIT :injected").unwrap();
    room.say("paced third line").unwrap();

    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::PRIVMSG("#room".into(), "hello".into())
    );
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::PRIVMSG("#room".into(), "QUIT :injected".into())
    );
    assert!(server.try_recv_sent().is_none());
    assert!(logs
        .text()
        .contains("outbound IRC rate limit reached; pacing queued commands"));

    tokio::time::advance(Duration::from_secs(8)).await;
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::PRIVMSG("#room".into(), "paced third line".into())
    );

    let (bot2, mut server2) = test_harness("GM");
    server2.send_line(":irc.srv 001 GM :Welcome to IRC GM!~gamemaster@127.0.0.1\r\n");
    let vhost_60 = format!("{}.users.cloak.example.org", "x".repeat(36));
    assert_eq!(vhost_60.len(), 60);
    server2.send_line(&format!(
        ":irc.srv 396 GM {vhost_60} :is now your hidden host\r\n"
    ));
    server2.settle().await;
    server2.complete_join("#heist-channel", &["Alice"]);
    server2.settle().await;

    let r2 = bot2.room("#heist-channel").unwrap();
    r2.say(("word ".repeat(120)).bold().red()).unwrap();

    let mut chunks = Vec::new();
    while let Some(cmd) = server2.try_recv_sent() {
        let Command::PRIVMSG(target, payload) = cmd else {
            panic!("expected PRIVMSG, got {cmd:?}");
        };
        let relayed = format!(":GM!~gamemaster@{vhost_60} PRIVMSG {target} :{payload}\r\n");
        assert!(
            relayed.len() <= 512,
            "relayed line exceeded 512 bytes: {} bytes",
            relayed.len()
        );
        let parsed = Text::parse(&payload);
        for sp in parsed.spans() {
            assert!(sp.style.bold);
            assert_eq!(sp.style.fg, Some(ColorValue::Palette(Color::Red)));
        }
        chunks.push(payload);
    }
    assert!(chunks.len() >= 2);

    let mut inb = r2.inbox();
    server2.chat("Alice", "#heist-channel", "\x02\x0304!join\x03\x02");
    server2.settle().await;

    let msg = inb.wait_cmd("!join").await.unwrap();
    assert_eq!(msg.text, "!join");
    assert!(msg.is_cmd("!join"));
    let styled = msg.to_styled();
    assert_eq!(styled.plain(), "!join");
    assert!(styled.spans()[0].style.bold);
    assert_eq!(
        styled.spans()[0].style.fg,
        Some(ColorValue::Palette(Color::Red))
    );
}
