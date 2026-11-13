use irc::proto::Command;

use super::*;
use crate::testing::test_harness;

#[tokio::test]
async fn resolve_invite_target_accepts_user_str_and_string_and_rejects_invalid() {
    let (bot, server) = test_harness("gm");
    let (other_bot, _other_srv) = test_harness("gm2");

    let alice = bot.user("alice").unwrap();
    let foreign = other_bot.user("alice").unwrap();

    let (nick, tr) = resolve_invite_target(&alice, alice.upgrade_core().as_ref().unwrap()).unwrap();
    assert_eq!(nick, "alice");
    assert!(tr.is_some());

    let core = bot.room("#lobby").unwrap().core().clone();
    let (s_nick, s_tr) = resolve_invite_target("bob", &core).unwrap();
    assert_eq!(s_nick, "bob");
    assert!(s_tr.is_none());

    let owned = String::from("carol");
    let (o_nick, o_tr) = resolve_invite_target(&owned, &core).unwrap();
    assert_eq!(o_nick, "carol");
    assert!(o_tr.is_none());

    // Foreign session User rejected:
    assert!(matches!(
        resolve_invite_target(&foreign, &core),
        Err(Error::InvalidInput(_))
    ));

    // Offline User rejected with UserLeft:
    server.quit_user("alice", Some("gone"));
    server.settle().await;
    assert!(matches!(
        resolve_invite_target(&alice, &core),
        Err(Error::UserLeft { .. })
    ));
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
