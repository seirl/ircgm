use irc::proto::Command;

use super::*;
use crate::testing::test_harness;

#[tokio::test]
async fn room_invite_and_inbox_user_enforce_session_and_presence() {
    let (bot, mut server) = test_harness("gm");
    let (other_bot, _other_srv) = test_harness("gm2");

    server.complete_join("#lobby", &["alice", "bob"]);
    server.settle().await;
    let room = bot.room("#lobby").unwrap().with_ask_config(
        &crate::AskConfig::new().ignore_commands().accept_notices(),
    );

    let alice = bot.user("alice").unwrap();
    let bob = bot.user("bob").unwrap();
    let foreign = other_bot.user("alice").unwrap();

    room.invite(&alice).unwrap();
    assert_eq!(
        server.recv_sent().await,
        Some(Command::INVITE("alice".into(), "#lobby".into()))
    );

    // Foreign session User rejected:
    assert!(matches!(room.invite(&foreign), Err(Error::InvalidInput(_))));

    // `Room::inbox_user` inherits `Room`'s `ask_config` (`ignore_commands` + `accept_notices`)
    // and ignores other users in the same room:
    let mut alice_in = room.inbox_user(&alice);
    server.chat("bob", "#lobby", "bob noise");
    server.chat("alice", "#lobby", "!ignored_cmd");
    server.notice("alice", "#lobby", "alice notice");
    server.settle().await;
    assert_eq!(alice_in.next_chat().await.unwrap().text, "alice notice");

    // Offline User rejected with UserLeft:
    server.quit_user("alice", Some("gone"));
    server.settle().await;
    assert!(matches!(room.invite(&alice), Err(Error::UserLeft { .. })));
    let _ = bob;
}

#[tokio::test]
async fn room_queries_and_moderation_batching_respect_world_state() {
    let (bot, mut server) = test_harness("gm");
    server.isupport("MODES=2");

    let join_task = tokio::spawn({
        let bot = bot.clone();
        async move { bot.join("#arena").await }
    });
    assert!(matches!(server.recv_sent().await, Some(Command::JOIN(ch, _, _)) if ch == "#arena"));
    server.complete_join("#arena", &["+alice", "bob", "carol"]);
    server.initial_topic("#arena", "Championship Round");
    server.settle().await;

    let room = join_task.await.unwrap().unwrap();
    assert!(room.bot_is_op());
    assert_eq!(room.topic().as_deref(), Some("Championship Round"));
    let nicks: Vec<String> = room.members().into_iter().map(|m| m.nick).collect();
    assert_eq!(nicks, vec!["alice", "bob", "carol", "gm"]);
    assert_eq!(room.nicks(), vec!["alice", "bob", "carol", "gm"]);
    assert!(room.has_nick("ALICE"));

    let alice = bot.user("alice").unwrap();
    let bob = bot.user("bob").unwrap();
    let carol = bot.user("carol").unwrap();
    assert!(room.has_user(&alice));
    assert!(room.member(&alice).unwrap().is_voiced);
    assert!(!room.member(&bob).unwrap().is_voiced);

    // MODES=2 batches 3 users into `+vv alice bob` and `+v carol`:
    room.voice_many([&alice, &bob, &carol]).unwrap();
    assert_eq!(
        server.recv_sent().await,
        Some(Command::Raw(
            "MODE".into(),
            vec!["#arena".into(), "+vv".into(), "alice".into(), "bob".into()]
        ))
    );
    assert_eq!(
        server.recv_sent().await,
        Some(Command::Raw(
            "MODE".into(),
            vec!["#arena".into(), "+v".into(), "carol".into()]
        ))
    );

    // De-op the bot -> moderation fails locally with NotChannelOp:
    server.mode("oper", "#arena", "-o gm");
    server.settle().await;
    assert!(!room.bot_is_op());
    assert!(matches!(
        room.voice(&alice),
        Err(Error::NotChannelOp { .. })
    ));
}

#[tokio::test]
async fn room_inbox_users_and_ask_users_ignore_spectators_and_end_when_all_players_leave() {
    let (bot, mut server) = test_harness("gm");
    let join_task = tokio::spawn({
        let bot = bot.clone();
        async move { bot.join("#table").await }
    });
    let _ = server
        .accept_breakout("#table", &["alice", "bob", "spectator"])
        .await;
    let room = join_task.await.unwrap().unwrap();

    let alice = bot.user("alice").unwrap();
    let bob = bot.user("bob").unwrap();

    // `ask_users` ignores spectator chatter and survives one listed player parting:
    let ask_task = tokio::spawn({
        let room = room.clone();
        let players = vec![alice.clone(), bob.clone()];
        async move {
            room.ask_users(&players, "Type !deal", |m| {
                m.is_cmd("!deal").then(|| m.sender.nick())
            })
            .await
        }
    });
    server.flush().await;
    assert_eq!(server.take_messages_to("#table"), vec!["Type !deal"]);

    server.chat("spectator", "#table", "!deal");
    server.part_user("alice", "#table", Some("out"));
    server.chat("bob", "#table", "!deal");

    let dealer = ask_task.await.unwrap().unwrap().unwrap();
    assert_eq!(dealer, "bob");

    // Once `bob` also parts, `inbox_users` fails with `Error::UserLeft` and `ask_users` resolves
    // to `Answer::Left`:
    let mut sub_inbox = room.inbox_users([&alice, &bob]);
    server.part_user("bob", "#table", Some("cashout"));
    match sub_inbox.next_chat().await {
        Err(Error::UserLeft { nick, reason }) => {
            assert_eq!(nick, "bob");
            assert_eq!(reason.as_deref(), Some("cashout"));
        }
        other => panic!("expected UserLeft(bob, cashout), got {other:?}"),
    }

    assert!(matches!(
        room.ask_users([&alice, &bob], "Anyone left?", |m| Some(m.text.clone()))
            .await,
        Ok(crate::Answer::Left { .. })
    ));
}

#[tokio::test]
async fn room_moderation_tracks_renames_and_never_hits_nick_stealer_after_quit() {
    let (bot, mut server) = test_harness("gm");
    server.complete_join("#arena", &["alice", "bob", "mallory"]);
    server.settle().await;

    let room = bot.room("#arena").unwrap();
    let alice = bot.user("alice").unwrap();
    let bob = bot.user("bob").unwrap();

    // Rename before moderation uses new nick:
    server.rename_user("alice", "alicia");
    server.settle().await;
    room.voice(&alice).unwrap();
    assert_eq!(
        server.recv_sent().await,
        Some(Command::Raw(
            "MODE".into(),
            vec!["#arena".into(), "+v".into(), "alicia".into()]
        ))
    );

    // Quit + nick steal: `kick(&alice)` fails with `UserLeft` and `voice_many` skips `alice`:
    server.quit_user("alicia", Some("bye"));
    server.rename_user("mallory", "alicia");
    server.settle().await;

    assert!(matches!(
        room.kick(&alice, Some("out")),
        Err(Error::UserLeft { ref nick, .. }) if nick == "alicia"
    ));
    assert!(server.try_recv_sent().is_none());

    room.voice_many([&alice, &bob]).unwrap();
    assert_eq!(
        server.recv_sent().await,
        Some(Command::Raw(
            "MODE".into(),
            vec!["#arena".into(), "+v".into(), "bob".into()]
        ))
    );
}

#[tokio::test]
async fn moderation_on_unjoined_room_is_rejected_locally() {
    let (bot, mut server) = test_harness("gm");
    let alice = bot.user("alice").unwrap();
    let nowhere = bot.room("#nowhere").unwrap();

    assert!(matches!(
        nowhere.kick(&alice, None),
        Err(Error::BotLeftRoom { ref channel, reason: None }) if channel == "#nowhere"
    ));
    assert!(matches!(
        nowhere.set_mode("+m", &[]),
        Err(Error::BotLeftRoom { .. })
    ));
    tokio::task::yield_now().await;
    assert!(server.try_recv_sent().is_none());

    // Chat on an untracked room is still allowed (the server decides):
    nowhere.say("hello?").unwrap();
    assert_eq!(
        server.recv_sent().await,
        Some(Command::PRIVMSG("#nowhere".into(), "hello?".into()))
    );
}

#[tokio::test]
async fn bounce_401_wakes_parked_inbox_user_with_user_left() {
    let (bot, server) = test_harness("gm");
    server.complete_join("#lobby", &["alice", "bob"]);
    server.settle().await;
    let room = bot.room("#lobby").unwrap();
    let alice = bot.user("alice").unwrap();

    // Park a `Room::inbox_user` read before alice silently vanishes (no QUIT ever arrives):
    let mut watch_alice = room.inbox_user(&alice);
    let parked = tokio::spawn(async move { watch_alice.next_chat().await });
    tokio::task::yield_now().await;
    assert!(!parked.is_finished());

    // A `401` for alice (e.g. bouncing an unrelated DM) must wake the parked read immediately:
    server.bounce(401, "alice", "No such nick/channel");
    let res = tokio::time::timeout(std::time::Duration::from_secs(1), parked)
        .await
        .expect("401 bounce must wake a parked UsersInRoom inbox")
        .unwrap();
    assert!(
        matches!(res, Err(Error::UserLeft { ref nick, .. }) if nick == "alice"),
        "{res:?}"
    );
}

#[tokio::test]
async fn moderation_guard_restores_flags_pre_voiced_users_and_only_touched_topic() {
    let (bot, mut server) = test_harness("gm");
    server.complete_join("#town", &["+alice", "bob", "carol"]);
    server.initial_topic("#town", "Lobby Open");
    server.settle().await;

    let room = bot.room("#town").unwrap();
    let alice = bot.user("alice").unwrap();
    let bob = bot.user("bob").unwrap();
    let carol = bot.user("carol").unwrap();

    // 1. Guard that modifies +m, +N, voices bob+carol, and devoices pre-voiced alice (WITHOUT
    // changing topic) restores -mN, -v bob, +v alice (after carol was already devoiced mid-game),
    // and leaves topic untouched:
    {
        let mut guard = room.moderation_guard().unwrap();
        // Parameterized modes (`k`) are rejected by `set_flag`:
        assert!(matches!(
            guard.set_flag('k', true),
            Err(Error::InvalidInput(_))
        ));
        guard.set_moderated(true).unwrap();
        guard.set_flag('N', true).unwrap();
        guard.voice_many([&bob, &carol]).unwrap();
        guard.devoice(&alice).unwrap();
        guard.devoice(&carol).unwrap();
        while server.try_recv_sent().is_some() {}
    }

    let mut teardown = Vec::new();
    while let Some(cmd) = server.try_recv_sent() {
        teardown.push(cmd);
    }
    assert_eq!(
        teardown,
        vec![
            Command::Raw("MODE".into(), vec!["#town".into(), "-mN".into()]),
            Command::Raw(
                "MODE".into(),
                vec!["#town".into(), "-v".into(), "bob".into()]
            ),
            Command::Raw(
                "MODE".into(),
                vec!["#town".into(), "+v".into(), "alice".into()]
            ),
        ]
    );

    // 2. Guard that updates topic restores the initial topic on explicit `restore()`, while
    // `disarm()` suppresses teardown:
    let mut g2 = room.moderation_guard().unwrap();
    g2.set_topic("Night 1").unwrap();
    let _ = server.recv_sent().await;
    g2.restore().unwrap();
    assert_eq!(
        server.recv_sent().await,
        Some(Command::TOPIC("#town".into(), Some("Lobby Open".into())))
    );

    let mut g3 = room.moderation_guard().unwrap();
    g3.set_moderated(true).unwrap();
    let _ = server.recv_sent().await;
    g3.disarm();
    assert!(server.try_recv_sent().is_none());

    // 3. Rejects unjoined rooms (`BotLeftRoom`), non-op rooms (`NotChannelOp`), modeless `+`
    // channels (`NotChannelOp`), and servers whose `005 CHANMODES` omits `m` (`InvalidInput`):
    let unjoined = bot.room("#unjoined").unwrap();
    assert!(matches!(
        unjoined.moderation_guard(),
        Err(Error::BotLeftRoom { .. })
    ));

    server.mode("ChanServ", "#town", "-o gm");
    server.settle().await;
    assert!(matches!(
        room.moderation_guard(),
        Err(Error::NotChannelOp { ref channel }) if channel == "#town"
    ));

    server.isupport("CHANTYPES=#&+ CHANMODES=b,k,l,it");
    server.complete_join("+modeless", &["alice"]);
    server.complete_join("#nomod", &["alice"]);
    server.settle().await;
    assert!(matches!(
        bot.room("+modeless").unwrap().moderation_guard(),
        Err(Error::NotChannelOp { .. })
    ));
    assert!(matches!(
        bot.room("#nomod").unwrap().moderation_guard(),
        Err(Error::InvalidInput(_))
    ));
}
