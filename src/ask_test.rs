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
    assert_eq!(ok_ans.into_result().unwrap(), 7);

    let to_ans = Answer::from_result(Err::<u32, Error>(Error::TimedOut)).unwrap();
    assert!(to_ans.is_timed_out());
    assert_eq!(to_ans.clone().ok(), None);
    assert!(matches!(to_ans.into_result(), Err(Error::TimedOut)));

    let left_ans = Answer::from_result(Err::<u32, Error>(Error::UserLeft {
        nick: "Bob".into(),
        reason: Some("Quit".into()),
    }))
    .unwrap();
    assert!(left_ans.is_left());
    assert_eq!(left_ans.clone().ok(), None);
    assert!(matches!(left_ans.into_result(), Err(Error::UserLeft { .. })));

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
        unreach_ans.into_result(),
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
