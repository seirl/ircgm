use super::*;
use crate::error::DisconnectReason;

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

    // 4. Inner `Err(Error::TimedOut)` propagates verbatim instead of being mistaken for outer phase expiry!
    let inner_timeout = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        Err::<(), Error>(Error::TimedOut)
    }
    .run_for(Duration::from_secs(10))
    .await;
    assert!(matches!(inner_timeout, Err(Error::TimedOut)));

    let inner_deadline_timeout = async { Err::<(), Error>(Error::TimedOut) }
        .run_until_deadline(Instant::now() + Duration::from_secs(10))
        .await;
    assert!(matches!(inner_deadline_timeout, Err(Error::TimedOut)));
}
