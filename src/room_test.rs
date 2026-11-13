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
    server.send_line(":srv 005 gm MODES=2 :are supported\r\n");

    let join_task = tokio::spawn({
        let bot = bot.clone();
        async move { bot.join("#arena").await }
    });
    assert!(matches!(server.recv_sent().await, Some(Command::JOIN(ch, _, _)) if ch == "#arena"));
    server.complete_join("#arena", &["+alice", "bob", "carol"]);
    server.send_line(":srv 332 gm #arena :Championship Round\r\n");
    server.settle().await;

    let room = join_task.await.unwrap().unwrap();
    assert!(room.bot_is_op());
    assert_eq!(room.topic().as_deref(), Some("Championship Round"));
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
    server.send_line(":oper!~o@h MODE #arena -o gm\r\n");
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
