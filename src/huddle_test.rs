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
    assert!(sender.any_reachable_at(&bounced, u64::MAX));

    bounced.insert(w1.id());
    assert!(sender.any_reachable_at(&bounced, u64::MAX));

    // If w2 quits while w1 bounced, nobody reachable remains from the `QUIT` on, but w2 still
    // counts as reachable at earlier wire positions:
    server.quit_user("wolf2", None);
    server.settle().await;
    let UserStatus::Quit { at_seq, .. } = w2.track().status() else {
        panic!("wolf2 must have quit");
    };
    assert!(!sender.any_reachable_at(&bounced, at_seq));
    assert!(sender.any_reachable_at(&bounced, at_seq - 1));
}

#[tokio::test]
async fn empty_vs_all_offline_huddle_and_member_ask_user_without_deadlock() {
    let (bot, mut server) = test_harness("gm");

    // 1. Truly 0-argument huddle fails with `InvalidInput` (no fabricated `nick: ""`!):
    let mut empty = bot.huddle_available(std::iter::empty::<&User>()).await.unwrap();
    assert!(empty.is_empty());
    let res = empty.ask("Anyone here?", |m| Some(m.text.clone())).await;
    assert!(matches!(res, Err(Error::InvalidInput(_))));

    // 2. Huddle whose candidates were offline preserves real quitter nick in `Answer::Left`:
    let gone = bot.user("ghostwolf").unwrap();
    server.quit_user("ghostwolf", Some("away"));
    server.settle().await;
    let mut all_offline = bot.huddle_available([&gone]).await.unwrap();
    assert!(all_offline.is_empty());
    assert!(matches!(
        all_offline.ask("Anyone here?", |m| Some(m.text.clone())).await,
        Ok(crate::Answer::Left { ref nick, ref reason }) if nick == "ghostwolf" && reason.as_deref() == Some("away")
    ));

    // 3. `has_user` and `ask_user` reuse the already-held member lock without self-deadlock:
    let w1 = bot.user("wolf1").unwrap();
    let w2 = bot.user("wolf2").unwrap();
    let outsider = bot.user("villager").unwrap();
    let mut pack = bot.huddle([&w1, &w2]).await.unwrap();
    assert!(pack.has_user(&w1));
    assert!(!pack.has_user(&outsider));

    let ask_member = pack.ask_user(&w2, "Confirm bite?", |m| Some(m.text.clone()));
    let drive = async {
        assert_eq!(server.recv_message_to("wolf2").await, "Confirm bite?");
        server.dm("wolf2", "confirmed");
        server.settle().await;
    };
    let (ans, ()) = tokio::join!(ask_member, drive);
    assert_eq!(ans.unwrap().unwrap(), "confirmed");

    assert!(matches!(
        pack.ask_user(&outsider, "Hello?", |m| Some(m.text.clone())).await,
        Err(Error::InvalidInput(_))
    ));
}

#[tokio::test]
async fn huddle_ask_user_reads_the_shared_inbox_once_and_never_relays_the_answer() {
    let (bot, mut server) = test_harness("gm");
    let w1 = bot.user("wolf1").unwrap();
    let w2 = bot.user("wolf2").unwrap();
    let w3 = bot.user("wolf3").unwrap();
    let mut pack = bot
        .huddle([&w1, &w2, &w3])
        .await
        .unwrap()
        .with_relay("[Pack] ");

    let ask = pack.ask_user(&w2, "Confirm bite?", |m| {
        (m.text == "yes").then(|| m.text.clone())
    });
    let drive = async {
        assert_eq!(server.recv_message_to("wolf2").await, "Confirm bite?");
        server.dm("wolf1", "bite alice");
        server.dm("wolf2", "maybe");
        server.dm("wolf2", "yes");
        server.settle().await;
    };
    let (ans, ()) = tokio::join!(ask, drive);
    assert_eq!(ans.unwrap().unwrap(), "yes");

    // The prompt went to wolf2 alone; teammate banter was relayed, wolf2's replies were not:
    assert!(server.take_messages_to("wolf1").is_empty());
    assert_eq!(
        server.take_messages_to("wolf2"),
        vec!["[Pack] <wolf1> bite alice"]
    );
    assert_eq!(
        server.take_messages_to("wolf3"),
        vec!["[Pack] <wolf1> bite alice"]
    );

    // No stale copy of the exchange is left behind in the huddle inbox:
    server.dm("wolf3", "after");
    server.settle().await;
    assert_eq!(pack.inbox().next_chat().await.unwrap().text, "after");

    // A target quitting mid-prompt ends it right away, although teammates remain:
    let ask = pack.ask_user(&w3, "Still there?", |m| Some(m.text.clone()));
    let drive = async {
        assert_eq!(server.recv_message_to("wolf3").await, "Still there?");
        server.quit_user("wolf3", Some("bye"));
        server.settle().await;
    };
    let (ans, ()) = tokio::join!(
        tokio::time::timeout(std::time::Duration::from_secs(1), ask),
        drive
    );
    assert!(matches!(
        ans.expect("ask_user must not wait out its timeout").unwrap(),
        crate::Answer::Left { ref nick, ref reason } if nick == "wolf3" && reason.as_deref() == Some("bye")
    ));
}

#[tokio::test]
async fn failed_ask_user_on_departed_member_keeps_the_shared_inbox_backlog() {
    let (bot, server) = test_harness("gm");
    let w1 = bot.user("wolf1").unwrap();
    let w2 = bot.user("wolf2").unwrap();
    let mut pack = bot.huddle([&w1, &w2]).await.unwrap();

    server.dm("wolf1", "still here");
    server.quit_user("wolf2", Some("bye"));
    server.settle().await;

    // The prompt fails up front, before touching wolf1's unread message:
    assert!(matches!(
        pack.ask_user(&w2, "Confirm?", |m| Some(m.text.clone())).await,
        Ok(crate::Answer::Left { ref nick, .. }) if nick == "wolf2"
    ));
    assert_eq!(pack.inbox().next_chat().await.unwrap().text, "still here");
}
