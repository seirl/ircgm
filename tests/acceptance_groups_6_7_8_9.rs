mod common;

use std::time::Duration;

use futures::future::try_join_all;
use irc::proto::Command;
use ircgm::fmt::{Color, ColorValue};
use ircgm::prelude::*;
use ircgm::testing::{test_harness, test_harness_with};
use ircgm::{race_all, BreakoutOptions, CaseMap, DisconnectReason};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

// ============================================================================
// Group 6 (DESIGN.md §6.6): Acyclic Bot drop, 376/422 registration barrier,
// Breakout invites, priority scheduling, & bot.quit drain
// ============================================================================

#[tokio::test]
async fn group6_acyclic_bot_drop_releases_bot_core_with_surviving_user_and_member() {
    let (bot, server) = test_harness("GM");
    server.complete_join("#lobby", &["Alice"]);
    server.settle().await;

    let room = bot.room("#lobby").unwrap();
    let members = room.members();
    assert_eq!(members.len(), 2);
    let alice = bot.user("Alice").unwrap();
    let mut empty_inbox = bot.inbox_all();
    let buffered_inbox = bot.inbox_all();

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
async fn group6_loopback_tcp_connect_cap_motd_barrier_and_timeout_fallback() {
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
    assert!(bot.room("#{init}").unwrap().members().is_empty()); // ASCII casemap active before JOIN!
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
async fn group6_breakout_invite_and_wait_vs_invite_available_and_room_invite() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#lobby", &["OfflineGuy", "U1", "U2", "U3"]);
    server.settle().await;

    let lobby = bot.room("#lobby").unwrap();
    let offline_u = bot.user("OfflineGuy").unwrap();
    let u1 = bot.user("U1").unwrap();
    let u2 = bot.user("U2").unwrap();
    let u3 = bot.user("U3").unwrap();

    server.quit_user("OfflineGuy", Some("gone"));
    server.settle().await;

    // 1. `room.invite(&offline_u)` fails immediately with `UserLeft` and emits ZERO traffic:
    let inv_err = lobby.invite(&offline_u).unwrap_err();
    assert!(matches!(inv_err, Error::UserLeft { .. }));
    assert!(server.try_recv_sent().is_none());

    // `room.invite(&u1)` accepts `&User` directly:
    lobby.invite(&u1).unwrap();
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::INVITE("U1".into(), "#lobby".into())
    );

    // Create breakout `#vault`:
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
    server.complete_join("#vault", &[]);
    let vault = vault_task.await.unwrap().unwrap();

    // 2. `invite_and_wait` with an already-offline `User` fails immediately without sending `INVITE`:
    let strict_err = vault
        .invite_and_wait([&offline_u, &u1], Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(matches!(strict_err, Error::UserLeft { .. }));
    assert!(server.try_recv_sent().is_none());

    // 3. `invite_available` skips `offline_u`, invites `u1, u2, u3`; `u1` joins, `u2` quits, `u3` times out -> `Ok(vec![u1])`:
    let avail_fut = vault.invite_available([&offline_u, &u1, &u2, &u3], Duration::from_millis(40));
    let drive = async {
        assert_eq!(
            server.recv_sent().await.unwrap(),
            Command::INVITE("U1".into(), "#vault".into())
        );
        assert_eq!(
            server.recv_sent().await.unwrap(),
            Command::INVITE("U2".into(), "#vault".into())
        );
        assert_eq!(
            server.recv_sent().await.unwrap(),
            Command::INVITE("U3".into(), "#vault".into())
        );
        server.send_line(":U1!~U1@test.host JOIN #vault\r\n");
        server.quit_user("U2", Some("nah"));
        server.settle().await;
    };
    let (joined, ()) = tokio::join!(avail_fut, drive);
    assert_eq!(joined.unwrap(), vec![u1.clone()]);

    // If none join (only `u3` invited and doesn't join) -> `Ok(vec![])`:
    let none_joined = vault
        .invite_available([&u3], Duration::from_millis(20))
        .await
        .unwrap();
    let _ = server.recv_sent().await; // drain INVITE U3
    assert!(none_joined.is_empty());

    // Cleanly close `vault` via `.close().await`:
    let close_fut = vault.close();
    let ack_part = async {
        assert_eq!(
            server.recv_sent().await.unwrap(),
            Command::KICK("#vault".into(), "U1".into(), Some("Session ended".into()))
        );
        assert_eq!(
            server.recv_sent().await.unwrap(),
            Command::PART("#vault".into(), None)
        );
        server.send_line(":GM!~GM@test.host PART #vault\r\n");
        server.settle().await;
    };
    let (close_res, ()) = tokio::join!(close_fut, ack_part);
    close_res.unwrap();
}

#[tokio::test]
async fn group6_priority_scheduling_supervision_and_bot_quit_drain() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#lobby", &["Alice", "Bob"]);
    server.settle().await;
    let lobby = bot.room("#lobby").unwrap();

    // Verify `race_all` + `.until().or_until()` + `ok_or_pending()`:
    let dyn_stops: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = &'static str> + Send>>> = vec![
        Box::pin(std::future::pending::<&'static str>()),
        Box::pin(async { "dynamic_stop" }),
    ];
    assert_eq!(race_all(dyn_stops).await, "dynamic_stop");

    // Create `#vault` breakout with Alice and Bob inside, then supervise game under `.until(bot.shutdown_requested())`:
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
        // Notice: `vault` ALREADY dropped when the inner future dropped, queuing Background KICKs + PART!
        // Now emit an Interactive farewell `say` AFTER `vault` dropped—Interactive MUST overtake or run in per-class FIFO,
        // and `bot.quit` MUST drain all Interactive + Background commands before `QUIT`!
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

    // Verify farewell PRIVMSG, Background KICKs for Alice & Bob -> PART #vault, and QUIT LAST!
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
    assert_eq!(quit_pos, sent.len() - 1, "QUIT must be the final command: {sent:?}");
}

// ============================================================================
// Group 7 (DESIGN.md §6.7): Unwatched-channel skip, multi-bucket dedup,
// lag isolation, 75%-fill warn, #[track_caller] Error::Lagged hint, & .lossy()
// ============================================================================

#[tokio::test]
async fn group7_unwatched_channel_zero_alloc_and_multichannel_dedup() {
    let (bot, server) = test_harness("GM");
    server.complete_join("#watched", &[]);
    server.settle().await;

    let baseline_tracks = server.tracked_user_count();

    // 1. 50 chat lines on `#unwatched` with zero `all_ids` or `#unwatched` slots -> ZERO `UserTrack` allocations!
    for i in 0..50 {
        server.send_line(&format!(":Stranger{i}!u@h PRIVMSG #unwatched :noisy spam {i}\r\n"));
    }
    server.settle().await;
    assert_eq!(server.tracked_user_count(), baseline_tracks);

    // 2. Multi-channel `NickChanged` & `Quit` dispatch AT MOST ONCE per multi-room `Inbox`:
    server.complete_join("#r1", &["Alice"]);
    server.complete_join("#r2", &["Alice"]);
    server.settle().await;

    let r1 = bot.room("#r1").unwrap();
    let r2 = bot.room("#r2").unwrap();
    let mut multi = bot.inbox_rooms([&r1, &r2]);

    server.send_line(":Alice!~Alice@test.host NICK :AliceRenamed\r\n");
    server.quit_user("AliceRenamed", Some("done"));
    server.chat("Observer", "#r1", "sentinel");
    server.settle().await;

    let ev1 = multi.next_event().await.unwrap();
    let ev2 = multi.next_event().await.unwrap();
    let ev3 = multi.next_event().await.unwrap();
    assert!(matches!(ev1, Event::NickChanged { ref new_nick, .. } if new_nick == "AliceRenamed"));
    assert!(matches!(ev2, Event::Quit { ref nick, .. } if nick == "AliceRenamed"));
    assert!(matches!(ev3, Event::Chat(ref m) if m.text == "sentinel"));
}

#[tokio::test]
async fn group7_lag_isolation_75pct_warn_track_caller_hint_and_lossy_recovery() {
    let (logs, _guard) = common::capture_warns();
    let cfg = BotConfig {
        inbox_capacity: 4,
        ..BotConfig::default()
    };
    let (bot, server) = test_harness_with("GM", cfg);
    server.complete_join("#lobby", &["Alice", "Bob"]);
    server.settle().await;

    let alice = bot.user("Alice").unwrap();
    let bob = bot.user("Bob").unwrap();
    let room = bot.room("#lobby").unwrap();

    // 1. Hold unpolled `ExclusiveDm` on Alice while injecting 100 `#lobby` lines -> ZERO lag on Alice's DM!
    let mut alice_dm = alice.exclusive().await.unwrap();
    for i in 0..100 {
        server.chat("Bob", "#lobby", &format!("lobby noise {i}"));
    }
    server.dm("Alice", "clean dm");
    server.settle().await;
    assert_eq!(alice_dm.inbox().next_chat().await.unwrap().text, "clean dm");
    drop(alice_dm);

    // 2. 75%-fill warning (3 of 4) + `Error::Lagged` on overflow (6 total -> `skipped == 2`) with `#[track_caller]`:
    let open_line = line!() + 1;
    let mut lag_in = alice.inbox();

    for i in 1..=3 {
        server.dm("Alice", &format!("m{i}"));
    }
    server.settle().await;
    let warn_text = logs.text();
    assert!(
        warn_text.contains("inbox buffer reached >=75% capacity")
            && warn_text.contains("user Alice"),
        "expected 75% capacity warn, got: {warn_text}"
    );

    // Push 3 more (6 total, capacity 4 -> 2 dropped):
    for i in 4..=6 {
        server.dm("Alice", &format!("m{i}"));
    }
    server.settle().await;

    let lag_err = lag_in.next_chat().await.unwrap_err();
    let rendered = lag_err.to_string();
    match lag_err {
        Error::Lagged {
            scope,
            skipped,
            opened_at,
        } => {
            assert_eq!(scope, "user Alice");
            assert_eq!(skipped, 2);
            assert_eq!(opened_at.line(), open_line);
        }
        other => panic!("expected Error::Lagged, got {other:?}"),
    }
    assert!(
        rendered.contains(
            "Did you leave an Inbox open across an .await? Use .clear() or .lossy() if intended."
        ),
        "missing actionable hint in: {rendered}"
    );

    // 3. `.lossy()` in multi-voter `try_join_all` recovers when Alice floods 10 junk lines (overflowing capacity 4!)
    //    without returning `Err(Error::Lagged)` or aborting other voters:
    let voters = vec![alice.clone(), bob.clone()];
    let mut lossy_dms: Vec<_> = voters
        .into_iter()
        .map(|u| (u.clone(), u.inbox().lossy()))
        .collect();
    let _ = room.say("Vote now!");

    // Overflow Alice's 4-slot buffer with 10 junk lines before polling:
    for i in 0..10 {
        server.dm("Alice", &format!("spam {i}"));
    }
    server.settle().await;

    let collect_fut = try_join_all(lossy_dms.iter_mut().map(|(u, inb)| async move {
        let pick = inb
            .wait_map(|m| m.cmd_args("!vote").map(str::to_owned))
            .await?;
        Ok::<_, Error>((u.nick(), pick))
    }));
    let send_votes = async {
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        server.dm("Alice", "!vote Bob");
        server.dm("Bob", "!vote Alice");
        server.settle().await;
    };
    let (votes_res, ()) = tokio::join!(collect_fut, send_votes);
    let votes = votes_res.unwrap();
    assert_eq!(
        votes,
        vec![
            ("Alice".into(), "Bob".into()),
            ("Bob".into(), "Alice".into())
        ]
    );
}

// ============================================================================
// Group 8 (DESIGN.md §6.8): DeliveryFailed (401/403/404/442/486/492/531/716),
// Event::DeliveryFailed, BotLeftRoom, non-occupant ask_user, & echo-message
// ============================================================================

#[tokio::test]
async fn group8_delivery_failed_bot_left_room_absent_ask_user_and_self_echo() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#room", &["Alice"]);
    server.complete_join("#other", &["Bob"]);
    server.settle().await;

    let room = bot.room("#room").unwrap();
    let alice = bot.user("Alice").unwrap();
    let bob = bot.user("Bob").unwrap();

    // 1. `room.ask_user(&bob, ...)` on synced `#room` where `Bob` is absent fails immediately
    //    with `Err(Error::UserLeft { .. })` WITHOUT sending `PRIVMSG`!
    let absent_err = room
        .ask_user(&bob, "Bob are you here?", |_| Some(()))
        .await
        .unwrap_err();
    assert!(matches!(absent_err, Error::UserLeft { ref nick, .. } if nick == "Bob"));
    assert!(server.try_recv_sent().is_none());

    // 2. IRCv3 `echo-message`: self `:GM!b@h PRIVMSG #room :!join` NEVER triggers `wait_cmd` or `ask`,
    //    while still appearing on `next_raw`:
    let mut raw_in = room.inbox();
    let mut cmd_in = room.inbox();
    server.send_line(":GM!b@h PRIVMSG #room :!join\r\n");
    server.chat("Alice", "#room", "!join");
    server.settle().await;

    let m = cmd_in.wait_cmd("!join").await.unwrap();
    assert_eq!(m.nick, "Alice"); // Skipped GM's self-echo!
    let first_raw = raw_in.next_raw().await.unwrap();
    assert!(matches!(&first_raw.command, Command::PRIVMSG(ch, txt) if ch == "#room" && txt == "!join"));

    // 3. `room.ask` aborts immediately with `Err(Error::DeliveryFailed)` on `404 ERR_CANNOTSENDTOCHAN`:
    let r_404 = room.clone();
    let ask_404 = tokio::spawn(async move { r_404.ask("Hello?", |_| Some(())).await });
    tokio::task::yield_now().await;
    let _ = server.take_messages_to("#room");
    server.send_line(":irc.srv 404 GM #room :Cannot send to channel (+m)\r\n");
    server.settle().await;
    let err_404 = ask_404.await.unwrap().unwrap_err();
    assert!(matches!(err_404, Error::DeliveryFailed { ref target, code: 404, .. } if target == "#room"));

    // 4. `Event::DeliveryFailed` surfaced on `bot.inbox_dms().next_event()` for one-way `alice.say(...)`,
    //    plus PM-block codes `486`, `492`, `531`, `716` aborting `alice.ask(...)` -> `Turn::Unreachable`:
    let mut dms_events = bot.inbox_dms();
    alice.say("Your secret role is Seer!").unwrap();
    let _ = server.take_messages_to("Alice");
    server.send_line(":irc.srv 716 GM Alice :is in +g mode (callerid)\r\n");
    server.settle().await;

    let ev = dms_events.next_event().await.unwrap();
    assert!(matches!(ev, Event::DeliveryFailed { ref target, code: 716, .. } if target == "Alice"));

    for code in [486u16, 492, 531, 716] {
        let a = alice.clone();
        let t = tokio::spawn(async move { a.ask("Ping?", |_| Some(())).settled().await });
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let _ = server.take_messages_to("Alice");
        server.send_line(&format!(":irc.srv {code} GM Alice :blocked ({code})\r\n"));
        server.settle().await;
        let out = t.await.unwrap().unwrap();
        assert!(
            matches!(out, Turn::Unreachable { ref target, code: c, .. } if target == "Alice" && c == code),
            "expected Unreachable({code}), got {out:?}"
        );
    }

    // 5. Bot itself `KICK`ed from `#room` aborts active `room.ask` with `Err(Error::BotLeftRoom)`:
    let r_kick = room.clone();
    let ask_kicked = tokio::spawn(async move { r_kick.ask("Anyone?", |_| Some(())).await });
    tokio::task::yield_now().await;
    let _ = server.take_messages_to("#room");
    server.send_line(":Op!~op@h KICK #room GM :bots out\r\n");
    server.settle().await;
    let err_left = ask_kicked.await.unwrap().unwrap_err();
    assert!(matches!(
        err_left,
        Error::BotLeftRoom { ref channel, ref reason } if channel == "#room" && reason.as_deref() == Some("bots out")
    ));
}

// ============================================================================
// Group 9 (DESIGN.md §6.9): CRLF splitting, OutboundSink pacing warn,
// 396 cloak 512-byte wrapping with styles, & inbound formatting strip / to_styled
// ============================================================================

#[tokio::test(start_paused = true)]
async fn group9_crlf_split_pacing_warn_396_cloak_wrapping_and_inbound_formatting() {
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

    // 1. CRLF injection splits into 2 separate `PRIVMSG` commands, and 3rd message triggers burst pacing + warn!
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

    // 2. Inject `396` cloak with a 60-byte vhost and verify dynamic 512-byte wrapping preserves Style:
    let (bot2, mut server2) = test_harness("GM");
    server2.send_line(":irc.srv 001 GM :Welcome to IRC GM!~gamemaster@127.0.0.1\r\n");
    let vhost_60 = format!("{}.users.cloak.example.org", "x".repeat(36));
    assert_eq!(vhost_60.len(), 60);
    server2.send_line(&format!(":irc.srv 396 GM {vhost_60} :is now your hidden host\r\n"));
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

    // 3. Inbound `"\x02!join\x02"` strips formatting in `msg.text` / `msg.is_cmd` and preserves style in `to_styled()`:
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
