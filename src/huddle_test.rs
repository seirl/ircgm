use std::collections::HashSet;

use super::*;
use crate::testing::test_harness;

#[tokio::test]
async fn huddle_relay_suppresses_commands_by_default_and_allows_opt_in() {
    let (bot, mut server) = test_harness("gm");
    let w1 = bot.user("wolf1").unwrap();
    let w2 = bot.user("wolf2").unwrap();

    let mut pack = bot.huddle([&w1, &w2]).await.unwrap().with_relay("[Pack] ");
    assert_eq!(pack.len(), 2);
    assert!(!pack.is_empty());

    // Normal banter relays to the other member:
    server.dm("wolf1", "let's get alice");
    let chat = pack.inbox().next_chat().await.unwrap();
    pack.relay_to_others(&chat).unwrap();
    server.settle().await;
    assert_eq!(
        server.take_messages_to("wolf2"),
        vec!["[Pack] <wolf1> let's get alice"]
    );
    assert!(server.take_messages_to("wolf1").is_empty());

    // Command-prefixed message is suppressed by default (`relay_commands == false`):
    server.dm("wolf1", "!help");
    let cmd = pack.inbox().next_chat().await.unwrap();
    pack.relay_to_others(&cmd).unwrap();
    server.settle().await;
    assert!(server.take_messages_to("wolf2").is_empty());

    // Opting into `set_relay_commands(true)` forwards command lines too:
    pack.set_relay_commands(true);
    pack.relay_to_others(&cmd).unwrap();
    server.settle().await;
    assert_eq!(
        server.take_messages_to("wolf2"),
        vec!["[Pack] <wolf1> !help"]
    );
}

#[tokio::test]
async fn huddle_sender_tracks_bounced_and_offline_members() {
    let (bot, server) = test_harness("gm");
    let w1 = bot.user("wolf1").unwrap();
    let w2 = bot.user("wolf2").unwrap();

    let pack = bot.huddle([&w1, &w2]).await.unwrap();
    let sender = pack.sender_view();

    let outsider = bot.user("outsider").unwrap();
    assert_eq!(
        sender.find_matching_user(Some(w1.id().as_u64())),
        Some(&w1)
    );
    assert_eq!(
        sender.find_matching_user(Some(w2.id().as_u64())),
        Some(&w2)
    );
    assert_eq!(
        sender.find_matching_user(Some(outsider.id().as_u64())),
        None
    );
    assert_eq!(sender.find_matching_user(None), None);

    let mut bounced = HashSet::new();
    assert!(sender.any_reachable_online(&bounced));

    bounced.insert(w1.id());
    assert!(sender.any_reachable_online(&bounced));

    // If w2 quits while w1 bounced, nobody reachable remains:
    server.quit_user("wolf2", None);
    server.settle().await;
    assert!(!sender.any_reachable_online(&bounced));
}

#[tokio::test]
async fn empty_vs_all_offline_huddle_and_member_dm_mut_ask_user_without_deadlock() {
    let (bot, mut server) = test_harness("gm");

    // 1. Truly 0-argument huddle fails with `InvalidInput` (no fabricated `nick: ""`!):
    let mut empty = bot.huddle_available(std::iter::empty::<&User>()).await.unwrap();
    assert!(empty.is_empty());
    let res = empty.ask("Anyone here?", |m| Some(m.text.clone())).await;
    assert!(matches!(res, Err(Error::InvalidInput(_))));

    // 2. Huddle whose candidates were offline preserves real quitter nick in `UserLeft`:
    let gone = bot.user("ghostwolf").unwrap();
    server.quit_user("ghostwolf", Some("away"));
    server.settle().await;
    let mut all_offline = bot.huddle_available([&gone]).await.unwrap();
    assert!(all_offline.is_empty());
    assert!(matches!(
        all_offline.ask("Anyone here?", |m| Some(m.text.clone())).await,
        Err(Error::UserLeft { ref nick, ref reason }) if nick == "ghostwolf" && reason.as_deref() == Some("away")
    ));

    // 3. `has_user`, `dm_mut`, and `ask_user` borrow the already-locked guard without self-deadlock:
    let w1 = bot.user("wolf1").unwrap();
    let w2 = bot.user("wolf2").unwrap();
    let outsider = bot.user("villager").unwrap();
    let mut pack = bot.huddle([&w1, &w2]).await.unwrap();
    assert!(pack.has_user(&w1));
    assert!(!pack.has_user(&outsider));
    assert!(pack.dm_mut(&outsider).is_none());
    pack.dm_mut(&w1).unwrap().say("Alpha instructions").unwrap();
    assert_eq!(server.recv_message_to("wolf1").await, "Alpha instructions");

    let ask_member = pack.ask_user(&w2, "Confirm bite?", |m| Some(m.text.clone()));
    let drive = async {
        assert_eq!(server.recv_message_to("wolf2").await, "Confirm bite?");
        server.dm("wolf2", "confirmed");
        server.settle().await;
    };
    let (ans, ()) = tokio::join!(ask_member, drive);
    assert_eq!(ans.unwrap(), "confirmed");

    assert!(matches!(
        pack.ask_user(&outsider, "Hello?", |m| Some(m.text.clone())).await,
        Err(Error::InvalidInput(_))
    ));
}
