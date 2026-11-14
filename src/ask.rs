use std::borrow::Borrow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
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
use crate::chat::{Chat, GroupChat, InboxRef};
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

/// Context passed to [`AskConfig::timeout_message_fn`] when a prompt's reply timer expires.
pub struct TimeoutCtx<'a> {
    /// Where the prompt was sent: `Target::Channel("#blackjack")` or `Target::Dm`.
    pub target: &'a Target,
    /// The single remaining player being waited on (`Some(user)` when waiting on one responder;
    /// `None` when waiting on an open room or multiple unanswered responders).
    pub user: Option<&'a User>,
}

const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_secs(300);

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

#[derive(Clone)]
pub(crate) enum TimeoutHandler {
    Static(Text),
    Dynamic(Arc<dyn Fn(&TimeoutCtx<'_>) -> Text + Send + Sync + 'static>),
}

impl TimeoutHandler {
    fn render(&self, ctx: &TimeoutCtx<'_>) -> Text {
        match self {
            TimeoutHandler::Static(t) => t.clone(),
            TimeoutHandler::Dynamic(f) => f(ctx),
        }
    }
}

impl fmt::Debug for TimeoutHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TimeoutHandler::Static(t) => f.debug_tuple("Static").field(t).finish(),
            TimeoutHandler::Dynamic(_) => f.write_str("Dynamic(<fn>)"),
        }
    }
}

/// Prompt configuration passed to [`Ask::config`] and [`AskMany::config`].
#[derive(Clone, Debug)]
pub struct AskConfig {
    time_limit: TimeLimit,
    pub(crate) warnings: Vec<(Duration, Text)>,
    pub(crate) timeout_handler: Option<TimeoutHandler>,
    pub(crate) ignore_commands: bool,
    pub(crate) accept_notices: bool,
}

impl Default for AskConfig {
    fn default() -> Self {
        Self {
            time_limit: TimeLimit::Duration(DEFAULT_REPLY_TIMEOUT),
            warnings: Vec::new(),
            timeout_handler: None,
            ignore_commands: false,
            accept_notices: false,
        }
    }
}

impl AskConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// Shorthand for `AskConfig::new().reply_timeout(duration)`.
    pub fn timeout(duration: Duration) -> Self {
        Self::new().reply_timeout(duration)
    }

    /// Shorthand for `AskConfig::new().reply_deadline(instant)`.
    pub fn deadline(instant: Instant) -> Self {
        Self::new().reply_deadline(instant)
    }

    /// Shorthand for `AskConfig::new().no_reply_timeout()`.
    pub fn untimed() -> Self {
        Self::new().no_reply_timeout()
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
        self.timeout_handler = Some(TimeoutHandler::Static(msg.into()));
        self
    }

    pub fn timeout_message_fn<F, M>(mut self, f: F) -> Self
    where
        F: Fn(&TimeoutCtx<'_>) -> M + Send + Sync + 'static,
        M: Into<Text>,
    {
        self.timeout_handler = Some(TimeoutHandler::Dynamic(Arc::new(move |ctx| f(ctx).into())));
        self
    }

    pub fn no_timeout_message(mut self) -> Self {
        self.timeout_handler = None;
        self
    }
}

/// Sends `prompt` to any [`Chat`] (such as `&room`, `&breakout`, `&mut huddle`, `&mut dm`, or
/// `&mut generic_chat`) and returns a lazy [`Ask`] future awaiting one reply.
#[track_caller]
pub fn ask<'a, C, F, R>(chat: C, prompt: impl Into<Text>, matcher: F) -> Ask<'a, F, R>
where
    C: Chat + 'a,
    F: FnMut(&ChatMessage) -> R + Send + 'a,
    R: IntoAskMatch + 'a,
    R::Output: Send + 'a,
{
    Ask::from_chat(chat, prompt.into(), matcher)
}

/// Sends `prompt` to any [`GroupChat`] (such as `&room`, `&breakout`, `&mut huddle`, or
/// `&mut generic_group`) and returns a lazy [`AskMany`] future collecting `0..=N` replies.
#[track_caller]
pub fn ask_many<'a, G, F, R>(group: G, prompt: impl Into<Text>, matcher: F) -> AskMany<'a, F, R>
where
    G: GroupChat + 'a,
    F: FnMut(&ChatMessage) -> R + Send + 'a,
    R: IntoAskMatch + 'a,
    R::Output: Send + 'a,
{
    AskMany::from_group(group, prompt.into(), matcher)
}

impl Room {
    #[track_caller]
    pub fn ask<'a, F, R>(&'a self, prompt: impl Into<Text>, matcher: F) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoAskMatch + 'a,
        R::Output: Send + 'a,
    {
        Ask::from_group(self, prompt.into(), matcher)
    }

    /// Shorthand for `self.ask(prompt, matcher).from_user(user)` (sets [`TimeoutCtx::user`]
    /// to `Some(user)`).
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
        ask_many(self, prompt, matcher)
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
        Ask::from_group(self, prompt.into(), matcher)
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
        ask_many(self, prompt, matcher)
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
        ask(self, prompt, matcher)
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

trait ChatDyn: Send + Sync {
    fn say_text(&self, text: Text) -> Result<()>;
    fn notice_text(&self, text: Text) -> Result<()>;
    fn target(&self) -> Target;
    fn participants(&self) -> Option<Vec<User>>;
    fn inbox(&mut self) -> InboxRef<'_>;
}

impl<C: Chat + ?Sized> ChatDyn for C {
    fn say_text(&self, text: Text) -> Result<()> {
        self.say(text)
    }
    fn notice_text(&self, text: Text) -> Result<()> {
        self.notice(text)
    }
    fn target(&self) -> Target {
        Chat::target(self)
    }
    fn participants(&self) -> Option<Vec<User>> {
        Chat::participants(self)
    }
    fn inbox(&mut self) -> InboxRef<'_> {
        Chat::inbox(self)
    }
}

trait PromptEndpoint: Send + Sync {
    fn chat(&self) -> &dyn ChatDyn;
    fn chat_mut(&mut self) -> &mut dyn ChatDyn;
    fn is_group(&self) -> bool {
        false
    }
    fn group_has_user(&self, _user: &User) -> Option<bool> {
        None
    }
    fn inbox_for(&mut self, _users: Option<&[User]>) -> InboxRef<'_> {
        self.chat_mut().inbox()
    }
}

struct ChatEndpoint<C>(C);

impl<C: Chat> PromptEndpoint for ChatEndpoint<C> {
    fn chat(&self) -> &dyn ChatDyn {
        &self.0
    }
    fn chat_mut(&mut self) -> &mut dyn ChatDyn {
        &mut self.0
    }
}

struct GroupEndpoint<G>(G);

impl<G: GroupChat> PromptEndpoint for GroupEndpoint<G> {
    fn chat(&self) -> &dyn ChatDyn {
        &self.0
    }
    fn chat_mut(&mut self) -> &mut dyn ChatDyn {
        &mut self.0
    }
    fn is_group(&self) -> bool {
        true
    }
    fn group_has_user(&self, user: &User) -> Option<bool> {
        Some(self.0.has_user(user))
    }
    fn inbox_for(&mut self, users: Option<&[User]>) -> InboxRef<'_> {
        match users {
            Some(us) => self.0.inbox_users(us),
            None => self.0.inbox(),
        }
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

enum AskSource<'a> {
    Endpoint {
        endpoint: Box<dyn PromptEndpoint + 'a>,
        restricted_users: Option<Vec<User>>,
        caller: &'static Location<'static>,
    },
    UserDm {
        user: &'a User,
        caller: &'static Location<'static>,
    },
    Invalid(&'static str),
}

struct AskUnstarted<'a, F> {
    cfg: AskConfig,
    source: AskSource<'a>,
    prompt: Text,
    matcher: F,
}

/// Lazy single-answer prompt future returned by [`ask`], [`crate::Room::ask`],
/// [`crate::Room::ask_user`], [`User::ask`], [`crate::ExclusiveDm::ask`], and
/// [`crate::Huddle::ask`].
///
/// Awaiting an `Ask` resolves to `Result<Answer<T>>` so timeouts, player departures, and DM
/// bounces are handled explicitly while broken sessions/channels propagate via `?`. Chain
/// `.await?.ok()` to collapse any non-answer into `Option<T>`, or `.await?.required()?` to
/// promote non-answers into `Err(Error::...)`.
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Ask<'a, F, R: IntoAskMatch> {
    state: LazyState<'a, AskUnstarted<'a, F>, Result<Answer<R::Output>>>,
}

impl<'a, F, R: IntoAskMatch> Ask<'a, F, R> {
    #[track_caller]
    pub(crate) fn from_chat<C: Chat + 'a>(chat: C, prompt: Text, matcher: F) -> Self {
        Self::new(
            AskSource::Endpoint {
                endpoint: Box::new(ChatEndpoint(chat)),
                restricted_users: None,
                caller: Location::caller(),
            },
            prompt,
            matcher,
        )
    }

    #[track_caller]
    pub(crate) fn from_group<G: GroupChat + 'a>(group: G, prompt: Text, matcher: F) -> Self {
        Self::new(
            AskSource::Endpoint {
                endpoint: Box::new(GroupEndpoint(group)),
                restricted_users: None,
                caller: Location::caller(),
            },
            prompt,
            matcher,
        )
    }

    pub(crate) fn from_user_dm(
        user: &'a User,
        caller: &'static Location<'static>,
        prompt: Text,
        matcher: F,
    ) -> Self {
        Self::new(AskSource::UserDm { user, caller }, prompt, matcher)
    }

    fn new(source: AskSource<'a>, prompt: Text, matcher: F) -> Self {
        Self {
            state: LazyState::new(AskUnstarted {
                cfg: AskConfig::default(),
                source,
                prompt,
                matcher,
            }),
        }
    }

    /// Restricts a group prompt so only `user` may answer (and sets [`TimeoutCtx::user`] to
    /// `Some(user)`).
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
        self.state.map_unstarted(|u| match &mut u.source {
            AskSource::Endpoint {
                endpoint,
                restricted_users,
                ..
            } if endpoint.is_group() => {
                *restricted_users = Some(collected);
            }
            AskSource::Endpoint { .. } | AskSource::UserDm { .. } => {
                u.source = AskSource::Invalid("from_user / from_users requires a group prompt");
            }
            AskSource::Invalid(_) => {}
        });
        self
    }

    /// Applies an [`AskConfig`] (by value or `&AskConfig`) to this prompt.
    pub fn config(mut self, cfg: impl Borrow<AskConfig>) -> Self {
        let cloned = cfg.borrow().clone();
        self.state.map_unstarted(|u| u.cfg = cloned);
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
        self.get_mut().state.poll_with(cx, |u| {
            let span = make_ask_span(&u.cfg, &u.source);
            run_ask(u.cfg, u.source, u.prompt, u.matcher).instrument(span)
        })
    }
}

/// Collection of responses returned by [`ask_many`], [`crate::Room::ask_many`], and
/// [`crate::Huddle::ask_many`].
///
/// Iterating over `Answers<T>` or `&Answers<T>` yields accepted `(User, T)` pairs in order
/// (input order for closed rosters via [`AskMany::from_users`] or [`crate::Huddle::ask_many`], and
/// first-response order for open [`crate::Room::ask_many`] prompts). For closed-roster prompts,
/// [`Answers::answer_for`] and [`Answers::into_per_user`] also inspect per-player
/// [`Answer::TimedOut`], [`Answer::Left`], and [`Answer::Unreachable`] outcomes.
#[derive(Clone, Debug)]
pub struct Answers<T> {
    entries: Vec<(User, Option<Answer<T>>)>,
    index_by_id: HashMap<UserId, usize>,
}

impl<T> Default for Answers<T> {
    fn default() -> Self {
        Self::new_open()
    }
}

impl<T> Answers<T> {
    fn new_open() -> Self {
        Self {
            entries: Vec::new(),
            index_by_id: HashMap::new(),
        }
    }

    fn new_closed(voters: Vec<User>) -> Self {
        let mut entries = Vec::with_capacity(voters.len());
        let mut index_by_id = HashMap::with_capacity(voters.len());
        for u in voters {
            if let std::collections::hash_map::Entry::Vacant(e) = index_by_id.entry(u.id()) {
                e.insert(entries.len());
                entries.push((u, None));
            }
        }
        Self {
            entries,
            index_by_id,
        }
    }

    fn record_answer(&mut self, user: &User, val: T) {
        if let Some(&idx) = self.index_by_id.get(&user.id()) {
            self.entries[idx].1 = Some(Answer::Ok(val));
        } else {
            let idx = self.entries.len();
            self.index_by_id.insert(user.id(), idx);
            self.entries.push((user.clone(), Some(Answer::Ok(val))));
        }
    }

    fn has_answered(&self, uid: UserId) -> bool {
        self.index_by_id
            .get(&uid)
            .is_some_and(|&idx| matches!(self.entries[idx].1, Some(Answer::Ok(_))))
    }

    fn record_left_if_pending(
        &mut self,
        idx: usize,
        nick: String,
        reason: Option<String>,
        last_terminal: &mut Option<Answer<()>>,
    ) {
        if self.entries[idx].1.is_none() {
            *last_terminal = Some(Answer::Left {
                nick: nick.clone(),
                reason: reason.clone(),
            });
            self.entries[idx].1 = Some(Answer::Left { nick, reason });
        }
    }

    fn record_unreachable_if_pending(
        &mut self,
        idx: usize,
        nick: String,
        code: u16,
        message: String,
        last_terminal: &mut Option<Answer<()>>,
    ) {
        if self.entries[idx].1.is_none() {
            *last_terminal = Some(Answer::Unreachable {
                nick: nick.clone(),
                code,
                message: message.clone(),
            });
            self.entries[idx].1 = Some(Answer::Unreachable {
                nick,
                code,
                message,
            });
        }
    }

    fn evict_open(&mut self, uid: UserId) -> bool {
        let Some(removed_idx) = self.index_by_id.remove(&uid) else {
            return false;
        };
        self.entries.remove(removed_idx);
        for (new_idx, (u, _)) in self.entries.iter().enumerate().skip(removed_idx) {
            self.index_by_id.insert(u.id(), new_idx);
        }
        true
    }

    /// Number of accepted answers currently stored.
    pub fn len(&self) -> usize {
        self.entries
            .iter()
            .filter(|(_, a)| matches!(a, Some(Answer::Ok(_))))
            .count()
    }

    /// Returns `true` if no accepted answers are stored.
    pub fn is_empty(&self) -> bool {
        !self
            .entries
            .iter()
            .any(|(_, a)| matches!(a, Some(Answer::Ok(_))))
    }

    /// Returns `true` if `user` has an accepted answer.
    pub fn contains_user(&self, user: &User) -> bool {
        self.get(user).is_some()
    }

    /// Borrows `user`'s accepted answer, if any.
    pub fn get(&self, user: &User) -> Option<&T> {
        let &idx = self.index_by_id.get(&user.id())?;
        self.entries[idx].1.as_ref()?.as_ok()
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

    /// Borrows `user`'s full [`Answer`] outcome (`Ok`, `TimedOut`, `Left`, or `Unreachable`) when
    /// `user` was part of this prompt's roster or submitted an answer.
    pub fn answer_for(&self, user: &User) -> Option<Answer<&T>> {
        let &idx = self.index_by_id.get(&user.id())?;
        Some(match &self.entries[idx].1 {
            Some(Answer::Ok(val)) => Answer::Ok(val),
            None | Some(Answer::TimedOut) => Answer::TimedOut,
            Some(Answer::Left { nick, reason }) => Answer::Left {
                nick: nick.clone(),
                reason: reason.clone(),
            },
            Some(Answer::Unreachable {
                nick,
                code,
                message,
            }) => Answer::Unreachable {
                nick: nick.clone(),
                code: *code,
                message: message.clone(),
            },
        })
    }

    /// Consumes `self` into per-user `(User, Answer<T>)` outcomes in deduplicated roster order
    /// (including `TimedOut`, `Left`, and `Unreachable` entries for closed-roster prompts).
    pub fn into_per_user(self) -> Vec<(User, Answer<T>)> {
        self.entries
            .into_iter()
            .map(|(u, ans)| (u, ans.unwrap_or(Answer::TimedOut)))
            .collect()
    }
}

/// Borrowing iterator over accepted `(&User, &T)` pairs in [`Answers`].
pub struct AnswersIter<'a, T>(std::slice::Iter<'a, (User, Option<Answer<T>>)>);

impl<'a, T> Iterator for AnswersIter<'a, T> {
    type Item = (&'a User, &'a T);

    fn next(&mut self) -> Option<Self::Item> {
        for (u, ans) in self.0.by_ref() {
            if let Some(Answer::Ok(val)) = ans {
                return Some((u, val));
            }
        }
        None
    }
}

/// Owning iterator over accepted `(User, T)` pairs in [`Answers`].
pub struct AnswersIntoIter<T>(std::vec::IntoIter<(User, Option<Answer<T>>)>);

impl<T> Iterator for AnswersIntoIter<T> {
    type Item = (User, T);

    fn next(&mut self) -> Option<Self::Item> {
        for (u, ans) in self.0.by_ref() {
            if let Some(Answer::Ok(val)) = ans {
                return Some((u, val));
            }
        }
        None
    }
}

impl<T> IntoIterator for Answers<T> {
    type Item = (User, T);
    type IntoIter = AnswersIntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        AnswersIntoIter(self.entries.into_iter())
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

struct AskManyUnstarted<'a, F, O> {
    endpoint: Box<dyn PromptEndpoint + 'a>,
    caller: &'static Location<'static>,
    restricted_users: Option<Vec<User>>,
    cfg: AskConfig,
    prompt: Text,
    matcher: F,
    allow_change: bool,
    stop_pred: Option<StopPredicate<'a, O>>,
    invalid_max_zero: bool,
}

/// Lazy multi-responder prompt future returned by [`ask_many`], [`crate::Room::ask_many`], and
/// [`crate::Huddle::ask_many`].
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct AskMany<'a, F, R: IntoAskMatch> {
    state: LazyState<'a, AskManyUnstarted<'a, F, R::Output>, Result<Answers<R::Output>>>,
}

impl<'a, F, R: IntoAskMatch> AskMany<'a, F, R> {
    #[track_caller]
    pub(crate) fn from_group<G: GroupChat + 'a>(group: G, prompt: Text, matcher: F) -> Self {
        Self {
            state: LazyState::new(AskManyUnstarted {
                endpoint: Box::new(GroupEndpoint(group)),
                caller: Location::caller(),
                restricted_users: None,
                cfg: AskConfig::default(),
                prompt,
                matcher,
                allow_change: false,
                stop_pred: None,
                invalid_max_zero: false,
            }),
        }
    }

    /// Restricts eligible responders to `users` (deduplicated in input order).
    pub fn from_users<I, U>(mut self, users: I) -> Self
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        let collected: Vec<User> = users.into_iter().map(|u| u.borrow().clone()).collect();
        self.state
            .map_unstarted(|u| u.restricted_users = Some(collected));
        self
    }

    /// Allows responders to overwrite an already-accepted answer until the deadline or
    /// [`AskMany::stop_when`] / [`AskMany::max_answers`] completes the prompt.
    pub fn allow_change(mut self, allow: bool) -> Self {
        self.state.map_unstarted(|u| u.allow_change = allow);
        self
    }

    /// Stops early as soon as `max` distinct users have active accepted answers (`max >= 1`).
    pub fn max_answers(mut self, max: usize) -> Self
    where
        R::Output: 'a,
    {
        self.state.map_unstarted(|u| {
            if max == 0 {
                u.invalid_max_zero = true;
            } else {
                u.invalid_max_zero = false;
                u.stop_pred = Some(Box::new(move |answers: &Answers<R::Output>| {
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
        self.state.map_unstarted(|u| {
            u.invalid_max_zero = false;
            u.stop_pred = Some(Box::new(predicate));
        });
        self
    }

    /// Applies an [`AskConfig`] (by value or `&AskConfig`) to this prompt.
    pub fn config(mut self, cfg: impl Borrow<AskConfig>) -> Self {
        let cloned = cfg.borrow().clone();
        self.state.map_unstarted(|u| u.cfg = cloned);
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
        self.get_mut().state.poll_with(cx, |u| {
            let timeout_ms = match u.cfg.time_limit {
                TimeLimit::Duration(d) => Some(d.as_millis() as u64),
                TimeLimit::Deadline(_) | TimeLimit::None => None,
            };
            let target = u.endpoint.chat().target();
            let span = tracing::debug_span!(
                "ircgm::ask_many",
                target = %target_span_label(&target),
                restricted = u.restricted_users.as_ref().map(Vec::len),
                timeout_ms = timeout_ms,
            );
            run_ask_many(u).instrument(span)
        })
    }
}

fn target_span_label(target: &Target) -> &str {
    match target {
        Target::Channel(ch) => ch.as_str(),
        Target::Dm => "DM",
    }
}

fn make_ask_span(cfg: &AskConfig, source: &AskSource<'_>) -> tracing::Span {
    let timeout_ms = match cfg.time_limit {
        TimeLimit::Duration(d) => Some(d.as_millis() as u64),
        TimeLimit::Deadline(_) | TimeLimit::None => None,
    };
    match source {
        AskSource::Endpoint {
            endpoint,
            restricted_users,
            ..
        } => {
            let target = endpoint.chat().target();
            let single_nick = match restricted_users.as_deref() {
                Some([only]) => Some(only.nick()),
                None => match endpoint.chat().participants().as_deref() {
                    Some([only]) if !endpoint.is_group() => Some(only.nick()),
                    _ => None,
                },
                Some(_) => None,
            };
            tracing::debug_span!(
                "ircgm::ask",
                target = %target_span_label(&target),
                user = single_nick.as_deref(),
                timeout_ms = timeout_ms
            )
        }
        AskSource::UserDm { user, .. } => tracing::debug_span!(
            "ircgm::ask",
            target = "DM",
            user = %user.nick(),
            timeout_ms = timeout_ms
        ),
        AskSource::Invalid(_) => tracing::debug_span!("ircgm::ask", timeout_ms = timeout_ms),
    }
}

fn compute_schedule(cfg: &AskConfig, sent_at: Instant) -> (Option<Instant>, VecDeque<(Instant, Text)>) {
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

fn send_reply_to_msg(chat: &dyn ChatDyn, is_notice: bool, text: Text) {
    if is_notice {
        let _ = chat.notice_text(text);
    } else {
        let _ = chat.say_text(text);
    }
}

fn snapshot_all_departed<T>(
    voters: &Option<Vec<User>>,
    owned_inbox: Option<&Inbox>,
    answers: &mut Answers<T>,
    departed: &mut HashSet<UserId>,
    last_terminal: &mut Option<Answer<()>>,
) -> bool {
    let Some(vs) = voters.as_deref() else {
        return false;
    };
    if vs.is_empty() {
        return false;
    }
    for (idx, u) in vs.iter().enumerate() {
        if let Some(Error::UserLeft { nick, reason }) = u.quit_error() {
            answers.record_left_if_pending(idx, nick, reason, last_terminal);
            departed.insert(u.id());
        } else if owned_inbox.is_some_and(|ib| ib.is_departed_in_room(u.track_id())) {
            answers.record_left_if_pending(idx, u.nick(), None, last_terminal);
            departed.insert(u.id());
        }
    }
    departed.len() == vs.len()
}

#[allow(clippy::too_many_arguments)]
async fn run_prompt<'a, F, R>(
    endpoint: &mut (dyn PromptEndpoint + 'a),
    caller: &'static Location<'static>,
    restricted_users: Option<Vec<User>>,
    cfg: AskConfig,
    prompt: Text,
    mut matcher: F,
    allow_change: bool,
    mut stop_pred: Option<StopPredicate<'a, R::Output>>,
) -> Result<PromptRunOutcome<R::Output>>
where
    F: FnMut(&ChatMessage) -> R + Send + 'a,
    R: IntoAskMatch + 'a,
    R::Output: Send + 'a,
{
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

    let base_participants = endpoint.chat().participants();
    let voters = match restricted_users {
        Some(users) => Some(dedup_users(users)),
        None => base_participants.clone().map(dedup_users),
    };
    let mut answers = match &voters {
        Some(vs) if !vs.is_empty() => Answers::new_closed(vs.clone()),
        _ => Answers::new_open(),
    };

    let mut departed: HashSet<UserId> = HashSet::new();
    let mut last_terminal: Option<Answer<()>> = None;

    let (mut owned_inbox, inbox_core) = {
        let initial_ref = endpoint.inbox_for(voters.as_deref());
        if initial_ref.is_invalid_session() {
            return Err(Error::InvalidInput(
                "User handle belongs to a different Bot session",
            ));
        }
        let inbox_core = initial_ref.core().cloned();
        let is_owned = matches!(initial_ref, InboxRef::Owned(_));
        let owned = match initial_ref {
            InboxRef::Owned(mut ib) => {
                ib.set_opened_at(caller);
                Some(ib)
            }
            InboxRef::Borrowed(_) => None,
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
            if let Target::Channel(ch) = endpoint.chat().target() {
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

        if !is_owned {
            if snapshot_all_departed(
                &voters,
                None,
                &mut answers,
                &mut departed,
                &mut last_terminal,
            ) {
                return Ok(PromptRunOutcome {
                    answers,
                    last_terminal,
                    timed_out: false,
                });
            }
            endpoint.chat_mut().inbox().clear();
        }
        if snapshot_all_departed(
            &voters,
            owned.as_ref(),
            &mut answers,
            &mut departed,
            &mut last_terminal,
        ) {
            return Ok(PromptRunOutcome {
                answers,
                last_terminal,
                timed_out: false,
            });
        }
        (owned, inbox_core)
    };

    match endpoint.chat().say_text(prompt.clone()) {
        Ok(()) => {}
        Err(Error::UserLeft { nick, reason }) => {
            last_terminal = Some(Answer::Left {
                nick: nick.clone(),
                reason: reason.clone(),
            });
            for idx in 0..answers.entries.len() {
                answers.record_left_if_pending(
                    idx,
                    nick.clone(),
                    reason.clone(),
                    &mut last_terminal,
                );
            }
            return Ok(PromptRunOutcome {
                answers,
                last_terminal,
                timed_out: false,
            });
        }
        Err(fatal) => return Err(fatal),
    }

    let sent_at = Instant::now();
    let (deadline, mut pending_warnings) = compute_schedule(&cfg, sent_at);
    let endpoint_target = endpoint.chat().target();
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
            let mut borrowed_inbox;
            let active_inbox: &mut Inbox = match &mut owned_inbox {
                Some(inbox) => inbox,
                None => {
                    borrowed_inbox = endpoint.chat_mut().inbox();
                    &mut borrowed_inbox
                }
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
                        let _ = endpoint.chat().say_text(warn_msg);
                    }
                }
            }
            LoopStep::Deadline => {
                timed_out = true;
                let (should_notify, single_user) = match voters.as_deref() {
                    Some(vs) => {
                        let mut timed_out_indices = Vec::new();
                        for (idx, (_, slot)) in answers.entries.iter_mut().enumerate() {
                            if slot.is_none() {
                                *slot = Some(Answer::TimedOut);
                                timed_out_indices.push(idx);
                            }
                        }
                        let user = match timed_out_indices.as_slice() {
                            [only] => Some(&vs[*only]),
                            _ => None,
                        };
                        (!timed_out_indices.is_empty(), user)
                    }
                    None => (answers.is_empty(), None),
                };
                if should_notify {
                    if let Some(handler) = &cfg.timeout_handler {
                        let ctx = TimeoutCtx {
                            target: &endpoint_target,
                            user: single_user,
                        };
                        let _ = endpoint.chat().say_text(handler.render(&ctx));
                    }
                }
                break;
            }
            LoopStep::Wire(wire_res) => {
                let wire = match wire_res {
                    Ok(w) => w,
                    Err(Error::UserLeft { nick, reason }) => {
                        last_terminal = Some(Answer::Left {
                            nick: nick.clone(),
                            reason: reason.clone(),
                        });
                        for idx in 0..answers.entries.len() {
                            answers.record_left_if_pending(
                                idx,
                                nick.clone(),
                                reason.clone(),
                                &mut last_terminal,
                            );
                        }
                        break;
                    }
                    Err(fatal) => return Err(fatal),
                };

                match &wire.kind {
                    WireKind::BotLeftRoom { channel, reason } => {
                        if let Target::Channel(ref expected_chan) = endpoint_target {
                            if casemap.str_eq(channel, expected_chan) {
                                return Err(Error::BotLeftRoom {
                                    channel: channel.clone(),
                                    reason: reason.clone(),
                                });
                            }
                        }
                    }
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
                        let pool = base_participants.as_deref().or(voters.as_deref());
                        if let (Some(pool), Some(tid)) = (pool, wire.bounce_user_id()) {
                            if let Some(member) = pool.iter().find(|u| u.track_id() == tid) {
                                if let Some(&idx) = answers.index_by_id.get(&member.id()) {
                                    departed.insert(member.id());
                                    answers.record_unreachable_if_pending(
                                        idx,
                                        target.clone(),
                                        *code,
                                        message.clone(),
                                        &mut last_terminal,
                                    );
                                    if is_naturally_complete(&voters, &answers, &departed) {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    WireKind::Event(
                        Event::Parted {
                            channel,
                            user,
                            nick,
                            reason,
                            ..
                        }
                        | Event::Kicked {
                            channel,
                            user,
                            nick,
                            reason,
                            ..
                        },
                    ) => {
                        if let Target::Channel(ref expected_chan) = endpoint_target {
                            if casemap.str_eq(channel, expected_chan)
                                && on_user_departure(
                                    &voters,
                                    user.id(),
                                    nick.clone(),
                                    reason.clone(),
                                    &mut answers,
                                    &mut departed,
                                    &mut last_terminal,
                                    &mut stop_pred,
                                    allow_change,
                                )
                            {
                                break;
                            }
                        }
                    }
                    WireKind::Event(Event::Quit {
                        user, nick, reason, ..
                    }) => {
                        if on_user_departure(
                            &voters,
                            user.id(),
                            nick.clone(),
                            reason.clone(),
                            &mut answers,
                            &mut departed,
                            &mut last_terminal,
                            &mut stop_pred,
                            allow_change,
                        ) {
                            break;
                        }
                    }
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

                        let target_matches = match (&endpoint_target, &msg.target) {
                            (Target::Dm, Target::Dm) => true,
                            (Target::Channel(expected), Target::Channel(actual)) => {
                                casemap.str_eq(expected, actual)
                            }
                            _ => false,
                        };
                        if !target_matches {
                            continue;
                        }
                        if voters.is_some()
                            && (!answers.index_by_id.contains_key(&msg.sender.id())
                                || departed.contains(&msg.sender.id()))
                        {
                            continue;
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
                                    send_reply_to_msg(endpoint.chat(), msg.is_notice(), ack_msg);
                                }
                                answers.record_answer(&msg.sender, val);
                                if let Some(ref mut pred) = stop_pred {
                                    if pred(&answers) {
                                        break;
                                    }
                                }
                                if is_naturally_complete(&voters, &answers, &departed) {
                                    break;
                                }
                            }
                            AskMatchInner::Reject(feedback) => {
                                send_reply_to_msg(endpoint.chat(), msg.is_notice(), feedback);
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

#[allow(clippy::too_many_arguments)]
fn on_user_departure<O>(
    voters: &Option<Vec<User>>,
    uid: UserId,
    nick: String,
    reason: Option<String>,
    answers: &mut Answers<O>,
    departed: &mut HashSet<UserId>,
    last_terminal: &mut Option<Answer<()>>,
    stop_pred: &mut Option<StopPredicate<'_, O>>,
    allow_change: bool,
) -> bool {
    let Some(vs) = voters.as_deref() else {
        if answers.evict_open(uid) && !answers.is_empty() {
            if let Some(ref mut pred) = stop_pred {
                return pred(answers);
            }
        }
        return false;
    };
    let Some(&idx) = answers.index_by_id.get(&uid) else {
        return false;
    };
    departed.insert(uid);
    answers.record_left_if_pending(idx, nick, reason, last_terminal);
    if allow_change {
        departed.len() == vs.len()
    } else {
        vs.iter()
            .all(|u| answers.has_answered(u.id()) || departed.contains(&u.id()))
    }
}

async fn run_ask<'a, F, R>(
    cfg: AskConfig,
    source: AskSource<'a>,
    prompt: Text,
    matcher: F,
) -> Result<Answer<R::Output>>
where
    F: FnMut(&ChatMessage) -> R + Send + 'a,
    R: IntoAskMatch + 'a,
    R::Output: Send + 'a,
{
    if prompt.is_empty() {
        return Err(Error::InvalidInput("Ask prompt cannot be empty"));
    }
    let stop_at_one: Option<StopPredicate<'a, R::Output>> =
        Some(Box::new(|answers: &Answers<R::Output>| !answers.is_empty()));

    let res = match source {
        AskSource::Invalid(msg) => return Err(Error::InvalidInput(msg)),
        AskSource::Endpoint {
            mut endpoint,
            restricted_users,
            caller,
        } => {
            run_prompt(
                endpoint.as_mut(),
                caller,
                restricted_users,
                cfg,
                prompt,
                matcher,
                false,
                stop_at_one,
            )
            .await
        }
        AskSource::UserDm { user, caller } => match user.exclusive_at(caller).await {
            Ok(dm) => {
                let mut endpoint = ChatEndpoint(dm);
                run_prompt(
                    &mut endpoint,
                    caller,
                    None,
                    cfg,
                    prompt,
                    matcher,
                    false,
                    stop_at_one,
                )
                .await
            }
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

async fn run_ask_many<'a, F, R>(
    unstarted: AskManyUnstarted<'a, F, R::Output>,
) -> Result<Answers<R::Output>>
where
    F: FnMut(&ChatMessage) -> R + Send + 'a,
    R: IntoAskMatch + 'a,
    R::Output: Send + 'a,
{
    let AskManyUnstarted {
        mut endpoint,
        caller,
        restricted_users,
        cfg,
        prompt,
        matcher,
        allow_change,
        stop_pred,
        invalid_max_zero,
    } = unstarted;

    if prompt.is_empty() {
        return Err(Error::InvalidInput("Ask prompt cannot be empty"));
    }
    if invalid_max_zero {
        return Err(Error::InvalidInput("AskMany::max_answers requires max >= 1"));
    }
    let is_open_group = endpoint.chat().participants().is_none() && restricted_users.is_none();
    if matches!(cfg.time_limit, TimeLimit::None) && stop_pred.is_none() {
        if allow_change {
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

    run_prompt(
        endpoint.as_mut(),
        caller,
        restricted_users,
        cfg,
        prompt,
        matcher,
        allow_change,
        stop_pred,
    )
    .await
    .map(|out| out.answers)
}

#[cfg(test)]
#[path = "ask_test.rs"]
mod test;
