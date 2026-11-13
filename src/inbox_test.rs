use super::*;
use crate::testing::test_harness;

#[tokio::test]
async fn inbox_scope_deduplicates_case_folded_rooms_and_same_user() {
    let (bot, _server) = test_harness("gm");
    let r1 = bot.room("#Lobby").unwrap();
    let r2 = bot.room("#lobby").unwrap();
    let r3 = bot.room("#Arena").unwrap();
    let alice1 = bot.user("Alice").unwrap();
    let alice2 = bot.user("alice").unwrap();
    let bob = bot.user("bob").unwrap();

    let scope = InboxScope::new()
        .rooms([&r1, &r2, &r3])
        .dms_from([&alice1, &alice2, &bob]);

    assert_eq!(scope.rooms.len(), 2);
    assert_eq!(scope.dm_users.len(), 2);
}

#[tokio::test]
async fn filter_scope_labels_format_cleanly() {
    let (bot, _server) = test_harness("gm");
    let alice = bot.user("alice").unwrap();
    let bob = bot.user("bob").unwrap();

    assert_eq!(Filter::All.scope_label(), "bot");
    assert_eq!(Filter::AllDms.scope_label(), "dm");
    assert_eq!(
        Filter::Scoped {
            rooms: vec![],
            dm_users: vec![alice.track_arc()],
        }
        .scope_label(),
        "user alice"
    );
    assert_eq!(
        Filter::UsersInRoom {
            tracks: vec![alice.track_arc()],
            channel_name: "#quiz".into(),
        }
        .scope_label(),
        "user alice in #quiz"
    );
    assert_eq!(
        Filter::UsersInRoom {
            tracks: vec![alice.track_arc(), bob.track_arc()],
            channel_name: "#quiz".into(),
        }
        .scope_label(),
        "users [alice, bob] in #quiz"
    );
    assert_eq!(
        Filter::Scoped {
            rooms: vec!["#a".into()],
            dm_users: vec![],
        }
        .scope_label(),
        "room #a"
    );
    assert_eq!(
        Filter::Scoped {
            rooms: vec!["#a".into(), "#b".into()],
            dm_users: vec![alice.track_arc(), bob.track_arc()],
        }
        .scope_label(),
        "scope(rooms: [#a, #b], users: [alice, bob])"
    );
}

#[tokio::test]
async fn registry_tracks_channel_interest_and_deduplicates_multi_match_events() {
    let (bot, server) = test_harness("gm");
    let room_a = bot.room("#a").unwrap();
    let room_b = bot.room("#b").unwrap();
    let alice = bot.user("alice").unwrap();
    let cm = bot.casemap();

    // Initially no inbox watches `#a`:
    assert!(!room_a
        .core()
        .inboxes
        .has_interest_in_channel(&cm.fold("#a")));

    // Subscribe a combined scope watching `#a`, `#b`, and DMs from `alice`:
    let mut combined = bot.inbox_scope(InboxScope::new().rooms([&room_a, &room_b]).dm_from(&alice));
    assert!(room_a
        .core()
        .inboxes
        .has_interest_in_channel(&cm.fold("#a")));

    // Put alice in both `#a` and `#b`, then have alice `/nick` so `candidates` hits `#a`, `#b`, AND `by_user`:
    server.send_line(":alice!~a@h JOIN #a\r\n");
    server.send_line(":alice!~a@h JOIN #b\r\n");
    server.send_line(":alice!~a@h NICK alicia\r\n");
    server.chat("alicia", "#a", "after nick");
    server.settle().await;

    // Drain events: Joined(#a), Joined(#b), ONE NickChanged (not 3!), Chat("after nick"):
    assert!(matches!(
        combined.next_event().await.unwrap(),
        Event::Joined { .. }
    ));
    assert!(matches!(
        combined.next_event().await.unwrap(),
        Event::Joined { .. }
    ));
    match combined.next_event().await.unwrap() {
        Event::NickChanged {
            old_nick, new_nick, ..
        } => {
            assert_eq!(old_nick, "alice");
            assert_eq!(new_nick, "alicia");
        }
        other => panic!("expected single NickChanged, got {other:?}"),
    }
    match combined.next_event().await.unwrap() {
        Event::Chat(m) => assert_eq!(m.text, "after nick"),
        other => panic!("expected Chat immediately after single NickChanged, got {other:?}"),
    }

    drop(combined);
    assert!(!room_a
        .core()
        .inboxes
        .has_interest_in_channel(&cm.fold("#a")));
}

#[tokio::test]
async fn consuming_into_combinators_resolve_and_unsubscribe_on_completion() {
    let (bot, server) = test_harness("gm");
    server.complete_join("#lobby", &["alice", "bob", "carol"]);
    server.settle().await;
    let room = bot.room("#lobby").unwrap();

    let wait_cmd_fut = room.inbox().into_wait_cmd("!go");
    server.chat("alice", "#lobby", "not yet");
    server.chat("bob", "#lobby", "!go now");
    let cmd_msg = wait_cmd_fut.await.unwrap();
    assert_eq!(cmd_msg.nick, "bob");
    assert_eq!(cmd_msg.cmd_args("!go"), Some("now"));

    let wait_map_fut = room
        .inbox()
        .into_wait_map(|m| m.cmd_arg("!bet")?.parse::<u32>().ok());
    server.chat("alice", "#lobby", "!bet nope");
    server.chat("alice", "#lobby", "!bet 250");
    assert_eq!(wait_map_fut.await.unwrap(), 250);

    // `wait_cmd` and `wait_cmd_arg` match even when `.ignore_commands()` is active on the inbox:
    let mut room_ignored = room.inbox().ignore_commands();
    server.chat("carol", "#lobby", "!DEAL  ace  ");
    let (deal_msg, deal_arg) = room_ignored.wait_cmd_arg("!deal").await.unwrap();
    assert_eq!(deal_msg.nick, "carol");
    assert_eq!(deal_arg, "ace");

    let mut ev_inbox = room.inbox();
    server.chat("carol", "#lobby", "noise");
    server.join_user("dave", "#lobby");
    let joined = ev_inbox
        .wait_event_map(|ev| match ev {
            Event::Joined { user, .. } => Some(user.nick()),
            _ => None,
        })
        .await
        .unwrap();
    assert_eq!(joined, "dave");

    // Lossy `on_cmd` trigger survives > `inbox_capacity` flood and parks `Pending` on departure:
    let cfg = crate::BotConfig {
        inbox_capacity: 2,
        ..crate::BotConfig::default()
    };
    let (bot2, srv2) = crate::testing::test_harness_with("gm2", cfg);
    srv2.complete_join("#flood", &["alice", "bob"]);
    srv2.settle().await;
    let flood_room = bot2.room("#flood").unwrap();
    let alice2 = bot2.user("alice").unwrap();

    let stop_task = tokio::spawn(flood_room.inbox().on_cmd("!stop"));
    for i in 0..10 {
        srv2.chat("bob", "#flood", &format!("spam {i}"));
    }
    srv2.settle().await;
    tokio::task::yield_now().await;
    srv2.chat("alice", "#flood", "!STOP now");
    srv2.settle().await;
    assert_eq!(stop_task.await.unwrap().nick, "alice");

    // `alice2.inbox().on_cmd("!cancel")` parks Pending instead of firing when `alice2` quits:
    let cancel_trigger = alice2.inbox().on_cmd("!cancel");
    srv2.quit_user("alice", Some("bye"));
    srv2.settle().await;
    let out = crate::GameFutureExt::until(async { "finished" }, cancel_trigger).await;
    assert!(matches!(out, crate::Outcome::Completed("finished")));

    // Packet-burst ordering on `Filter::Scoped` (`[PRIVMSG, KICK GM]`) delivers chat before `BotLeftRoom`,
    // and `[KICK GM, PRIVMSG]` rejects the post-kick chat:
    let mut scoped_burst = room.inbox();
    server.chat("alice", "#lobby", "before kick");
    server.send_line(":oper!~o@h KICK #lobby gm :bye\r\n");
    server.chat("alice", "#lobby", "after kick");
    server.settle().await;
    assert_eq!(scoped_burst.next_chat().await.unwrap().text, "before kick");
    assert!(matches!(
        scoped_burst.next_chat().await,
        Err(Error::BotLeftRoom { .. })
    ));
}
