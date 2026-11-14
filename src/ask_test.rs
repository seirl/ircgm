use super::*;
use crate::error::DisconnectReason;
use crate::testing::test_harness;

#[test]
fn into_ask_match_and_answer_conversions() {
    assert_eq!(Some(42).into_ask_match(), AskMatch::accept(42));
    assert_eq!(None::<i32>.into_ask_match(), AskMatch::ignore());

    let ack: AskMatch<i32> = AskMatch::accept_say(7, "Locked in!");
    assert_eq!(
        ack.into_ask_match().0,
        AskMatchInner::Accept(7, Some(Text::from("Locked in!")))
    );

    let rej: AskMatch<i32> = AskMatch::reject("Invalid number");
    assert_eq!(
        rej.into_ask_match().0,
        AskMatchInner::Reject(Text::from("Invalid number"))
    );

    let ok_ans = Answer::from_result(Ok::<u32, Error>(7)).unwrap();
    assert_eq!(ok_ans, Answer::Ok(7));
    assert!(ok_ans.is_ok());
    assert_eq!(ok_ans.as_ok(), Some(&7));
    assert_eq!(ok_ans.clone().ok(), Some(7));
    assert_eq!(ok_ans.clone().unwrap(), 7);
    assert_eq!(ok_ans.required().unwrap(), 7);

    let to_ans = Answer::from_result(Err::<u32, Error>(Error::TimedOut)).unwrap();
    assert!(to_ans.is_timed_out());
    assert_eq!(to_ans.clone().ok(), None);
    assert!(matches!(to_ans.required(), Err(Error::TimedOut)));

    let left_ans = Answer::from_result(Err::<u32, Error>(Error::UserLeft {
        nick: "Bob".into(),
        reason: Some("Quit".into()),
    }))
    .unwrap();
    assert!(left_ans.is_left());
    assert_eq!(left_ans.clone().ok(), None);
    assert!(matches!(left_ans.required(), Err(Error::UserLeft { .. })));

    let unreach_ans = Answer::from_result(Err::<u32, Error>(Error::UserUnreachable {
        nick: "Alice".into(),
        code: 716,
        message: "is in +g mode".into(),
    }))
    .unwrap();
    assert!(unreach_ans.is_unreachable());
    assert_eq!(
        unreach_ans,
        Answer::Unreachable {
            nick: "Alice".into(),
            code: 716,
            message: "is in +g mode".into(),
        }
    );
    assert!(matches!(
        unreach_ans.required(),
        Err(Error::UserUnreachable { code: 716, .. })
    ));

    // Channel and session errors remain `Err`:
    assert!(matches!(
        Answer::from_result(Err::<u32, Error>(Error::ChannelSendFailed {
            channel: "#lobby".into(),
            code: 404,
            message: "Cannot send".into(),
        })),
        Err(Error::ChannelSendFailed { code: 404, .. })
    ));
    assert!(matches!(
        Answer::from_result(Err::<u32, Error>(Error::Disconnected(DisconnectReason::Eof))),
        Err(Error::Disconnected(DisconnectReason::Eof))
    ));
}

#[test]
fn compute_schedule_filters_and_sorts_countdown_warnings() {
    let now = Instant::now();
    let def_to = Duration::from_secs(300);

    // Default `AskConfig::new()` uses `default_timeout`:
    let (def_dl, def_warns) = compute_schedule(&AskConfig::new(), now, def_to);
    assert_eq!(def_dl, Some(now + def_to));
    assert!(def_warns.is_empty());

    // TimeLimit::None produces no deadline and no warnings even if configured:
    let none_cfg = AskConfig::untimed().warn_remaining(Duration::from_secs(10), "10s!");
    let (dl, warns) = compute_schedule(&none_cfg, now, def_to);
    assert!(dl.is_none());
    assert!(warns.is_empty());

    // TimeLimit::Duration filters out rem == 0 and rem >= dur, and sorts chronologically:
    let dur_cfg = AskConfig::timeout(Duration::from_secs(30))
        .warn_remaining(Duration::from_secs(5), "5s left")
        .warn_remaining(Duration::ZERO, "zero ignored")
        .warn_remaining(Duration::from_secs(30), "equal to timeout ignored")
        .warn_remaining(Duration::from_secs(45), "greater than timeout ignored")
        .warn_remaining(Duration::from_secs(20), "20s left");

    let (dl, warns) = compute_schedule(&dur_cfg, now, def_to);
    assert_eq!(dl, Some(now + Duration::from_secs(30)));
    assert_eq!(warns.len(), 2);
    // 20s remaining fires at now + 10s; 5s remaining fires at now + 25s:
    assert_eq!(warns[0].0, now + Duration::from_secs(10));
    assert_eq!(warns[0].1.plain(), "20s left");
    assert_eq!(warns[1].0, now + Duration::from_secs(25));
    assert_eq!(warns[1].1.plain(), "5s left");

    // TimeLimit::Deadline computes budget relative to sent_at:
    let target_dl = now + Duration::from_secs(15);
    let dl_cfg = AskConfig::deadline(target_dl)
        .warn_remaining(Duration::from_secs(20), "exceeds budget")
        .warn_remaining(Duration::from_secs(5), "5s left");
    let (dl, warns) = compute_schedule(&dl_cfg, now, def_to);
    assert_eq!(dl, Some(target_dl));
    assert_eq!(warns.len(), 1);
    assert_eq!(warns[0].0, now + Duration::from_secs(10));
}

#[tokio::test]
async fn timeout_handler_renders_static_and_dynamic_messages() {
    let (bot, _server) = test_harness("gm");
    let alice = bot.user("Alice").unwrap();
    let target = Target::Channel("#blackjack".into());

    let ctx = TimeoutCtx {
        target: &target,
        user: Some(&alice),
    };

    let stat = AskConfig::new().timeout_message("Time's up!");
    assert_eq!(
        stat.timeout_handler.as_ref().unwrap().render(&ctx).plain(),
        "Time's up!"
    );

    let dyn_cfg = AskConfig::new().timeout_message_fn(|c| {
        format!(
            "{} timed out in {:?}",
            c.user.map(User::nick).unwrap_or_default(),
            c.target
        )
    });
    assert_eq!(
        dyn_cfg
            .timeout_handler
            .as_ref()
            .unwrap()
            .render(&ctx)
            .plain(),
        "Alice timed out in Channel(\"#blackjack\")"
    );

    let cleared = dyn_cfg.clear_warnings().no_timeout_message();
    assert!(cleared.timeout_handler.is_none());
}

#[tokio::test]
async fn huddle_ask_ends_when_the_last_reachable_member_quits_after_others_bounce() {
    let (bot, mut server) = test_harness("gm");
    let users: Vec<User> = ["w1", "w2", "w3", "w4"]
        .iter()
        .map(|n| bot.user(n).unwrap())
        .collect();

    // 1. `w1` bounces 716 but stays online, so the huddle inbox stays open. The ask has already
    //    consumed that bounce when `w2` quits, leaving nobody who can answer; previously the
    //    prompt then sat out its 300s reply timeout:
    let mut pack = bot.huddle("Pack", &users[..2]).await.unwrap();
    let ask = pack.ask("Target?", |m| Some(m.text.clone()));
    let drive = async {
        assert_eq!(server.recv_message_to("w1").await, "Target?");
        assert_eq!(server.recv_message_to("w2").await, "Target?");
        server.bounce(716, "w1", "is in +g mode");
        server.settle().await;
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        server.quit_user("w2", Some("bye"));
        server.settle().await;
    };
    let (res, ()) = tokio::join!(tokio::time::timeout(Duration::from_secs(1), ask), drive);
    assert!(matches!(
        res.expect("must not wait out the 300s reply timeout"),
        Ok(Answer::Left { ref nick, ref reason }) if nick == "w2" && reason.as_deref() == Some("bye")
    ));
    drop(pack);

    // 2. Same events, both buffered before the ask reads either: reachability is judged at each
    //    event's wire position, so the bounce (while `w4` was still online) does not end the
    //    prompt; the later `QUIT` does:
    let mut pack = bot.huddle("Pack", &users[2..]).await.unwrap();
    let ask = pack.ask("Target?", |m| Some(m.text.clone()));
    let drive = async {
        assert_eq!(server.recv_message_to("w3").await, "Target?");
        assert_eq!(server.recv_message_to("w4").await, "Target?");
        server.bounce(716, "w3", "is in +g mode");
        server.quit_user("w4", Some("gone"));
        server.settle().await;
    };
    let (res, ()) = tokio::join!(tokio::time::timeout(Duration::from_secs(1), ask), drive);
    assert!(matches!(
        res.expect("must not wait out the 300s reply timeout"),
        Ok(Answer::Left { ref nick, .. }) if nick == "w4"
    ));
}

#[tokio::test]
async fn ask_answers_a_notice_reply_with_notice_rejects_and_acks() {
    use irc::proto::Command;

    let (bot, mut server) = test_harness("gm");
    server.complete_join("#lobby", &["alice"]);
    server.settle().await;
    let alice = bot.user("alice").unwrap();
    let lobby = bot.room("#lobby").unwrap();
    let pick = |m: &ChatMessage| match m.text.as_str() {
        "yes" => AskMatch::accept_say((), "Locked in"),
        _ => AskMatch::reject("Say yes"),
    };

    // DM prompt answered by `NOTICE`s: the reject and the ack are `NOTICE`s too.
    let notice_cfg = AskConfig::new().accept_notices();
    let ask = alice.ask("Ready?", pick).config(&notice_cfg);
    let drive = async {
        assert_eq!(
            server.recv_sent().await,
            Some(Command::PRIVMSG("alice".into(), "Ready?".into()))
        );
        server.notice("alice", "gm", "no");
        server.notice("alice", "gm", "yes");
        server.settle().await;
    };
    let (ans, ()) = tokio::join!(ask, drive);
    assert!(ans.unwrap().is_ok());
    assert_eq!(
        server.recv_sent().await,
        Some(Command::NOTICE("alice".into(), "Say yes".into()))
    );
    assert_eq!(
        server.recv_sent().await,
        Some(Command::NOTICE("alice".into(), "Locked in".into()))
    );

    // Channel prompt: a `NOTICE` answer gets a `NOTICE` ack, a `PRIVMSG` answer a `PRIVMSG` one.
    for (notice, expected) in [
        (true, Command::NOTICE("#lobby".into(), "Locked in".into())),
        (false, Command::PRIVMSG("#lobby".into(), "Locked in".into())),
    ] {
        let ask = lobby.ask("Ready?", pick).config(&notice_cfg);
        let drive = async {
            assert_eq!(
                server.recv_sent().await,
                Some(Command::PRIVMSG("#lobby".into(), "Ready?".into()))
            );
            if notice {
                server.notice("alice", "#lobby", "yes");
            } else {
                server.chat("alice", "#lobby", "yes");
            }
            server.settle().await;
        };
        let (ans, ()) = tokio::join!(ask, drive);
        assert!(ans.unwrap().is_ok());
        assert_eq!(server.recv_sent().await, Some(expected));
    }
}

#[tokio::test(start_paused = true)]
async fn bot_config_default_reply_timeout_applies_to_unconfigured_prompts() {
    let cfg = crate::BotConfig {
        default_reply_timeout: Duration::from_secs(7),
        ..crate::BotConfig::default()
    };
    let (bot, mut server) = crate::testing::test_harness_with("gm", cfg);
    server.complete_join("#lobby", &["alice"]);
    server.settle().await;
    let room = bot.room("#lobby").unwrap();

    let ask_task = tokio::spawn({
        let room = room.clone();
        async move { room.ask("Ping?", |m| Some(m.text.clone())).await }
    });
    assert_eq!(server.recv_message_to("#lobby").await, "Ping?");
    server.advance(Duration::from_secs(6)).await;
    assert!(!ask_task.is_finished());
    server.advance(Duration::from_secs(1)).await;
    assert_eq!(ask_task.await.unwrap().unwrap(), Answer::TimedOut);
}

#[tokio::test(start_paused = true)]
async fn ask_many_from_users_locks_in_deduplicates_warns_and_finishes_early_on_departure() {
    let (bot, mut server) = test_harness("gm");
    server.complete_join("#town", &["alice", "bob", "carol", "spectator"]);
    server.settle().await;

    let room = bot.room("#town").unwrap();
    let alice = bot.user("alice").unwrap();
    let bob = bot.user("bob").unwrap();
    let carol = bot.user("carol").unwrap();

    let poll_task = tokio::spawn({
        let room = room.clone();
        let voters = vec![alice.clone(), bob.clone(), alice.clone(), carol.clone()];
        async move {
            room.ask_many("Type `!ready` (20s):", |m| {
                if m.is_cmd("!ready") {
                    AskMatch::accept_say(m.text.clone(), format!("{} ready!", m.nick))
                } else if m.is_cmd("!unready") {
                    AskMatch::reject("Cannot unready once started!")
                } else {
                    AskMatch::ignore()
                }
            })
            .from_users(&voters)
            .config(
                AskConfig::timeout(Duration::from_secs(20))
                    .warn_remaining(Duration::from_secs(5), "5s left!")
                    .timeout_message("Someone was too slow!"),
            )
            .await
        }
    });

    assert_eq!(
        server.recv_message_to("#town").await,
        "Type `!ready` (20s):"
    );

    // Spectator ignored; Alice gets rejected on `!unready`, then locks in `!ready`, and her second
    // `!ready` is ignored once locked in:
    server.chat("spectator", "#town", "!ready");
    server.chat("alice", "#town", "!unready");
    server.chat("alice", "#town", "!ready");
    server.chat("alice", "#town", "!ready");
    server.chat("bob", "#town", "!ready");
    server.settle().await;
    assert_eq!(
        server.take_messages_to("#town"),
        vec![
            "Cannot unready once started!",
            "alice ready!",
            "bob ready!",
        ]
    );

    // Advance 15s -> 5s countdown warning fires while waiting only on Carol:
    server.advance(Duration::from_secs(15)).await;
    assert_eq!(server.take_messages_to("#town"), vec!["5s left!"]);
    assert!(!poll_task.is_finished());

    // Carol parts with 5s still on the clock -> finishes immediately without timeout message!
    server.part_user("carol", "#town", Some("gtg"));
    server.settle().await;

    let answers = poll_task.await.unwrap().unwrap();
    assert_eq!(answers.len(), 2);
    assert_eq!(answers.get(&alice), Some(&"!ready".to_owned()));
    let per_user = answers.into_per_user();
    assert_eq!(per_user.len(), 3);
    assert_eq!(per_user[0], (alice, Answer::Ok("!ready".into())));
    assert_eq!(per_user[1], (bob, Answer::Ok("!ready".into())));
    assert_eq!(per_user[2].0, carol);
    assert!(matches!(
        &per_user[2].1,
        Answer::Left { nick, reason } if nick == "carol" && reason.as_deref() == Some("gtg")
    ));
    assert!(server.take_messages_to("#town").is_empty());
}

#[tokio::test(start_paused = true)]
async fn ask_many_allow_change_and_open_newcomer_recruitment_with_stop_when() {
    let (bot, mut server) = test_harness("gm");
    server.complete_join("#town", &["alice", "bob", "carol"]);
    server.settle().await;

    let room = bot.room("#town").unwrap();
    let alice = bot.user("alice").unwrap();
    let bob = bot.user("bob").unwrap();
    let carol = bot.user("carol").unwrap();

    // 1. `allow_change(true)` without a timeout or `stop_when`/`max_answers` is rejected up front:
    let no_timer_err = room
        .ask_many("Vote:", |_| AskMatch::accept(()))
        .from_users([&alice])
        .config(AskConfig::untimed())
        .allow_change(true)
        .await
        .unwrap_err();
    assert!(matches!(no_timer_err, Error::InvalidInput(_)));

    // 2. Closed-roster revocable ballot until deadline:
    let poll_task = tokio::spawn({
        let room = room.clone();
        let voters = vec![alice.clone(), bob.clone(), carol.clone()];
        async move {
            room.ask_many("Cast `!vote <target>` (10s):", |m| {
                let Some(arg) = m.cmd_arg("!vote") else {
                    return AskMatch::ignore();
                };
                if arg == "self" {
                    AskMatch::reject("No self-votes!")
                } else {
                    AskMatch::accept_say(arg.to_owned(), format!("Recorded {arg}"))
                }
            })
            .from_users(&voters)
            .allow_change(true)
            .config(
                AskConfig::timeout(Duration::from_secs(10)).timeout_message_fn(|ctx| {
                    match ctx.user {
                        Some(u) => Text::from(format!("{} didn't vote!", u.nick())),
                        None => Text::from("Multiple non-voters!"),
                    }
                }),
            )
            .await
        }
    });

    assert_eq!(
        server.recv_message_to("#town").await,
        "Cast `!vote <target>` (10s):"
    );
    server.chat("alice", "#town", "!vote bob");
    server.dm("alice", "!vote ignored_dm");
    server.chat("alice", "#town", "!vote self");
    server.chat("alice", "#town", "!vote carol");
    server.chat("bob", "#town", "!vote carol");
    server.settle().await;

    assert_eq!(
        server.take_messages_to("#town"),
        vec![
            "Recorded bob",
            "No self-votes!",
            "Recorded carol",
            "Recorded carol",
        ]
    );
    assert!(!poll_task.is_finished());

    server.advance(Duration::from_secs(10)).await;
    let per_user = poll_task.await.unwrap().unwrap().into_per_user();
    assert_eq!(per_user[0], (alice.clone(), Answer::Ok("carol".into())));
    assert_eq!(per_user[1], (bob.clone(), Answer::Ok("carol".into())));
    assert_eq!(per_user[2], (carol.clone(), Answer::TimedOut));
    assert_eq!(server.take_messages_to("#town"), vec!["carol didn't vote!"]);

    // 3. Open `room.ask_many` welcomes newcomers joining mid-prompt, handles `!join` / `!leave`
    // via `bool` + `allow_change(true)`, evicts `PART`ed users, and stops on `stop_when`:
    let lobby_task = tokio::spawn({
        let room = room.clone();
        async move {
            room.ask_many("Type `!join` / `!leave` (need 2):", |m| {
                if m.is_cmd("!join") {
                    Some(true)
                } else if m.is_cmd("!leave") {
                    Some(false)
                } else {
                    None
                }
            })
            .allow_change(true)
            .config(AskConfig::untimed())
            .stop_when(|a| a.values().filter(|&&yes| yes).count() >= 2)
            .await
        }
    });

    assert_eq!(
        server.recv_message_to("#town").await,
        "Type `!join` / `!leave` (need 2):"
    );
    // Alice joins then `!leave`s (`false`); Bob joins then `PART`s (evicted); Carol joins, and
    // newcomer Dave `/join`s mid-prompt and types `!join` -> hits 2 active `true`s immediately!
    server.chat("alice", "#town", "!join");
    server.chat("alice", "#town", "!leave");
    server.chat("bob", "#town", "!join");
    server.part_user("bob", "#town", None);
    server.chat("carol", "#town", "!join");
    server.join_user("dave", "#town");
    server.settle().await;
    assert!(!lobby_task.is_finished());

    server.chat("dave", "#town", "!join");
    server.settle().await;
    let signups = lobby_task.await.unwrap().unwrap();
    let dave = bot.user("dave").unwrap();
    let joined: Vec<User> = signups
        .into_iter()
        .filter_map(|(u, yes)| yes.then_some(u))
        .collect();
    assert_eq!(joined, vec![carol, dave]);
}
