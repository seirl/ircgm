use std::borrow::Borrow;
use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::panic::Location;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use irc_colors::Text;
use tokio::time::Instant;
use tracing::Instrument as _;

use crate::casemap::CaseMap;
use crate::error::{Error, Result};
use crate::exclusive_dm::ExclusiveDm;
use crate::huddle::Huddle;
use crate::inbox::Inbox;
use crate::message::{ChatMessage, Event, Target, WireEvent, WireKind};
use crate::room::Room;
use crate::state::BotPresence;
use crate::user::{User, UserId};

mod private {
    pub trait Sealed {}
    impl<T> Sealed for Option<T> {}
    impl<T> Sealed for super::AskMatch<T> {}
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AskMatchInner<T> {
    Accept(T, Option<Text>),
    Reject(Text),
    Ignore,
}

/// Outcome returned by [`Ask`] / [`AskMany`] matchers that want to send an inline confirmation or
/// validation feedback without restarting the reply timer or leaving the prompt loop.
///
/// Construct with [`AskMatch::accept`], [`AskMatch::accept_say`], [`AskMatch::reject`], or
/// [`AskMatch::ignore`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskMatch<T>(AskMatchInner<T>);

impl<T> AskMatch<T> {
    /// Accepts `val` silently and records the answer.
    pub fn accept(val: T) -> Self {
        Self(AskMatchInner::Accept(val, None))
    }

    /// Accepts `val` and immediately enqueues `msg` to the prompt's target (sent as a `NOTICE`
    /// when the accepted reply was one).
    pub fn accept_say(val: T, msg: impl Into<Text>) -> Self {
        Self(AskMatchInner::Accept(val, Some(msg.into())))
    }

    /// Sends validation feedback `msg` to the prompt's target (as a `NOTICE` if the reply was one)
    /// and keeps waiting without resetting the reply timer.
    pub fn reject(msg: impl Into<Text>) -> Self {
        Self(AskMatchInner::Reject(msg.into()))
    }

    /// Silently ignores this message and keeps waiting.
    pub const fn ignore() -> Self {
        Self(AskMatchInner::Ignore)
    }
}

/// Sealed trait implemented for both `Option<T>` (`Some(v) -> AskMatch::accept(v)`,
/// `None -> AskMatch::ignore()`) and [`AskMatch<T>`].
pub trait IntoAskMatch: private::Sealed {
    type Output;
    fn into_ask_match(self) -> AskMatch<Self::Output>;
}

impl<T> IntoAskMatch for Option<T> {
    type Output = T;

    fn into_ask_match(self) -> AskMatch<T> {
        match self {
            Some(v) => AskMatch::accept(v),
            None => AskMatch::ignore(),
        }
    }
}

impl<T> IntoAskMatch for AskMatch<T> {
    type Output = T;

    fn into_ask_match(self) -> AskMatch<T> {
        self
    }
}

/// Outcome of an [`Ask`] prompt, separating normal player/prompt outcomes (`Ok`, `TimedOut`,
/// `Left`, `Unreachable`) from fatal session or channel [`Error`]s.
///
/// Use [`Answer::ok`] when any non-answer should map to `None`, or [`Answer::required`] when
/// non-answers should fail with `Err`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer<T> {
    Ok(T),
    TimedOut,
    Left {
        nick: String,
        reason: Option<String>,
    },
    Unreachable {
        nick: String,
        code: u16,
        message: String,
    },
}

impl<T> Answer<T> {
    /// Converts `Err(Error::TimedOut | Error::UserLeft | Error::UserUnreachable)` into
    /// `Ok(Answer::...)`, preserving fatal session/channel/input errors as `Err`.
    pub fn from_result(res: Result<T>) -> Result<Self> {
        match res {
            Ok(val) => Ok(Answer::Ok(val)),
            Err(Error::TimedOut) => Ok(Answer::TimedOut),
            Err(Error::UserLeft { nick, reason }) => Ok(Answer::Left { nick, reason }),
            Err(Error::UserUnreachable {
                nick,
                code,
                message,
            }) => Ok(Answer::Unreachable {
                nick,
                code,
                message,
            }),
            Err(e) => Err(e),
        }
    }

    /// Inverse of [`Answer::from_result`]: promotes `TimedOut`, `Left`, and `Unreachable` into
    /// `Err(Error::...)` so `?` short-circuits sequential multi-step prompts.
    pub fn required(self) -> Result<T> {
        match self {
            Answer::Ok(val) => Ok(val),
            Answer::TimedOut => Err(Error::TimedOut),
            Answer::Left { nick, reason } => Err(Error::UserLeft { nick, reason }),
            Answer::Unreachable {
                nick,
                code,
                message,
            } => Err(Error::UserUnreachable {
                nick,
                code,
                message,
            }),
        }
    }

    /// Returns `Some(val)` on [`Answer::Ok`], or `None` on `TimedOut | Left | Unreachable`.
    pub fn ok(self) -> Option<T> {
        match self {
            Answer::Ok(val) => Some(val),
            Answer::TimedOut | Answer::Left { .. } | Answer::Unreachable { .. } => None,
        }
    }

    /// Borrows `Some(&val)` on [`Answer::Ok`], or `None` otherwise.
    pub fn as_ok(&self) -> Option<&T> {
        match self {
            Answer::Ok(val) => Some(val),
            Answer::TimedOut | Answer::Left { .. } | Answer::Unreachable { .. } => None,
        }
    }

    /// Extracts `val` from [`Answer::Ok`], panicking if the prompt did not receive an answer.
    #[track_caller]
    pub fn unwrap(self) -> T {
        match self {
            Answer::Ok(val) => val,
            Answer::TimedOut => panic!("called `Answer::unwrap()` on `Answer::TimedOut`"),
            Answer::Left { nick, reason } => panic!(
                "called `Answer::unwrap()` on `Answer::Left {{ nick: {nick:?}, reason: {reason:?} }}`"
            ),
            Answer::Unreachable {
                nick,
                code,
                message,
            } => panic!(
                "called `Answer::unwrap()` on `Answer::Unreachable {{ nick: {nick:?}, code: {code}, message: {message:?} }}`"
            ),
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, Answer::Ok(_))
    }

    pub fn is_timed_out(&self) -> bool {
        matches!(self, Answer::TimedOut)
    }

    pub fn is_left(&self) -> bool {
        matches!(self, Answer::Left { .. })
    }

    pub fn is_unreachable(&self) -> bool {
        matches!(self, Answer::Unreachable { .. })
    }
}

pub(crate) const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug)]
enum TimeLimit {
    None,
    Duration(Duration),
    Deadline(Instant),
}

impl TimeLimit {
    fn deadline_from(self, base: Instant) -> Option<Instant> {
        match self {
            Self::None => None,
            Self::Duration(dur) => Some(crate::future::deadline_after_from(base, dur)),
            Self::Deadline(dl) => Some(dl),
        }
    }
}

/// Prompt configuration passed to [`Ask::config`] and [`AskMany::config`].
#[derive(Clone, Debug)]
pub struct AskConfig {
    time_limit: TimeLimit,
    pub(crate) warnings: Vec<(Duration, Text)>,
    pub(crate) timeout_message: Option<Text>,
    pub(crate) ignore_commands: bool,
    pub(crate) accept_notices: bool,
}

impl Default for AskConfig {
    fn default() -> Self {
        Self {
            time_limit: TimeLimit::Duration(DEFAULT_REPLY_TIMEOUT),
            warnings: Vec::new(),
            timeout_message: None,
            ignore_commands: false,
            accept_notices: false,
        }
    }
}

impl AskConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reply_timeout(mut self, duration: Duration) -> Self {
        self.time_limit = TimeLimit::Duration(duration);
        self
    }

    pub fn reply_deadline(mut self, instant: Instant) -> Self {
        self.time_limit = TimeLimit::Deadline(instant);
        self
    }

    pub fn no_reply_timeout(mut self) -> Self {
        self.time_limit = TimeLimit::None;
        self
    }

    pub fn warn_remaining(mut self, remaining: Duration, msg: impl Into<Text>) -> Self {
        self.warnings.push((remaining, msg.into()));
        self
    }

    pub fn clear_warnings(mut self) -> Self {
        self.warnings.clear();
        self
    }

    pub fn ignore_commands(mut self) -> Self {
        self.ignore_commands = true;
        self
    }

    pub fn accept_notices(mut self) -> Self {
        self.accept_notices = true;
        self
    }

    pub fn timeout_message(mut self, msg: impl Into<Text>) -> Self {
        self.timeout_message = Some(msg.into());
        self
    }

    pub fn no_timeout_message(mut self) -> Self {
        self.timeout_message = None;
        self
    }
}

impl Room {
    #[track_caller]
    pub fn ask<'a, F, R>(&'a self, prompt: impl Into<Text>, matcher: F) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoAskMatch + 'a,
        R::Output: Send + 'a,
    {
        Ask::from_endpoint(
            Endpoint::Room(self),
            Location::caller(),
            prompt.into(),
            matcher,
        )
    }

    /// Shorthand for `self.ask(prompt, matcher).from_user(user)`.
    #[track_caller]
    pub fn ask_user<'a, F, R>(
        &'a self,
        user: &User,
        prompt: impl Into<Text>,
        matcher: F,
    ) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoAskMatch + 'a,
        R::Output: Send + 'a,
    {
        self.ask(prompt, matcher).from_user(user)
    }

    /// Prompts this channel once and collects answers from `1..N` responders (`Result<Answers<T>>`).
    ///
    /// By default anyone in the room (including newcomers joining mid-prompt) may respond; chain
    /// [`AskMany::from_users`] to restrict to a specific set of players, [`AskMany::allow_change`]
    /// to let responders update their answer, and [`AskMany::max_answers`] or
    /// [`AskMany::stop_when`] to customize early completion.
    #[track_caller]
    pub fn ask_many<'a, F, R>(&'a self, prompt: impl Into<Text>, matcher: F) -> AskMany<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoAskMatch + 'a,
        R::Output: Send + 'a,
    {
        AskMany::from_endpoint(
            Endpoint::Room(self),
            Location::caller(),
            prompt.into(),
            matcher,
        )
    }
}

impl Huddle {
    #[track_caller]
    pub fn ask<'a, F, R>(&'a mut self, prompt: impl Into<Text>, matcher: F) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoAskMatch + 'a,
        R::Output: Send + 'a,
    {
        Ask::from_endpoint(
            Endpoint::Huddle(self),
            Location::caller(),
            prompt.into(),
            matcher,
        )
    }

    /// Broadcasts `prompt` to all online huddle members and collects up to one answer per member
    /// (or restricted subset via [`AskMany::from_users`]) while relaying member chat.
    #[track_caller]
    pub fn ask_many<'a, F, R>(
        &'a mut self,
        prompt: impl Into<Text>,
        matcher: F,
    ) -> AskMany<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoAskMatch + 'a,
        R::Output: Send + 'a,
    {
        AskMany::from_endpoint(
            Endpoint::Huddle(self),
            Location::caller(),
            prompt.into(),
            matcher,
        )
    }
}

impl ExclusiveDm {
    #[track_caller]
    pub fn ask<'a, F, R>(&'a mut self, prompt: impl Into<Text>, matcher: F) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoAskMatch + 'a,
        R::Output: Send + 'a,
    {
        Ask::from_endpoint(
            Endpoint::ExclusiveDm(self),
            Location::caller(),
            prompt.into(),
            matcher,
        )
    }
}

impl User {
    /// Acquires `exclusive()`, sends `prompt` in DM, and awaits one matching reply.
    #[track_caller]
    pub fn ask<'a, F, R>(&'a self, prompt: impl Into<Text>, matcher: F) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoAskMatch + 'a,
        R::Output: Send + 'a,
    {
        Ask::from_user_dm(self, Location::caller(), prompt.into(), matcher)
    }
}

enum Endpoint<'a> {
    Room(&'a Room),
    ExclusiveDm(&'a mut ExclusiveDm),
    Huddle(&'a mut Huddle),
}

impl Endpoint<'_> {
    fn say_text(&self, text: Text) -> Result<()> {
        match self {
            Self::Room(r) => r.say(text),
            Self::ExclusiveDm(dm) => dm.say(text),
            Self::Huddle(h) => h.say(text),
        }
    }

    fn notice_text(&self, text: Text) -> Result<()> {
        match self {
            Self::Room(r) => r.notice(text),
            Self::ExclusiveDm(dm) => dm.notice(text),
            Self::Huddle(h) => h.notice(text),
        }
    }

    fn target(&self) -> Target {
        match self {
            Self::Room(r) => Target::Channel(r.name().to_owned()),
            Self::ExclusiveDm(_) | Self::Huddle(_) => Target::Dm,
        }
    }

    fn is_group(&self) -> bool {
        matches!(self, Self::Room(_) | Self::Huddle(_))
    }

    fn participants(&self) -> Option<Vec<User>> {
        match self {
            Self::Room(_) => None,
            Self::ExclusiveDm(dm) => Some(vec![dm.user().clone()]),
            Self::Huddle(h) => Some(h.users()),
        }
    }

    fn group_has_user(&self, user: &User) -> Option<bool> {
        match self {
            Self::Room(r) => Some(r.has_user(user)),
            Self::Huddle(h) => Some(h.has_user(user)),
            Self::ExclusiveDm(_) => None,
        }
    }

    fn borrowed_inbox(&mut self) -> Option<&mut Inbox> {
        match self {
            Self::Room(_) => None,
            Self::ExclusiveDm(dm) => Some(dm.inbox()),
            Self::Huddle(h) => Some(h.inbox()),
        }
    }
}

/// Collection of accepted `(User, T)` responses returned by [`crate::Room::ask_many`] and
/// [`crate::Huddle::ask_many`], ordered by first accepted response.
#[derive(Clone, Debug)]
pub struct Answers<T> {
    entries: Vec<(User, T)>,
}

impl<T> Default for Answers<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<T> Answers<T> {
    fn has_answered(&self, uid: UserId) -> bool {
        self.entries.iter().any(|(u, _)| u.id() == uid)
    }

    fn record(&mut self, user: &User, val: T) {
        if let Some((_, slot)) = self.entries.iter_mut().find(|(u, _)| u.id() == user.id()) {
            *slot = val;
        } else {
            self.entries.push((user.clone(), val));
        }
    }

    fn evict(&mut self, uid: UserId) -> bool {
        let Some(idx) = self.entries.iter().position(|(u, _)| u.id() == uid) else {
            return false;
        };
        self.entries.remove(idx);
        true
    }

    /// Number of accepted answers currently stored.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if no accepted answers are stored.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns `true` if `user` has an accepted answer.
    pub fn contains_user(&self, user: &User) -> bool {
        self.has_answered(user.id())
    }

    /// Borrows `user`'s accepted answer, if any.
    pub fn get(&self, user: &User) -> Option<&T> {
        self.entries
            .iter()
            .find(|(u, _)| u.id() == user.id())
            .map(|(_, v)| v)
    }

    /// Iterates over accepted `(&User, &T)` pairs in order.
    pub fn iter(&self) -> AnswersIter<'_, T> {
        AnswersIter(self.entries.iter())
    }

    /// Iterates over every [`User`] who submitted an accepted answer.
    pub fn users(&self) -> impl Iterator<Item = &User> {
        self.iter().map(|(u, _)| u)
    }

    /// Iterates over accepted answer values in order.
    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.iter().map(|(_, v)| v)
    }
}

/// Borrowing iterator over accepted `(&User, &T)` pairs in [`Answers`].
pub struct AnswersIter<'a, T>(std::slice::Iter<'a, (User, T)>);

impl<'a, T> Iterator for AnswersIter<'a, T> {
    type Item = (&'a User, &'a T);

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(u, v)| (u, v))
    }
}

impl<T> IntoIterator for Answers<T> {
    type Item = (User, T);
    type IntoIter = std::vec::IntoIter<(User, T)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl<'a, T> IntoIterator for &'a Answers<T> {
    type Item = (&'a User, &'a T);
    type IntoIter = AnswersIter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

type StopPredicate<'a, O> = Box<dyn FnMut(&Answers<O>) -> bool + Send + 'a>;

enum PromptSource<'a> {
    Endpoint(Endpoint<'a>),
    UserDm(&'a User),
}

struct PromptParams<'a, F, O> {
    caller: &'static Location<'static>,
    restricted_users: Option<Vec<User>>,
    cfg: AskConfig,
    prompt: Text,
    matcher: F,
    allow_change: bool,
    stop_pred: Option<StopPredicate<'a, O>>,
}

struct PromptSpec<'a, F, O> {
    source: PromptSource<'a>,
    params: PromptParams<'a, F, O>,
    invalid_arg: Option<&'static str>,
}

impl<'a, F, O> PromptSpec<'a, F, O> {
    fn new(
        source: PromptSource<'a>,
        caller: &'static Location<'static>,
        prompt: Text,
        matcher: F,
    ) -> Self {
        Self {
            source,
            params: PromptParams {
                caller,
                restricted_users: None,
                cfg: AskConfig::default(),
                prompt,
                matcher,
                allow_change: false,
                stop_pred: None,
            },
            invalid_arg: None,
        }
    }

    fn set_from_users(&mut self, users: Vec<User>) {
        match &self.source {
            PromptSource::Endpoint(ep) if ep.is_group() => {
                self.params.restricted_users = Some(users);
            }
            PromptSource::Endpoint(_) | PromptSource::UserDm(_) => {
                self.invalid_arg = Some("from_user / from_users requires a group prompt");
            }
        }
    }

    fn make_span(&self, name: &'static str) -> tracing::Span {
        let target = match &self.source {
            PromptSource::Endpoint(ep) => match ep.target() {
                Target::Channel(ch) => ch,
                Target::Dm => "DM".to_owned(),
            },
            PromptSource::UserDm(_) => "DM".to_owned(),
        };
        let timeout_ms = match self.params.cfg.time_limit {
            TimeLimit::Duration(d) => Some(d.as_millis() as u64),
            TimeLimit::Deadline(_) | TimeLimit::None => None,
        };
        tracing::debug_span!("ircgm::prompt", kind = name, %target, timeout_ms)
    }
}

enum LazyState<'a, U, Out> {
    Unstarted(Box<U>),
    Running(Pin<Box<dyn Future<Output = Out> + Send + 'a>>),
    Done,
}

impl<'a, U, Out> LazyState<'a, U, Out> {
    fn new(unstarted: U) -> Self {
        Self::Unstarted(Box::new(unstarted))
    }

    fn map_unstarted(&mut self, f: impl FnOnce(&mut U)) {
        match self {
            Self::Unstarted(u) => f(u),
            Self::Running(_) | Self::Done => {
                unreachable!("prompt builder methods consume self before polling")
            }
        }
    }

    fn poll_with<Fut>(&mut self, cx: &mut Context<'_>, start: impl FnOnce(U) -> Fut) -> Poll<Out>
    where
        Fut: Future<Output = Out> + Send + 'a,
    {
        if matches!(self, Self::Unstarted(_)) {
            let Self::Unstarted(u) = std::mem::replace(self, Self::Done) else {
                unreachable!()
            };
            *self = Self::Running(Box::pin(start(*u)));
        }
        match self {
            Self::Running(fut) => match fut.as_mut().poll(cx) {
                Poll::Ready(res) => {
                    *self = Self::Done;
                    Poll::Ready(res)
                }
                Poll::Pending => Poll::Pending,
            },
            Self::Done => panic!("prompt future polled after completion"),
            Self::Unstarted(_) => unreachable!(),
        }
    }
}

type AskState<'a, F, O> = LazyState<'a, PromptSpec<'a, F, O>, Result<Answer<O>>>;
type AskManyState<'a, F, O> = LazyState<'a, PromptSpec<'a, F, O>, Result<Answers<O>>>;

/// Lazy single-answer prompt future returned by [`crate::Room::ask`],
/// [`crate::Room::ask_user`], [`User::ask`], [`crate::ExclusiveDm::ask`], and
/// [`crate::Huddle::ask`].
///
/// Awaiting an `Ask` resolves to `Result<Answer<T>>` so timeouts, player departures, and DM
/// bounces are handled explicitly while broken sessions/channels propagate via `?`. Chain
/// `.await?.ok()` to collapse any non-answer into `Option<T>`, or `.await?.required()?` to
/// promote non-answers into `Err(Error::...)`.
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Ask<'a, F, R: IntoAskMatch> {
    state: AskState<'a, F, R::Output>,
}

impl<'a, F, R: IntoAskMatch> Ask<'a, F, R> {
    fn from_endpoint(
        endpoint: Endpoint<'a>,
        caller: &'static Location<'static>,
        prompt: Text,
        matcher: F,
    ) -> Self {
        Self {
            state: LazyState::new(PromptSpec::new(
                PromptSource::Endpoint(endpoint),
                caller,
                prompt,
                matcher,
            )),
        }
    }

    fn from_user_dm(
        user: &'a User,
        caller: &'static Location<'static>,
        prompt: Text,
        matcher: F,
    ) -> Self {
        Self {
            state: LazyState::new(PromptSpec::new(
                PromptSource::UserDm(user),
                caller,
                prompt,
                matcher,
            )),
        }
    }

    /// Restricts a group prompt so only `user` may answer.
    pub fn from_user(self, user: &User) -> Self {
        self.from_users(std::iter::once(user))
    }

    /// Restricts a group prompt so only users in `users` may answer (first valid reply wins;
    /// non-listed users are ignored).
    pub fn from_users<I, U>(mut self, users: I) -> Self
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        let collected: Vec<User> = users.into_iter().map(|u| u.borrow().clone()).collect();
        self.state.map_unstarted(|s| s.set_from_users(collected));
        self
    }

    /// Applies an [`AskConfig`] (by value or `&AskConfig`) to this prompt.
    pub fn config(mut self, cfg: impl Borrow<AskConfig>) -> Self {
        let cloned = cfg.borrow().clone();
        self.state.map_unstarted(|s| s.params.cfg = cloned);
        self
    }
}

impl<'a, F, R> Future for Ask<'a, F, R>
where
    F: FnMut(&ChatMessage) -> R + Send + 'a,
    R: IntoAskMatch + 'a,
    R::Output: Send + 'a,
{
    type Output = Result<Answer<R::Output>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().state.poll_with(cx, |mut spec| {
            spec.params.stop_pred =
                Some(Box::new(|answers: &Answers<R::Output>| !answers.is_empty()));
            let span = spec.make_span("ask");
            run_ask(spec).instrument(span)
        })
    }
}

/// Lazy multi-responder prompt future returned by [`crate::Room::ask_many`] and
/// [`crate::Huddle::ask_many`].
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct AskMany<'a, F, R: IntoAskMatch> {
    state: AskManyState<'a, F, R::Output>,
}

impl<'a, F, R: IntoAskMatch> AskMany<'a, F, R> {
    fn from_endpoint(
        endpoint: Endpoint<'a>,
        caller: &'static Location<'static>,
        prompt: Text,
        matcher: F,
    ) -> Self {
        Self {
            state: LazyState::new(PromptSpec::new(
                PromptSource::Endpoint(endpoint),
                caller,
                prompt,
                matcher,
            )),
        }
    }

    /// Restricts eligible responders to `users` (deduplicated in input order).
    pub fn from_users<I, U>(mut self, users: I) -> Self
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        let collected: Vec<User> = users.into_iter().map(|u| u.borrow().clone()).collect();
        self.state.map_unstarted(|s| s.set_from_users(collected));
        self
    }

    /// Allows responders to overwrite an already-accepted answer until the deadline or
    /// [`AskMany::stop_when`] / [`AskMany::max_answers`] completes the prompt.
    pub fn allow_change(mut self, allow: bool) -> Self {
        self.state.map_unstarted(|s| s.params.allow_change = allow);
        self
    }

    /// Stops early as soon as `max` distinct users have active accepted answers (`max >= 1`).
    pub fn max_answers(mut self, max: usize) -> Self
    where
        R::Output: 'a,
    {
        self.state.map_unstarted(|s| {
            if max == 0 {
                s.invalid_arg = Some("AskMany::max_answers requires max >= 1");
            } else {
                s.invalid_arg = None;
                s.params.stop_pred = Some(Box::new(move |answers: &Answers<R::Output>| {
                    answers.len() >= max
                }));
            }
        });
        self
    }

    /// Installs a custom early-stopping predicate evaluated against [`Answers`] whenever an answer
    /// is recorded (or an open-room responder departs).
    pub fn stop_when<P>(mut self, predicate: P) -> Self
    where
        P: FnMut(&Answers<R::Output>) -> bool + Send + 'a,
    {
        self.state.map_unstarted(|s| {
            s.invalid_arg = None;
            s.params.stop_pred = Some(Box::new(predicate));
        });
        self
    }

    /// Applies an [`AskConfig`] (by value or `&AskConfig`) to this prompt.
    pub fn config(mut self, cfg: impl Borrow<AskConfig>) -> Self {
        let cloned = cfg.borrow().clone();
        self.state.map_unstarted(|s| s.params.cfg = cloned);
        self
    }
}

impl<'a, F, R> Future for AskMany<'a, F, R>
where
    F: FnMut(&ChatMessage) -> R + Send + 'a,
    R: IntoAskMatch + 'a,
    R::Output: Send + 'a,
{
    type Output = Result<Answers<R::Output>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().state.poll_with(cx, |spec| {
            let span = spec.make_span("ask_many");
            run_ask_many(spec).instrument(span)
        })
    }
}

fn compute_schedule(
    cfg: &AskConfig,
    sent_at: Instant,
) -> (Option<Instant>, VecDeque<(Instant, Text)>) {
    let Some(deadline) = cfg.time_limit.deadline_from(sent_at) else {
        return (None, VecDeque::new());
    };
    let budget = deadline.saturating_duration_since(sent_at);
    let mut warns: Vec<_> = cfg
        .warnings
        .iter()
        .filter(|(rem, _)| !rem.is_zero() && budget > *rem)
        .map(|(rem, msg)| (deadline - *rem, msg.clone()))
        .collect();
    warns.sort_by_key(|(at, _)| *at);
    (Some(deadline), VecDeque::from(warns))
}

struct PromptRunOutcome<T> {
    answers: Answers<T>,
    last_terminal: Option<Answer<()>>,
    timed_out: bool,
}

impl<T> PromptRunOutcome<T> {
    fn into_single_answer(self) -> Answer<T> {
        if let Some((_, val)) = self.answers.into_iter().next() {
            return Answer::Ok(val);
        }
        if self.timed_out {
            return Answer::TimedOut;
        }
        match self.last_terminal {
            Some(Answer::Left { nick, reason }) => Answer::Left { nick, reason },
            Some(Answer::Unreachable {
                nick,
                code,
                message,
            }) => Answer::Unreachable {
                nick,
                code,
                message,
            },
            Some(Answer::TimedOut | Answer::Ok(())) | None => Answer::TimedOut,
        }
    }
}

fn dedup_users(users: Vec<User>) -> Vec<User> {
    let mut out = Vec::with_capacity(users.len());
    let mut seen = HashSet::with_capacity(users.len());
    for u in users {
        if seen.insert(u.id()) {
            out.push(u);
        }
    }
    out
}

fn send_reply_to_msg(endpoint: &Endpoint<'_>, is_notice: bool, text: Text) {
    if is_notice {
        let _ = endpoint.notice_text(text);
    } else {
        let _ = endpoint.say_text(text);
    }
}

async fn run_prompt<'a, F, R>(
    mut endpoint: Endpoint<'_>,
    params: PromptParams<'a, F, R::Output>,
) -> Result<PromptRunOutcome<R::Output>>
where
    F: FnMut(&ChatMessage) -> R + Send + 'a,
    R: IntoAskMatch + 'a,
    R::Output: Send + 'a,
{
    let PromptParams {
        caller,
        restricted_users,
        cfg,
        prompt,
        mut matcher,
        allow_change,
        mut stop_pred,
    } = params;
    if let Some(ref users) = restricted_users {
        if users.is_empty() {
            return Err(Error::InvalidInput(
                "from_users requires at least one User",
            ));
        }
        if !endpoint.is_group() {
            return Err(Error::InvalidInput(
                "from_user / from_users requires a group prompt",
            ));
        }
    }

    let base_participants = endpoint.participants();
    let voters = match restricted_users {
        Some(users) => Some(dedup_users(users)),
        None => base_participants.clone().map(dedup_users),
    };
    let mut answers = Answers::default();
    let mut departed: HashSet<UserId> = HashSet::new();
    let mut last_terminal: Option<Answer<()>> = None;

    let (mut owned_inbox, inbox_core) = {
        let owned = match &endpoint {
            Endpoint::Room(r) => {
                let ib = match voters.as_deref() {
                    Some(us) => r.inbox_users_at(us, caller),
                    None => r.inbox_at(caller),
                };
                if ib.is_invalid_session() {
                    return Err(Error::InvalidInput(
                        "User handle belongs to a different Bot session",
                    ));
                }
                Some(ib)
            }
            Endpoint::ExclusiveDm(_) | Endpoint::Huddle(_) => None,
        };
        let inbox_core = match &owned {
            Some(ib) => ib.core().cloned(),
            None => endpoint.borrowed_inbox().and_then(|ib| ib.core().cloned()),
        };

        if let Some(ref core) = inbox_core {
            if let Some(ref vs) = voters {
                for u in vs {
                    if !u.belongs_to(core) {
                        return Err(Error::InvalidInput(
                            "User handle belongs to a different Bot session",
                        ));
                    }
                }
            }
            core.check_connected()?;
            if let Target::Channel(ch) = endpoint.target() {
                core.state
                    .read()
                    .unwrap()
                    .check_bot_in_channel(&ch, BotPresence::Tracked)?;
            }
        }
        if base_participants.is_some() {
            if let Some(ref vs) = voters {
                for u in vs {
                    if endpoint.group_has_user(u) == Some(false) {
                        return Err(u.quit_error().unwrap_or(Error::InvalidInput(
                            "User is not a member of this group",
                        )));
                    }
                }
            }
        }

        if let Some(ref vs) = voters {
            if !vs.is_empty() {
                for u in vs {
                    if let Some(Error::UserLeft { nick, reason }) = u.quit_error() {
                        if departed.insert(u.id()) {
                            last_terminal = Some(Answer::Left { nick, reason });
                        }
                    } else if owned
                        .as_ref()
                        .is_some_and(|ib| ib.is_departed_in_room(u.track_id()))
                        && departed.insert(u.id())
                    {
                        last_terminal = Some(Answer::Left {
                            nick: u.nick(),
                            reason: None,
                        });
                    }
                }
                if departed.len() == vs.len() {
                    return Ok(PromptRunOutcome {
                        answers,
                        last_terminal,
                        timed_out: false,
                    });
                }
            }
        }
        if let Some(ib) = endpoint.borrowed_inbox() {
            ib.clear();
        }
        (owned, inbox_core)
    };

    match endpoint.say_text(prompt) {
        Ok(()) => {}
        Err(Error::UserLeft { nick, reason }) => {
            return Ok(PromptRunOutcome {
                answers,
                last_terminal: Some(Answer::Left { nick, reason }),
                timed_out: false,
            });
        }
        Err(fatal) => return Err(fatal),
    }

    let (deadline, mut pending_warnings) = compute_schedule(&cfg, Instant::now());
    let endpoint_target = endpoint.target();
    let casemap = inbox_core
        .as_ref()
        .map_or_else(CaseMap::default, |c| c.casemap());

    let is_naturally_complete =
        |voters: &Option<Vec<User>>, answers: &Answers<R::Output>, departed: &HashSet<UserId>| {
            let Some(vs) = voters.as_deref() else {
                return false;
            };
            if allow_change {
                departed.len() == vs.len()
            } else {
                vs.iter()
                    .all(|u| answers.has_answered(u.id()) || departed.contains(&u.id()))
            }
        };

    enum LoopStep {
        Wire(Result<Arc<WireEvent>>),
        Warn,
        Deadline,
    }

    let mut timed_out = false;

    loop {
        let step = {
            let active_inbox: &mut Inbox = match &mut owned_inbox {
                Some(inbox) => inbox,
                None => endpoint
                    .borrowed_inbox()
                    .expect("non-Room endpoints always borrow an inbox"),
            };
            let next_warn_at = pending_warnings.front().map(|(at, _)| *at);
            tokio::select! {
                biased;
                res = std::future::poll_fn(|cx| active_inbox.poll_next_wire(cx)) => LoopStep::Wire(res),
                () = async {
                    match deadline {
                        Some(dl) => tokio::time::sleep_until(dl).await,
                        None => std::future::pending::<()>().await,
                    }
                } => LoopStep::Deadline,
                () = async {
                    match next_warn_at {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending::<()>().await,
                    }
                } => LoopStep::Warn,
            }
        };

        match step {
            LoopStep::Warn => {
                if let Some((_, warn_msg)) = pending_warnings.pop_front() {
                    if deadline.is_some_and(|dl| Instant::now() < dl) {
                        let _ = endpoint.say_text(warn_msg);
                    }
                }
            }
            LoopStep::Deadline => {
                timed_out = true;
                let any_unanswered = match voters.as_deref() {
                    Some(vs) => vs
                        .iter()
                        .any(|u| !answers.has_answered(u.id()) && !departed.contains(&u.id())),
                    None => answers.is_empty(),
                };
                if any_unanswered {
                    if let Some(ref msg) = cfg.timeout_message {
                        let _ = endpoint.say_text(msg.clone());
                    }
                }
                break;
            }
            LoopStep::Wire(wire_res) => {
                let wire = match wire_res {
                    Ok(w) => w,
                    Err(Error::UserLeft { nick, reason }) => {
                        last_terminal = Some(Answer::Left { nick, reason });
                        break;
                    }
                    Err(fatal) => return Err(fatal),
                };

                match &wire.kind {
                    WireKind::Event(Event::DeliveryFailed {
                        target,
                        code,
                        message,
                        ..
                    }) => {
                        if let Target::Channel(ref expected_chan) = endpoint_target {
                            if casemap.str_eq(target, expected_chan) {
                                return Err(Error::ChannelSendFailed {
                                    channel: target.clone(),
                                    code: *code,
                                    message: message.clone(),
                                });
                            }
                        }
                        if let (Some(vs), Some(tid)) = (voters.as_deref(), wire.bounce_user_id()) {
                            if let Some(u) = vs.iter().find(|u| u.track_id() == tid) {
                                if !answers.has_answered(u.id()) && departed.insert(u.id()) {
                                    last_terminal = Some(Answer::Unreachable {
                                        nick: target.clone(),
                                        code: *code,
                                        message: message.clone(),
                                    });
                                }
                                if is_naturally_complete(&voters, &answers, &departed) {
                                    break;
                                }
                            }
                        }
                    }
                    WireKind::Event(
                        Event::Parted {
                            user, nick, reason, ..
                        }
                        | Event::Kicked {
                            user, nick, reason, ..
                        },
                    ) if !matches!(endpoint_target, Target::Channel(_)) => {}
                    WireKind::Event(
                        Event::Parted {
                            user, nick, reason, ..
                        }
                        | Event::Kicked {
                            user, nick, reason, ..
                        }
                        | Event::Quit {
                            user, nick, reason, ..
                        },
                    ) => match voters.as_deref() {
                        Some(vs) => {
                            if vs.iter().any(|v| v.id() == user.id()) {
                                if !answers.has_answered(user.id()) && departed.insert(user.id()) {
                                    last_terminal = Some(Answer::Left {
                                        nick: nick.clone(),
                                        reason: reason.clone(),
                                    });
                                }
                                if is_naturally_complete(&voters, &answers, &departed) {
                                    break;
                                }
                            }
                        }
                        None => {
                            if answers.evict(user.id())
                                && !answers.is_empty()
                                && stop_pred.as_mut().is_some_and(|p| p(&answers))
                            {
                                break;
                            }
                        }
                    },
                    _ => {
                        let candidate = match &wire.kind {
                            WireKind::Event(Event::Chat(msg)) => Some(msg),
                            WireKind::Event(Event::Notice { .. }) if cfg.accept_notices => {
                                wire.human_notice()
                            }
                            _ => None,
                        };
                        let Some(msg) = candidate else {
                            continue;
                        };
                        if let Some(ref vs) = voters {
                            if !vs.iter().any(|u| u.id() == msg.sender.id())
                                || departed.contains(&msg.sender.id())
                            {
                                continue;
                            }
                        }
                        if (!allow_change && answers.has_answered(msg.sender.id()))
                            || (cfg.ignore_commands
                                && inbox_core
                                    .as_ref()
                                    .is_some_and(|c| c.is_command_text(&msg.text)))
                        {
                            continue;
                        }

                        match matcher(msg).into_ask_match().0 {
                            // Mirror reply kind: a `NOTICE` must never trigger an automatic
                            // `PRIVMSG` (RFC 2812 §3.3.2).
                            AskMatchInner::Accept(val, ack) => {
                                if let Some(ack_msg) = ack {
                                    send_reply_to_msg(&endpoint, msg.is_notice(), ack_msg);
                                }
                                answers.record(&msg.sender, val);
                                if stop_pred.as_mut().is_some_and(|p| p(&answers))
                                    || is_naturally_complete(&voters, &answers, &departed)
                                {
                                    break;
                                }
                            }
                            AskMatchInner::Reject(feedback) => {
                                send_reply_to_msg(&endpoint, msg.is_notice(), feedback);
                            }
                            AskMatchInner::Ignore => {}
                        }
                    }
                }
            }
        }
    }

    Ok(PromptRunOutcome {
        answers,
        last_terminal,
        timed_out,
    })
}

async fn run_ask<'a, F, R>(spec: PromptSpec<'a, F, R::Output>) -> Result<Answer<R::Output>>
where
    F: FnMut(&ChatMessage) -> R + Send + 'a,
    R: IntoAskMatch + 'a,
    R::Output: Send + 'a,
{
    if let Some(msg) = spec.invalid_arg {
        return Err(Error::InvalidInput(msg));
    }
    if spec.params.prompt.is_empty() {
        return Err(Error::InvalidInput("Ask prompt cannot be empty"));
    }

    let res = match spec.source {
        PromptSource::Endpoint(endpoint) => run_prompt(endpoint, spec.params).await,
        PromptSource::UserDm(user) => match user.exclusive_at(spec.params.caller).await {
            Ok(mut dm) => run_prompt(Endpoint::ExclusiveDm(&mut dm), spec.params).await,
            Err(e) => {
                tracing::debug!(error = %e, "ask aborted before DM prompt write");
                return Answer::from_result(Err(e));
            }
        },
    };

    match res {
        Ok(outcome) => Ok(outcome.into_single_answer()),
        Err(e) => Answer::from_result(Err(e)),
    }
}

async fn run_ask_many<'a, F, R>(spec: PromptSpec<'a, F, R::Output>) -> Result<Answers<R::Output>>
where
    F: FnMut(&ChatMessage) -> R + Send + 'a,
    R: IntoAskMatch + 'a,
    R::Output: Send + 'a,
{
    if let Some(msg) = spec.invalid_arg {
        return Err(Error::InvalidInput(msg));
    }
    if spec.params.prompt.is_empty() {
        return Err(Error::InvalidInput("Ask prompt cannot be empty"));
    }
    let PromptSource::Endpoint(endpoint) = spec.source else {
        unreachable!("AskMany is only constructed from Endpoint")
    };
    let is_open_group =
        endpoint.participants().is_none() && spec.params.restricted_users.is_none();
    if matches!(spec.params.cfg.time_limit, TimeLimit::None) && spec.params.stop_pred.is_none() {
        if spec.params.allow_change {
            return Err(Error::InvalidInput(
                "AskMany::allow_change(true) without stop_when/max_answers requires a reply_timeout or reply_deadline",
            ));
        }
        if is_open_group {
            return Err(Error::InvalidInput(
                "open Room::ask_many without from_users or stop_when/max_answers requires a reply_timeout or reply_deadline",
            ));
        }
    }

    run_prompt(endpoint, spec.params)
        .await
        .map(|out| out.answers)
}

#[cfg(test)]
#[path = "ask_test.rs"]
mod test;
