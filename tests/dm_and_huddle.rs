mod common;

use std::time::Duration;

use ircgm::prelude::*;
use ircgm::testing::test_harness;

#[tokio::test(start_paused = true)]
async fn exclusive_serialization_and_15s_holder_location_warning() {
    let (logs, _guard) = common::capture_warns();
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#lobby", &["Alice"]);
    server.settle().await;

    let alice = bot.user("Alice").unwrap();

    // Acquire `ExclusiveDm` on THIS line so `holder_line` matches `#[track_caller]`:
    let holder_line = line!() + 1;
    let mut dm = alice.exclusive().await.unwrap();
    dm.say("Send 3 items:")
        .expect("initial ExclusiveDm prompt");

    let alice_bg = alice.clone();
    let interrupt_task = tokio::spawn(async move {
        alice_bg
            .ask("Interrupt?", |m| Some(m.text.clone()))
            .reply_timeout(Duration::from_secs(30))
            .await
    });
    tokio::task::yield_now().await;

    assert_eq!(server.take_messages_to("Alice"), vec!["Send 3 items:"]);

    tokio::time::advance(Duration::from_secs(16)).await;
    tokio::task::yield_now().await;
    let warn_out = logs.text();
    assert!(
        warn_out.contains("waiting >15s to acquire User ExclusiveDm mutex"),
        "expected 15s warn, got: {warn_out}"
    );
    assert!(
        warn_out.contains(&format!("dm_and_huddle.rs:{holder_line}")),
        "expected holder Location dm_and_huddle.rs:{holder_line} in warn: {warn_out}"
    );

    assert!(server.take_messages_to("Alice").is_empty());

    server.dm("Alice", "rope");
    server.dm("Alice", "drill");
    server.dm("Alice", "mask");
    server.settle().await;

    let items = [
        dm.inbox().next_chat().await.unwrap().text,
        dm.inbox().next_chat().await.unwrap().text,
        dm.inbox().next_chat().await.unwrap().text,
    ];
    assert_eq!(items, ["rope", "drill", "mask"]);
    assert!(server.take_messages_to("Alice").is_empty());

    drop(dm);
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert_eq!(server.take_messages_to("Alice"), vec!["Interrupt?"]);

    server.dm("Alice", "yes!");
    server.settle().await;
    assert_eq!(interrupt_task.await.unwrap().unwrap(), "yes!");
}

#[tokio::test]
async fn cancel_during_exclusive_dm_and_ignore_commands() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#lobby", &["Alice"]);
    server.settle().await;

    let alice = bot.user("Alice").unwrap();

    // 1. Without `ignore_commands()`, `!cancel` IS consumed as normal text:
    {
        let mut dm = alice.exclusive().await.unwrap();
        server.dm("Alice", "!cancel");
        server.settle().await;
        assert_eq!(dm.inbox().next_chat().await.unwrap().text, "!cancel");
    }

    // 2. With `with_ignore_commands()`, `!cancel` cancels the flow, drops `dm`, and unblocks a queued `alice.ask`!
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let alice_flow = alice.clone();
    let flow_task = tokio::spawn(async move {
        let cancel_sig = alice_flow.inbox().into_wait_cmd("!cancel").ok_or_pending();
        let body = async {
            let mut dm = alice_flow.exclusive().await?.with_ignore_commands();
            let _ = ready_tx.send(());
            let first = dm.inbox().next_chat().await?.text;
            let second = dm.inbox().next_chat().await?.text;
            Ok::<_, Error>((first, second))
        };
        body.until(cancel_sig).await
    });

    ready_rx.await.unwrap();

    let alice_queued = alice.clone();
    let queued_ask = tokio::spawn(async move {
        alice_queued
            .ask("Queued question?", |m| Some(m.text.clone()))
            .await
    });
    tokio::task::yield_now().await;
    assert!(server.take_messages_to("Alice").is_empty());

    server.dm("Alice", "grappling hook");
    server.dm("Alice", "!cancel");
    server.settle().await;

    let outcome = flow_task.await.unwrap();
    assert!(matches!(outcome, Outcome::Stopped(_)));

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert_eq!(server.take_messages_to("Alice"), vec!["Queued question?"]);

    server.dm("Alice", "proceeding");
    server.settle().await;
    assert_eq!(queued_ask.await.unwrap().unwrap(), "proceeding");
}

#[tokio::test(start_paused = true)]
async fn stale_inbox_cleared_between_exclusive_dm_asks() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#lobby", &["Alice"]);
    server.settle().await;

    let alice = bot.user("Alice").unwrap();
    let mut dm = alice.exclusive().await.unwrap();

    let res1 = dm
        .ask("Question 1?", |m| Some(m.text.clone()))
        .reply_timeout(Duration::from_secs(5))
        .optional()
        .await
        .unwrap();
    assert!(res1.is_none());
    let _ = server.take_messages_to("Alice");

    server.dm("Alice", "late_answer_to_q1");
    server.settle().await;

    let mut ask2 = Box::pin(dm.ask("Question 2?", |m| Some(m.text.clone())));
    tokio::select! {
        res = &mut ask2 => panic!("Ask #2 matched stale reply: {res:?}"),
        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
    }
    assert_eq!(server.take_messages_to("Alice"), vec!["Question 2?"]);

    server.dm("Alice", "fresh_answer_to_q2");
    server.settle().await;
    assert_eq!(ask2.await.unwrap(), "fresh_answer_to_q2");
}

#[tokio::test]
async fn exclusive_many_available_and_huddle_semantics() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#village", &["Wolf1", "Wolf2", "Wolf3", "Villager"]);
    server.settle().await;

    let w1 = bot.user("Wolf1").unwrap();
    let w2 = bot.user("Wolf2").unwrap();
    let w3 = bot.user("Wolf3").unwrap();
    let villager = bot.user("Villager").unwrap();

    let dup_err = bot.exclusive_many([&w1, &w1]).await.err().unwrap();
    assert!(matches!(dup_err, Error::InvalidInput(_)));

    let b1 = bot.clone();
    let b2 = bot.clone();
    let (w1a, w2a, w3a) = (w1.clone(), w2.clone(), w3.clone());
    let (w1b, w2b, w3b) = (w1.clone(), w2.clone(), w3.clone());
    let (g_forward, g_reverse) = tokio::join!(
        tokio::spawn(async move {
            let guards = b1.exclusive_many([&w1a, &w2a, &w3a]).await.unwrap();
            let order: Vec<String> = guards.iter().map(|g| g.user().nick()).collect();
            tokio::task::yield_now().await;
            order
        }),
        tokio::spawn(async move {
            let guards = b2.exclusive_many([&w3b, &w2b, &w1b]).await.unwrap();
            let order: Vec<String> = guards.iter().map(|g| g.user().nick()).collect();
            tokio::task::yield_now().await;
            order
        }),
    );
    assert_eq!(g_forward.unwrap(), vec!["Wolf1", "Wolf2", "Wolf3"]);
    assert_eq!(g_reverse.unwrap(), vec!["Wolf3", "Wolf2", "Wolf1"]);

    server.quit_user("Wolf3", Some("disconnected"));
    server.settle().await;

    let err_many = bot.exclusive_many([&w1, &w2, &w3]).await.err().unwrap();
    assert!(matches!(err_many, Error::UserLeft { ref nick, .. } if nick == "Wolf3"));

    let avail = bot.exclusive_available([&w2, &w3, &w1]).await.unwrap();
    let avail_nicks: Vec<String> = avail.iter().map(|g| g.user().nick()).collect();
    assert_eq!(avail_nicks, vec!["Wolf2", "Wolf1"]);
    drop(avail);

    assert!(bot.exclusive_available([&w3]).await.unwrap().is_empty());

    {
        let mut pack = bot
            .huddle_available([&w1, &w2, &w3])
            .await
            .unwrap()
            .with_relay("[Pack] ");
        assert_eq!(pack.len(), 2);
        assert!(!pack.is_empty());

        let ask_fut = pack.ask("Who to bite?", |m| {
            let Some(target) = m.cmd_args("!kill") else {
                return Verdict::Ignore;
            };
            if villager.eq_nick(target) {
                Verdict::Accept((m.sender.clone(), target.to_owned()))
            } else {
                Verdict::reject("Invalid target!")
            }
        });

        let interact = async {
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            assert_eq!(server.take_messages_to("Wolf1"), vec!["Who to bite?"]);
            assert_eq!(server.take_messages_to("Wolf2"), vec!["Who to bite?"]);

            server.dm("Wolf1", "!status");
            server.dm("Wolf1", "let's get Villager");
            server.dm("Wolf1", "!kill Nobody");
            server.dm("Wolf2", "!kill Villager");
            server.dm("Wolf1", "!kill Villager");
            server.settle().await;
        };

        let (res, ()) = tokio::join!(ask_fut, interact);
        let (killer, target) = res.unwrap();
        assert_eq!(killer, w2);
        assert_eq!(target, "Villager");

        assert_eq!(server.take_messages_to("Wolf1"), vec!["Invalid target!"]);
        assert_eq!(
            server.take_messages_to("Wolf2"),
            vec!["[Pack] <Wolf1> let's get Villager"]
        );

        pack.inbox().clear();
        pack.set_relay_commands(true);
        server.dm("Wolf1", "!ping");
        server.settle().await;
        let cmd_msg = pack.inbox().next_chat().await.unwrap();
        pack.relay_to_others(&cmd_msg).unwrap();
        assert_eq!(
            server.take_messages_to("Wolf2"),
            vec!["[Pack] <Wolf1> !ping"]
        );
    }

    let _g = w1.exclusive().await.unwrap();
    drop(_g);

    let mut empty_huddle = bot.huddle_available([&w3]).await.unwrap();
    assert!(empty_huddle.is_empty());
    let empty_err = empty_huddle
        .ask("Anyone?", |_| Some(()))
        .await
        .unwrap_err();
    assert!(matches!(empty_err, Error::UserLeft { .. }));
}
