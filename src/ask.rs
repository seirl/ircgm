use std::collections::{HashMap, HashSet};
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

use crate::bot::BotCore;
use crate::error::{Error, Result};
use crate::exclusive_dm::ExclusiveDm;
use crate::huddle::{Huddle, HuddleSender};
use crate::inbox::Inbox;
use crate::message::{ChatMessage, Event, Target, WireEvent, WireKind};
use crate::outbound::{OutboundChatKind, WriteReceipt};
use crate::room::Room;
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

/// Outcome returned by [`Ask`] matchers that want to send an inline confirmation or
/// validation feedback without restarting the reply timer or leaving the `Ask` loop.
///
/// Construct with [`AskMatch::accept`], [`AskMatch::accept_say`], [`AskMatch::reject`], or
/// [`AskMatch::ignore`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskMatch<T>(AskMatchInner<T>);

impl<T> AskMatch<T> {
    /// Accepts `val` silently and resolves the [`Ask`] with [`Answer::Ok`].
    pub fn accept(val: T) -> Self {
        Self(AskMatchInner::Accept(val, None))
    }

    /// Accepts `val`, immediately enqueues `msg` to the prompt's target (in-channel for
    /// [`Room`], in-DM while [`ExclusiveDm`] is still held for [`User`]/[`ExclusiveDm`], or
    /// broadcast to all members for [`Huddle`]), and resolves the [`Ask`] with [`Answer::Ok`].
    /// `msg` is sent as a `NOTICE` when the accepted reply was one.
    pub fn accept_say(val: T, msg: impl Into<Text>) -> Self {
        Self(AskMatchInner::Accept(val, Some(msg.into())))
    }

    /// Sends validation feedback `msg` to the responder (as a `NOTICE` if the reply was one) and
    /// keeps waiting without resetting the reply timer.
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
            Answer::Left { nick, reason } => {
                panic!(
                    "called `Answer::unwrap()` on `Answer::Left {{ nick: {nick:?}, reason: {reason:?} }}`"
                )
            }
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

/// Context passed to [`AskConfig::timeout_message_fn`] and [`Ask::timeout_message_fn`]
/// when an [`Ask`]'s own reply timer expires.
pub struct TimeoutCtx<'a> {
    /// Where the prompt was sent (and where any timeout message will be sent):
    /// `Target::Channel("#blackjack")` or `Target::Dm`.
    pub target: &'a Target,
    /// The specific player being waited on (`Some(user)` on `room.ask_user`, `user.ask`, `dm.ask`;
    /// `None` on open-floor `room.ask` and group `huddle.ask`).
    pub user: Option<&'a User>,
    /// `Some(dur)` when configured with a relative timeout (`reply_timeout`),
    /// or `None` when configured with an absolute `Instant` (`reply_deadline`).
    pub configured_timeout: Option<Duration>,
    /// Actual wall-clock time spent waiting on the reply after `OutboundSink`'s write receipt resolved.
    pub elapsed: Duration,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum TimeLimit {
    None,
    Duration(Duration),
    Deadline(Instant),
}

impl TimeLimit {
    pub(crate) fn deadline_from(self, base: Instant) -> Option<Instant> {
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

/// Reusable [`Ask`] configuration sharing an exact 1-to-1 builder vocabulary with [`Ask`].
#[derive(Clone, Debug)]
pub struct AskConfig {
    pub(crate) time_limit: TimeLimit,
    pub(crate) warnings: Vec<(Duration, Text)>,
    pub(crate) timeout_handler: Option<TimeoutHandler>,
    pub(crate) ignore_commands: bool,
    pub(crate) accept_notices: bool,
}

impl Default for AskConfig {
    fn default() -> Self {
        Self {
            time_limit: TimeLimit::Duration(Duration::from_secs(300)),
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

    /// Undoes [`AskConfig::ignore_commands`]: command-prefixed lines are matched again (the
    /// default).
    pub fn allow_commands(mut self) -> Self {
        self.ignore_commands = false;
        self
    }

    /// Undoes [`AskConfig::accept_notices`]: human `NOTICE`s are skipped again (the default).
    pub fn ignore_notices(mut self) -> Self {
        self.accept_notices = false;
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

    pub(crate) fn apply_flags_to_inbox(&self, inbox: &mut Inbox) {
        inbox.ignore_commands = self.ignore_commands;
        inbox.accept_notices = self.accept_notices;
    }

    pub(crate) fn apply_to_locked_inbox(&self, inbox: &mut Inbox) {
        self.apply_flags_to_inbox(inbox);
        inbox.read_limit = self.time_limit;
    }
}

pub(crate) enum AskSource<'a> {
    RoomOpen {
        room: &'a Room,
        caller: &'static Location<'static>,
    },
    RoomUser {
        room: &'a Room,
        user: &'a User,
        caller: &'static Location<'static>,
    },
    RoomUsers {
        room: &'a Room,
        users: Vec<User>,
        caller: &'static Location<'static>,
    },
    UserDm {
        user: &'a User,
        caller: &'static Location<'static>,
    },
    BorrowedDm {
        dm: &'a mut ExclusiveDm,
    },
    BorrowedHuddle {
        huddle: &'a mut Huddle,
    },
    HuddleMember {
        huddle: &'a mut Huddle,
        user: User,
    },
    ImmediateErr(Error),
}

/// Delivery bookkeeping of one running [`Ask`], owned by its wire-event hook (see
/// [`EndpointSender::check_event`]): which DM recipients bounced, and how many times each one's
/// prompt was re-sent after a `401` rename race.
#[derive(Default)]
struct DeliveryState {
    bounced: HashSet<UserId>,
    rename_retries: HashMap<UserId, u8>,
}

enum EndpointSender<'a> {
    RoomOpen {
        room: &'a Room,
    },
    RoomUser {
        room: &'a Room,
        user: &'a User,
    },
    Dm {
        core: &'a Arc<BotCore>,
        user: &'a User,
        prompt: Text,
    },
    Huddle {
        sender: HuddleSender<'a>,
        prompt: Text,
    },
}

impl<'a> EndpointSender<'a> {
    fn timeout_target_and_user(&self) -> (Target, Option<&'a User>) {
        match self {
            EndpointSender::RoomOpen { room } => (Target::Channel(room.name().to_owned()), None),
            EndpointSender::RoomUser { room, user } => {
                (Target::Channel(room.name().to_owned()), Some(user))
            }
            EndpointSender::Dm { user, .. } => (Target::Dm, Some(user)),
            EndpointSender::Huddle { .. } => (Target::Dm, None),
        }
    }

    /// Sends `text` where the prompt went: the channel, the DM'd user, or every huddle member.
    fn send_to_target(&self, kind: OutboundChatKind, text: Text) {
        match self {
            EndpointSender::RoomOpen { room } | EndpointSender::RoomUser { room, .. } => {
                let _ = room.send_chat(kind, text);
            }
            EndpointSender::Dm { user, .. } => {
                let _ = user.send_chat(kind, text);
            }
            EndpointSender::Huddle { sender, .. } => {
                let _ = sender.broadcast_kind(kind, text);
            }
        }
    }

    fn send_reject(&self, sender_user: &User, kind: OutboundChatKind, feedback: Text) {
        match self {
            EndpointSender::RoomOpen { .. }
            | EndpointSender::RoomUser { .. }
            | EndpointSender::Dm { .. } => self.send_to_target(kind, feedback),
            EndpointSender::Huddle { .. } => {
                let _ = sender_user.send_chat(kind, feedback);
            }
        }
    }

    fn handle_ignore(&self, msg: &ChatMessage) {
        if let EndpointSender::Huddle { sender, .. } = self {
            if sender.has_relay() {
                let _ = sender.relay_to_others(msg);
            }
        }
    }

    /// Judges a non-reply wire event: returns the error that ends the prompt once its
    /// responders are out of reach, or `None` to keep waiting.
    ///
    /// Reachability is tracked across both kinds of event that can take a responder away: DM
    /// bounces (`401`, `716`, `486`, ...) and `QUIT`s.
    fn check_event(&self, wire: &WireEvent, state: &mut DeliveryState) -> Option<Error> {
        match &wire.kind {
            WireKind::Event(Event::DeliveryFailed {
                target,
                code,
                message,
                ..
            }) => self.check_send_error(wire, target, *code, message, state),
            WireKind::Event(Event::Quit {
                user: quitter,
                nick,
                reason,
                ..
            }) => {
                let gone = || Error::UserLeft {
                    nick: nick.clone(),
                    reason: reason.clone(),
                };
                match self {
                    // `UsersInRoom` inboxes end on their own once every awaited user is gone:
                    EndpointSender::RoomOpen { .. } | EndpointSender::RoomUser { .. } => None,
                    EndpointSender::Dm { user, .. } => (quitter == *user).then(gone),
                    EndpointSender::Huddle { sender, .. } => {
                        // A member quitting can leave only bounced members behind, which the
                        // huddle inbox (alive while any member is online) never notices:
                        sender.find_matching_user(Some(quitter.track_id()))?;
                        (!sender.any_reachable_at(&state.bounced, wire.seq)).then(gone)
                    }
                }
            }
            _ => None,
        }
    }

    fn check_send_error(
        &self,
        wire: &WireEvent,
        target: &str,
        code: u16,
        message: &str,
        state: &mut DeliveryState,
    ) -> Option<Error> {
        let target_user_id = wire.bounce_user_id();
        let unreachable = || Error::UserUnreachable {
            nick: target.to_owned(),
            code,
            message: message.to_owned(),
        };
        match self {
            EndpointSender::RoomOpen { room } | EndpointSender::RoomUser { room, .. } => room
                .casemap()
                .str_eq(target, room.name())
                .then(|| Error::ChannelSendFailed {
                    channel: target.to_owned(),
                    code,
                    message: message.to_owned(),
                }),
            EndpointSender::Dm { core, user, prompt } => {
                if target_user_id != Some(user.track_id()) {
                    return None;
                }
                if code == 401 && Self::resend_to_renamed(core, user, target, prompt, state) {
                    return None;
                }
                Some(unreachable())
            }
            EndpointSender::Huddle { sender, prompt } => {
                let member = sender.find_matching_user(target_user_id)?;
                if code == 401
                    && Self::resend_to_renamed(sender.core(), member, target, prompt, state)
                {
                    return None;
                }
                state.bounced.insert(member.id());
                (!sender.any_reachable_at(&state.bounced, wire.seq)).then(unreachable)
            }
        }
    }

    /// On a `401` for `user`'s former nick, re-sends `prompt` to their current nick (bounded per
    /// user), returning `true` if it did.
    fn resend_to_renamed(
        core: &Arc<BotCore>,
        user: &User,
        bounced_nick: &str,
        prompt: &Text,
        state: &mut DeliveryState,
    ) -> bool {
        let retries = state.rename_retries.entry(user.id()).or_insert(0);
        match user.track().next_rename_retry_nick(bounced_nick, retries) {
            Some(new_nick) => {
                let _ = core.send_chat(OutboundChatKind::Privmsg, &new_nick, prompt.clone());
                true
            }
            None => false,
        }
    }
}

type BoxAskFuture<'a, O> = Pin<Box<dyn Future<Output = Result<O>> + Send + 'a>>;

struct AskUnstarted<'a, F> {
    cfg: AskConfig,
    source: AskSource<'a>,
    prompt: Text,
    matcher: F,
}

enum AskState<'a, F, O> {
    Unstarted(Box<AskUnstarted<'a, F>>),
    Running(BoxAskFuture<'a, O>),
    Done,
}

/// Lazy prompt-and-reply future returned by [`Room::ask`], [`Room::ask_user`], [`Room::ask_users`],
/// [`User::ask`], [`ExclusiveDm::ask`], and [`Huddle::ask`].
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
    pub(crate) fn new(
        source: AskSource<'a>,
        base_cfg: AskConfig,
        prompt: Text,
        matcher: F,
    ) -> Self {
        Self {
            state: AskState::Unstarted(Box::new(AskUnstarted {
                cfg: base_cfg,
                source,
                prompt,
                matcher,
            })),
        }
    }

    fn map_cfg(mut self, f: impl FnOnce(AskConfig) -> AskConfig) -> Self {
        match &mut self.state {
            AskState::Unstarted(unstarted) => {
                let taken = std::mem::take(&mut unstarted.cfg);
                unstarted.cfg = f(taken);
                self
            }
            AskState::Running(_) | AskState::Done => {
                unreachable!("Ask builder methods consume self before polling")
            }
        }
    }

    /// Applies a reusable [`AskConfig`] value to this prompt.
    pub fn config(self, cfg: &AskConfig) -> Self {
        self.map_cfg(|_| cfg.clone())
    }

    /// Overrides the post-write reply deadline for this prompt.
    pub fn reply_timeout(self, duration: Duration) -> Self {
        self.map_cfg(|c| c.reply_timeout(duration))
    }

    /// Overrides the post-write deadline for this prompt using an absolute [`Instant`].
    pub fn reply_deadline(self, instant: Instant) -> Self {
        self.map_cfg(|c| c.reply_deadline(instant))
    }

    /// Disables the reply deadline for this prompt.
    pub fn no_reply_timeout(self) -> Self {
        self.map_cfg(AskConfig::no_reply_timeout)
    }

    /// Schedules a countdown warning `msg` when `remaining` time is left on this `Ask`'s
    /// post-write reply clock.
    pub fn warn_remaining(self, remaining: Duration, msg: impl Into<Text>) -> Self {
        self.map_cfg(|c| c.warn_remaining(remaining, msg))
    }

    /// Clears any countdown warnings inherited from the handle's or `config`'s [`AskConfig`].
    pub fn clear_warnings(self) -> Self {
        self.map_cfg(AskConfig::clear_warnings)
    }

    /// Opt-in command skipping for this prompt (`BotConfig::command_prefixes`, default `["!"]`).
    pub fn ignore_commands(self) -> Self {
        self.map_cfg(AskConfig::ignore_commands)
    }

    /// Opt-in human `NOTICE` acceptance for this prompt (`MessageKind::Notice`).
    pub fn accept_notices(self) -> Self {
        self.map_cfg(AskConfig::accept_notices)
    }

    /// Matches command-prefixed lines on this prompt even if the handle's [`AskConfig`] ignores
    /// them.
    pub fn allow_commands(self) -> Self {
        self.map_cfg(AskConfig::allow_commands)
    }

    /// Skips human `NOTICE`s on this prompt even if the handle's [`AskConfig`] accepts them.
    pub fn ignore_notices(self) -> Self {
        self.map_cfg(AskConfig::ignore_notices)
    }

    /// Overrides the message sent to the target if *this `Ask`'s own reply timer* expires.
    pub fn timeout_message(self, msg: impl Into<Text>) -> Self {
        self.map_cfg(|c| c.timeout_message(msg))
    }

    pub fn timeout_message_fn<G, M>(self, f: G) -> Self
    where
        G: Fn(&TimeoutCtx<'_>) -> M + Send + Sync + 'static,
        M: Into<Text>,
    {
        self.map_cfg(|c| c.timeout_message_fn(f))
    }

    /// Suppresses any inherited timeout message on this prompt.
    pub fn no_timeout_message(self) -> Self {
        self.map_cfg(AskConfig::no_timeout_message)
    }
}

fn make_span(cfg: &AskConfig, source: &AskSource<'_>) -> tracing::Span {
    let timeout_ms = match cfg.time_limit {
        TimeLimit::Duration(d) => Some(d.as_millis() as u64),
        TimeLimit::Deadline(_) | TimeLimit::None => None,
    };
    match source {
        AskSource::RoomOpen { room, .. } | AskSource::RoomUsers { room, .. } => {
            tracing::debug_span!(
                "ircgm::ask",
                target = room.name(),
                user = Option::<&str>::None,
                timeout_ms = timeout_ms
            )
        }
        AskSource::RoomUser { room, user, .. } => tracing::debug_span!(
            "ircgm::ask",
            target = room.name(),
            user = %user.nick(),
            timeout_ms = timeout_ms
        ),
        AskSource::UserDm { user, .. } => tracing::debug_span!(
            "ircgm::ask",
            target = "DM",
            user = %user.nick(),
            timeout_ms = timeout_ms
        ),
        AskSource::BorrowedDm { dm } => tracing::debug_span!(
            "ircgm::ask",
            target = "DM",
            user = %dm.user().nick(),
            timeout_ms = timeout_ms
        ),
        AskSource::BorrowedHuddle { .. } => tracing::debug_span!(
            "ircgm::ask",
            target = "Huddle",
            user = Option::<&str>::None,
            timeout_ms = timeout_ms
        ),
        AskSource::HuddleMember { user, .. } => tracing::debug_span!(
            "ircgm::ask",
            target = "Huddle",
            user = %user.nick(),
            timeout_ms = timeout_ms
        ),
        AskSource::ImmediateErr(_) => tracing::debug_span!("ircgm::ask", timeout_ms = timeout_ms),
    }
}

fn compute_schedule(
    cfg: &AskConfig,
    write_completed_at: Instant,
) -> (Option<Duration>, Option<Instant>, Vec<(Instant, Text)>) {
    let Some(deadline) = cfg.time_limit.deadline_from(write_completed_at) else {
        return (None, None, Vec::new());
    };
    let configured_timeout = match cfg.time_limit {
        TimeLimit::Duration(dur) => Some(dur),
        TimeLimit::None | TimeLimit::Deadline(_) => None,
    };
    let budget = deadline.saturating_duration_since(write_completed_at);
    let mut warns: Vec<_> = cfg
        .warnings
        .iter()
        .filter(|(rem, _)| !rem.is_zero() && budget > *rem)
        .map(|(rem, msg)| (deadline - *rem, msg.clone()))
        .collect();
    warns.sort_by_key(|(at, _)| *at);
    (configured_timeout, Some(deadline), warns)
}

async fn await_ask_loop<'a, F, R>(
    cfg: &AskConfig,
    matcher: &mut F,
    sender: EndpointSender<'a>,
    inbox: &mut Inbox,
    receipt: WriteReceipt,
) -> Result<R::Output>
where
    F: FnMut(&ChatMessage) -> R + Send,
    R: IntoAskMatch,
{
    receipt.wait().await;
    let write_completed_at = Instant::now();
    tracing::debug!("ask prompt written; waiting for reply");

    let (configured_timeout, deadline, warnings) = compute_schedule(cfg, write_completed_at);

    let mut delivery = DeliveryState::default();
    let reply_fut = inbox.wait_reply_map(
        cfg.ignore_commands,
        cfg.accept_notices,
        |wire| {
            let err = sender.check_event(wire, &mut delivery);
            if let Some(ref e) = err {
                tracing::debug!(error = %e, "ask responders out of reach");
            }
            err
        },
        |msg| match matcher(msg).into_ask_match().0 {
            // Answers to a reply mirror its kind: a `NOTICE` must never trigger an automatic
            // `PRIVMSG` (RFC 2812 §3.3.2).
            AskMatchInner::Accept(val, ack) => {
                tracing::debug!(
                    sender = %msg.nick,
                    has_ack = ack.is_some(),
                    "ask reply accepted"
                );
                if let Some(ack_msg) = ack {
                    sender.send_to_target(msg.reply_kind(), ack_msg);
                }
                Some(val)
            }
            AskMatchInner::Reject(feedback) => {
                sender.send_reject(&msg.sender, msg.reply_kind(), feedback);
                None
            }
            AskMatchInner::Ignore => {
                sender.handle_ignore(msg);
                None
            }
        },
    );

    let timer_fut = async {
        match deadline {
            None => std::future::pending::<()>().await,
            Some(dl) => {
                let send_warnings = async {
                    for (warn_at, warn_msg) in warnings {
                        tokio::time::sleep_until(warn_at).await;
                        if Instant::now() < dl {
                            sender.send_to_target(OutboundChatKind::Privmsg, warn_msg);
                        }
                    }
                    std::future::pending::<()>().await
                };
                tokio::select! {
                    biased;
                    () = tokio::time::sleep_until(dl) => {}
                    () = send_warnings => {}
                }
            }
        }
    };

    tokio::select! {
        biased;
        res = reply_fut => {
            if let Err(ref e) = res {
                tracing::debug!(error = %e, "ask interrupted");
            }
            res
        }
        () = timer_fut => {
            if let Some(handler) = &cfg.timeout_handler {
                let (target, user_ref) = sender.timeout_target_and_user();
                let ctx = TimeoutCtx {
                    target: &target,
                    user: user_ref,
                    configured_timeout,
                    elapsed: write_completed_at.elapsed(),
                };
                sender.send_to_target(OutboundChatKind::Privmsg, handler.render(&ctx));
            }
            tracing::debug!("ask reply timer expired");
            Err(Error::TimedOut)
        }
    }
}

async fn run_ask<'a, F, R>(
    cfg: AskConfig,
    source: AskSource<'a>,
    prompt: Text,
    mut matcher: F,
) -> Result<R::Output>
where
    F: FnMut(&ChatMessage) -> R + Send + 'a,
    R: IntoAskMatch + 'a,
    R::Output: Send + 'a,
{
    if prompt.is_empty() {
        return Err(Error::InvalidInput("Ask prompt cannot be empty"));
    }
    match source {
        AskSource::ImmediateErr(err) => Err(err),
        AskSource::RoomOpen { room, caller } => {
            let inbox = room.inbox_at(caller);
            let sender = EndpointSender::RoomOpen { room };
            ask_in_room(&cfg, &mut matcher, room, inbox, sender, prompt).await
        }
        AskSource::RoomUser { room, user, caller } => {
            if !user.belongs_to(room.core()) {
                return Err(Error::InvalidInput(
                    "User handle belongs to a different Bot session",
                ));
            }
            room.ensure_user_askable(user)?;
            let inbox = room.inbox_users_at(std::slice::from_ref(user), caller);
            let sender = EndpointSender::RoomUser { room, user };
            ask_in_room(&cfg, &mut matcher, room, inbox, sender, prompt).await
        }
        AskSource::RoomUsers {
            room,
            users,
            caller,
        } => {
            room.ensure_any_user_askable(&users)?;
            let inbox = room.inbox_users_at(&users, caller);
            let sender = EndpointSender::RoomOpen { room };
            ask_in_room(&cfg, &mut matcher, room, inbox, sender, prompt).await
        }
        AskSource::UserDm { user, caller } => {
            let mut dm = match user.exclusive_at(caller).await {
                Ok(dm) => dm,
                Err(e) => {
                    tracing::debug!(error = %e, "ask aborted before DM prompt write");
                    return Err(e);
                }
            };
            ask_in_dm(&cfg, &mut matcher, &mut dm, prompt).await
        }
        AskSource::BorrowedDm { dm } => ask_in_dm(&cfg, &mut matcher, dm, prompt).await,
        AskSource::BorrowedHuddle { huddle } => {
            let (sender, inbox) = huddle.split_for_ask();
            // Fail before clearing the shared inbox, which holds other members' unread messages:
            sender.core().check_connected()?;
            inbox.clear();
            let receipt = sender.broadcast_prompt_with_receipt(prompt.clone())?;
            let sender = EndpointSender::Huddle { sender, prompt };
            await_ask_loop(&cfg, &mut matcher, sender, inbox, receipt).await
        }
        AskSource::HuddleMember { huddle, user } => {
            let (members, inbox) = huddle.split_for_ask();
            // Fail before clearing the shared inbox, which holds other members' unread messages:
            members.core().check_connected()?;
            if let Some(err) = user.quit_error() {
                return Err(err);
            }
            inbox.clear();
            let receipt = user.send_chat(OutboundChatKind::Privmsg, prompt.clone())?;
            // Only `user` answers, over the huddle's single inbox; teammates' banter meanwhile is
            // relayed (if enabled) rather than matched:
            let mut member_matcher = |msg: &ChatMessage| {
                if msg.sender == user {
                    matcher(msg).into_ask_match()
                } else {
                    if members.has_relay() {
                        let _ = members.relay_to_others(msg);
                    }
                    AskMatch::ignore()
                }
            };
            // The huddle inbox outlives `user`, so `Dm`'s own `QUIT` check ends the prompt:
            let sender = EndpointSender::Dm {
                core: members.core(),
                user: &user,
                prompt,
            };
            await_ask_loop(&cfg, &mut member_matcher, sender, inbox, receipt).await
        }
    }
}

/// Channel prompt: writes `prompt` to `room` (which fails if the bot is known to be out of it)
/// and awaits a reply on the already-subscribed `inbox`.
async fn ask_in_room<F, R>(
    cfg: &AskConfig,
    matcher: &mut F,
    room: &Room,
    mut inbox: Inbox,
    sender: EndpointSender<'_>,
    prompt: Text,
) -> Result<R::Output>
where
    F: FnMut(&ChatMessage) -> R + Send,
    R: IntoAskMatch,
{
    let receipt = room.send_chat(OutboundChatKind::Privmsg, prompt)?;
    await_ask_loop(cfg, matcher, sender, &mut inbox, receipt).await
}

/// DM prompt over a held [`ExclusiveDm`]: drops any backlog, writes `prompt` (which fails if
/// the session or the user is gone) and awaits a reply on the DM's own inbox.
async fn ask_in_dm<F, R>(
    cfg: &AskConfig,
    matcher: &mut F,
    dm: &mut ExclusiveDm,
    prompt: Text,
) -> Result<R::Output>
where
    F: FnMut(&ChatMessage) -> R + Send,
    R: IntoAskMatch,
{
    dm.inbox().clear();
    let receipt = dm
        .user()
        .send_chat(OutboundChatKind::Privmsg, prompt.clone())?;
    let (core, user, inbox) = dm.split_for_ask();
    let sender = EndpointSender::Dm { core, user, prompt };
    await_ask_loop(cfg, matcher, sender, inbox, receipt).await
}

impl<'a, F, R> Future for Ask<'a, F, R>
where
    F: FnMut(&ChatMessage) -> R + Send + 'a,
    R: IntoAskMatch + 'a,
    R::Output: Send + 'a,
{
    type Output = Result<Answer<R::Output>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if matches!(this.state, AskState::Unstarted(_)) {
            let AskState::Unstarted(unstarted) =
                std::mem::replace(&mut this.state, AskState::Done)
            else {
                unreachable!()
            };
            let AskUnstarted {
                cfg,
                source,
                prompt,
                matcher,
            } = *unstarted;
            let span = make_span(&cfg, &source);
            let fut = run_ask(cfg, source, prompt, matcher).instrument(span);
            this.state = AskState::Running(Box::pin(fut));
        }

        match &mut this.state {
            AskState::Running(fut) => match fut.as_mut().poll(cx) {
                Poll::Ready(res) => {
                    this.state = AskState::Done;
                    Poll::Ready(Answer::from_result(res))
                }
                Poll::Pending => Poll::Pending,
            },
            AskState::Done => panic!("Ask polled after completion"),
            AskState::Unstarted(_) => unreachable!(),
        }
    }
}

#[cfg(test)]
#[path = "ask_test.rs"]
mod test;

