use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::{Sleep, Instant};

use crate::error::{Error, Result};

/// Outcome of racing a game future against one or more stop triggers via [`GameFutureExt::until`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome<T, S> {
    Completed(T),
    Stopped(S),
}

/// Per-player outcome for multi-player turns, races, and votes where a single player timing out,
/// leaving, or blocking DMs (`+g` / `+R`) should be handled locally without aborting the whole game.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Turn<T> {
    Reply(T),
    TimedOut,
    Left {
        nick: String,
        reason: Option<String>,
    },
    Unreachable {
        target: String,
        code: u16,
        message: String,
    },
}

impl<T> Turn<T> {
    /// Returns `Some(val)` on `Reply(val)`, or `None` on `TimedOut | Left { .. } | Unreachable { .. }`.
    pub fn into_reply(self) -> Option<T> {
        match self {
            Turn::Reply(val) => Some(val),
            Turn::TimedOut | Turn::Left { .. } | Turn::Unreachable { .. } => None,
        }
    }

    pub fn as_reply(&self) -> Option<&T> {
        match self {
            Turn::Reply(val) => Some(val),
            Turn::TimedOut | Turn::Left { .. } | Turn::Unreachable { .. } => None,
        }
    }

    pub fn is_timed_out(&self) -> bool {
        matches!(self, Turn::TimedOut)
    }

    pub fn is_left(&self) -> bool {
        matches!(self, Turn::Left { .. })
    }

    pub fn is_unreachable(&self) -> bool {
        matches!(self, Turn::Unreachable { .. })
    }
}

/// Races a runtime-sized `0..=N` collection of futures yielding `F::Output` in registration order.
///
/// Unlike `futures::future::select_all`, an empty iterator **never panics**—it stays
/// `Poll::Pending` forever, making it safe to pass a dynamic `Vec` of `0..=N` stop triggers
/// into `.until(race_all(triggers))`.
pub fn race_all<I, F>(futures: I) -> RaceAll<F>
where
    I: IntoIterator<Item = F>,
    F: Future,
{
    RaceAll {
        futures: futures.into_iter().map(Box::pin).collect(),
    }
}

/// Future returned by [`race_all`].
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct RaceAll<F> {
    futures: Vec<Pin<Box<F>>>,
}

impl<F: Future> Future for RaceAll<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        for fut in &mut self.futures {
            if let Poll::Ready(out) = fut.as_mut().poll(cx) {
                return Poll::Ready(out);
            }
        }
        Poll::Pending
    }
}

/// Future returned by [`Until::or_until`], polling `first` before `second` (`biased` left-to-right).
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct OrUntil<S1, S2> {
    first: Pin<Box<S1>>,
    second: Pin<Box<S2>>,
}

impl<S1, S2> Future for OrUntil<S1, S2>
where
    S1: Future,
    S2: Future<Output = S1::Output>,
{
    type Output = S1::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Poll::Ready(out) = self.first.as_mut().poll(cx) {
            return Poll::Ready(out);
        }
        self.second.as_mut().poll(cx)
    }
}

/// Future returned by [`GameFutureExt::until`]. Polls `stop` before `game` (`biased`).
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Until<G, S> {
    game: Pin<Box<G>>,
    stop: Pin<Box<S>>,
}

impl<G: Future, S: Future> Until<G, S> {
    /// Chains another stop condition producing the same stop output `S::Output` (`biased`:
    /// earlier stop conditions are polled before later ones, and all stop conditions take
    /// priority over `G`).
    pub fn or_until<S2>(self, next_stop: S2) -> Until<G, OrUntil<S, S2>>
    where
        S2: Future<Output = S::Output>,
    {
        Until {
            game: self.game,
            stop: Box::pin(OrUntil {
                first: self.stop,
                second: Box::pin(next_stop),
            }),
        }
    }
}

impl<G: Future, S: Future> Future for Until<G, S> {
    type Output = Outcome<G::Output, S::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Biased: always poll `stop` before `game` so stop signals win ties.
        if let Poll::Ready(stop_val) = self.stop.as_mut().poll(cx) {
            return Poll::Ready(Outcome::Stopped(stop_val));
        }
        if let Poll::Ready(game_val) = self.game.as_mut().poll(cx) {
            return Poll::Ready(Outcome::Completed(game_val));
        }
        Poll::Pending
    }
}

/// Future returned by [`GameFutureExt::map_output`].
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct MapOutput<F, M> {
    fut: Pin<Box<F>>,
    mapper: Option<M>,
}

impl<F, U, M> Future for MapOutput<F, M>
where
    F: Future,
    M: FnOnce(F::Output) -> U + Unpin,
{
    type Output = U;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        match this.fut.as_mut().poll(cx) {
            Poll::Ready(val) => {
                let mapper = this
                    .mapper
                    .take()
                    .expect("MapOutput polled after completion");
                Poll::Ready(mapper(val))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Future returned by [`GameFutureExt::ok_or_pending`].
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct OkOrPending<F> {
    fut: Option<Pin<Box<F>>>,
}

impl<F, T> Future for OkOrPending<F>
where
    F: Future<Output = Result<T>>,
{
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(fut) = self.fut.as_mut() else {
            return Poll::Pending;
        };
        match fut.as_mut().poll(cx) {
            Poll::Ready(Ok(val)) => {
                self.fut = None;
                Poll::Ready(val)
            }
            Poll::Ready(Err(_)) => {
                // Park forever on error so a disconnected stop listener never triggers false stop.
                self.fut = None;
                Poll::Pending
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Future returned by [`GameFutureExt::with_timeout`].
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct WithTimeout<F> {
    fut: Pin<Box<F>>,
    duration: Duration,
    sleep: Option<Pin<Box<Sleep>>>,
}

impl<F, T> Future for WithTimeout<F>
where
    F: Future<Output = Result<T>>,
{
    type Output = Result<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let sleep = this
            .sleep
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(this.duration)));

        if let Poll::Ready(res) = this.fut.as_mut().poll(cx) {
            return Poll::Ready(res);
        }
        if sleep.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(Error::TimedOut));
        }
        Poll::Pending
    }
}

/// Future returned by [`GameFutureExt::with_deadline`].
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct WithDeadline<F> {
    fut: Pin<Box<F>>,
    deadline: Instant,
    sleep: Option<Pin<Box<Sleep>>>,
}

impl<F, T> Future for WithDeadline<F>
where
    F: Future<Output = Result<T>>,
{
    type Output = Result<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let sleep = this
            .sleep
            .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(this.deadline)));

        if let Poll::Ready(res) = this.fut.as_mut().poll(cx) {
            return Poll::Ready(res);
        }
        if sleep.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(Error::TimedOut));
        }
        Poll::Pending
    }
}

/// Future returned by [`GameFutureExt::optional`].
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Optional<F> {
    fut: Pin<Box<F>>,
}

impl<F, T> Future for Optional<F>
where
    F: Future<Output = Result<T>>,
{
    type Output = Result<Option<T>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.fut.as_mut().poll(cx) {
            Poll::Ready(Ok(val)) => Poll::Ready(Ok(Some(val))),
            Poll::Ready(Err(Error::TimedOut)) => Poll::Ready(Ok(None)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Future returned by [`GameFutureExt::settled`].
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Settled<F> {
    fut: Pin<Box<F>>,
}

impl<F, T> Future for Settled<F>
where
    F: Future<Output = Result<T>>,
{
    type Output = Result<Turn<T>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.fut.as_mut().poll(cx) {
            Poll::Ready(Ok(val)) => Poll::Ready(Ok(Turn::Reply(val))),
            Poll::Ready(Err(Error::TimedOut)) => Poll::Ready(Ok(Turn::TimedOut)),
            Poll::Ready(Err(Error::UserLeft { nick, reason })) => {
                Poll::Ready(Ok(Turn::Left { nick, reason }))
            }
            Poll::Ready(Err(Error::DeliveryFailed {
                target,
                code,
                message,
            })) => Poll::Ready(Ok(Turn::Unreachable {
                target,
                code,
                message,
            })),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Supervision and pure result-mapping combinators for game futures.
///
/// Named `GameFutureExt` so importing it never collides with `futures::FutureExt`.
/// All timeout and result-mapping combinators (`with_timeout`, `with_deadline`, `optional`,
/// `settled`) are **pure wrappers** that never alter `Ask` messaging.
pub trait GameFutureExt: Future + Sized {
    /// Races `self` against `stop` using `biased` selection (`stop` wins ties); chain
    /// `.or_until(...)` on the returned [`Until`] to attach arbitrarily many stop triggers.
    fn until<S: Future>(self, stop: S) -> Until<Self, S> {
        Until {
            game: Box::pin(self),
            stop: Box::pin(stop),
        }
    }

    /// Lightweight output mapper (`Future<Output = A> -> Future<Output = U>`), handy for tagging
    /// heterogeneous stop futures into a flat stop enum.
    fn map_output<U, F: FnOnce(Self::Output) -> U + Unpin>(self, f: F) -> MapOutput<Self, F> {
        MapOutput {
            fut: Box::pin(self),
            mapper: Some(f),
        }
    }

    /// Converts a `Future<Output = Result<T>>` into a stop-trigger future yielding `T` on
    /// `Ok(val)` and parking `Poll::Pending` on `Err(_)`.
    fn ok_or_pending<T>(self) -> OkOrPending<Self>
    where
        Self: Future<Output = Result<T>>,
    {
        OkOrPending {
            fut: Some(Box::pin(self)),
        }
    }

    /// Outer wall-clock timeout: races any `Future<Output = Result<T>>` against `duration`,
    /// cancelling the inner future and returning `Err(Error::TimedOut)` on expiry.
    fn with_timeout<T>(self, duration: Duration) -> WithTimeout<Self>
    where
        Self: Future<Output = Result<T>>,
    {
        WithTimeout {
            fut: Box::pin(self),
            duration,
            sleep: None,
        }
    }

    /// Outer absolute-deadline timeout: enforces a fixed phase end-time across loops.
    fn with_deadline<T>(self, deadline: Instant) -> WithDeadline<Self>
    where
        Self: Future<Output = Result<T>>,
    {
        WithDeadline {
            fut: Box::pin(self),
            deadline,
            sleep: None,
        }
    }

    /// Pure result mapper converting `Err(Error::TimedOut)` to `Ok(None)` while preserving all
    /// other `Err(e)` variants for `?`.
    fn optional<T>(self) -> Optional<Self>
    where
        Self: Future<Output = Result<T>>,
    {
        Optional {
            fut: Box::pin(self),
        }
    }

    /// Pure result mapper converting `Err(Error::TimedOut)`, `Err(Error::UserLeft { .. })`, and
    /// `Err(Error::DeliveryFailed { .. })` into `Ok(Turn<T>)` while preserving fatal errors as `Err`.
    fn settled<T>(self) -> Settled<Self>
    where
        Self: Future<Output = Result<T>>,
    {
        Settled {
            fut: Box::pin(self),
        }
    }
}

impl<F: Future> GameFutureExt for F {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DisconnectReason;
    use std::panic::Location;

    #[tokio::test]
    async fn until_and_or_until_biased_tie_breaking() {
        // All 3 stop futures and the game future are immediately ready:
        // stop #1 must win over stop #2, stop #3, and game!
        let res = (async { 42 })
            .until(async { "s1" })
            .or_until(async { "s2" })
            .or_until(async { "s3" })
            .await;
        assert_eq!(res, Outcome::Stopped("s1"));

        // Stop #1 pending, stop #2 and #3 ready -> stop #2 wins!
        let res2 = (async { 42 })
            .until(std::future::pending::<&str>())
            .or_until(async { "s2" })
            .or_until(async { "s3" })
            .await;
        assert_eq!(res2, Outcome::Stopped("s2"));

        // All stops pending -> game completes!
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
    async fn optional_and_settled_mappings_and_is_fatal() {
        // optional: TimedOut -> Ok(None), UserLeft -> Err
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

        // settled: Reply, TimedOut, UserLeft, DeliveryFailed -> Ok(Turn::*)
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

        let err_left = Error::UserLeft {
            nick: "Bob".into(),
            reason: None,
        };
        assert!(!err_left.is_fatal());
        let s_left = async { Err::<u32, Error>(err_left) }.settled().await.unwrap();
        assert!(s_left.is_left());

        let err_unreach = Error::DeliveryFailed {
            target: "Alice".into(),
            code: 716,
            message: "is in +g mode".into(),
        };
        assert!(!err_unreach.is_fatal());
        let s_unreach = async { Err::<u32, Error>(err_unreach) }
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

        // Fatal errors propagate as Err:
        let err_disc = Error::Disconnected(DisconnectReason::Eof);
        assert!(err_disc.is_fatal());
        let res_disc = async { Err::<u32, Error>(err_disc) }.settled().await;
        assert!(matches!(
            res_disc,
            Err(Error::Disconnected(DisconnectReason::Eof))
        ));

        let loc = Location::caller();
        let err_lag = Error::Lagged {
            scope: "user Alice".into(),
            skipped: 5,
            opened_at: loc,
        };
        assert!(err_lag.is_fatal());
        let msg = err_lag.to_string();
        assert!(msg.contains("user Alice"));
        assert!(msg.contains("5 messages"));
        assert!(msg.contains(&loc.to_string()));
        assert!(msg.contains("Did you leave an Inbox open across an .await? Use .clear() or .lossy() if intended."));

        let res_lag = async { Err::<u32, Error>(err_lag) }.settled().await;
        assert!(matches!(res_lag, Err(Error::Lagged { skipped: 5, .. })));
    }
}
