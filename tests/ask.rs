use std::time::Duration;

use futures::future::try_join_all;
use irc::proto::Command;
use ircgm::prelude::*;
use ircgm::testing::{test_harness, test_harness_with};
use ircgm::{BotConfig, DisconnectReason, Target};

#[tokio::test(start_paused = true)]
async fn verdict_reject_warn_remaining_clear_warnings_and_closure_inference() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#lobby", &["Alice"]);
    server.settle().await;

    let alice = bot.user("Alice").unwrap();

    let base_cfg = AskConfig::new()
        .reply_timeout(Duration::from_secs(20))
        .warn_remaining(Duration::from_secs(10), "10s inherited warn")
        .ignore_commands()
        .timeout_message("Time expired!");

    let alice_c = alice.clone();
    let cfg_c = base_cfg.clone();
    let ask_task = tokio::spawn(async move {
        alice_c
            .ask("Pick 1..=3:", |m| match m.text.trim().parse::<u8>() {
                Ok(n @ 1..=3) => Verdict::accept(n),
                _ => Verdict::reject("Invalid! Pick 1, 2, or 3."),
            })
            .config(&cfg_c)
            .clear_warnings()
            .warn_remaining(Duration::from_secs(5), "5s left!")
            .await
    });
    tokio::task::yield_now().await;
    assert_eq!(server.take_messages_to("Alice"), vec!["Pick 1..=3:"]);

    tokio::time::advance(Duration::from_secs(11)).await;
    tokio::task::yield_now().await;
    assert!(server.take_messages_to("Alice").is_empty());

    server.dm("Alice", "!help");
    server.dm("Alice", "99");
    server.settle().await;
    tokio::task::yield_now().await;
    assert_eq!(
        server.take_messages_to("Alice"),
        vec!["Invalid! Pick 1, 2, or 3."]
    );

    tokio::time::advance(Duration::from_secs(4)).await;
    tokio::task::yield_now().await;
    assert_eq!(server.take_messages_to("Alice"), vec!["5s left!"]);

    tokio::time::advance(Duration::from_secs(5)).await;
    let err = ask_task.await.unwrap().unwrap_err();
    assert!(matches!(err, Error::TimedOut));
    assert_eq!(server.take_messages_to("Alice"), vec!["Time expired!"]);

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
async fn verdict_accept_ack_preserves_wire_order_across_queued_dm_asks_and_broadcasts_on_huddle() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#lobby", &["Alice", "Bob"]);
    server.settle().await;

    let room = bot.room("#lobby").unwrap();
    let alice = bot.user("Alice").unwrap();
    let bob = bot.user("Bob").unwrap();

    // 1. Two concurrent `alice.ask(...)` calls: `accept_ack` on Task 1 is enqueued while Task 1
    //    still holds `ExclusiveDm`, strictly preceding Task 2's prompt on the wire!
    let a1 = alice.clone();
    let t1 = tokio::spawn(async move {
        a1.ask("Prompt #1", |m| {
            if m.text.trim() == "first" {
                Verdict::accept_ack("one", "✔ Ack #1")
            } else {
                Verdict::ignore()
            }
        })
        .await
    });
    tokio::task::yield_now().await;

    let a2 = alice.clone();
    let t2 = tokio::spawn(async move {
        a2.ask("Prompt #2", |m| {
            if m.text.trim() == "second" {
                Verdict::accept_ack("two", "✔ Ack #2")
            } else {
                Verdict::ignore()
            }
        })
        .await
    });
    server.flush().await;
    assert_eq!(server.take_messages_to("Alice"), vec!["Prompt #1"]);

    server.dm("Alice", "first");
    server.settle().await;
    // `✔ Ack #1` MUST precede `Prompt #2`!
    assert_eq!(
        server.take_messages_to("Alice"),
        vec!["✔ Ack #1", "Prompt #2"]
    );
    assert_eq!(t1.await.unwrap().unwrap(), "one");

    server.dm("Alice", "second");
    server.settle().await;
    assert_eq!(server.take_messages_to("Alice"), vec!["✔ Ack #2"]);
    assert_eq!(t2.await.unwrap().unwrap(), "two");

    // 2. `Room::ask` with `Verdict::accept_ack` acks in the channel:
    let r = room.clone();
    let r_task = tokio::spawn(async move {
        r.ask("Type !ready:", |m| {
            if m.is_cmd("!ready") {
                Verdict::accept_ack(m.sender.nick(), format!("✔ {} is ready!", m.sender.nick()))
            } else {
                Verdict::ignore()
            }
        })
        .await
    });
    server.flush().await;
    let _ = server.take_messages_to("#lobby");
    server.chat("Bob", "#lobby", "!ready");
    server.settle().await;
    assert_eq!(r_task.await.unwrap().unwrap(), "Bob");
    assert_eq!(server.take_messages_to("#lobby"), vec!["✔ Bob is ready!"]);

    // 3. `Huddle::ask` with `Verdict::accept_ack` broadcasts the ack to all huddle members:
    let h_bot = bot.clone();
    let h_alice = alice.clone();
    let h_bob = bob.clone();
    let h_task = tokio::spawn(async move {
        let mut huddle = h_bot.huddle([&h_alice, &h_bob]).await?;
        huddle
            .ask("Who takes watch?", |m| {
                m.cmd_arg("!watch").map_or_else(Verdict::ignore, |who| {
                    Verdict::accept_ack(
                        who.to_owned(),
                        format!("✔ {} assigned {} to watch!", m.sender.nick(), who),
                    )
                })
            })
            .await
    });
    server.flush().await;
    let _ = server.take_messages_to("Alice");
    let _ = server.take_messages_to("Bob");

    server.dm("Alice", "!watch Bob");
    server.settle().await;
    assert_eq!(h_task.await.unwrap().unwrap(), "Bob");
    assert_eq!(
        server.take_messages_to("Alice"),
        vec!["✔ Alice assigned Bob to watch!"]
    );
    assert_eq!(
        server.take_messages_to("Bob"),
        vec!["✔ Alice assigned Bob to watch!"]
    );
}

#[tokio::test(start_paused = true)]
async fn write_receipt_pacing_single_timer_and_combinator_table() {
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

    assert_eq!(server.take_messages_to("P1"), vec!["Prompt 1"]);
    assert_eq!(server.take_messages_to("P2"), vec!["Prompt 2"]);
    assert!(server.take_messages_to("P3").is_empty());
    assert!(server.take_messages_to("P4").is_empty());

    tokio::time::advance(Duration::from_secs(8)).await;
    tokio::task::yield_now().await;
    assert_eq!(server.take_messages_to("P3"), vec!["Prompt 3"]);
    assert_eq!(server.take_messages_to("P4"), vec!["Prompt 4"]);

    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    let (r1, e1) = t1.await.unwrap();
    let (r2, e2) = t2.await.unwrap();
    assert!(matches!(r1, Err(Error::TimedOut)));
    assert!(matches!(r2, Err(Error::TimedOut)));
    assert_eq!(e1, Duration::from_secs(10));
    assert_eq!(e2, Duration::from_secs(10));
    assert!(
        !t3.is_finished(),
        "Ask #3 expired prematurely before t=18s!"
    );

    tokio::time::advance(Duration::from_secs(5)).await;
    server.dm("P4", "ok");
    server.settle().await;
    let (r4, e4) = t4.await.unwrap();
    assert!(r4.is_ok());
    assert_eq!(e4, Duration::from_secs(15));
    assert!(!t3.is_finished());

    tokio::time::advance(Duration::from_secs(3)).await;
    let (r3, e3) = t3.await.unwrap();
    assert!(matches!(r3, Err(Error::TimedOut)));
    assert_eq!(e3, Duration::from_secs(18));

    // Single-timer override on `ExclusiveDm::ask`:
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

    // Combinator table: `optional()`, `settled()`, and `with_timeout()`
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

    let r = room_msg
        .ask("Q1", |_| Option::<()>::None)
        .optional()
        .await
        .unwrap();
    assert_eq!(r, None);
    assert_eq!(
        server3.take_messages_to("#tab"),
        vec!["Q1", "Timeout fired!"]
    );

    let r = room_msg
        .ask("Q2", |_| Option::<()>::None)
        .settled()
        .await
        .unwrap();
    assert_eq!(r, Turn::TimedOut);
    assert_eq!(
        server3.take_messages_to("#tab"),
        vec!["Q2", "Timeout fired!"]
    );

    let r = room_msg
        .ask("Q3", |_| Option::<()>::None)
        .no_timeout_message()
        .optional()
        .await
        .unwrap();
    assert_eq!(r, None);
    assert_eq!(server3.take_messages_to("#tab"), vec!["Q3"]);

    let r = room_msg
        .ask("Q4", |_| Option::<()>::None)
        .with_timeout(Duration::from_secs(3))
        .optional()
        .await
        .unwrap();
    assert_eq!(r, None);
    assert_eq!(server3.take_messages_to("#tab"), vec!["Q4"]);

    let r = room_silent
        .ask("Q5", |_| Option::<()>::None)
        .settled()
        .await
        .unwrap();
    assert_eq!(r, Turn::TimedOut);
    assert_eq!(server3.take_messages_to("#tab"), vec!["Q5"]);
}

#[tokio::test(start_paused = true)]
async fn room_ask_user_nick_tracking_timeout_ctx_and_part_kick_settled() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#r", &["Alice", "Bob", "Cara"]);
    server.settle().await;

    let alice = bot.user("Alice").unwrap();
    let cara = bot.user("Cara").unwrap();

    let room = bot.room("#r").unwrap().with_ask_config(
        &AskConfig::new()
            .reply_timeout(Duration::from_secs(30))
            .timeout_message_fn(|ctx| {
                assert_eq!(ctx.configured_timeout, Some(Duration::from_secs(30)));
                assert_eq!(ctx.target, &Target::Channel("#r".into()));
                let u = ctx.user.expect("room.ask_user sets TimeoutCtx::user");
                format!("{} took too long (30s)!", u.nick())
            }),
    );

    let r_turn = room.clone();
    let a_turn = alice.clone();
    let turn_task = tokio::spawn(async move {
        r_turn
            .ask_user(&a_turn, "Alice: hit or stand?", |m| {
                (m.text.trim() == "hit").then_some(())
            })
            .settled()
            .await
    });
    tokio::task::yield_now().await;
    assert_eq!(server.take_messages_to("#r"), vec!["Alice: hit or stand?"]);

    server.chat("Bob", "#r", "hit");
    server.send_line(":Alice!~Alice@test.host NICK :Alice2\r\n");
    server.settle().await;
    assert!(!turn_task.is_finished());

    tokio::time::advance(Duration::from_secs(30)).await;
    let outcome = turn_task.await.unwrap().unwrap();
    assert_eq!(outcome, Turn::TimedOut);
    assert_eq!(
        server.take_messages_to("#r"),
        vec!["Alice2 took too long (30s)!"]
    );

    let r_part = room.clone();
    let a_part = alice.clone();
    let part_task = tokio::spawn(async move {
        r_part
            .ask_user(&a_part, "Again?", |m| Some(m.text.clone()))
            .settled()
            .await
    });
    tokio::task::yield_now().await;
    let _ = server.take_messages_to("#r");

    server.send_line(":Alice2!~Alice@test.host PART #r :gotta go\r\n");
    server.settle().await;
    let part_out = part_task.await.unwrap().unwrap();
    assert_eq!(
        part_out,
        Turn::Left {
            nick: "Alice2".into(),
            reason: Some("gotta go".into()),
        }
    );

    let r_kick = room.clone();
    let c_kick = cara.clone();
    let kick_task = tokio::spawn(async move {
        r_kick
            .ask_user(&c_kick, "Cara?", |m| Some(m.text.clone()))
            .settled()
            .await
    });
    tokio::task::yield_now().await;
    let _ = server.take_messages_to("#r");

    server.send_line(":Op!~op@test.host KICK #r Cara :removed\r\n");
    server.settle().await;
    let kick_out = kick_task.await.unwrap().unwrap();
    assert_eq!(
        kick_out,
        Turn::Left {
            nick: "Cara".into(),
            reason: Some("removed".into()),
        }
    );
}

#[tokio::test]
async fn settled_unreachable_pm_block_and_fatal_disconnect_propagation() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#game", &["P1", "P2", "P3"]);
    server.settle().await;

    let p1 = bot.user("P1").unwrap();
    let p2 = bot.user("P2").unwrap();
    let p3 = bot.user("P3").unwrap();

    let players = vec![p1.clone(), p2.clone(), p3.clone()];
    let all_turns = tokio::spawn(async move {
        try_join_all(players.into_iter().map(|p| async move {
            let t = p
                .ask("Secret prompt!", |m| Some(m.text.clone()))
                .settled()
                .await?;
            Ok::<_, Error>((p.nick(), t))
        }))
        .await
    });

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    let _ = server.take_messages_to("P1");
    let _ = server.take_messages_to("P2");
    let _ = server.take_messages_to("P3");

    server.dm("P1", "my quip");
    server.send_line(":irc.srv 486 GM P2 :You must log in with services to message this user\r\n");
    server.send_line(":irc.srv 716 GM P3 :is in +g mode (server-side ignore)\r\n");
    server.settle().await;

    let results = all_turns.await.unwrap().unwrap();
    assert_eq!(results[0], ("P1".into(), Turn::Reply("my quip".into())));
    assert!(matches!(
        results[1].1,
        Turn::Unreachable { ref target, code: 486, .. } if target == "P2"
    ));
    assert!(matches!(
        results[2].1,
        Turn::Unreachable { ref target, code: 716, .. } if target == "P3"
    ));

    let p1_disc = p1.clone();
    let disc_task = tokio::spawn(async move {
        p1_disc
            .ask("Still connected?", |m| Some(m.text.clone()))
            .settled()
            .await
    });
    tokio::task::yield_now().await;
    server.disconnect();

    let err = disc_task.await.unwrap().unwrap_err();
    assert!(err.is_fatal());
    assert!(matches!(err, Error::Disconnected(DisconnectReason::Eof)));
}

#[tokio::test]
async fn delivery_failed_bot_left_room_absent_ask_user_and_self_echo() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#room", &["Alice"]);
    server.complete_join("#other", &["Bob"]);
    server.settle().await;

    let room = bot.room("#room").unwrap();
    let alice = bot.user("Alice").unwrap();
    let bob = bot.user("Bob").unwrap();

    let absent_err = room
        .ask_user(&bob, "Bob are you here?", |_| Some(()))
        .await
        .unwrap_err();
    assert!(matches!(absent_err, Error::UserLeft { ref nick, .. } if nick == "Bob"));
    assert!(server.try_recv_sent().is_none());

    let mut raw_in = room.inbox();
    let mut cmd_in = room.inbox();
    server.send_line(":GM!b@h PRIVMSG #room :!join\r\n");
    server.chat("Alice", "#room", "!join");
    server.settle().await;

    let m = cmd_in.wait_cmd("!join").await.unwrap();
    assert_eq!(m.nick, "Alice");
    let first_raw = raw_in.next_raw().await.unwrap();
    assert!(
        matches!(&first_raw.command, Command::PRIVMSG(ch, txt) if ch == "#room" && txt == "!join")
    );

    // Empty prompt rejected upfront without wire traffic:
    assert!(matches!(
        room.ask("", |_| Some(())).await,
        Err(Error::InvalidInput(_))
    ));

    let r_404 = room.clone();
    let ask_404 = tokio::spawn(async move { r_404.ask("Hello?", |_| Some(())).settled().await });
    assert_eq!(server.recv_message_to("#room").await, "Hello?");
    server.send_line(":irc.srv 404 GM #room :Cannot send to channel (+m)\r\n");
    server.settle().await;
    let err_404 = ask_404.await.unwrap().unwrap_err();
    assert!(
        matches!(err_404, Error::ChannelSendFailed { ref channel, code: 404, .. } if channel == "#room")
    );

    let mut dms_events = bot.inbox_dms();
    alice.say("Your secret role is Seer!").unwrap();
    let _ = server.take_messages_to("Alice");
    server.send_line(":irc.srv 716 GM Alice :is in +g mode (callerid)\r\n");
    server.settle().await;

    let ev = dms_events.next_event().await.unwrap();
    assert!(
        matches!(ev, Event::DeliveryFailed { ref target, code: 716, .. } if target == "Alice")
    );

    for code in [486u16, 492, 531, 716] {
        let a = alice.clone();
        let t = tokio::spawn(async move { a.ask("Ping?", |_| Some(())).settled().await });
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let _ = server.take_messages_to("Alice");
        server.send_line(&format!(":irc.srv {code} GM Alice :blocked ({code})\r\n"));
        server.settle().await;
        let out = t.await.unwrap().unwrap();
        assert!(
            matches!(out, Turn::Unreachable { ref target, code: c, .. } if target == "Alice" && c == code),
            "expected Unreachable({code}), got {out:?}"
        );
    }

    let r_kick = room.clone();
    let ask_kicked = tokio::spawn(async move { r_kick.ask("Anyone?", |_| Some(())).await });
    tokio::task::yield_now().await;
    let _ = server.take_messages_to("#room");
    server.send_line(":Op!~op@h KICK #room GM :bots out\r\n");
    server.settle().await;
    let err_left = ask_kicked.await.unwrap().unwrap_err();
    assert!(matches!(
        err_left,
        Error::BotLeftRoom { ref channel, ref reason } if channel == "#room" && reason.as_deref() == Some("bots out")
    ));
}

#[tokio::test]
async fn dm_and_huddle_ask_retry_inflight_rename_bounces_and_evict_claimed_former_nicks() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#room", &["Alice", "Mallory", "Wolf1", "Wolf2"]);
    server.settle().await;

    let alice = bot.user("Alice").unwrap();
    let w1 = bot.user("Wolf1").unwrap();
    let w2 = bot.user("Wolf2").unwrap();

    // 1. `alice.ask` sends to `Alice`, `Alice` renames to `Alicia` in flight, server bounces `401 Alice`
    //    -> `Ask` automatically re-sends `"First prompt?"` to `"Alicia"` and accepts her reply!
    let a1 = alice.clone();
    let ask1 = tokio::spawn(async move {
        a1.ask("First prompt?", |m| Some(m.text.clone()))
            .settled()
            .await
    });
    assert_eq!(server.recv_message_to("Alice").await, "First prompt?");

    server.rename_user("Alice", "Alicia");
    server.send_line(":irc.srv 401 GM Alice :No such nick/channel\r\n");
    assert_eq!(server.recv_message_to("Alicia").await, "First prompt?");
    server.dm("Alicia", "renamed and replied!");
    server.settle().await;

    let out1 = ask1.await.unwrap().unwrap();
    assert_eq!(out1, Turn::Reply("renamed and replied!".into()));
    assert!(alice.is_online());
    assert_eq!(alice.nick(), "Alicia");

    // 2. `Mallory` claims the vacated nick `"Alice"`; bounces for `"Alice"` now belong to `Mallory`
    //    and do NOT abort `Alicia`'s active prompt:
    server.rename_user("Mallory", "Alice");
    server.settle().await;

    let a2 = alice.clone();
    let ask2 = tokio::spawn(async move { a2.ask("Second prompt?", |m| Some(m.text.clone())).await });
    assert_eq!(server.recv_message_to("Alicia").await, "Second prompt?");

    server.send_line(":irc.srv 716 GM Alice :callerid\r\n");
    server.dm("Alicia", "got it!");
    server.settle().await;
    assert_eq!(ask2.await.unwrap().unwrap(), "got it!");

    // 3. `Huddle::ask` where `Wolf1` renames in flight (`401 Wolf1` -> re-sends to `AlphaWolf`) while
    //    `Wolf2` bounces `716`, then `AlphaWolf` also bounces `716` -> `UserUnreachable`:
    let mut pack = bot.huddle([&w1, &w2]).await.unwrap();
    let huddle_fut = pack.ask("Target?", |m| Some(m.text.clone()));
    let drive_huddle = async {
        assert_eq!(server.recv_message_to("Wolf1").await, "Target?");
        assert_eq!(server.recv_message_to("Wolf2").await, "Target?");
        server.rename_user("Wolf1", "AlphaWolf");
        server.send_line(":irc.srv 401 GM Wolf1 :No such nick/channel\r\n");
        server.send_line(":irc.srv 716 GM Wolf2 :callerid\r\n");
        assert_eq!(server.recv_message_to("AlphaWolf").await, "Target?");
        server.send_line(":irc.srv 716 GM AlphaWolf :callerid\r\n");
        server.settle().await;
    };
    let (huddle_res, ()) = tokio::join!(huddle_fut, drive_huddle);
    assert!(matches!(
        huddle_res,
        Err(Error::UserUnreachable { ref nick, code: 716, .. }) if nick == "AlphaWolf"
    ));
}
