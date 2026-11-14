use irc::proto::Command;

use super::*;
use crate::testing::{test_harness, test_harness_with};

#[tokio::test]
async fn bot_config_custom_command_prefixes_control_is_command_text() {
    let cfg = BotConfig::default().with_command_prefixes(["!", "."]);
    let (bot, _server) = test_harness_with("gm", cfg);
    let core = &bot.owner.core;

    assert!(core.is_command_text("!join"));
    assert!(core.is_command_text("   .status now"));
    assert!(!core.is_command_text("hello !world"));
    assert!(!core.is_command_text("   "));
}

#[tokio::test]
async fn chat_message_reply_mirrors_kind_and_checks_bot_left_room() {
    let (bot, mut server) = test_harness("gm");
    server.complete_join("#lobby", &["alice"]);
    server.settle().await;
    let mut all = bot.inbox().accept_notices();

    // 1. Channel PRIVMSG -> channel PRIVMSG:
    server.chat("alice", "#lobby", "hello room");
    let ch_msg = all.next_chat().await.unwrap();
    ch_msg.reply("hi channel").unwrap();
    assert_eq!(
        server.recv_sent().await,
        Some(Command::PRIVMSG("#lobby".into(), "hi channel".into()))
    );

    // 2. DM ACTION -> DM PRIVMSG:
    server.send_line(":alice!~a@h PRIVMSG gm :\x01ACTION waves\x01\r\n");
    let act_msg = all.next_chat().await.unwrap();
    assert!(act_msg.is_action());
    act_msg.reply("waves back").unwrap();
    assert_eq!(
        server.recv_sent().await,
        Some(Command::PRIVMSG("alice".into(), "waves back".into()))
    );

    // 3. Human NOTICE -> NOTICE:
    server.send_line(":alice!~a@h NOTICE gm :psst\r\n");
    let not_msg = all.next_chat().await.unwrap();
    assert!(not_msg.is_notice());
    not_msg.reply("quiet reply").unwrap();
    assert_eq!(
        server.recv_sent().await,
        Some(Command::NOTICE("alice".into(), "quiet reply".into()))
    );

    // 4. After bot is kicked from `#lobby`, `ch_msg.reply(...)` fails locally with `BotLeftRoom`:
    server.send_line(":oper!~o@h KICK #lobby gm :out\r\n");
    server.settle().await;
    assert!(matches!(
        ch_msg.reply("too late"),
        Err(Error::BotLeftRoom { ref channel, .. }) if channel == "#lobby"
    ));
    assert!(server.try_recv_sent().is_none());
}

#[tokio::test]
async fn shutdown_requested_resolves_before_and_after_request_shutdown() {
    let (bot, _server) = test_harness("gm");
    assert!(!bot.is_shutting_down());

    // Dropping unused `shutdown_requested()` futures (e.g. across loop iterations) cleans up receivers:
    for _ in 0..32 {
        drop(bot.shutdown_requested());
    }
    assert_eq!(bot.owner.core.shutdown_tx.receiver_count(), 0);

    let waiter = bot.shutdown_requested();
    assert_eq!(bot.owner.core.shutdown_tx.receiver_count(), 1);
    bot.request_shutdown();
    assert!(bot.is_shutting_down());
    waiter.await;

    // Subsequent calls resolve immediately:
    bot.shutdown_requested().await;

    // Coalescing `request_shutdown()` followed by `Eof` disconnect preserves `is_shutting_down()`
    // and wakes an unpolled `shutdown_requested()` future instead of parking forever:
    let (bot2, srv2) = test_harness("gm2");
    let unpolled_waiter = bot2.shutdown_requested();
    bot2.request_shutdown();
    srv2.disconnect();
    assert_eq!(
        bot2.owner.core.wait_disconnected().await,
        DisconnectReason::Eof
    );
    assert!(bot2.is_shutting_down());
    tokio::time::timeout(Duration::from_secs(1), unpolled_waiter)
        .await
        .expect("unpolled shutdown_requested() hung across Eof disconnect");
}

#[tokio::test]
async fn chat_message_reply_tracks_sender_rename_and_refuses_impostor_after_quit() {
    let (bot, mut server) = test_harness("gm");
    server.complete_join("#lobby", &["alice", "mallory"]);
    server.settle().await;

    let mut dms = bot.inbox_dms();
    server.dm("alice", "secret question");
    server.settle().await;
    let dm_msg = dms.next_chat().await.unwrap();

    // Alice renames -> `dm_msg.reply` targets her new live nickname:
    server.rename_user("alice", "alicia");
    server.settle().await;
    dm_msg.reply("secret answer").unwrap();
    assert_eq!(
        server.recv_sent().await,
        Some(Command::PRIVMSG("alicia".into(), "secret answer".into()))
    );

    // Alicia quits and Mallory steals `"alicia"` -> `dm_msg.reply` refuses with `UserLeft`:
    server.quit_user("alicia", Some("gone"));
    server.rename_user("mallory", "alicia");
    server.settle().await;
    assert!(matches!(
        dm_msg.reply("do not leak"),
        Err(Error::UserLeft { ref nick, .. }) if nick == "alicia"
    ));
    assert!(server.try_recv_sent().is_none());
}

#[tokio::test]
async fn user_ids_are_globally_unique_and_cross_bot_inbox_scope_is_rejected() {
    let (bot1, _srv1) = test_harness("gm1");
    let (bot2, srv2) = test_harness("gm2");

    let alice1 = bot1.user("Alice").unwrap();
    let mallory2 = bot2.user("Mallory").unwrap();
    assert_ne!(alice1.id(), mallory2.id());
    assert_ne!(alice1, mallory2);

    let mut foreign_inbox = bot2.inbox_dms_from([&alice1]);
    srv2.dm("Mallory", "private to gm2");
    srv2.settle().await;
    assert!(matches!(
        foreign_inbox.next_chat().await,
        Err(Error::InvalidInput(_))
    ));

    let room1 = bot1.room("#lobby").unwrap();
    let mut foreign_room_inbox = bot2.inbox_rooms([&room1]);
    assert!(matches!(
        foreign_room_inbox.next_chat().await,
        Err(Error::InvalidInput(_))
    ));
}

#[tokio::test]
async fn socket_eof_aborts_throttled_outbound_write_receipt_immediately() {
    let cfg = BotConfig {
        outbound_burst: 1,
        outbound_window: Duration::from_secs(3600),
        ..BotConfig::default()
    };
    let (bot, mut server) = test_harness_with("gm", cfg);
    server.complete_join("#lobby", &["alice"]);
    server.settle().await;
    let room = bot.room("#lobby").unwrap();

    // Consume the single burst slot so the next outbound send is throttled for 3600s:
    room.say("first line consumes burst").unwrap();
    assert_eq!(
        server.recv_sent().await,
        Some(Command::PRIVMSG(
            "#lobby".into(),
            "first line consumes burst".into()
        ))
    );

    // Spawn an `Ask` whose prompt is parked in `OutboundSink` awaiting `WriteReceipt::wait`:
    let ask_task = tokio::spawn(async move {
        room.ask("throttled prompt", |m| Some(m.text.clone())).await
    });
    tokio::task::yield_now().await;
    assert!(server.try_recv_sent().is_none());

    // Socket EOF must abort `OutboundSink` atomically so `WriteReceipt::wait` unblocks immediately:
    server.disconnect();
    let res = tokio::time::timeout(Duration::from_secs(1), ask_task)
        .await
        .expect("throttled Ask stalled on WriteReceipt::wait after EOF")
        .unwrap();
    assert!(matches!(
        res,
        Err(Error::Disconnected(DisconnectReason::Eof))
    ));
}

#[tokio::test]
async fn rejoin_after_kick_awaits_366_and_coalesces_concurrent_joins_onto_one_join() {
    let (bot, mut server) = test_harness("gm");
    server.complete_join("#lobby", &["alice"]);
    server.settle().await;
    server.send_line(":oper!~o@h KICK #lobby gm :out\r\n");
    server.settle().await;

    // Two concurrent rejoins of a `Left` channel (differently cased) send exactly one `JOIN`:
    let j1 = tokio::spawn({
        let bot = bot.clone();
        async move { bot.join("#lobby").await }
    });
    let j2 = tokio::spawn({
        let bot = bot.clone();
        async move { bot.join("#LOBBY").await }
    });
    let first = server.recv_sent().await.unwrap();
    assert!(matches!(&first, Command::JOIN(ch, None, None) if ch.eq_ignore_ascii_case("#lobby")));
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(server.try_recv_sent().is_none());

    // Both waiters resolve together on `366` with the canonical channel name:
    server.complete_join("#lobby", &["alice"]);
    let r1 = tokio::time::timeout(Duration::from_secs(1), j1)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let r2 = tokio::time::timeout(Duration::from_secs(1), j2)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(r1.name(), "#lobby");
    assert_eq!(r2.name(), "#lobby");

    // A refused join resolves every waiter with `JoinFailed`:
    let j3 = tokio::spawn({
        let bot = bot.clone();
        async move { bot.join("#vip").await }
    });
    assert_eq!(
        server.recv_sent().await,
        Some(Command::JOIN("#vip".into(), None, None))
    );
    server.send_line(":srv 473 gm #vip :Cannot join channel (+i)\r\n");
    assert!(matches!(
        j3.await.unwrap(),
        Err(Error::JoinFailed { ref channel, code: 473, .. }) if channel == "#vip"
    ));
}

#[tokio::test]
async fn join_during_parting_breakout_rejoins_instead_of_returning_dying_room() {
    let (bot, mut server) = test_harness("gm");
    let setup = tokio::spawn({
        let bot = bot.clone();
        async move {
            bot.breakout_with(crate::BreakoutOptions::exact("#vault").no_modes())
                .await
        }
    });
    assert_eq!(
        server.recv_sent().await,
        Some(Command::JOIN("#vault".into(), None, None))
    );
    server.complete_join("#vault", &[]);
    let vault = setup.await.unwrap().unwrap();

    // Dropping the `Breakout` queues our `PART`; an immediate `join` must not short-circuit on
    // the still-`Joined` roster, but queue its `JOIN` behind the `PART` and await the rejoin:
    drop(vault);
    let rejoin = tokio::spawn({
        let bot = bot.clone();
        async move { bot.join("#vault").await }
    });
    let part = server.recv_sent().await.unwrap();
    assert!(matches!(&part, Command::PART(ch, _) if ch == "#vault"), "{part:?}");
    assert_eq!(
        server.recv_sent().await,
        Some(Command::JOIN("#vault".into(), None, None))
    );
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(!rejoin.is_finished());

    server.send_line(":gm!~gm@test.host PART #vault\r\n");
    server.complete_join("#vault", &["alice"]);
    let room = tokio::time::timeout(Duration::from_secs(1), rejoin)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(room.members().len(), 2);
}

#[tokio::test]
async fn kick_between_self_join_and_366_resolves_join_with_bot_left_room() {
    let (bot, mut server) = test_harness("gm");
    let join = tokio::spawn({
        let bot = bot.clone();
        async move { bot.join("#c").await }
    });
    assert_eq!(
        server.recv_sent().await,
        Some(Command::JOIN("#c".into(), None, None))
    );
    server.send_line(":gm!~gm@test.host JOIN #c\r\n");
    server.send_line(":oper!~o@h KICK #c gm :not welcome\r\n");
    let res = tokio::time::timeout(Duration::from_secs(1), join)
        .await
        .expect("mid-join KICK must resolve Bot::join promptly")
        .unwrap();
    assert!(matches!(
        res,
        Err(Error::BotLeftRoom { ref channel, ref reason })
            if channel == "#c" && reason.as_deref() == Some("not welcome")
    ));
}

#[tokio::test]
async fn exact_breakout_on_already_joined_room_is_rejected_without_tearing_it_down() {
    let (bot, mut server) = test_harness("gm");
    server.complete_join("#lobby", &["alice"]);
    server.settle().await;

    // `BreakoutOptions::exact("#lobby")` must not adopt (and later KICK/PART) the active lobby:
    let res = bot
        .breakout_with(crate::BreakoutOptions::exact("#LOBBY").no_modes())
        .await;
    assert!(matches!(res, Err(Error::InvalidInput(_))), "{res:?}");
    tokio::task::yield_now().await;
    assert!(server.try_recv_sent().is_none());

    let lobby = bot.room("#lobby").unwrap();
    assert_eq!(lobby.members().len(), 2);
    lobby.say("still here").unwrap();
    assert_eq!(
        server.recv_sent().await,
        Some(Command::PRIVMSG("#lobby".into(), "still here".into()))
    );

    // A room with an in-flight `Bot::join` is claimed too:
    let pending = tokio::spawn({
        let bot = bot.clone();
        async move { bot.join("#game").await }
    });
    assert_eq!(
        server.recv_sent().await,
        Some(Command::JOIN("#game".into(), None, None))
    );
    let res = bot
        .breakout_with(crate::BreakoutOptions::exact("#game").no_modes())
        .await;
    assert!(matches!(res, Err(Error::InvalidInput(_))), "{res:?}");
    server.complete_join("#game", &[]);
    assert_eq!(pending.await.unwrap().unwrap().name(), "#game");
    assert!(server.try_recv_sent().is_none());
}

#[tokio::test]
async fn completed_breakout_part_evicts_unwatched_channel_but_room_part_and_kick_keep_left() {
    let (bot, mut server) = test_harness("gm");
    let core = Arc::clone(&bot.owner.core);
    let left_at = |chan: &str| {
        core.state
            .read()
            .unwrap()
            .channel(chan)
            .map(|c| c.left_at_seq().is_some())
    };

    // 1. Unwatched `Breakout`: our own `PART` echo evicts the entry instead of leaking a `Left`:
    let setup = tokio::spawn({
        let bot = bot.clone();
        async move {
            bot.breakout_with(crate::BreakoutOptions::exact("#tmp").no_modes())
                .await
        }
    });
    assert!(matches!(server.recv_sent().await, Some(Command::JOIN(ch, _, _)) if ch == "#tmp"));
    server.complete_join("#tmp", &[]);
    drop(setup.await.unwrap().unwrap());
    assert!(matches!(server.recv_sent().await, Some(Command::PART(ch, _)) if ch == "#tmp"));
    server.send_line(":gm!~gm@test.host PART #tmp\r\n");
    server.settle().await;
    assert_eq!(left_at("#tmp"), None);

    // 2. A watched `Breakout` (channel-scoped `Inbox` still open) keeps `Left` for that inbox:
    let setup = tokio::spawn({
        let bot = bot.clone();
        async move {
            bot.breakout_with(crate::BreakoutOptions::exact("#watched").no_modes())
                .await
        }
    });
    assert!(matches!(server.recv_sent().await, Some(Command::JOIN(ch, _, _)) if ch == "#watched"));
    server.complete_join("#watched", &[]);
    let watched = setup.await.unwrap().unwrap();
    let mut watched_inbox = watched.inbox();
    drop(watched);
    assert!(matches!(server.recv_sent().await, Some(Command::PART(ch, _)) if ch == "#watched"));
    server.send_line(":gm!~gm@test.host PART #watched\r\n");
    server.settle().await;
    assert_eq!(left_at("#watched"), Some(true));
    assert!(matches!(
        watched_inbox.next_chat().await,
        Err(Error::BotLeftRoom { .. })
    ));

    // 3. An explicit, unwatched `Room::part` keeps `Left`, so the held `Room` and freshly
    //    opened inboxes still fail locally with `BotLeftRoom`:
    server.complete_join("#parted", &[]);
    server.settle().await;
    let room = bot.room("#parted").unwrap();
    room.part(None).unwrap();
    assert!(matches!(server.recv_sent().await, Some(Command::PART(ch, _)) if ch == "#parted"));
    server.send_line(":gm!~gm@test.host PART #parted\r\n");
    server.settle().await;
    assert_eq!(left_at("#parted"), Some(true));
    assert!(matches!(room.say("hi"), Err(Error::BotLeftRoom { .. })));
    assert!(matches!(
        room.inbox().next_chat().await,
        Err(Error::BotLeftRoom { .. })
    ));
    assert!(server.try_recv_sent().is_none());

    // 4. An involuntary `KICK` keeps `Left` even when unwatched, so `Room` handles fail locally:
    server.complete_join("#kicked", &[]);
    server.settle().await;
    server.send_line(":oper!~o@h KICK #kicked gm :bye\r\n");
    server.settle().await;
    assert_eq!(left_at("#kicked"), Some(true));
    assert!(matches!(
        bot.room("#kicked").unwrap().say("hi"),
        Err(Error::BotLeftRoom { .. })
    ));
}

#[tokio::test]
async fn shared_join_refuses_to_coalesce_onto_breakout_pending_join() {
    let (bot, mut server) = test_harness("gm");
    let setup = tokio::spawn({
        let bot = bot.clone();
        async move {
            bot.breakout_with(crate::BreakoutOptions::exact("#game").no_modes())
                .await
        }
    });
    assert_eq!(
        server.recv_sent().await,
        Some(Command::JOIN("#game".into(), None, None))
    );

    // A plain `join` must not piggyback on the breakout's in-flight claim (whose room the
    // breakout's drop will KICK/PART):
    assert!(matches!(
        bot.join("#GAME").await,
        Err(Error::InvalidInput(_))
    ));
    tokio::task::yield_now().await;
    assert!(server.try_recv_sent().is_none());

    server.complete_join("#game", &[]);
    assert_eq!(setup.await.unwrap().unwrap().name(), "#game");
}
