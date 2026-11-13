use super::*;
use crate::testing::test_harness;

#[test]
fn into_verdict_converts_option_and_preserves_verdict() {
    assert_eq!(Some(42).into_verdict(), Verdict::accept(42));
    assert_eq!(None::<i32>.into_verdict(), Verdict::ignore());

    let ack: Verdict<i32> = Verdict::accept_ack(7, "Locked in!");
    assert_eq!(
        ack.into_verdict().0,
        VerdictInner::Accept(7, Some(Text::from("Locked in!")))
    );

    let rej: Verdict<i32> = Verdict::reject("Invalid number");
    assert_eq!(
        rej.into_verdict().0,
        VerdictInner::Reject(Text::from("Invalid number"))
    );
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
