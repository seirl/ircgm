use super::*;
use crate::error::DisconnectReason;
use std::panic::Location;

#[tokio::test]
async fn until_or_until_and_map_output() {
    let res = (async { 42 })
        .until((async { 1 }).map_output(|n| format!("s{n}")))
        .or_until(async { "s2".to_owned() })
        .or_until(async { "s3".to_owned() })
        .await;
    assert_eq!(res, Outcome::Stopped("s1".to_owned()));

    let res2 = (async { 42 })
        .until(std::future::pending::<&str>())
        .or_until(async { "s2" })
        .or_until(async { "s3" })
        .await;
    assert_eq!(res2, Outcome::Stopped("s2"));

    let res3 = (async { 42 })
        .until(std::future::pending::<&str>())
        .or_until(std::future::pending::<&str>())
        .await;
    assert_eq!(res3, Outcome::Completed(42));
}

#[tokio::test]
async fn ok_or_pending_parks_on_err() {
    let failing_stop = async {
        Err::<&str, Error>(Error::UserLeft {
            nick: "Alice".into(),
            reason: None,
        })
    }
    .ok_or_pending();

    let out = (async {
        tokio::task::yield_now().await;
        "game_done"
    })
    .until(failing_stop)
    .await;
    assert_eq!(out, Outcome::Completed("game_done"));
}

#[tokio::test]
async fn empty_race_all_stays_pending_without_panicking() {
    let empty: Vec<std::future::Ready<u32>> = Vec::new();
    let out = (async { "ok" }).until(race_all(empty)).await;
    assert_eq!(out, Outcome::Completed("ok"));

    let ready_race = race_all([std::future::ready(10), std::future::ready(20)]);
    assert_eq!(ready_race.await, 10);
}

#[tokio::test(start_paused = true)]
async fn virtual_clock_with_timeout_and_with_deadline() {
    let slow = async {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok::<_, Error>("done")
    };
    let err = slow.with_timeout(Duration::from_secs(10)).await.unwrap_err();
    assert!(matches!(err, Error::TimedOut));

    let deadline = Instant::now() + Duration::from_secs(15);
    let fast = async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        Ok::<_, Error>(99)
    };
    assert_eq!(fast.with_deadline(deadline).await.unwrap(), 99);

    let too_slow = async {
        tokio::time::sleep(Duration::from_secs(20)).await;
        Ok::<_, Error>(99)
    };
    assert!(matches!(
        too_slow.with_deadline(deadline).await.unwrap_err(),
        Error::TimedOut
    ));
}

#[tokio::test]
async fn optional_and_settled_mappings() {
    let t_out = async { Err::<u32, Error>(Error::TimedOut) }.optional().await;
    assert_eq!(t_out.unwrap(), None);

    let u_left = async {
        Err::<u32, Error>(Error::UserLeft {
            nick: "Bob".into(),
            reason: Some("Quit".into()),
        })
    }
    .optional()
    .await;
    assert!(matches!(u_left, Err(Error::UserLeft { .. })));

    let s_ok = async { Ok::<u32, Error>(7) }.settled().await.unwrap();
    assert_eq!(s_ok, Turn::Reply(7));
    assert_eq!(s_ok.as_reply(), Some(&7));
    assert_eq!(s_ok.into_reply(), Some(7));

    let s_to = async { Err::<u32, Error>(Error::TimedOut) }
        .settled()
        .await
        .unwrap();
    assert!(s_to.is_timed_out());
    assert_eq!(s_to.into_reply(), None);

    let s_left = async {
        Err::<u32, Error>(Error::UserLeft {
            nick: "Bob".into(),
            reason: None,
        })
    }
    .settled()
    .await
    .unwrap();
    assert!(s_left.is_left());

    let s_unreach = async {
        Err::<u32, Error>(Error::DeliveryFailed {
            target: "Alice".into(),
            code: 716,
            message: "is in +g mode".into(),
        })
    }
    .settled()
    .await
    .unwrap();
    assert!(s_unreach.is_unreachable());
    assert_eq!(
        s_unreach,
        Turn::Unreachable {
            target: "Alice".into(),
            code: 716,
            message: "is in +g mode".into(),
        }
    );

    let res_disc = async { Err::<u32, Error>(Error::Disconnected(DisconnectReason::Eof)) }
        .settled()
        .await;
    assert!(matches!(
        res_disc,
        Err(Error::Disconnected(DisconnectReason::Eof))
    ));

    let err_lag = Error::Lagged {
        scope: "user Alice".into(),
        skipped: 5,
        opened_at: Location::caller(),
    };
    let res_lag = async { Err::<u32, Error>(err_lag) }.settled().await;
    assert!(matches!(res_lag, Err(Error::Lagged { skipped: 5, .. })));
}

#[tokio::test(start_paused = true)]
async fn run_for_and_run_until_deadline_infer_diverging_and_early_exit_loops() {
    // 1. Diverging `loop { ... }` compiles with zero `Infallible` or `Ok::<_, Error>` annotations!
    let mut ticks = 0usize;
    async {
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            ticks += 1;
        }
    }
    .run_for(Duration::from_secs(10))
    .await
    .unwrap();
    assert_eq!(ticks, 3);

    // 2. Early-exiting block infers plain `Ok(())` without turbofish:
    let deadline = Instant::now() + Duration::from_secs(20);
    async {
        tokio::time::sleep(Duration::from_secs(4)).await;
        Ok(())
    }
    .run_until_deadline(deadline)
    .await
    .unwrap();
    assert!(Instant::now() < deadline);

    // 3. Fatal errors still propagate instead of being swallowed:
    let fatal = async { Err(Error::Disconnected(DisconnectReason::Eof)) }
        .run_for(Duration::from_secs(5))
        .await;
    assert!(matches!(
        fatal,
        Err(Error::Disconnected(DisconnectReason::Eof))
    ));
}

#[tokio::test]
async fn until_ok_or_until_ok_and_map_ok_ignore_err_and_trigger_on_ok() {
    let failing = async {
        Err::<&str, Error>(Error::UserLeft {
            nick: "Alice".into(),
            reason: None,
        })
    };
    let succeeding = async { Ok::<u32, Error>(7) }.map_ok(|n| if n == 7 { "stopped" } else { "?" });

    let out = (async {
        tokio::task::yield_now().await;
        "completed"
    })
    .until_ok(failing)
    .or_until_ok(succeeding)
    .await;
    assert_eq!(out, Outcome::Stopped("stopped"));
}
