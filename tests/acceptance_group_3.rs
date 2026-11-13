mod common;

use std::time::Duration;

use ircgm::prelude::*;
use ircgm::testing::{test_harness, test_harness_with};

#[tokio::test(start_paused = true)]
async fn group3_exclusive_serialization_and_15s_holder_location_warning() {
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

    // Launch concurrent `alice.ask("Interrupt?", ...)` while `dm` holds `dm_mutex`:
    let alice_bg = alice.clone();
    let interrupt_task = tokio::spawn(async move {
        alice_bg
            .ask("Interrupt?", |m| Some(m.text.clone()))
            .reply_timeout(Duration::from_secs(30))
            .await
    });
    tokio::task::yield_now().await;

    // `"Interrupt?"` MUST NOT be on the wire while `dm` holds `dm_mutex`:
    assert_eq!(server.take_messages_to("Alice"), vec!["Send 3 items:"]);

    // Hold `dm_mutex` past 15s while `interrupt_task` waits -> emits `tracing::warn!` naming holder `file:line`:
    tokio::time::advance(Duration::from_secs(16)).await;
    tokio::task::yield_now().await;
    let warn_out = logs.text();
    assert!(
        warn_out.contains("waiting >15s to acquire User ExclusiveDm mutex"),
        "expected 15s warn, got: {warn_out}"
    );
    assert!(
        warn_out.contains(&format!("acceptance_group_3.rs:{holder_line}")),
        "expected holder Location acceptance_group_3.rs:{holder_line} in warn: {warn_out}"
    );

    // Still hasn't sent `"Interrupt?"`:
    assert!(server.take_messages_to("Alice").is_empty());

    // Deliver 3 messages exclusively to `dm`:
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

    // Dropping `dm` releases `dm_mutex`, immediately allowing `"Interrupt?"` onto the wire!
    drop(dm);
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert_eq!(server.take_messages_to("Alice"), vec!["Interrupt?"]);

    server.dm("Alice", "yes!");
    server.settle().await;
    assert_eq!(interrupt_task.await.unwrap().unwrap(), "yes!");
}

#[tokio::test]
async fn group3_cancel_during_exclusive_dm_and_ignore_commands() {
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

    // 2. With `with_ignore_commands()`, `!cancel` is skipped by free-text read, caught by
    //    `alice.inbox().wait_cmd("!cancel")`, cancels the flow, drops `dm`, and unblocks a queued `alice.ask`!
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let alice_flow = alice.clone();
    let flow_task = tokio::spawn(async move {
        let mut cancel_in = alice_flow.inbox();
        let cancel_sig = cancel_in.wait_cmd("!cancel").ok_or_pending();
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

    // Queue a follow-up `alice.ask(...)` behind `flow_task`'s `ExclusiveDm`:
    let alice_queued = alice.clone();
    let queued_ask = tokio::spawn(async move {
        alice_queued
            .ask("Queued question?", |m| Some(m.text.clone()))
            .await
    });
    tokio::task::yield_now().await;
    assert!(server.take_messages_to("Alice").is_empty());

    // Alice sends 1 item, then `!cancel`:
    server.dm("Alice", "grappling hook");
    server.dm("Alice", "!cancel");
    server.settle().await;

    let outcome = flow_task.await.unwrap();
    assert!(matches!(outcome, Outcome::Stopped(_)));

    // `dm_mutex` released -> `"Queued question?"` is immediately sent!
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert_eq!(server.take_messages_to("Alice"), vec!["Queued question?"]);

    server.dm("Alice", "proceeding");
    server.settle().await;
    assert_eq!(queued_ask.await.unwrap().unwrap(), "proceeding");
}

#[tokio::test(start_paused = true)]
async fn group3_verdict_reject_warn_remaining_clear_warnings_and_closure_inference() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#lobby", &["Alice"]);
    server.settle().await;

    let alice = bot.user("Alice").unwrap();

    let base_cfg = AskConfig::new()
        .reply_timeout(Duration::from_secs(20))
        .warn_remaining(Duration::from_secs(10), "10s inherited warn")
        .ignore_commands()
        .timeout_message("Time expired!");

    // Verify `.clear_warnings()` suppresses inherited 10s warn while `.warn_remaining(5s, ...)` adds 5s warn,
    // `.ignore_commands()` skips `!help` BEFORE matcher (no `Verdict::Reject`!), and `Verdict::Reject` does NOT reset the 20s clock:
    let alice_c = alice.clone();
    let cfg_c = base_cfg.clone();
    let ask_task = tokio::spawn(async move {
        alice_c
            .ask("Pick 1..=3:", |m| match m.text.trim().parse::<u8>() {
                Ok(n @ 1..=3) => Verdict::Accept(n),
                _ => Verdict::reject("Invalid! Pick 1, 2, or 3."),
            })
            .config(&cfg_c)
            .clear_warnings()
            .warn_remaining(Duration::from_secs(5), "5s left!")
            .await
    });
    tokio::task::yield_now().await;
    assert_eq!(server.take_messages_to("Alice"), vec!["Pick 1..=3:"]);

    // At t = 11s: inherited 10s warning was cleared, so NO warning fired!
    tokio::time::advance(Duration::from_secs(11)).await;
    tokio::task::yield_now().await;
    assert!(server.take_messages_to("Alice").is_empty());

    // At t = 11s: Alice sends `!help` (ignored by `.ignore_commands()`, NO reject!) and `"99"` (rejected!):
    server.dm("Alice", "!help");
    server.dm("Alice", "99");
    server.settle().await;
    tokio::task::yield_now().await;
    assert_eq!(
        server.take_messages_to("Alice"),
        vec!["Invalid! Pick 1, 2, or 3."]
    );

    // At t = 15s (5s remaining on original 20s clock): 5s warning fires (proving reject at t=11s did NOT reset clock!):
    tokio::time::advance(Duration::from_secs(4)).await;
    tokio::task::yield_now().await;
    assert_eq!(server.take_messages_to("Alice"), vec!["5s left!"]);

    // At t = 20s: original 20s clock expires!
    tokio::time::advance(Duration::from_secs(5)).await;
    let err = ask_task.await.unwrap().unwrap_err();
    assert!(matches!(err, Error::TimedOut));
    assert_eq!(server.take_messages_to("Alice"), vec!["Time expired!"]);

    // Also verify `AskConfig::warn_remaining` directly AND multi-arm `Option<T>` closure inference:
    let alice_c2 = alice.clone();
    let opt_task = tokio::spawn(async move {
        alice_c2
            .ask("yes or no?", |m| match m.text.trim() {
                "yes" => Some(true),
                "no" => Some(false),
                _ => None,
            })
            .config(&base_cfg)
            .await
    });
    tokio::task::yield_now().await;
    assert_eq!(server.take_messages_to("Alice"), vec!["yes or no?"]);
    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::task::yield_now().await;
    assert_eq!(server.take_messages_to("Alice"), vec!["10s inherited warn"]);
    server.dm("Alice", "yes");
    server.settle().await;
    assert!(opt_task.await.unwrap().unwrap());
}

#[tokio::test(start_paused = true)]
async fn group3_stale_inbox_cleared_between_exclusive_dm_asks() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#lobby", &["Alice"]);
    server.settle().await;

    let alice = bot.user("Alice").unwrap();
    let mut dm = alice.exclusive().await.unwrap();

    // Ask #1 times out after 5s:
    let res1 = dm
        .ask("Question 1?", |m| Some(m.text.clone()))
        .reply_timeout(Duration::from_secs(5))
        .optional()
        .await
        .unwrap();
    assert!(res1.is_none());
    let _ = server.take_messages_to("Alice");

    // Alice's late answer to Question 1 arrives while idle between Ask #1 and Ask #2:
    server.dm("Alice", "late_answer_to_q1");
    server.settle().await;

    // Ask #2 starts -> `dm.ask` calls `dm.inbox.clear()` so `late_answer_to_q1` is discarded!
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

#[tokio::test(start_paused = true)]
async fn group3_write_receipt_pacing_single_timer_and_combinator_table() {
    // 1. WriteReceipt pacing: `outbound_burst = 2`, `outbound_window = 8s`.
    // Enqueue 4 prompts at t = 0 -> Ask #3 is throttled until `t = 8s`, so its 10s `reply_timeout`
    // MUST expire at `t = 18s` (NOT `t = 10s`!).
    let cfg = BotConfig {
        outbound_burst: 2,
        outbound_window: Duration::from_secs(8),
        ..BotConfig::default()
    };
    let (bot, mut server) = test_harness_with("GM", cfg);
    server.complete_join("#r", &["P1", "P2", "P3", "P4"]);
    server.settle().await;

    let p1 = bot.user("P1").unwrap();
    let p2 = bot.user("P2").unwrap();
    let p3 = bot.user("P3").unwrap();
    let p4 = bot.user("P4").unwrap();

    let spawn_ask = |u: User, label: &'static str| {
        tokio::spawn(async move {
            let start = tokio::time::Instant::now();
            let res = u
                .ask(label, |m| (m.text == "ok").then_some(()))
                .reply_timeout(Duration::from_secs(10))
                .await;
            (res, start.elapsed())
        })
    };

    let t1 = spawn_ask(p1, "Prompt 1");
    let t2 = spawn_ask(p2, "Prompt 2");
    let t3 = spawn_ask(p3, "Prompt 3");
    let t4 = spawn_ask(p4, "Prompt 4");
    tokio::task::yield_now().await;

    // At t = 0: only Prompt 1 and Prompt 2 were written!
    assert_eq!(server.take_messages_to("P1"), vec!["Prompt 1"]);
    assert_eq!(server.take_messages_to("P2"), vec!["Prompt 2"]);
    assert!(server.take_messages_to("P3").is_empty());
    assert!(server.take_messages_to("P4").is_empty());

    // Advance to t = 8s -> Prompt 3 and Prompt 4 are written now (`WriteReceipt` resolves at t = 8s!):
    tokio::time::advance(Duration::from_secs(8)).await;
    tokio::task::yield_now().await;
    assert_eq!(server.take_messages_to("P3"), vec!["Prompt 3"]);
    assert_eq!(server.take_messages_to("P4"), vec!["Prompt 4"]);

    // Advance to t = 10s -> t1 & t2 expire at t = 10s, while t3 & t4 MUST STILL BE ALIVE!
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    let (r1, e1) = t1.await.unwrap();
    let (r2, e2) = t2.await.unwrap();
    assert!(matches!(r1, Err(Error::TimedOut)));
    assert!(matches!(r2, Err(Error::TimedOut)));
    assert_eq!(e1, Duration::from_secs(10));
    assert_eq!(e2, Duration::from_secs(10));
    assert!(!t3.is_finished(), "Ask #3 expired prematurely before t=18s!");

    // At t = 15s (7s after Prompt 4's write receipt!), P4 replies `"ok"` and succeeds!
    tokio::time::advance(Duration::from_secs(5)).await;
    server.dm("P4", "ok");
    server.settle().await;
    let (r4, e4) = t4.await.unwrap();
    assert!(r4.is_ok());
    assert_eq!(e4, Duration::from_secs(15));
    assert!(!t3.is_finished());

    // At t = 18s (10s after Prompt 3's write receipt at t = 8s): Ask #3 expires at exact `t = 18s`!
    tokio::time::advance(Duration::from_secs(3)).await;
    let (r3, e3) = t3.await.unwrap();
    assert!(matches!(r3, Err(Error::TimedOut)));
    assert_eq!(e3, Duration::from_secs(18));

    // 2. Single-timer regression on `ExclusiveDm::ask`:
    // Configure `ExclusiveDm` with a 30s `AskConfig`, then override one `dm.ask` to `120s` and another to `.no_reply_timeout()`:
    let (bot2, server2) = test_harness("GM");
    server2.complete_join("#r", &["Alice"]);
    server2.settle().await;
    let alice = bot2.user("Alice").unwrap();
    let mut dm = alice
        .exclusive()
        .await
        .unwrap()
        .with_ask_config(&AskConfig::new().reply_timeout(Duration::from_secs(30)));

    let alice_s2 = server2;
    let answer_later = async {
        tokio::time::sleep(Duration::from_secs(90)).await;
        alice_s2.dm("Alice", "after_90s");
        alice_s2.settle().await;
        tokio::time::sleep(Duration::from_secs(400)).await;
        alice_s2.dm("Alice", "after_400s");
        alice_s2.settle().await;
    };
    let asks = async {
        let a1 = dm
            .ask("120s prompt", |m| Some(m.text.clone()))
            .reply_timeout(Duration::from_secs(120))
            .await?;
        let a2 = dm
            .ask("unbounded prompt", |m| Some(m.text.clone()))
            .no_reply_timeout()
            .await?;
        Ok::<_, Error>((a1, a2))
    };
    let (_, asks_res) = tokio::join!(answer_later, asks);
    let (a1, a2) = asks_res.unwrap();
    assert_eq!(a1, "after_90s");
    assert_eq!(a2, "after_400s");
    drop(dm);

    // 3. Combinator table test: `optional()`, `settled()`, and `with_timeout()` NEVER emit or suppress messages!
    let (bot3, mut server3) = test_harness("GM");
    server3.complete_join("#tab", &["Alice"]);
    server3.settle().await;
    let room_msg = bot3.room("#tab").unwrap().with_ask_config(
        &AskConfig::new()
            .reply_timeout(Duration::from_secs(10))
            .timeout_message("Timeout fired!"),
    );
    let room_silent = bot3.room("#tab").unwrap().with_ask_config(
        &AskConfig::new()
            .reply_timeout(Duration::from_secs(10))
            .no_timeout_message(),
    );

    // Case A: `room_msg.ask(...).optional()` -> inner Ask expires -> DOES emit "Timeout fired!", maps to `Ok(None)`
    let r = room_msg
        .ask("Q1", |_| Option::<()>::None)
        .optional()
        .await
        .unwrap();
    assert_eq!(r, None);
    assert_eq!(server3.take_messages_to("#tab"), vec!["Q1", "Timeout fired!"]);

    // Case B: `room_msg.ask(...).settled()` -> inner Ask expires -> DOES emit "Timeout fired!", maps to `Ok(Turn::TimedOut)`
    let r = room_msg
        .ask("Q2", |_| Option::<()>::None)
        .settled()
        .await
        .unwrap();
    assert_eq!(r, Turn::TimedOut);
    assert_eq!(server3.take_messages_to("#tab"), vec!["Q2", "Timeout fired!"]);

    // Case C: `room_msg.ask(...).no_timeout_message().optional()` -> suppressed explicitly -> NO timeout message!
    let r = room_msg
        .ask("Q3", |_| Option::<()>::None)
        .no_timeout_message()
        .optional()
        .await
        .unwrap();
    assert_eq!(r, None);
    assert_eq!(server3.take_messages_to("#tab"), vec!["Q3"]);

    // Case D: `room_msg.ask(...).with_timeout(3s).optional()` -> outer `with_timeout(3s)` cancels inner 10s Ask early -> NO timeout message!
    let r = room_msg
        .ask("Q4", |_| Option::<()>::None)
        .with_timeout(Duration::from_secs(3))
        .optional()
        .await
        .unwrap();
    assert_eq!(r, None);
    assert_eq!(server3.take_messages_to("#tab"), vec!["Q4"]);

    // Case E: `room_silent.ask(...).settled()` -> `no_timeout_message` -> NO timeout message!
    let r = room_silent
        .ask("Q5", |_| Option::<()>::None)
        .settled()
        .await
        .unwrap();
    assert_eq!(r, Turn::TimedOut);
    assert_eq!(server3.take_messages_to("#tab"), vec!["Q5"]);
}

#[tokio::test]
async fn group3_exclusive_many_available_and_huddle_semantics() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#village", &["Wolf1", "Wolf2", "Wolf3", "Villager"]);
    server.settle().await;

    let w1 = bot.user("Wolf1").unwrap();
    let w2 = bot.user("Wolf2").unwrap();
    let w3 = bot.user("Wolf3").unwrap();
    let villager = bot.user("Villager").unwrap();

    // 1. Duplicate user fails fast with `Error::InvalidInput`:
    let dup_err = bot.exclusive_many([&w1, &w1]).await.err().unwrap();
    assert!(matches!(dup_err, Error::InvalidInput(_)));

    // 2. Reversed-order concurrent `exclusive_many` calls never deadlock and return guards in caller order:
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

    // 3. Offline user: `exclusive_many` fails with `UserLeft` (releasing already-acquired locks!),
    //    whereas `exclusive_available` skips offline users and returns remaining online guards:
    server.quit_user("Wolf3", Some("disconnected"));
    server.settle().await;

    let err_many = bot.exclusive_many([&w1, &w2, &w3]).await.err().unwrap();
    assert!(matches!(err_many, Error::UserLeft { ref nick, .. } if nick == "Wolf3"));

    // Prove `w1` & `w2` locks were cleanly released when `w3` failed:
    let avail = bot.exclusive_available([&w2, &w3, &w1]).await.unwrap();
    let avail_nicks: Vec<String> = avail.iter().map(|g| g.user().nick()).collect();
    assert_eq!(avail_nicks, vec!["Wolf2", "Wolf1"]);
    drop(avail);

    // All offline -> `exclusive_available` returns `Ok(vec![])`:
    assert!(bot.exclusive_available([&w3]).await.unwrap().is_empty());

    // 4. `Huddle` (`huddle_available`, `with_relay`, default command-relay suppression, private `Verdict::Reject`, `seq` order, RAII drop):
    {
        // Note: NO `.with_ignore_commands()` needed — `pack.ask` matches `!kill <nick>` directly,
        // while `Huddle`'s default `relay_commands = false` prevents `!status` / `!help` from being echoed to teammates!
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

        // Drive `ask_fut` alongside simulated wolf whispers/commands:
        let interact = async {
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            assert_eq!(server.take_messages_to("Wolf1"), vec!["Who to bite?"]);
            assert_eq!(server.take_messages_to("Wolf2"), vec!["Who to bite?"]);

            // a) Unrelated `!status` command: returns `Verdict::Ignore` in matcher, and is NOT relayed by default!
            server.dm("Wolf1", "!status");
            // b) Banter (`Verdict::Ignore`): relayed to Wolf2 ONLY (`[Pack] <Wolf1> let's get Villager`)!
            server.dm("Wolf1", "let's get Villager");
            // c) Invalid `!kill Nobody`: `Verdict::Reject` sent PRIVATELY to Wolf1 ONLY (Wolf2 gets nothing)!
            server.dm("Wolf1", "!kill Nobody");
            // d) Same-batch `!kill Villager` from Wolf2 BEFORE Wolf1 -> `seq` order picks Wolf2!
            server.dm("Wolf2", "!kill Villager");
            server.dm("Wolf1", "!kill Villager");
            server.settle().await;
        };

        let (res, ()) = tokio::join!(ask_fut, interact);
        let (killer, target) = res.unwrap();
        assert_eq!(killer, w2);
        assert_eq!(target, "Villager");

        // Verify Wolf1 received only its private `Verdict::Reject` (not its own banter!),
        // and Wolf2 received only Wolf1's relayed banter (not Wolf1's `Verdict::Reject` or `!status`!):
        assert_eq!(server.take_messages_to("Wolf1"), vec!["Invalid target!"]);
        assert_eq!(
            server.take_messages_to("Wolf2"),
            vec!["[Pack] <Wolf1> let's get Villager"]
        );

        // Opting in via `set_relay_commands(true)` relays commands when explicitly desired:
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
    } // `pack` drops here -> releases `dm_mutex` on Wolf1 & Wolf2!

    // Verify Wolf1's `dm_mutex` is immediately acquirable after `pack` drop:
    let _g = w1.exclusive().await.unwrap();
    drop(_g);

    // 5. `Huddle` with 0 online users fails immediately with `Err(Error::UserLeft { .. })`:
    let mut empty_huddle = bot.huddle_available([&w3]).await.unwrap();
    assert!(empty_huddle.is_empty());
    let empty_err = empty_huddle
        .ask("Anyone?", |_| Some(()))
        .await
        .unwrap_err();
    assert!(matches!(empty_err, Error::UserLeft { .. }));
}
