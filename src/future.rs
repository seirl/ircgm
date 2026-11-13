//! Cancellation, supervision, and timer combinators for `ircgm` futures.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::{Instant, Sleep};

use crate::error::{Error, Result};

/// Outcome of racing a game future against one or more stop triggers via [`GameFutureExt::until`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome<T, S> {
    Completed(T),
    Stopped(S),
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

/// Safely computes `base + duration`, saturating far in the future instead of panicking on huge
/// durations like `Duration::MAX` (matching `tokio::time::sleep`'s far-future saturation).
pub(crate) fn deadline_after_from(base: Instant, duration: Duration) -> Instant {
    const FAR_FUTURE: Duration = Duration::from_secs(86_400 * 365 * 30);
    base.checked_add(duration)
        .unwrap_or_else(|| base + FAR_FUTURE)
}

pub(crate) fn deadline_after(duration: Duration) -> Instant {
    deadline_after_from(Instant::now(), duration)
}

struct Timed<F> {
    fut: Pin<Box<F>>,
    deadline: Instant,
    sleep: Option<Pin<Box<Sleep>>>,
}

impl<F: Future> Timed<F> {
    fn new(fut: F, deadline: Instant) -> Self {
        Self {
            fut: Box::pin(fut),
            deadline,
            sleep: None,
        }
    }

    /// Polls `self.fut` first; if pending, polls the deadline timer (`Ok(val)` from `fut`, `Err(())` on expiry).
    fn poll_timed(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<F::Output, ()>> {
        let deadline = self.deadline;
        let sleep = self
            .sleep
            .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(deadline)));

        if let Poll::Ready(res) = self.fut.as_mut().poll(cx) {
            return Poll::Ready(Ok(res));
        }
        if sleep.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(()));
        }
        Poll::Pending
    }
}

/// Future returned by [`GameFutureExt::with_timeout`] and [`GameFutureExt::with_deadline`].
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct WithTimeout<F> {
    inner: Timed<F>,
}

impl<F, T> Future for WithTimeout<F>
where
    F: Future<Output = Result<T>>,
{
    type Output = Result<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.inner.poll_timed(cx) {
            Poll::Ready(Ok(res)) => Poll::Ready(res),
            Poll::Ready(Err(())) => Poll::Ready(Err(Error::TimedOut)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Future returned by [`GameFutureExt::run_for`] and [`GameFutureExt::run_until_deadline`].
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct RunFor<F> {
    inner: Timed<F>,
}

impl<F> Future for RunFor<F>
where
    F: Future<Output = Result<()>>,
{
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.inner.poll_timed(cx) {
            Poll::Ready(Ok(res)) => Poll::Ready(res),
            Poll::Ready(Err(())) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Cancellation and timer combinators for game futures.
///
/// Named `GameFutureExt` so importing it never collides with `futures::FutureExt`.
/// All timer combinators (`with_timeout`, `with_deadline`, `run_for`, `run_until_deadline`)
/// are **pure wrappers** that never alter `Ask` messaging.
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

    /// Outer wall-clock timeout: races any `Future<Output = Result<T>>` against `duration`,
    /// cancelling the inner future and returning `Err(Error::TimedOut)` on expiry.
    fn with_timeout<T>(self, duration: Duration) -> WithTimeout<Self>
    where
        Self: Future<Output = Result<T>>,
    {
        self.with_deadline(deadline_after(duration))
    }

    /// Outer absolute-deadline timeout: enforces a fixed phase end-time across loops.
    fn with_deadline<T>(self, deadline: Instant) -> WithTimeout<Self>
    where
        Self: Future<Output = Result<T>>,
    {
        WithTimeout {
            inner: Timed::new(self, deadline),
        }
    }

    /// Runs a `Result<()>` collection/phase loop for at most `duration`, returning `Ok(())` both
    /// on early `Ok(())` completion and when `duration` elapses (while propagating all `Err`s from
    /// the inner future verbatim, including unhandled inner `Err(Error::TimedOut)`).
    /// Fixing `Output = Result<()>` lets diverging `loop { ... }` blocks compile with zero
    /// `Infallible` or turbofish annotations.
    fn run_for(self, duration: Duration) -> RunFor<Self>
    where
        Self: Future<Output = Result<()>>,
    {
        self.run_until_deadline(deadline_after(duration))
    }

    /// Absolute-deadline counterpart to [`GameFutureExt::run_for`].
    fn run_until_deadline(self, deadline: Instant) -> RunFor<Self>
    where
        Self: Future<Output = Result<()>>,
    {
        RunFor {
            inner: Timed::new(self, deadline),
        }
    }
}

impl<F: Future> GameFutureExt for F {}

#[cfg(test)]
#[path = "future_test.rs"]
mod test;
