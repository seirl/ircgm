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

    // TimeLimit::None produces no deadline and no warnings even if configured:
    let none_cfg = AskConfig::new()
        .no_reply_timeout()
        .warn_remaining(Duration::from_secs(10), "10s!");
    let (cfg_dur, dl, warns) = compute_schedule(&none_cfg, now);
    assert!(cfg_dur.is_none());
    assert!(dl.is_none());
    assert!(warns.is_empty());

    // TimeLimit::Duration filters out rem == 0 and rem >= dur, and sorts chronologically:
    let dur_cfg = AskConfig::new()
        .reply_timeout(Duration::from_secs(30))
        .warn_remaining(Duration::from_secs(5), "5s left")
        .warn_remaining(Duration::ZERO, "zero ignored")
        .warn_remaining(Duration::from_secs(30), "equal to timeout ignored")
        .warn_remaining(Duration::from_secs(45), "greater than timeout ignored")
        .warn_remaining(Duration::from_secs(20), "20s left");

    let (cfg_dur, dl, warns) = compute_schedule(&dur_cfg, now);
    assert_eq!(cfg_dur, Some(Duration::from_secs(30)));
    assert_eq!(dl, Some(now + Duration::from_secs(30)));
    assert_eq!(warns.len(), 2);
    // 20s remaining fires at now + 10s; 5s remaining fires at now + 25s:
    assert_eq!(warns[0].0, now + Duration::from_secs(10));
    assert_eq!(warns[0].1.plain(), "20s left");
    assert_eq!(warns[1].0, now + Duration::from_secs(25));
    assert_eq!(warns[1].1.plain(), "5s left");

    // TimeLimit::Deadline computes budget relative to write_completed_at:
    let target_dl = now + Duration::from_secs(15);
    let dl_cfg = AskConfig::new()
        .reply_deadline(target_dl)
        .warn_remaining(Duration::from_secs(20), "exceeds budget")
        .warn_remaining(Duration::from_secs(5), "5s left");
    let (cfg_dur, dl, warns) = compute_schedule(&dl_cfg, now);
    assert!(cfg_dur.is_none());
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
        configured_timeout: Some(Duration::from_secs(20)),
        elapsed: Duration::from_secs(20),
    };

    let stat = AskConfig::new().timeout_message("Time's up!");
    assert_eq!(
        stat.timeout_handler.as_ref().unwrap().render(&ctx).plain(),
        "Time's up!"
    );

    let dyn_cfg = AskConfig::new().timeout_message_fn(|c| {
        format!(
            "{} timed out after {}s",
            c.user.map(User::nick).unwrap_or_default(),
            c.configured_timeout.unwrap_or_default().as_secs()
        )
    });
    assert_eq!(
        dyn_cfg
            .timeout_handler
            .as_ref()
            .unwrap()
            .render(&ctx)
            .plain(),
        "Alice timed out after 20s"
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
    let mut pack = bot.huddle(&users[..2]).await.unwrap();
    let ask = pack.ask("Target?", |m| Some(m.text.clone()));
    let drive = async {
        assert_eq!(server.recv_message_to("w1").await, "Target?");
        assert_eq!(server.recv_message_to("w2").await, "Target?");
        server.send_line(":srv 716 gm w1 :is in +g mode\r\n");
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
    let mut pack = bot.huddle(&users[2..]).await.unwrap();
    let ask = pack.ask("Target?", |m| Some(m.text.clone()));
    let drive = async {
        assert_eq!(server.recv_message_to("w3").await, "Target?");
        assert_eq!(server.recv_message_to("w4").await, "Target?");
        server.send_line(":srv 716 gm w3 :is in +g mode\r\n");
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
    let ask = alice.ask("Ready?", pick).accept_notices();
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
        let ask = lobby.ask("Ready?", pick).accept_notices();
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
