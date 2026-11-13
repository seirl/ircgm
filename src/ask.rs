use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::panic::Location;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use irc_colors::Text;
use tokio::time::{Instant, Sleep};

use crate::bot::BotCore;
use crate::error::{Error, Result};
use crate::huddle::Huddle;
use crate::inbox::Inbox;
use crate::message::{ChatMessage, Target, WireKind};
use crate::outbound::{OutboundChatKind, WriteReceipt};
use crate::room::Room;
use crate::user::{ExclusiveDm, User};

mod private {
    pub trait Sealed {}
    impl<T> Sealed for Option<T> {}
    impl<T> Sealed for super::Verdict<T> {}
}

/// Tri-state result for [`Ask`] matchers that want to send validation feedback
/// without restarting the reply timer or leaving the `Ask` loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict<T> {
    /// Accepts `T` and resolves the `Ask` with `Ok(T)`.
    Accept(T),
    /// Sends validation feedback `Text` to the `Ask`'s target and keeps waiting
    /// **without** resetting the reply timer.
    Reject(Text),
    /// Silently ignores this message and keeps waiting.
    Ignore,
}

impl<T> Verdict<T> {
    pub fn reject(msg: impl Into<Text>) -> Self {
        Self::Reject(msg.into())
    }
}

/// Sealed trait implemented for both `Option<T>` (`Some(v) -> Accept(v)`, `None -> Ignore`)
/// and [`Verdict<T>`].
pub trait IntoVerdict: private::Sealed {
    type Output;
    fn into_verdict(self) -> Verdict<Self::Output>;
}

impl<T> IntoVerdict for Option<T> {
    type Output = T;

    fn into_verdict(self) -> Verdict<T> {
        match self {
            Some(v) => Verdict::Accept(v),
            None => Verdict::Ignore,
        }
    }
}

impl<T> IntoVerdict for Verdict<T> {
    type Output = T;

    fn into_verdict(self) -> Verdict<T> {
        self
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

    pub(crate) fn apply_to_inbox(&self, inbox: &mut Inbox) {
        match self.time_limit {
            TimeLimit::None => inbox.clear_read_timeout(),
            TimeLimit::Duration(d) => inbox.set_read_timeout(d),
            TimeLimit::Deadline(dl) => inbox.set_read_deadline(dl),
        }
        inbox.set_ignore_commands(self.ignore_commands);
        inbox.set_accept_notices(self.accept_notices);
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
}

enum ActiveEndpoint<'a> {
    RoomOpen {
        room: &'a Room,
        inbox: Inbox,
    },
    RoomUser {
        room: &'a Room,
        user: &'a User,
        inbox: Inbox,
    },
    OwnedDm {
        user: &'a User,
        dm: ExclusiveDm,
    },
    BorrowedDm {
        dm: &'a mut ExclusiveDm,
    },
    BorrowedHuddle {
        huddle: &'a mut Huddle,
        bounced_users: HashSet<u64>,
    },
}

impl<'a> ActiveEndpoint<'a> {
    fn core(&self) -> Option<&Arc<BotCore>> {
        match self {
            ActiveEndpoint::RoomOpen { room, .. } | ActiveEndpoint::RoomUser { room, .. } => {
                Some(room.core())
            }
            ActiveEndpoint::OwnedDm { dm, .. } => Some(dm.core()),
            ActiveEndpoint::BorrowedDm { dm } => Some(dm.core()),
            ActiveEndpoint::BorrowedHuddle { huddle, .. } => huddle.core(),
        }
    }

    fn inbox_mut(&mut self) -> &mut Inbox {
        match self {
            ActiveEndpoint::RoomOpen { inbox, .. } => inbox,
            ActiveEndpoint::RoomUser { inbox, .. } => inbox,
            ActiveEndpoint::OwnedDm { dm, .. } => dm.inbox(),
            ActiveEndpoint::BorrowedDm { dm } => dm.inbox(),
            ActiveEndpoint::BorrowedHuddle { huddle, .. } => huddle.inbox(),
        }
    }

    fn timeout_target_and_user(&self) -> (Target, Option<User>) {
        match self {
            ActiveEndpoint::RoomOpen { room, .. } => {
                (Target::Channel(room.name().to_owned()), None)
            }
            ActiveEndpoint::RoomUser { room, user, .. } => {
                (Target::Channel(room.name().to_owned()), Some((*user).clone()))
            }
            ActiveEndpoint::OwnedDm { user, .. } => (Target::Dm, Some((*user).clone())),
            ActiveEndpoint::BorrowedDm { dm } => (Target::Dm, Some(dm.user().clone())),
            ActiveEndpoint::BorrowedHuddle { .. } => (Target::Dm, None),
        }
    }

    fn send_broadcast_or_target(&self, text: Text) {
        match self {
            ActiveEndpoint::RoomOpen { room, .. } | ActiveEndpoint::RoomUser { room, .. } => {
                let _ = room.say(text);
            }
            ActiveEndpoint::OwnedDm { dm, .. } => {
                let _ = dm.say(text);
            }
            ActiveEndpoint::BorrowedDm { dm } => {
                let _ = dm.say(text);
            }
            ActiveEndpoint::BorrowedHuddle { huddle, .. } => {
                let _ = huddle.say(text);
            }
        }
    }

    fn send_reject(&self, sender: &User, feedback: Text) {
        match self {
            ActiveEndpoint::RoomOpen { room, .. } | ActiveEndpoint::RoomUser { room, .. } => {
                let _ = room.say(feedback);
            }
            ActiveEndpoint::OwnedDm { dm, .. } => {
                let _ = dm.say(feedback);
            }
            ActiveEndpoint::BorrowedDm { dm } => {
                let _ = dm.say(feedback);
            }
            ActiveEndpoint::BorrowedHuddle { .. } => {
                let _ = sender.say(feedback);
            }
        }
    }

    fn handle_ignore(&self, msg: &ChatMessage) {
        if let ActiveEndpoint::BorrowedHuddle { huddle, .. } = self {
            if huddle.has_relay() {
                let _ = huddle.relay_to_others(msg);
            }
        }
    }

    fn check_send_error(
        &mut self,
        target: &str,
        target_user_id: Option<u64>,
        code: u16,
        message: &str,
    ) -> Option<Error> {
        match self {
            ActiveEndpoint::RoomOpen { room, .. } | ActiveEndpoint::RoomUser { room, .. } => {
                if room.casemap().str_eq(target, room.name()) {
                    return Some(Error::DeliveryFailed {
                        target: target.to_owned(),
                        code,
                        message: message.to_owned(),
                    });
                }
                None
            }
            ActiveEndpoint::OwnedDm { user, .. } => {
                if target_user_id == Some(user.track_id()) || user.eq_nick(target) {
                    return Some(Error::DeliveryFailed {
                        target: target.to_owned(),
                        code,
                        message: message.to_owned(),
                    });
                }
                None
            }
            ActiveEndpoint::BorrowedDm { dm } => {
                let u = dm.user();
                if target_user_id == Some(u.track_id()) || u.eq_nick(target) {
                    return Some(Error::DeliveryFailed {
                        target: target.to_owned(),
                        code,
                        message: message.to_owned(),
                    });
                }
                None
            }
            ActiveEndpoint::BorrowedHuddle {
                huddle,
                bounced_users,
            } => {
                let matched_uid = target_user_id.filter(|id| huddle.contains_user_id(*id)).or_else(
                    || {
                        huddle
                            .users()
                            .iter()
                            .find(|u| u.eq_nick(target))
                            .map(User::track_id)
                    },
                );
                if let Some(uid) = matched_uid {
                    bounced_users.insert(uid);
                    let any_reachable_online = huddle
                        .users()
                        .iter()
                        .any(|u| u.is_online() && !bounced_users.contains(&u.track_id()));
                    if !any_reachable_online {
                        return Some(Error::DeliveryFailed {
                            target: target.to_owned(),
                            code,
                            message: message.to_owned(),
                        });
                    }
                }
                None
            }
        }
    }
}

type BoxLockFut<'a> = Pin<Box<dyn Future<Output = Result<ExclusiveDm>> + Send + 'a>>;

enum AskState<'a> {
    Unstarted {
        source: AskSource<'a>,
        prompt: Text,
    },
    AcquiringDm {
        user: &'a User,
        lock_fut: BoxLockFut<'a>,
        prompt: Text,
        span: tracing::Span,
    },
    AwaitingReceipt {
        endpoint: ActiveEndpoint<'a>,
        receipt: WriteReceipt,
        span: tracing::Span,
    },
    WaitingReply {
        endpoint: ActiveEndpoint<'a>,
        write_completed_at: Instant,
        configured_timeout: Option<Duration>,
        reply_sleep: Option<Pin<Box<Sleep>>>,
        warnings: Vec<(Pin<Box<Sleep>>, Text)>,
        span: tracing::Span,
    },
    Done,
}

/// Lazy prompt-and-reply future returned by [`Room::ask`], [`Room::ask_user`], [`User::ask`],
/// [`ExclusiveDm::ask`], and [`Huddle::ask`].
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Ask<'a, F, R> {
    cfg: AskConfig,
    matcher: Box<F>,
    state: AskState<'a>,
    _marker: std::marker::PhantomData<fn() -> R>,
}

struct BuiltTimers {
    configured_timeout: Option<Duration>,
    reply_sleep: Option<Pin<Box<Sleep>>>,
    warnings: Vec<(Pin<Box<Sleep>>, Text)>,
}

impl<'a, F, R> Ask<'a, F, R> {
    pub(crate) fn new(
        source: AskSource<'a>,
        base_cfg: AskConfig,
        prompt: Text,
        matcher: F,
    ) -> Self {
        Self {
            cfg: base_cfg,
            matcher: Box::new(matcher),
            state: AskState::Unstarted { source, prompt },
            _marker: std::marker::PhantomData,
        }
    }

    /// Applies a reusable [`AskConfig`] value to this prompt.
    pub fn config(mut self, cfg: &AskConfig) -> Self {
        self.cfg = cfg.clone();
        self
    }

    /// Overrides the post-write reply deadline for this prompt.
    pub fn reply_timeout(mut self, duration: Duration) -> Self {
        self.cfg.time_limit = TimeLimit::Duration(duration);
        self
    }

    /// Overrides the post-write deadline for this prompt using an absolute [`Instant`].
    pub fn reply_deadline(mut self, instant: Instant) -> Self {
        self.cfg.time_limit = TimeLimit::Deadline(instant);
        self
    }

    /// Disables the reply deadline for this prompt.
    pub fn no_reply_timeout(mut self) -> Self {
        self.cfg.time_limit = TimeLimit::None;
        self
    }

    /// Schedules a countdown warning `msg` when `remaining` time is left on this `Ask`'s
    /// post-write reply clock.
    pub fn warn_remaining(mut self, remaining: Duration, msg: impl Into<Text>) -> Self {
        self.cfg.warnings.push((remaining, msg.into()));
        self
    }

    /// Clears any countdown warnings inherited from the handle's or `config`'s [`AskConfig`].
    pub fn clear_warnings(mut self) -> Self {
        self.cfg.warnings.clear();
        self
    }

    /// Opt-in command skipping for this prompt (`BotConfig::command_prefixes`, default `["!"]`).
    pub fn ignore_commands(mut self) -> Self {
        self.cfg.ignore_commands = true;
        self
    }

    /// Opt-in human `NOTICE` acceptance for this prompt (`MessageKind::Notice`).
    pub fn accept_notices(mut self) -> Self {
        self.cfg.accept_notices = true;
        self
    }

    /// Overrides the message sent to the target if *this `Ask`'s own reply timer* expires.
    pub fn timeout_message(mut self, msg: impl Into<Text>) -> Self {
        self.cfg.timeout_handler = Some(TimeoutHandler::Static(msg.into()));
        self
    }

    pub fn timeout_message_fn<G, M>(mut self, f: G) -> Self
    where
        G: Fn(&TimeoutCtx<'_>) -> M + Send + Sync + 'static,
        M: Into<Text>,
    {
        self.cfg.timeout_handler =
            Some(TimeoutHandler::Dynamic(Arc::new(move |ctx| f(ctx).into())));
        self
    }

    /// Suppresses any inherited timeout message on this prompt.
    pub fn no_timeout_message(mut self) -> Self {
        self.cfg.timeout_handler = None;
        self
    }

    fn build_timers(cfg: &AskConfig, write_completed_at: Instant) -> BuiltTimers {
        match cfg.time_limit {
            TimeLimit::None => BuiltTimers {
                configured_timeout: None,
                reply_sleep: None,
                warnings: Vec::new(),
            },
            TimeLimit::Duration(dur) => {
                let deadline = write_completed_at + dur;
                let mut warns = Vec::new();
                for (rem, msg) in &cfg.warnings {
                    if dur > *rem {
                        let warn_at = write_completed_at + (dur - *rem);
                        warns.push((warn_at, msg.clone()));
                    }
                }
                warns.sort_by_key(|(at, _)| *at);
                let warn_sleeps = warns
                    .into_iter()
                    .map(|(at, m)| (Box::pin(tokio::time::sleep_until(at)), m))
                    .collect();
                BuiltTimers {
                    configured_timeout: Some(dur),
                    reply_sleep: Some(Box::pin(tokio::time::sleep_until(deadline))),
                    warnings: warn_sleeps,
                }
            }
            TimeLimit::Deadline(deadline) => {
                let budget = deadline.saturating_duration_since(write_completed_at);
                let mut warns = Vec::new();
                for (rem, msg) in &cfg.warnings {
                    if budget > *rem {
                        let warn_at = deadline - *rem;
                        warns.push((warn_at, msg.clone()));
                    }
                }
                warns.sort_by_key(|(at, _)| *at);
                let warn_sleeps = warns
                    .into_iter()
                    .map(|(at, m)| (Box::pin(tokio::time::sleep_until(at)), m))
                    .collect();
                BuiltTimers {
                    configured_timeout: None,
                    reply_sleep: Some(Box::pin(tokio::time::sleep_until(deadline))),
                    warnings: warn_sleeps,
                }
            }
        }
    }

    fn make_span(cfg: &AskConfig, target: &str, user: Option<&str>) -> tracing::Span {
        let timeout_ms = match cfg.time_limit {
            TimeLimit::Duration(d) => Some(d.as_millis() as u64),
            TimeLimit::Deadline(_) | TimeLimit::None => None,
        };
        tracing::debug_span!(
            "ircgm::ask",
            target = target,
            user = user,
            timeout_ms = timeout_ms
        )
    }
}

impl<'a, F, R> Future for Ask<'a, F, R>
where
    F: FnMut(&ChatMessage) -> R + Send + 'a,
    R: IntoVerdict + 'a,
    R::Output: Send + 'a,
{
    type Output = Result<R::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        loop {
            let state = std::mem::replace(&mut this.state, AskState::Done);
            match state {
                AskState::Unstarted { source, prompt } => match source {
                    AskSource::RoomOpen { room, caller } => {
                        if let Err(e) = room.ensure_bot_in_room_if_tracked() {
                            return Poll::Ready(Err(e));
                        }
                        let span = Self::make_span(&this.cfg, room.name(), None);
                        let inbox = room.inbox_at(caller);
                        let receipt = match room
                            .send_chat_with_receipt(OutboundChatKind::Privmsg, prompt)
                        {
                            Ok(r) => r,
                            Err(e) => return Poll::Ready(Err(e)),
                        };
                        this.state = AskState::AwaitingReceipt {
                            endpoint: ActiveEndpoint::RoomOpen { room, inbox },
                            receipt,
                            span,
                        };
                    }
                    AskSource::RoomUser { room, user, caller } => {
                        if !user.belongs_to(room.core()) {
                            return Poll::Ready(Err(Error::InvalidInput(
                                "User handle belongs to a different Bot session",
                            )));
                        }
                        if let Err(e) = room.ensure_user_askable(user) {
                            return Poll::Ready(Err(e));
                        }
                        let user_nick = user.nick();
                        let span = Self::make_span(&this.cfg, room.name(), Some(&user_nick));
                        let inbox = user.inbox_in_at(room, caller);
                        let receipt = match room
                            .send_chat_with_receipt(OutboundChatKind::Privmsg, prompt)
                        {
                            Ok(r) => r,
                            Err(e) => return Poll::Ready(Err(e)),
                        };
                        this.state = AskState::AwaitingReceipt {
                            endpoint: ActiveEndpoint::RoomUser { room, user, inbox },
                            receipt,
                            span,
                        };
                    }
                    AskSource::UserDm { user, caller } => {
                        let nick = user.nick();
                        let span = Self::make_span(&this.cfg, "DM", Some(&nick));
                        let lock_fut = Box::pin(user.exclusive_at(caller));
                        this.state = AskState::AcquiringDm {
                            user,
                            lock_fut,
                            prompt,
                            span,
                        };
                    }
                    AskSource::BorrowedDm { dm } => {
                        if let Err(e) = dm.core().check_connected() {
                            return Poll::Ready(Err(e));
                        }
                        if let Some(err) = dm.user().quit_error() {
                            return Poll::Ready(Err(err));
                        }
                        let nick = dm.user().nick();
                        let span = Self::make_span(&this.cfg, "DM", Some(&nick));
                        dm.inbox().clear();
                        let receipt =
                            match dm.send_chat_with_receipt(OutboundChatKind::Privmsg, prompt) {
                                Ok(r) => r,
                                Err(e) => return Poll::Ready(Err(e)),
                            };
                        this.state = AskState::AwaitingReceipt {
                            endpoint: ActiveEndpoint::BorrowedDm { dm },
                            receipt,
                            span,
                        };
                    }
                    AskSource::BorrowedHuddle { huddle } => {
                        if let Some(core) = huddle.core() {
                            if let Err(e) = core.check_connected() {
                                return Poll::Ready(Err(e));
                            }
                        }
                        let span = Self::make_span(&this.cfg, "Huddle", None);
                        huddle.inbox().clear();
                        let receipt = match huddle.broadcast_prompt_with_receipt(prompt) {
                            Ok(r) => r,
                            Err(e) => return Poll::Ready(Err(e)),
                        };
                        this.state = AskState::AwaitingReceipt {
                            endpoint: ActiveEndpoint::BorrowedHuddle {
                                huddle,
                                bounced_users: HashSet::new(),
                            },
                            receipt,
                            span,
                        };
                    }
                },

                AskState::AcquiringDm {
                    user,
                    mut lock_fut,
                    prompt,
                    span,
                } => match lock_fut.as_mut().poll(cx) {
                    Poll::Ready(Ok(mut dm)) => {
                        dm.inbox().clear();
                        let receipt =
                            match dm.send_chat_with_receipt(OutboundChatKind::Privmsg, prompt) {
                                Ok(r) => r,
                                Err(e) => return Poll::Ready(Err(e)),
                            };
                        this.state = AskState::AwaitingReceipt {
                            endpoint: ActiveEndpoint::OwnedDm { user, dm },
                            receipt,
                            span,
                        };
                    }
                    Poll::Ready(Err(e)) => {
                        let _g = span.enter();
                        tracing::debug!(error = %e, "ask aborted before DM prompt write");
                        return Poll::Ready(Err(e));
                    }
                    Poll::Pending => {
                        this.state = AskState::AcquiringDm {
                            user,
                            lock_fut,
                            prompt,
                            span,
                        };
                        return Poll::Pending;
                    }
                },

                AskState::AwaitingReceipt {
                    endpoint,
                    mut receipt,
                    span,
                } => match receipt.poll(cx) {
                    Poll::Ready(()) => {
                        let write_completed_at = Instant::now();
                        let BuiltTimers {
                            configured_timeout,
                            reply_sleep,
                            warnings,
                        } = Self::build_timers(&this.cfg, write_completed_at);
                        {
                            let _g = span.enter();
                            tracing::debug!("ask prompt written; waiting for reply");
                        }
                        this.state = AskState::WaitingReply {
                            endpoint,
                            write_completed_at,
                            configured_timeout,
                            reply_sleep,
                            warnings,
                            span,
                        };
                    }
                    Poll::Pending => {
                        this.state = AskState::AwaitingReceipt {
                            endpoint,
                            receipt,
                            span,
                        };
                        return Poll::Pending;
                    }
                },

                AskState::WaitingReply {
                    mut endpoint,
                    write_completed_at,
                    configured_timeout,
                    mut reply_sleep,
                    warnings,
                    span,
                } => {
                    // 1. Drain all ready incoming wire frames (bypassing inbox's idle `read_limit`!):
                    loop {
                        match endpoint.inbox_mut().poll_next_wire(cx, false) {
                            Poll::Ready(Ok(ev)) => {
                                match &ev.kind {
                                    WireKind::SendError {
                                        target,
                                        target_user_id,
                                        code,
                                        message,
                                    } => {
                                        if let Some(err) = endpoint.check_send_error(
                                            target,
                                            *target_user_id,
                                            *code,
                                            message,
                                        ) {
                                            let _g = span.enter();
                                            tracing::debug!(error = %err, "ask delivery bounced");
                                            return Poll::Ready(Err(err));
                                        }
                                    }
                                    WireKind::Chat(msg) => {
                                        if let Some(res) = Self::evaluate_candidate(
                                            &this.cfg,
                                            &mut this.matcher,
                                            &endpoint,
                                            msg,
                                            &span,
                                        ) {
                                            return Poll::Ready(res);
                                        }
                                    }
                                    WireKind::Notice {
                                        human_chat: Some(msg),
                                        ..
                                    } if this.cfg.accept_notices => {
                                        if let Some(res) = Self::evaluate_candidate(
                                            &this.cfg,
                                            &mut this.matcher,
                                            &endpoint,
                                            msg,
                                            &span,
                                        ) {
                                            return Poll::Ready(res);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            Poll::Ready(Err(e)) => {
                                let _g = span.enter();
                                tracing::debug!(error = %e, "ask interrupted");
                                return Poll::Ready(Err(e));
                            }
                            Poll::Pending => break,
                        }
                    }

                    // 2. Check post-write reply deadline:
                    if let Some(sleep) = reply_sleep.as_mut() {
                        if sleep.as_mut().poll(cx).is_ready() {
                            if let Some(handler) = &this.cfg.timeout_handler {
                                let (target, user_owned) = endpoint.timeout_target_and_user();
                                let ctx = TimeoutCtx {
                                    target: &target,
                                    user: user_owned.as_ref(),
                                    configured_timeout,
                                    elapsed: write_completed_at.elapsed(),
                                };
                                let msg = handler.render(&ctx);
                                endpoint.send_broadcast_or_target(msg);
                            }
                            {
                                let _g = span.enter();
                                tracing::debug!("ask reply timer expired");
                            }
                            return Poll::Ready(Err(Error::TimedOut));
                        }
                    }

                    // 3. Poll any pending `warn_remaining` timers:
                    let mut remaining_warns = Vec::with_capacity(warnings.len());
                    for (mut w_sleep, w_msg) in warnings {
                        if w_sleep.as_mut().poll(cx).is_ready() {
                            endpoint.send_broadcast_or_target(w_msg);
                        } else {
                            remaining_warns.push((w_sleep, w_msg));
                        }
                    }

                    this.state = AskState::WaitingReply {
                        endpoint,
                        write_completed_at,
                        configured_timeout,
                        reply_sleep,
                        warnings: remaining_warns,
                        span,
                    };
                    return Poll::Pending;
                }

                AskState::Done => panic!("Ask polled after completion"),
            }
        }
    }
}

impl<'a, F, R> Ask<'a, F, R>
where
    F: FnMut(&ChatMessage) -> R + Send + 'a,
    R: IntoVerdict + 'a,
    R::Output: Send + 'a,
{
    fn evaluate_candidate(
        cfg: &AskConfig,
        matcher: &mut F,
        endpoint: &ActiveEndpoint<'a>,
        msg: &ChatMessage,
        span: &tracing::Span,
    ) -> Option<Result<R::Output>> {
        if cfg.ignore_commands {
            if let Some(core) = endpoint.core() {
                if core.is_command_text(&msg.text) {
                    return None;
                }
            }
        }

        let verdict = matcher(msg).into_verdict();
        match verdict {
            Verdict::Accept(val) => {
                let _g = span.enter();
                tracing::debug!(sender = %msg.nick, seq = msg.seq, "ask reply accepted");
                Some(Ok(val))
            }
            Verdict::Reject(feedback) => {
                endpoint.send_reject(&msg.sender, feedback);
                None
            }
            Verdict::Ignore => {
                endpoint.handle_ignore(msg);
                None
            }
        }
    }
}
