mod common;

use std::time::Duration;

use futures::future::try_join_all;
use irc::proto::Command;
use ircgm::prelude::*;
use ircgm::testing::{test_harness, test_harness_with};
use ircgm::BotConfig;

#[tokio::test]
async fn packet_burst_and_wire_fair_inbox_users_race() {
    let (bot, server) = test_harness("GM");
    server.complete_join("#q", &["Alice", "Bob"]);
    server.settle().await;

    let room = bot.room("#q").unwrap();
    let mut inbox = room.inbox();

    server.send_line(":Alice!u@h PRIVMSG #q :!join\r\n");
    server.send_line(":Bob!u@h PRIVMSG #q :!join\r\n");
    server.settle().await;

    let m1 = inbox.wait_cmd("!join").await.unwrap();
    let m2 = inbox.wait_cmd("!join").await.unwrap();
    assert_eq!(m1.nick, "Alice");
    assert_eq!(m2.nick, "Bob");
    assert!(m1.seq < m2.seq);

    // `[alice, bob]` in vector order, but Bob's DM arrives FIRST on wire:
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
async fn multisource_inbox_scope_and_unified_source_exhaustion() {
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
    assert_eq!(r1_events.next_chat().await.unwrap().text, "one");
    assert_eq!(mixed.next_chat().await.unwrap().text, "one");
    assert_eq!(mixed.next_chat().await.unwrap().text, "two");
    assert_eq!(mixed.next_chat().await.unwrap().text, "four");
    assert_eq!(users_only.next_chat().await.unwrap().text, "two");
    assert_eq!(users_only.next_chat().await.unwrap().text, "four");

    server.quit_user("Alice", Some("leaving early"));
    server.dm("Bob", "bob_still_here");
    server.settle().await;

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

    let bob_reply = users_only
        .wait_map(|m| (m.text == "bob_still_here").then(|| m.sender.clone()))
        .await
        .unwrap();
    assert_eq!(bob_reply, bob);

    server.quit_user("Bob", Some("me too"));
    server.chat("Spectator", "#r1", "room_lives_on");
    server.settle().await;

    let err = users_only.wait_map(|_| Some(())).await.unwrap_err();
    assert!(matches!(err, Error::UserLeft { .. }), "got {err:?}");

    let survivor_msg = mixed
        .wait_for(|m| m.text == "room_lives_on")
        .await
        .unwrap();
    assert_eq!(survivor_msg.nick, "Spectator");
}

#[tokio::test]
async fn oneshot_non_backlog_and_inbox_dms_filtering() {
    let (bot, server) = test_harness("GM");
    server.complete_join("#lobby", &["Alice", "Bob"]);
    server.settle().await;

    let room = bot.room("#lobby").unwrap();

    let r_wait = room.clone();
    let wait_task = tokio::spawn(async move { r_wait.inbox().into_wait_cmd("!heist").await });
    tokio::task::yield_now().await;
    server.chat("Alice", "#lobby", "!heist");
    server.settle().await;
    let trigger = wait_task.await.unwrap().unwrap();
    assert_eq!(trigger.nick, "Alice");

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

    let mut fresh_in = room.inbox();
    let res = fresh_in
        .wait_cmd("!heist")
        .with_timeout(Duration::from_millis(20))
        .none_on_timeout()
        .await
        .unwrap();
    assert!(res.is_none());
}

#[tokio::test]
async fn notice_isolation_accept_notices_and_reply_mirroring() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#lobby", &["Alice", "OtherBot"]);
    server.settle().await;

    let room = bot.room("#lobby").unwrap();
    let mut default_in = bot.inbox_all();
    let mut notice_in = bot.inbox_all().accept_notices();
    let mut event_in = bot.inbox_all();

    server.send_line(":NickServ!s@services NOTICE GM :!heist\r\n");
    server.send_line(":irc.example.org NOTICE GM :!heist\r\n");
    server.send_line("@bot :OtherBot!b@h NOTICE #lobby :!heist\r\n");
    server.send_line(":Alice!u@h NOTICE GM :!vote Bob\r\n");
    server.chat("Alice", "#lobby", "sentinel");
    server.settle().await;

    let m_default = default_in.next_chat().await.unwrap();
    assert_eq!(m_default.text, "sentinel");
    assert_eq!(m_default.kind, MessageKind::Privmsg);
    assert!(!m_default.is_notice());

    let mut saw_nickserv_notice = false;
    for _ in 0..5 {
        if let Event::Notice { sender, text, .. } = event_in.next_event().await.unwrap() {
            if sender.as_ref().is_some_and(|u| u.eq_nick("NickServ")) && text == "!heist" {
                saw_nickserv_notice = true;
            }
        }
    }
    assert!(saw_nickserv_notice);

    let m_vote = notice_in.next_chat().await.unwrap();
    assert_eq!(m_vote.nick, "Alice");
    assert_eq!(m_vote.text, "!vote Bob");
    assert_eq!(m_vote.kind, MessageKind::Notice);
    assert!(m_vote.is_notice());
    assert!(m_vote.is_dm());
    assert_eq!(m_vote.cmd_args("!vote"), Some("Bob"));

    m_vote.reply("Recorded").unwrap();
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::NOTICE("Alice".into(), "Recorded".into())
    );

    let r_def = room.clone();
    let t1 = tokio::spawn(async move {
        r_def
            .ask("Default ask", |m| (m.text == "via_notice").then_some(()))
            .reply_timeout(Duration::from_millis(30))
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

    assert_eq!(
        t2.await.unwrap().unwrap(),
        Answer::Ok(MessageKind::Notice)
    );
    assert_eq!(t1.await.unwrap().unwrap().ok(), None);
}

#[tokio::test]
async fn unwatched_channel_zero_alloc_and_multichannel_dedup() {
    let (bot, server) = test_harness("GM");
    server.complete_join("#watched", &[]);
    server.settle().await;

    let baseline_tracks = server.tracked_user_count();

    for i in 0..50 {
        server.send_line(&format!(
            ":Stranger{i}!u@h PRIVMSG #unwatched :noisy spam {i}\r\n"
        ));
    }
    server.settle().await;
    assert_eq!(server.tracked_user_count(), baseline_tracks);

    server.complete_join("#r1", &["Alice"]);
    server.complete_join("#r2", &["Alice"]);
    server.settle().await;

    let r1 = bot.room("#r1").unwrap();
    let r2 = bot.room("#r2").unwrap();
    let mut multi = bot.inbox_rooms([&r1, &r2]);

    server.send_line(":Alice!~Alice@test.host NICK :AliceRenamed\r\n");
    server.quit_user("AliceRenamed", Some("done"));
    server.chat("Observer", "#r1", "sentinel");
    server.settle().await;

    let ev1 = multi.next_event().await.unwrap();
    let ev2 = multi.next_event().await.unwrap();
    let ev3 = multi.next_event().await.unwrap();
    assert!(matches!(ev1, Event::NickChanged { ref new_nick, .. } if new_nick == "AliceRenamed"));
    assert!(matches!(ev2, Event::Quit { ref nick, .. } if nick == "AliceRenamed"));
    assert!(matches!(ev3, Event::Chat(ref m) if m.text == "sentinel"));
}

#[tokio::test]
async fn lag_isolation_75pct_warn_track_caller_hint_and_lossy_recovery() {
    let (logs, _guard) = common::capture_warns();
    let cfg = BotConfig {
        inbox_capacity: 4,
        ..BotConfig::default()
    };
    let (bot, server) = test_harness_with("GM", cfg);
    server.complete_join("#lobby", &["Alice", "Bob"]);
    server.settle().await;

    let alice = bot.user("Alice").unwrap();
    let bob = bot.user("Bob").unwrap();
    let room = bot.room("#lobby").unwrap();

    let mut alice_dm = alice.exclusive().await.unwrap();
    for i in 0..100 {
        server.chat("Bob", "#lobby", &format!("lobby noise {i}"));
    }
    server.dm("Alice", "clean dm");
    server.settle().await;
    assert_eq!(alice_dm.inbox().next_chat().await.unwrap().text, "clean dm");
    drop(alice_dm);

    let open_line = line!() + 1;
    let mut lag_in = alice.inbox();

    for i in 1..=3 {
        server.dm("Alice", &format!("m{i}"));
    }
    server.settle().await;
    let warn_text = logs.text();
    assert!(
        warn_text.contains("inbox buffer reached >=75% capacity")
            && warn_text.contains("user Alice"),
        "expected 75% capacity warn, got: {warn_text}"
    );

    for i in 4..=6 {
        server.dm("Alice", &format!("m{i}"));
    }
    server.settle().await;

    let lag_err = lag_in.next_chat().await.unwrap_err();
    let rendered = lag_err.to_string();
    match lag_err {
        Error::Lagged {
            scope,
            skipped,
            opened_at,
        } => {
            assert_eq!(scope, "user Alice");
            assert_eq!(skipped, 2);
            assert_eq!(opened_at.line(), open_line);
        }
        other => panic!("expected Error::Lagged, got {other:?}"),
    }
    assert!(
        rendered.contains(
            "Did you leave an Inbox open across an .await? Use .clear() or .lossy() if intended."
        ),
        "missing actionable hint in: {rendered}"
    );

    let voters = vec![alice.clone(), bob.clone()];
    let mut lossy_dms: Vec<_> = voters
        .into_iter()
        .map(|u| (u.clone(), u.inbox().lossy()))
        .collect();
    let _ = room.say("Vote now!");

    for i in 0..10 {
        server.dm("Alice", &format!("spam {i}"));
    }
    server.settle().await;

    let collect_fut = try_join_all(lossy_dms.iter_mut().map(|(u, inb)| async move {
        let pick = inb
            .wait_map(|m| m.cmd_args("!vote").map(str::to_owned))
            .await?;
        Ok::<_, Error>((u.nick(), pick))
    }));
    let send_votes = async {
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        server.dm("Alice", "!vote Bob");
        server.dm("Bob", "!vote Alice");
        server.settle().await;
    };
    let (votes_res, ()) = tokio::join!(collect_fut, send_votes);
    let votes = votes_res.unwrap();
    assert_eq!(
        votes,
        vec![
            ("Alice".into(), "Bob".into()),
            ("Bob".into(), "Alice".into())
        ]
    );
}
