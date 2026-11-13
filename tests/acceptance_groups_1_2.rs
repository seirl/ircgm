use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, UNIX_EPOCH};

use irc::proto::Command;
use ircgm::prelude::*;
use ircgm::testing::test_harness;

// ============================================================================
// Group 1 (DESIGN.md §6.1): Burst packet batching, wire-order fair race,
// multi-source InboxScope, stamped identity across NICK/steal/QUIT, & Event fields
// ============================================================================

#[tokio::test]
async fn group1_packet_burst_and_wire_fair_inbox_users_race() {
    let (bot, server) = test_harness("GM");
    server.complete_join("#q", &["Alice", "Bob"]);
    server.settle().await;

    let room = bot.room("#q").unwrap();
    let mut inbox = room.inbox();

    // Inject back-to-back burst before polling:
    server.send_line(":Alice!u@h PRIVMSG #q :!join\r\n");
    server.send_line(":Bob!u@h PRIVMSG #q :!join\r\n");
    server.settle().await;

    let m1 = inbox.wait_cmd("!join").await.unwrap();
    let m2 = inbox.wait_cmd("!join").await.unwrap();
    assert_eq!(m1.nick, "Alice");
    assert_eq!(m2.nick, "Bob");
    assert!(m1.seq < m2.seq);

    // Fair-race regression: `[alice, bob]` in vector order, but Bob's DM arrives FIRST on wire:
    let alice = bot.user("Alice").unwrap();
    let bob = bot.user("Bob").unwrap();
    let mut race = bot.inbox_users([&alice, &bob]);

    server.send_line(":Bob!u@h PRIVMSG GM :102\r\n");
    server.send_line(":Alice!u@h PRIVMSG GM :102\r\n");
    server.settle().await;

    let winner = race
        .wait_map(|m| (m.text.trim() == "102").then(|| m.sender.clone()))
        .await
        .unwrap();
    assert_eq!(winner, bob);
    assert_eq!(winner.nick(), "Bob");
}

#[tokio::test]
async fn group1_multisource_inbox_scope_and_unified_source_exhaustion() {
    let (bot, server) = test_harness("GM");
    server.complete_join("#r1", &["Alice", "Bob"]);
    server.complete_join("#r2", &["Alice", "Bob"]);
    server.settle().await;

    let r1 = bot.room("#r1").unwrap();
    let r2 = bot.room("#r2").unwrap();
    let alice = bot.user("Alice").unwrap();
    let bob = bot.user("Bob").unwrap();

    let mut multi = bot.inbox_scope(InboxScope::new().rooms([&r1, &r2]).dms_from([&alice, &bob]));
    let mut r1_events = r1.inbox();
    let mut users_only = bot.inbox_users([&alice, &bob]);
    let mut mixed = bot.inbox_scope(InboxScope::new().room(&r1).dms_from([&alice, &bob]));

    // Interleaved batch across rooms + DMs:
    server.chat("Alice", "#r1", "one");
    server.dm("Bob", "two");
    server.chat("Alice", "#r2", "three");
    server.dm("Alice", "four");
    server.settle().await;

    let texts = [
        multi.next_chat().await.unwrap().text,
        multi.next_chat().await.unwrap().text,
        multi.next_chat().await.unwrap().text,
        multi.next_chat().await.unwrap().text,
    ];
    assert_eq!(texts, ["one", "two", "three", "four"]);
    // Drain the `#r1` message from `r1_events` and `mixed`:
    assert_eq!(r1_events.next_chat().await.unwrap().text, "one");
    assert_eq!(mixed.next_chat().await.unwrap().text, "one");
    assert_eq!(mixed.next_chat().await.unwrap().text, "two");
    assert_eq!(mixed.next_chat().await.unwrap().text, "four");
    assert_eq!(users_only.next_chat().await.unwrap().text, "two");
    assert_eq!(users_only.next_chat().await.unwrap().text, "four");

    // Alice quits:
    server.quit_user("Alice", Some("leaving early"));
    server.dm("Bob", "bob_still_here");
    server.settle().await;

    // `r1.inbox().next_event()` observes `Event::Quit` in-band:
    let ev = r1_events.next_event().await.unwrap();
    match ev {
        Event::Quit {
            user,
            nick,
            channels,
            reason,
            ..
        } => {
            assert_eq!(user, alice);
            assert_eq!(nick, "Alice");
            assert!(channels.iter().any(|c| c == "#r1"));
            assert!(channels.iter().any(|c| c == "#r2"));
            assert_eq!(reason.as_deref(), Some("leaving early"));
        }
        other => panic!("expected Event::Quit, got {other:?}"),
    }

    // `users_only` keeps running for `bob`!
    let bob_reply = users_only
        .wait_map(|m| (m.text == "bob_still_here").then(|| m.sender.clone()))
        .await
        .unwrap();
    assert_eq!(bob_reply, bob);

    // Now Bob ALSO quits:
    server.quit_user("Bob", Some("me too"));
    server.chat("Spectator", "#r1", "room_lives_on");
    server.settle().await;

    // `users_only` now has 0 live sources -> returns `Err(Error::UserLeft { .. })`!
    let err = users_only.wait_map(|_| Some(())).await.unwrap_err();
    assert!(matches!(err, Error::UserLeft { .. }), "got {err:?}");

    // Mixed `InboxScope` (`.room(&r1).dms_from([&alice, &bob])`) KEEPS RUNNING for `r1`!
    let survivor_msg = mixed
        .wait_for(|m| m.text == "room_lives_on")
        .await
        .unwrap();
    assert_eq!(survivor_msg.nick, "Spectator");
}

#[tokio::test]
async fn group1_single_batch_privmsg_rename_steal_quit_preserves_stamped_identity() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#r", &["Alice", "Mallory"]);
    server.settle().await;
    let _ = server.take_messages_to("#r");

    let room = bot.room("#r").unwrap();
    let alice = bot.user("Alice").unwrap();
    let mallory = bot.user("Mallory").unwrap();
    assert_ne!(alice, mallory);

    let mut all_in = bot.inbox_all();

    // Start `room.ask_user(&alice, ...)` so its subscription is active before the burst:
    let ask_room = room.clone();
    let ask_alice = alice.clone();
    let ask_task = tokio::spawn(async move {
        ask_room
            .ask_user(&ask_alice, "Your move?", |m| {
                (m.text.trim() == "hit").then(|| m.clone())
            })
            .await
    });
    tokio::task::yield_now().await;
    assert_eq!(server.take_messages_to("#r"), vec!["Your move?"]);

    // Single batch: Alice answers "hit", renames to Alice2, Mallory steals "Alice", Alice2 quits!
    server.send_line(":Alice!u@h PRIVMSG #r :hit\r\n");
    server.send_line(":Alice!u@h NICK :Alice2\r\n");
    server.send_line(":Mallory!m@h NICK :Alice\r\n");
    server.send_line(":Alice2!u@h QUIT :bye\r\n");
    server.settle().await;

    // `ask_user` MUST receive `"hit"` rather than failing with `UserLeft` or confusing Mallory!
    let hit_msg = ask_task.await.unwrap().unwrap();
    assert_eq!(hit_msg.text, "hit");
    assert_eq!(hit_msg.nick, "Alice"); // wire nick at send time
    assert_eq!(hit_msg.sender, alice);
    assert_eq!(hit_msg.sender.nick(), "Alice2"); // live nick after rename before quit!

    // Meanwhile `bot.user("Alice")` now resolves to `Mallory`, NOT the original `alice`:
    let current_alice = bot.user("Alice").unwrap();
    assert_eq!(current_alice, mallory);
    assert_ne!(hit_msg.sender, current_alice);

    // Also verify `all_in` saw the exact sequence:
    let first_chat = all_in.next_chat().await.unwrap();
    assert_eq!(first_chat.sender, alice);
    assert_ne!(first_chat.sender, current_alice);
}

#[tokio::test]
async fn group1_event_fields_server_time_ident_and_channels() {
    let (bot, server) = test_harness("GM");
    server.complete_join("#arena", &["@GM"]);
    server.settle().await;

    let mut inbox = bot.inbox_all();
    let before = tokio::time::Instant::now();

    server.send_line(":Cara!~cara@cloak.example JOIN #arena * :Cara\r\n");
    server.send_line("@time=2026-05-01T12:30:00.250Z :Cara!~cara@cloak.example PRIVMSG #arena :hello\r\n");
    server.send_line(":Cara!~cara@cloak.example NICK :Cara_afk\r\n");
    server.send_line(":GM!~GM@test.host KICK #arena Cara_afk :idle\r\n");
    server.send_line(":Dan!~dan@host.example JOIN #arena\r\n");
    server.send_line(":Dan!~dan@host.example PART #arena :brb\r\n");
    server.settle().await;

    // 1. Joined:
    let cara = match inbox.next_event().await.unwrap() {
        Event::Joined {
            channel,
            user,
            nick,
            ident,
            host,
        } => {
            assert_eq!(channel, "#arena");
            assert_eq!(nick, "Cara");
            assert_eq!(ident.as_deref(), Some("~cara"));
            assert_eq!(host.as_deref(), Some("cloak.example"));
            user
        }
        other => panic!("expected Joined, got {other:?}"),
    };

    // 1b. Chat with `@time` (`server_time` + `received_at` + `ident` + `host`):
    match inbox.next_event().await.unwrap() {
        Event::Chat(msg) => {
            assert_eq!(msg.sender, cara);
            assert_eq!(msg.ident.as_deref(), Some("~cara"));
            assert_eq!(msg.host.as_deref(), Some("cloak.example"));
            assert!(msg.received_at >= before);
            let ts = msg.server_time.expect("parsed @time");
            let dur = ts.duration_since(UNIX_EPOCH).unwrap();
            assert_eq!(dur.subsec_millis(), 250);
        }
        other => panic!("expected Chat, got {other:?}"),
    }

    // 2. NickChanged:
    match inbox.next_event().await.unwrap() {
        Event::NickChanged {
            user,
            old_nick,
            new_nick,
            channels,
        } => {
            assert_eq!(user, cara);
            assert_eq!(old_nick, "Cara");
            assert_eq!(new_nick, "Cara_afk");
            assert_eq!(channels, vec!["#arena".to_owned()]);
        }
        other => panic!("expected NickChanged, got {other:?}"),
    }

    // 3. Kicked:
    match inbox.next_event().await.unwrap() {
        Event::Kicked {
            channel,
            user,
            nick,
            by,
            reason,
        } => {
            assert_eq!(channel, "#arena");
            assert_eq!(user, cara);
            assert_eq!(nick, "Cara_afk");
            assert_eq!(by, "GM");
            assert_eq!(reason.as_deref(), Some("idle"));
        }
        other => panic!("expected Kicked, got {other:?}"),
    }

    // 4. Dan Joined + Parted:
    let dan = match inbox.next_event().await.unwrap() {
        Event::Joined { user, nick, .. } => {
            assert_eq!(nick, "Dan");
            user
        }
        other => panic!("expected Dan Joined, got {other:?}"),
    };
    match inbox.next_event().await.unwrap() {
        Event::Parted {
            channel,
            user,
            nick,
            reason,
        } => {
            assert_eq!(channel, "#arena");
            assert_eq!(user, dan);
            assert_eq!(nick, "Dan");
            assert_eq!(reason.as_deref(), Some("brb"));
        }
        other => panic!("expected Dan Parted, got {other:?}"),
    }
}

// ============================================================================
// Group 2 (DESIGN.md §6.2): One-shot non-backlog, inbox_dms, MessageKind,
// & NOTICE isolation / .accept_notices() + bot.reply NOTICE mirroring
// ============================================================================

#[tokio::test]
async fn group2_oneshot_non_backlog_and_inbox_dms_filtering() {
    let (bot, server) = test_harness("GM");
    server.complete_join("#lobby", &["Alice", "Bob"]);
    server.settle().await;

    let room = bot.room("#lobby").unwrap();

    // One-shot `room.inbox().wait_cmd("!heist").await` drops subscription immediately upon matching:
    let r_wait = room.clone();
    let wait_task = tokio::spawn(async move { r_wait.inbox().wait_cmd("!heist").await });
    tokio::task::yield_now().await;
    server.chat("Alice", "#lobby", "!heist");
    server.settle().await;
    let trigger = wait_task.await.unwrap().unwrap();
    assert_eq!(trigger.nick, "Alice");

    // Messages sent during the sub-task while NO lobby `Inbox` is open are never replayed later!
    server.chat("Bob", "#lobby", "!heist");
    server.settle().await;

    let mut dms = bot.inbox_dms();
    server.chat("Alice", "#lobby", "channel chatter ignored by inbox_dms");
    server.dm("Bob", "secret dm");
    server.settle().await;

    let dm_msg = dms.next_chat().await.unwrap();
    assert!(dm_msg.is_dm());
    assert_eq!(dm_msg.nick, "Bob");
    assert_eq!(dm_msg.text, "secret dm");

    // Opening a fresh `room.inbox()` now does NOT see Bob's earlier `!heist`:
    let mut fresh_in = room.inbox();
    let res = fresh_in
        .wait_cmd("!heist")
        .with_timeout(Duration::from_millis(20))
        .optional()
        .await
        .unwrap();
    assert!(res.is_none());
}

#[tokio::test]
async fn group2_notice_isolation_accept_notices_and_reply_mirroring() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#lobby", &["Alice", "OtherBot"]);
    server.settle().await;

    let room = bot.room("#lobby").unwrap();
    let mut default_in = bot.inbox_all();
    let mut notice_in = bot.inbox_all().accept_notices();
    let mut event_in = bot.inbox_all();

    // Inject 4 notices + 1 trailing normal chat sentinel:
    server.send_line(":NickServ!s@services NOTICE GM :!heist\r\n");
    server.send_line(":irc.example.org NOTICE GM :!heist\r\n");
    server.send_line("@bot :OtherBot!b@h NOTICE #lobby :!heist\r\n");
    server.send_line(":Alice!u@h NOTICE GM :!vote Bob\r\n");
    server.chat("Alice", "#lobby", "sentinel");
    server.settle().await;

    // 1. Default inbox ignores ALL FOUR notices and yields only `"sentinel"`:
    let m_default = default_in.next_chat().await.unwrap();
    assert_eq!(m_default.text, "sentinel");
    assert_eq!(m_default.kind, MessageKind::Privmsg);
    assert!(!m_default.is_notice());

    // 2. `event_in.next_event()` still surfaces `Event::Notice` for notices:
    let mut saw_nickserv_notice = false;
    for _ in 0..5 {
        if let Event::Notice { sender, text, .. } = event_in.next_event().await.unwrap() {
            if sender.as_ref().is_some_and(|u| u.eq_nick("NickServ")) && text == "!heist" {
                saw_nickserv_notice = true;
            }
        }
    }
    assert!(saw_nickserv_notice);

    // 3. `.accept_notices()` excludes NickServ, server notice, and `@bot` notice,
    //    and yields ONLY Alice's human `NOTICE` followed by `"sentinel"`!
    let m_vote = notice_in.next_chat().await.unwrap();
    assert_eq!(m_vote.nick, "Alice");
    assert_eq!(m_vote.text, "!vote Bob");
    assert_eq!(m_vote.kind, MessageKind::Notice);
    assert!(m_vote.is_notice());
    assert!(m_vote.is_dm());
    assert_eq!(m_vote.cmd_args("!vote"), Some("Bob"));

    // 4. `bot.reply(&m_vote, "Recorded")` MUST emit outbound `NOTICE` (RFC 2812 §3.3.2), NEVER `PRIVMSG`!
    bot.reply(&m_vote, "Recorded").unwrap();
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::NOTICE("Alice".into(), "Recorded".into())
    );

    // Also verify `Ask` with `.accept_notices()` accepts human `NOTICE`, whereas default `Ask` ignores it:
    let r_def = room.clone();
    let t1 = tokio::spawn(async move {
        r_def
            .ask("Default ask", |m| (m.text == "via_notice").then_some(()))
            .reply_timeout(Duration::from_millis(30))
            .optional()
            .await
    });
    let r_acc = room.clone();
    let t2 = tokio::spawn(async move {
        r_acc
            .ask("Notice ask", |m| {
                (m.text == "via_notice").then_some(m.kind)
            })
            .accept_notices()
            .await
    });
    tokio::task::yield_now().await;
    let _ = server.take_messages_to("#lobby");

    server.send_line(":Alice!u@h NOTICE #lobby :via_notice\r\n");
    server.settle().await;

    assert_eq!(t2.await.unwrap().unwrap(), MessageKind::Notice);
    assert_eq!(t1.await.unwrap().unwrap(), None);
}

#[tokio::test]
async fn group1_user_id_keys_and_consuming_inbox_combinators() {
    let (bot, server) = test_harness("GM");
    server.complete_join("#lobby", &["[Alice]", "Bob"]);
    server.settle().await;

    let room = bot.room("#lobby").unwrap();
    let alice_1 = bot.user("[Alice]").unwrap();
    let alice_folded = bot.user("{alice}").unwrap();
    let bob = bot.user("Bob").unwrap();

    // `UserId` is `Copy + Eq + Ord + Hash` with zero interior-mutability lints:
    let alice_id: UserId = alice_1.id();
    assert_eq!(alice_id, alice_folded.id());
    assert_ne!(alice_id, bob.id());
    assert!(alice_id.as_u64() > 0);
    assert_eq!(alice_id.to_string(), alice_id.as_u64().to_string());

    let mut scores: HashMap<UserId, usize> = HashMap::new();
    scores.insert(alice_id, 10);
    let mut seen: HashSet<UserId> = HashSet::new();
    assert!(seen.insert(alice_id));
    let mut ordered: BTreeSet<UserId> = BTreeSet::new();
    ordered.insert(bob.id());
    ordered.insert(alice_id);
    assert_eq!(ordered.len(), 2);

    // `/nick` preserves `UserId`; `QUIT` + rejoin allocates a fresh `UserId`:
    server.send_line(":[Alice]!u@h NICK :AliceRenamed\r\n");
    server.settle().await;
    assert_eq!(bot.user("AliceRenamed").unwrap().id(), alice_id);
    assert_eq!(scores[&alice_1.id()], 10);

    server.quit_user("AliceRenamed", Some("reconnecting"));
    server.send_line(":AliceRenamed!u@h JOIN #lobby\r\n");
    server.settle().await;
    let alice_rejoined = bot.user("AliceRenamed").unwrap();
    assert_ne!(alice_rejoined.id(), alice_id);
    assert!(seen.insert(alice_rejoined.id()));

    // Consuming one-shot `Inbox::into_*` combinators compose directly in `.until` / `.or_until` without `let mut`:
    let stop_fut = bot
        .inbox_all()
        .into_wait_for(|m| m.is_cmd("!stop"))
        .ok_or_pending();
    let abort_fut = bob.inbox().into_wait_cmd("!abort").ok_or_pending();

    let work = async move {
        let first = room.inbox().into_next_chat().await?;
        let mapped = room
            .inbox()
            .into_wait_map(|m| m.cmd_args("!pick").map(str::to_owned))
            .await?;
        Ok::<_, Error>((first.text, mapped))
    };

    let race_task = tokio::spawn(async move { work.until(stop_fut).or_until(abort_fut).await });
    tokio::task::yield_now().await;

    server.dm("Bob", "!abort");
    server.settle().await;

    match race_task.await.unwrap() {
        Outcome::Stopped(msg) => assert_eq!(msg.text, "!abort"),
        other => panic!("expected Outcome::Stopped, got {other:?}"),
    }
}
