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
#[path = "supervise_test.rs"]
mod test;
