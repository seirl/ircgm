use irc::proto::Command;

use super::*;
use crate::testing::test_harness;

#[tokio::test]
async fn exclusive_dm_records_and_clears_lock_holder_and_syncs_inbox_flags() {
    let (bot, mut server) = test_harness("gm");
    let alice = bot.user("alice").unwrap();

    assert!(alice.track().lock_holder.lock().unwrap().is_none());

    let mut dm = alice.exclusive().await.unwrap();
    dm.inbox().set_ignore_commands(true);
    dm.inbox().set_accept_notices(true);
    assert!(alice.track().lock_holder.lock().unwrap().is_some());
    assert_eq!(dm.user(), &alice);
    assert!(format!("{dm:?}").contains("ExclusiveDm"));

    dm.say("hi").unwrap();
    dm.notice("hud").unwrap();
    dm.action("waves").unwrap();
    assert_eq!(
        server.recv_sent().await,
        Some(Command::PRIVMSG("alice".into(), "hi".into()))
    );
    assert_eq!(
        server.recv_sent().await,
        Some(Command::NOTICE("alice".into(), "hud".into()))
    );
    assert_eq!(
        server.recv_sent().await,
        Some(Command::PRIVMSG(
            "alice".into(),
            "\x01ACTION waves\x01".into()
        ))
    );

    server.dm("alice", "!cmd_skipped");
    server.dm_notice("alice", "notice_accepted");
    server.settle().await;
    assert_eq!(dm.inbox().next_chat().await.unwrap().text, "notice_accepted");

    drop(dm);
    assert!(alice.track().lock_holder.lock().unwrap().is_none());
}

#[tokio::test]
async fn acquire_many_and_available_reject_foreign_and_duplicate_users() {
    let (bot1, _srv1) = test_harness("gm1");
    let (bot2, _srv2) = test_harness("gm2");

    let alice1 = bot1.user("alice").unwrap();
    let foreign = bot2.user("alice").unwrap();

    assert!(matches!(
        bot1.exclusive_many([&alice1, &foreign]).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        bot1.exclusive_available([&alice1, &foreign]).await,
        Err(Error::InvalidInput(_))
    ));

    // All four multi-user entry points share one acquisition path and one error:
    let dup = |res: Result<()>| {
        matches!(res, Err(Error::InvalidInput(msg)) if msg == "duplicate User in multi-user DM lock")
    };
    assert!(dup(bot1.exclusive_many([&alice1, &alice1]).await.map(drop)));
    assert!(dup(bot1.exclusive_available([&alice1, &alice1]).await.map(drop)));
    assert!(dup(bot1.huddle("Pack", [&alice1, &alice1]).await.map(drop)));
    assert!(dup(
        bot1.huddle_available("Pack", [&alice1, &alice1])
            .await
            .map(drop)
    ));
}

#[tokio::test]
async fn user_exclusive_available_returns_none_on_offline_or_mid_wait_quit_and_propagates_disconnect()
{
    let (bot, server) = test_harness("gm");
    let alice = bot.user("alice").unwrap();
    let bob = bot.user("bob").unwrap();

    // 1. Online user acquires `Some(dm)`:
    let held = alice.exclusive_available().await.unwrap().unwrap();
    assert_eq!(held.user(), &alice);

    // 2. Queued waiter resolves to `Ok(None)` immediately if `alice` quits while `held` is locked:
    let alice_wait = alice.clone();
    let waiter = tokio::spawn(async move { alice_wait.exclusive_available().await });
    server.flush().await;
    server.quit_user("alice", Some("bye"));
    server.settle().await;
    assert!(waiter.await.unwrap().unwrap().is_none());
    drop(held);

    // 3. Already-offline user returns `Ok(None)`:
    assert!(alice.exclusive_available().await.unwrap().is_none());

    // 4. Fatal session disconnect still propagates as `Err(Error::Disconnected(..))` (both mid-wait
    //    and once disconnected):
    let held_bob = bob.exclusive().await.unwrap();
    let bob_wait = bob.clone();
    let disc_waiter = tokio::spawn(async move { bob_wait.exclusive_available().await });
    server.flush().await;
    server.disconnect();
    assert!(matches!(
        disc_waiter.await.unwrap(),
        Err(Error::Disconnected(_))
    ));
    drop(held_bob);
    assert!(matches!(
        bob.exclusive_available().await,
        Err(Error::Disconnected(_))
    ));
}

#[tokio::test]
async fn exclusive_dm_group_broadcasts_wire_ordered_inbox_without_relay_and_splits_into_vec() {
    let (bot, mut server) = test_harness("gm");
    let alice = bot.user("alice").unwrap();
    let bob = bot.user("bob").unwrap();
    let carol = bot.user("carol").unwrap();
    let stranger = bot.user("stranger").unwrap();

    // Truly empty input returns `InvalidInput` on broadcast, whereas all-offline `exclusive_available`
    // returns `UserLeft` on broadcast:
    let empty_group = bot.exclusive_many::<_, &User>([]).await.unwrap();
    assert!(empty_group.is_empty());
    assert_eq!(empty_group.len(), 0);
    assert!(matches!(
        empty_group.say("hello"),
        Err(Error::InvalidInput("ExclusiveDmGroup has no members"))
    ));
    drop(empty_group);

    let mut group = bot.exclusive_many([&alice, &bob, &carol]).await.unwrap();
    assert_eq!(group.len(), 3);
    assert!(!group.is_empty());
    assert_eq!(group.users(), vec![alice.clone(), bob.clone(), carol.clone()]);
    assert!(group.has_user(&alice));
    assert!(group.has_user(&bob));
    assert!(!group.has_user(&stranger));
    assert!(format!("{group:?}").contains("ExclusiveDmGroup"));

    group.say("hello all").unwrap();
    group.notice("heads up").unwrap();
    group.action("waves").unwrap();
    for msg in ["hello all", "heads up", "\x01ACTION waves\x01"] {
        for nick in ["alice", "bob", "carol"] {
            let expected = if msg == "heads up" {
                Command::NOTICE(nick.into(), msg.into())
            } else {
                Command::PRIVMSG(nick.into(), msg.into())
            };
            assert_eq!(server.recv_sent().await, Some(expected));
        }
    }

    // Unlike `Huddle`, incoming member DMs arrive in wire order on `group.inbox()` with zero relay:
    server.dm("bob", "first");
    server.dm("alice", "second");
    server.settle().await;
    assert_eq!(group.inbox().next_chat().await.unwrap().text, "first");
    assert_eq!(group.inbox().next_chat().await.unwrap().text, "second");
    assert!(server.take_messages_to("alice").is_empty());
    assert!(server.take_messages_to("bob").is_empty());
    assert!(server.take_messages_to("carol").is_empty());

    // Mid-group quit is skipped cleanly by `say`, while all-departed returns `UserLeft`:
    server.quit_user("carol", Some("gone"));
    server.settle().await;
    group.say("still here").unwrap();
    server.flush().await;
    assert_eq!(server.take_messages_to("alice"), vec!["still here"]);
    assert_eq!(server.take_messages_to("bob"), vec!["still here"]);

    // `into_vec` transfers lock ownership without releasing any per-user DM mutex:
    let parts = group.into_vec();
    assert_eq!(parts.len(), 3);
    assert!(alice.track().dm_mutex.try_lock().is_err());
    assert!(bob.track().dm_mutex.try_lock().is_err());
    drop(parts);
    assert!(alice.track().dm_mutex.try_lock().is_ok());
    assert!(bob.track().dm_mutex.try_lock().is_ok());

    // All-offline `exclusive_available` surfaces `UserLeft` if caller tries to broadcast:
    let offline_only = bot.exclusive_available([&carol]).await.unwrap();
    assert!(offline_only.is_empty());
    assert!(matches!(
        offline_only.say("anyone?"),
        Err(Error::UserLeft { ref nick, .. }) if nick == "carol"
    ));
}
