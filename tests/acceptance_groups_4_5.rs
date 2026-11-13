mod common;

use std::time::Duration;

use futures::future::try_join_all;
use irc::proto::Command;
use ircgm::prelude::*;
use ircgm::testing::test_harness;
use ircgm::{BreakoutOptions, CaseMap, DisconnectReason};

// ============================================================================
// Group 4 (DESIGN.md §6.4): room.ask_user, /nick tracking, TimeoutCtx,
// & .settled() multi-player isolation
// ============================================================================

#[tokio::test(start_paused = true)]
async fn group4_room_ask_user_nick_tracking_timeout_ctx_and_part_kick_settled() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#r", &["Alice", "Bob", "Cara"]);
    server.settle().await;

    let alice = bot.user("Alice").unwrap();
    let cara = bot.user("Cara").unwrap();

    let room = bot.room("#r").unwrap().with_ask_config(
        &AskConfig::new()
            .reply_timeout(Duration::from_secs(30))
            .timeout_message_fn(|ctx| {
                assert_eq!(ctx.configured_timeout, Some(Duration::from_secs(30)));
                assert_eq!(ctx.target, &Target::Channel("#r".into()));
                let u = ctx.user.expect("room.ask_user sets TimeoutCtx::user");
                format!("{} took too long (30s)!", u.nick())
            }),
    );

    // 1. `room.ask_user(&alice, ...)` ignores Bob, tracks Alice -> Alice2, and calls out "Alice2" on timeout:
    let r_turn = room.clone();
    let a_turn = alice.clone();
    let turn_task = tokio::spawn(async move {
        r_turn
            .ask_user(&a_turn, "Alice: hit or stand?", |m| {
                (m.text.trim() == "hit").then_some(())
            })
            .settled()
            .await
    });
    tokio::task::yield_now().await;
    assert_eq!(server.take_messages_to("#r"), vec!["Alice: hit or stand?"]);

    // Bob replies "hit" -> ignored! Alice renames to Alice2!
    server.chat("Bob", "#r", "hit");
    server.send_line(":Alice!~Alice@test.host NICK :Alice2\r\n");
    server.settle().await;
    assert!(!turn_task.is_finished());

    // Advance 30s -> expires and calls out `"Alice2"` by her live nick!
    tokio::time::advance(Duration::from_secs(30)).await;
    let outcome = turn_task.await.unwrap().unwrap();
    assert_eq!(outcome, Turn::TimedOut);
    assert_eq!(
        server.take_messages_to("#r"),
        vec!["Alice2 took too long (30s)!"]
    );

    // 2. Mid-turn `PART` of `Alice2` resolves `.settled()` immediately to `Ok(Turn::Left { nick: "Alice2", .. })`:
    let r_part = room.clone();
    let a_part = alice.clone();
    let part_task = tokio::spawn(async move {
        r_part
            .ask_user(&a_part, "Again?", |m| Some(m.text.clone()))
            .settled()
            .await
    });
    tokio::task::yield_now().await;
    let _ = server.take_messages_to("#r");

    server.send_line(":Alice2!~Alice@test.host PART #r :gotta go\r\n");
    server.settle().await;
    let part_out = part_task.await.unwrap().unwrap();
    assert_eq!(
        part_out,
        Turn::Left {
            nick: "Alice2".into(),
            reason: Some("gotta go".into()),
        }
    );

    // 3. Mid-turn `KICK` of `Cara` resolves `.settled()` immediately to `Ok(Turn::Left { nick: "Cara", .. })`:
    let r_kick = room.clone();
    let c_kick = cara.clone();
    let kick_task = tokio::spawn(async move {
        r_kick
            .ask_user(&c_kick, "Cara?", |m| Some(m.text.clone()))
            .settled()
            .await
    });
    tokio::task::yield_now().await;
    let _ = server.take_messages_to("#r");

    server.send_line(":Op!~op@test.host KICK #r Cara :removed\r\n");
    server.settle().await;
    let kick_out = kick_task.await.unwrap().unwrap();
    assert_eq!(
        kick_out,
        Turn::Left {
            nick: "Cara".into(),
            reason: Some("removed".into()),
        }
    );
}

#[tokio::test]
async fn group4_settled_unreachable_pm_block_and_fatal_disconnect_propagation() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#game", &["P1", "P2", "P3"]);
    server.settle().await;

    let p1 = bot.user("P1").unwrap();
    let p2 = bot.user("P2").unwrap();
    let p3 = bot.user("P3").unwrap();

    // Multi-player `try_join_all` where P1 answers, P2 bounces with 486 (+R), and P3 bounces with 716 (+g callerid):
    let players = vec![p1.clone(), p2.clone(), p3.clone()];
    let all_turns = tokio::spawn(async move {
        try_join_all(players.into_iter().map(|p| async move {
            let t = p.ask("Secret prompt!", |m| Some(m.text.clone())).settled().await?;
            Ok::<_, Error>((p.nick(), t))
        }))
        .await
    });

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    let _ = server.take_messages_to("P1");
    let _ = server.take_messages_to("P2");
    let _ = server.take_messages_to("P3");

    server.dm("P1", "my quip");
    server.send_line(":irc.srv 486 GM P2 :You must log in with services to message this user\r\n");
    server.send_line(":irc.srv 716 GM P3 :is in +g mode (server-side ignore)\r\n");
    server.settle().await;

    let results = all_turns.await.unwrap().unwrap();
    assert_eq!(results[0], ("P1".into(), Turn::Reply("my quip".into())));
    assert!(matches!(
        results[1].1,
        Turn::Unreachable { ref target, code: 486, .. } if target == "P2"
    ));
    assert!(matches!(
        results[2].1,
        Turn::Unreachable { ref target, code: 716, .. } if target == "P3"
    ));

    // Verify `Error::DeliveryFailed` has `is_fatal() == false` whereas `Error::Disconnected` has `is_fatal() == true`
    // and propagates through `.settled()` as `Err`:
    let p1_disc = p1.clone();
    let disc_task = tokio::spawn(async move {
        p1_disc
            .ask("Still connected?", |m| Some(m.text.clone()))
            .settled()
            .await
    });
    tokio::task::yield_now().await;
    server.disconnect();

    let err = disc_task.await.unwrap().unwrap_err();
    assert!(err.is_fatal());
    assert!(matches!(err, Error::Disconnected(DisconnectReason::Eof)));
}

// ============================================================================
// Group 5 (DESIGN.md §6.5): WorldState, 005 (MODES, TARGMAX KICK, dynamic
// CASEMAPPING re-keying), batched moderation, NotChannelOp, 482 warn,
// netsplit, user.account(), & RFC 1459 identity
// ============================================================================

#[tokio::test]
async fn group5_worldstate_isupport_batched_moderation_targmax_kick_and_not_op() {
    let (logs, _guard) = common::capture_warns();
    let (bot, mut server) = test_harness("GM");

    server.send_line(
        ":irc.srv 005 GM PREFIX=(qaohv)~&@%+ MODES=4 TARGMAX=PRIVMSG:4,KICK:1 CHANMODES=eIbq,k,flj,CFLMPQScgimnprstuz CASEMAPPING=rfc1459 :are supported\r\n",
    );
    // Multi-packet 353 + 366 + 332 + parameterized MODE:
    server.send_line(":GM!~GM@test.host JOIN #c\r\n");
    server.send_line(":irc.srv 332 GM #c :Initial Mafia Topic\r\n");
    server.send_line(":irc.srv 353 GM = #c :@GM +U1 U2 U3 U4 U5\r\n");
    server.send_line(":irc.srv 353 GM = #c :U6 U7 U8 U9 U10\r\n");
    server.send_line(":irc.srv 366 GM #c :End of /NAMES list.\r\n");
    server.send_line(":GM!~GM@test.host MODE #c +klo secret 50 U2\r\n");
    server.settle().await;

    let room = bot.room("#c").unwrap();
    assert!(room.bot_is_op());
    assert_eq!(room.topic().as_deref(), Some("Initial Mafia Topic"));
    assert_eq!(room.members().len(), 11);

    let u1 = bot.user("U1").unwrap();
    let u2 = bot.user("U2").unwrap();
    assert!(room.has_user(&u1));
    assert!(room.member(&u1).unwrap().is_voiced);
    assert!(!room.member(&u1).unwrap().is_op);
    assert!(room.member(&u2).unwrap().is_op);

    // Preserve `User` identity across `NICK`:
    server.send_line(":U2!~U2@test.host NICK :U2_renamed\r\n");
    server.settle().await;
    assert!(room.has_user(&u2));
    assert_eq!(room.member(&u2).unwrap().nick, "U2_renamed");
    assert!(room.member(&u2).unwrap().is_op);

    // Rename back to U2 so nicks U1..=U10 are clean:
    server.send_line(":U2_renamed!~U2@test.host NICK :U2\r\n");
    // Topic update via `TOPIC` and `331`:
    server.send_line(":GM!~GM@test.host TOPIC #c :Night 1\r\n");
    server.settle().await;
    assert_eq!(room.topic().as_deref(), Some("Night 1"));

    // Batched `voice_many` & `devoice_many` over 10 users with `MODES=4` -> 3 commands (`4 + 4 + 2`):
    let ten_users: Vec<User> = (1..=10)
        .map(|i| bot.user(&format!("U{i}")).unwrap())
        .collect();
    room.voice_many(&ten_users).unwrap();
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::Raw(
            "MODE".into(),
            vec![
                "#c".into(),
                "+vvvv".into(),
                "U1".into(),
                "U2".into(),
                "U3".into(),
                "U4".into()
            ]
        )
    );
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::Raw(
            "MODE".into(),
            vec![
                "#c".into(),
                "+vvvv".into(),
                "U5".into(),
                "U6".into(),
                "U7".into(),
                "U8".into()
            ]
        )
    );
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::Raw(
            "MODE".into(),
            vec!["#c".into(), "+vv".into(), "U9".into(), "U10".into()]
        )
    );

    room.devoice_many(&ten_users).unwrap();
    for expected_flags in ["-vvvv", "-vvvv", "-vv"] {
        let cmd = server.recv_sent().await.unwrap();
        assert!(matches!(cmd, Command::Raw(ref m, ref a) if m == "MODE" && a[1] == expected_flags));
    }

    // `Breakout::drop` with `TARGMAX=KICK:1` -> individual KICKs + PART:
    let b1_task = tokio::spawn({
        let bot = bot.clone();
        async move {
            bot.breakout_with(BreakoutOptions::exact("#b1").modes(None::<&str>))
                .await
        }
    });
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::JOIN("#b1".into(), None, None)
    );
    server.complete_join("#b1", &["A", "B", "C"]);
    let b1 = b1_task.await.unwrap().unwrap();
    drop(b1);
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::KICK("#b1".into(), "A".into(), Some("Session ended".into()))
    );
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::KICK("#b1".into(), "B".into(), Some("Session ended".into()))
    );
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::KICK("#b1".into(), "C".into(), Some("Session ended".into()))
    );
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::PART("#b1".into(), None)
    );

    // Advertise `TARGMAX=KICK:3` -> `Breakout::drop` batches `KICK #b2 A,B,C`!
    server.send_line(":irc.srv 005 GM TARGMAX=KICK:3 :are supported\r\n");
    server.settle().await;
    let b2_task = tokio::spawn({
        let bot = bot.clone();
        async move {
            bot.breakout_with(BreakoutOptions::exact("#b2").modes(None::<&str>))
                .await
        }
    });
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::JOIN("#b2".into(), None, None)
    );
    server.complete_join("#b2", &["A", "B", "C", "D"]);
    let b2 = b2_task.await.unwrap().unwrap();
    drop(b2);
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::KICK("#b2".into(), "A,B,C".into(), Some("Session ended".into()))
    );
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::KICK("#b2".into(), "D".into(), Some("Session ended".into()))
    );
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::PART("#b2".into(), None)
    );

    // De-op the bot in `#c` -> moderation methods fail synchronously with `Err(Error::NotChannelOp)`:
    server.send_line(":ChanServ!s@services MODE #c -o GM\r\n");
    server.settle().await;
    assert!(!room.bot_is_op());
    for res in [
        room.kick(&u1, None),
        room.set_mode("+m", &[]),
        room.voice(&u1),
        room.devoice(&u1),
        room.set_topic("nope"),
    ] {
        assert!(matches!(res, Err(Error::NotChannelOp { ref channel }) if channel == "#c"));
    }

    // Asynchronous `482 ERR_CHANOPRIVSNEEDED` logs `tracing::warn!` without aborting an active `room.ask`:
    let mut raw_in = room.inbox();
    let ask_room = room.clone();
    let chat_ask = tokio::spawn(async move {
        ask_room
            .ask("Still listening?", |m| (m.text == "yep").then_some(()))
            .await
    });
    tokio::task::yield_now().await;
    let _ = server.take_messages_to("#c");

    server.send_line(":irc.srv 482 GM #c :You're not a channel operator\r\n");
    server.chat("U1", "#c", "yep");
    server.settle().await;

    chat_ask.await.unwrap().unwrap();
    assert!(logs.text().contains("482 ERR_CHANOPRIVSNEEDED"));
    let saw_482 = raw_in
        .wait_raw_map(|raw| matches!(&raw.command, Command::Response(r, _) if *r as u16 == 482).then_some(()))
        .await;
    assert!(saw_482.is_ok());
}

#[tokio::test]
async fn group5_dynamic_casemapping_rekey_rfc1459_netsplit_and_accounts() {
    let (bot, server) = test_harness("GM");
    server.complete_join("#[Lobby]", &["[Alice]"]);
    server.settle().await;

    assert_eq!(bot.casemap(), CaseMap::Rfc1459);

    // 1. Under default RFC 1459, `[Alice]` and `{alice}` resolve to the SAME `User` and `#[Lobby]` == `#{lobby}`:
    let u1 = bot.user("[Alice]").unwrap();
    let u2 = bot.user("{alice}").unwrap();
    assert_eq!(u1, u2);
    assert!(u1.eq_nick("{alice}"));

    let room_brace = bot.room("#{lobby}").unwrap();
    let mut room_in = room_brace.inbox();

    // 2. Dynamic `005 CASEMAPPING=ascii` mid-session re-keys `WorldState`, `UserRegistry`, and `InboxRegistry`!
    server.send_line(":irc.srv 005 GM CASEMAPPING=ascii :are supported\r\n");
    server.settle().await;
    assert_eq!(bot.casemap(), CaseMap::Ascii);
    assert!(u1.eq_nick("[alice]"));
    assert!(!u1.eq_nick("{alice}"));

    // Pre-existing `[Alice]` is still lookup-able as `[alice]` while `{alice}` is now a distinct user!
    assert_eq!(bot.user("[alice]").unwrap(), u1);
    let u_curly = bot.user("{alice}").unwrap();
    assert_ne!(u1, u_curly);

    // And pre-existing `room_in` (opened as `#{lobby}` before re-key? Wait: `room_brace` had name `"#{lobby}"`!
    // Let's also check an inbox opened on `#[Lobby]` re-keyed to ASCII `#[lobby]`!):
    let room_bracket = bot.room("#[lobby]").unwrap();
    assert!(room_bracket.has_user(&u1));
    let mut bracket_in = room_bracket.inbox();
    server.chat("[ALICE]", "#[LOBBY]", "ascii_works");
    server.settle().await;
    let m = bracket_in.next_chat().await.unwrap();
    assert_eq!(m.sender, u1);
    assert_eq!(m.text, "ascii_works");
    // Meanwhile `#{lobby}` is no longer matched by `#[LOBBY]` under `Ascii`:
    let no_match = room_in
        .next_chat()
        .with_timeout(Duration::from_millis(10))
        .optional()
        .await
        .unwrap();
    assert!(no_match.is_none());

    // 3. Account tracking (`extended-join`, `account-tag`, `ACCOUNT` + `*` logout) & `QUIT` netsplit vs normal + rejoin fresh track:
    let mut all_events = bot.inbox_all();
    server.send_line(":Bob!~b@h JOIN #[lobby] bob_acct :Bob Real\r\n");
    server.settle().await;
    let bob = bot.user("Bob").unwrap();
    assert_eq!(bob.account().as_deref(), Some("bob_acct"));

    server.send_line(":Bob!~b@h ACCOUNT *\r\n");
    server.settle().await;
    assert_eq!(bob.account(), None);

    server.send_line("@account=bob_v2 :Bob!~b@h PRIVMSG #[lobby] :reauthed\r\n");
    server.settle().await;
    assert_eq!(bob.account().as_deref(), Some("bob_v2"));

    // Netsplit QUIT (`"*.net *.split"`) vs normal QUIT:
    all_events.clear();
    server.quit_user("Bob", Some("hub.irc.org leaf.irc.org"));
    server.quit_user("[Alice]", Some("Client Quit"));
    server.settle().await;

    let ev1 = all_events.next_event().await.unwrap();
    let ev2 = all_events.next_event().await.unwrap();
    assert!(matches!(ev1, Event::Quit { netsplit: true, ref nick, .. } if nick == "Bob"));
    assert!(matches!(ev2, Event::Quit { netsplit: false, ref nick, .. } if nick == "[Alice]"));

    // Rejoining `Bob` allocates a FRESH `UserTrack` (`bob_rejoined != bob`):
    server.send_line(":Bob!~b@h JOIN #[lobby] * :Bob\r\n");
    server.settle().await;
    let bob_rejoined = bot.user("Bob").unwrap();
    assert_ne!(bob, bob_rejoined);
    assert!(!bob.is_online());
    assert!(bob_rejoined.is_online());
}
