use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::panic::Location;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use irc_colors::Text;
use tokio::time::Instant;
use tracing::Instrument as _;

use crate::bot::BotCore;
use crate::error::{Error, Result};
use crate::huddle::{Huddle, HuddleSender};
use crate::inbox::Inbox;
use crate::message::{ChatMessage, Target};
use crate::outbound::{OutboundChatKind, WriteReceipt};
use crate::room::Room;
use crate::user::{ExclusiveDm, User, UserId};

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
    },
    Huddle {
        sender: HuddleSender<'a>,
        bounced_users: Mutex<HashSet<UserId>>,
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

    fn send_dm(core: &Arc<BotCore>, user: &User, text: Text) {
        if core.check_connected().is_ok() && user.quit_error().is_none() {
            let nick = user.nick();
            let _ = core.send_chat_with_receipt(OutboundChatKind::Privmsg, &nick, text);
        }
    }

    fn send_broadcast_or_target(&self, text: Text) {
        match self {
            EndpointSender::RoomOpen { room } | EndpointSender::RoomUser { room, .. } => {
                let _ = room.say(text);
            }
            EndpointSender::Dm { core, user } => {
                Self::send_dm(core, user, text);
            }
            EndpointSender::Huddle { sender, .. } => {
                let _ = sender.broadcast_kind(OutboundChatKind::Privmsg, text);
            }
        }
    }

    fn send_reject(&self, sender_user: &User, feedback: Text) {
        match self {
            EndpointSender::RoomOpen { room } | EndpointSender::RoomUser { room, .. } => {
                let _ = room.say(feedback);
            }
            EndpointSender::Dm { core, user } => {
                Self::send_dm(core, user, feedback);
            }
            EndpointSender::Huddle { .. } => {
                let _ = sender_user.say(feedback);
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

    fn check_send_error(
        &self,
        target: &str,
        target_user_id: Option<u64>,
        code: u16,
        message: &str,
    ) -> Option<Error> {
        match self {
            EndpointSender::RoomOpen { room } | EndpointSender::RoomUser { room, .. } => room
                .casemap()
                .str_eq(target, room.name())
                .then(|| Error::DeliveryFailed {
                    target: target.to_owned(),
                    code,
                    message: message.to_owned(),
                }),
            EndpointSender::Dm { user, .. } => {
                (target_user_id == Some(user.track_id()) || user.eq_nick(target)).then(|| {
                    Error::DeliveryFailed {
                        target: target.to_owned(),
                        code,
                        message: message.to_owned(),
                    }
                })
            }
            EndpointSender::Huddle {
                sender,
                bounced_users,
            } => {
                if let Some(uid) = sender.find_matching_user_id(target, target_user_id) {
                    let mut bounced = bounced_users.lock().unwrap();
                    bounced.insert(uid);
                    if !sender.any_reachable_online(&bounced) {
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

type BoxAskFuture<'a, O> = Pin<Box<dyn Future<Output = Result<O>> + Send + 'a>>;

enum AskState<'a, F, O> {
    Unstarted {
        cfg: AskConfig,
        source: AskSource<'a>,
        prompt: Text,
        matcher: Box<F>,
    },
    Running(BoxAskFuture<'a, O>),
    Done,
}

/// Lazy prompt-and-reply future returned by [`Room::ask`], [`Room::ask_user`], [`User::ask`],
/// [`ExclusiveDm::ask`], and [`Huddle::ask`].
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Ask<'a, F, R: IntoVerdict> {
    state: AskState<'a, F, R::Output>,
}

impl<'a, F, R: IntoVerdict> Ask<'a, F, R> {
    pub(crate) fn new(
        source: AskSource<'a>,
        base_cfg: AskConfig,
        prompt: Text,
        matcher: F,
    ) -> Self {
        Self {
            state: AskState::Unstarted {
                cfg: base_cfg,
                source,
                prompt,
                matcher: Box::new(matcher),
            },
        }
    }

    fn cfg_mut(&mut self) -> &mut AskConfig {
        match &mut self.state {
            AskState::Unstarted { cfg, .. } => cfg,
            AskState::Running(_) | AskState::Done => {
                unreachable!("Ask builder methods consume self before polling")
            }
        }
    }

    /// Applies a reusable [`AskConfig`] value to this prompt.
    pub fn config(mut self, cfg: &AskConfig) -> Self {
        *self.cfg_mut() = cfg.clone();
        self
    }

    /// Overrides the post-write reply deadline for this prompt.
    pub fn reply_timeout(mut self, duration: Duration) -> Self {
        self.cfg_mut().time_limit = TimeLimit::Duration(duration);
        self
    }

    /// Overrides the post-write deadline for this prompt using an absolute [`Instant`].
    pub fn reply_deadline(mut self, instant: Instant) -> Self {
        self.cfg_mut().time_limit = TimeLimit::Deadline(instant);
        self
    }

    /// Disables the reply deadline for this prompt.
    pub fn no_reply_timeout(mut self) -> Self {
        self.cfg_mut().time_limit = TimeLimit::None;
        self
    }

    /// Schedules a countdown warning `msg` when `remaining` time is left on this `Ask`'s
    /// post-write reply clock.
    pub fn warn_remaining(mut self, remaining: Duration, msg: impl Into<Text>) -> Self {
        self.cfg_mut().warnings.push((remaining, msg.into()));
        self
    }

    /// Clears any countdown warnings inherited from the handle's or `config`'s [`AskConfig`].
    pub fn clear_warnings(mut self) -> Self {
        self.cfg_mut().warnings.clear();
        self
    }

    /// Opt-in command skipping for this prompt (`BotConfig::command_prefixes`, default `["!"]`).
    pub fn ignore_commands(mut self) -> Self {
        self.cfg_mut().ignore_commands = true;
        self
    }

    /// Opt-in human `NOTICE` acceptance for this prompt (`MessageKind::Notice`).
    pub fn accept_notices(mut self) -> Self {
        self.cfg_mut().accept_notices = true;
        self
    }

    /// Overrides the message sent to the target if *this `Ask`'s own reply timer* expires.
    pub fn timeout_message(mut self, msg: impl Into<Text>) -> Self {
        self.cfg_mut().timeout_handler = Some(TimeoutHandler::Static(msg.into()));
        self
    }

    pub fn timeout_message_fn<G, M>(mut self, f: G) -> Self
    where
        G: Fn(&TimeoutCtx<'_>) -> M + Send + Sync + 'static,
        M: Into<Text>,
    {
        self.cfg_mut().timeout_handler =
            Some(TimeoutHandler::Dynamic(Arc::new(move |ctx| f(ctx).into())));
        self
    }

    /// Suppresses any inherited timeout message on this prompt.
    pub fn no_timeout_message(mut self) -> Self {
        self.cfg_mut().timeout_handler = None;
        self
    }
}

fn make_span(cfg: &AskConfig, source: &AskSource<'_>) -> tracing::Span {
    let timeout_ms = match cfg.time_limit {
        TimeLimit::Duration(d) => Some(d.as_millis() as u64),
        TimeLimit::Deadline(_) | TimeLimit::None => None,
    };
    match source {
        AskSource::RoomOpen { room, .. } => tracing::debug_span!(
            "ircgm::ask",
            target = room.name(),
            user = Option::<&str>::None,
            timeout_ms = timeout_ms
        ),
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
    }
}

fn compute_schedule(
    cfg: &AskConfig,
    write_completed_at: Instant,
) -> (Option<Duration>, Option<Instant>, Vec<(Instant, Text)>) {
    match cfg.time_limit {
        TimeLimit::None => (None, None, Vec::new()),
        TimeLimit::Duration(dur) => {
            let deadline = write_completed_at + dur;
            let mut warns = Vec::new();
            for (rem, msg) in &cfg.warnings {
                if !rem.is_zero() && dur > *rem {
                    warns.push((write_completed_at + (dur - *rem), msg.clone()));
                }
            }
            warns.sort_by_key(|(at, _)| *at);
            (Some(dur), Some(deadline), warns)
        }
        TimeLimit::Deadline(deadline) => {
            let budget = deadline.saturating_duration_since(write_completed_at);
            let mut warns = Vec::new();
            for (rem, msg) in &cfg.warnings {
                if !rem.is_zero() && budget > *rem {
                    warns.push((deadline - *rem, msg.clone()));
                }
            }
            warns.sort_by_key(|(at, _)| *at);
            (None, Some(deadline), warns)
        }
    }
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
    R: IntoVerdict,
{
    receipt.wait().await;
    let write_completed_at = Instant::now();
    tracing::debug!("ask prompt written; waiting for reply");

    let (configured_timeout, deadline, warnings) = compute_schedule(cfg, write_completed_at);

    let reply_fut = inbox.wait_reply_map(
        cfg.ignore_commands,
        cfg.accept_notices,
        |target, target_user_id, code, message| {
            let err = sender.check_send_error(target, target_user_id, code, message);
            if let Some(ref e) = err {
                tracing::debug!(error = %e, "ask delivery bounced");
            }
            err
        },
        |msg| match matcher(msg).into_verdict() {
            Verdict::Accept(val) => {
                tracing::debug!(sender = %msg.nick, seq = msg.seq, "ask reply accepted");
                Some(val)
            }
            Verdict::Reject(feedback) => {
                sender.send_reject(&msg.sender, feedback);
                None
            }
            Verdict::Ignore => {
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
                            sender.send_broadcast_or_target(warn_msg);
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
                sender.send_broadcast_or_target(handler.render(&ctx));
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
    mut matcher: Box<F>,
) -> Result<R::Output>
where
    F: FnMut(&ChatMessage) -> R + Send + 'a,
    R: IntoVerdict + 'a,
    R::Output: Send + 'a,
{
    match source {
        AskSource::RoomOpen { room, caller } => {
            room.ensure_bot_in_room_if_tracked()?;
            let mut inbox = room.inbox_at(caller);
            let receipt = room.send_chat_with_receipt(OutboundChatKind::Privmsg, prompt)?;
            await_ask_loop(
                &cfg,
                &mut *matcher,
                EndpointSender::RoomOpen { room },
                &mut inbox,
                receipt,
            )
            .await
        }
        AskSource::RoomUser { room, user, caller } => {
            if !user.belongs_to(room.core()) {
                return Err(Error::InvalidInput(
                    "User handle belongs to a different Bot session",
                ));
            }
            room.ensure_user_askable(user)?;
            let mut inbox = user.inbox_in_at(room, caller);
            let receipt = room.send_chat_with_receipt(OutboundChatKind::Privmsg, prompt)?;
            await_ask_loop(
                &cfg,
                &mut *matcher,
                EndpointSender::RoomUser { room, user },
                &mut inbox,
                receipt,
            )
            .await
        }
        AskSource::UserDm { user, caller } => {
            let mut dm = match user.exclusive_at(caller).await {
                Ok(dm) => dm,
                Err(e) => {
                    tracing::debug!(error = %e, "ask aborted before DM prompt write");
                    return Err(e);
                }
            };
            dm.inbox().clear();
            let receipt = dm.send_chat_with_receipt(OutboundChatKind::Privmsg, prompt)?;
            let (core, dm_user, inbox) = dm.split_for_ask();
            await_ask_loop(
                &cfg,
                &mut *matcher,
                EndpointSender::Dm {
                    core,
                    user: dm_user,
                },
                inbox,
                receipt,
            )
            .await
        }
        AskSource::BorrowedDm { dm } => {
            dm.core().check_connected()?;
            if let Some(err) = dm.user().quit_error() {
                return Err(err);
            }
            dm.inbox().clear();
            let receipt = dm.send_chat_with_receipt(OutboundChatKind::Privmsg, prompt)?;
            let (core, dm_user, inbox) = dm.split_for_ask();
            await_ask_loop(
                &cfg,
                &mut *matcher,
                EndpointSender::Dm {
                    core,
                    user: dm_user,
                },
                inbox,
                receipt,
            )
            .await
        }
        AskSource::BorrowedHuddle { huddle } => {
            huddle.check_connected()?;
            huddle.inbox().clear();
            let (sender, inbox) = huddle.split_for_ask();
            let receipt = sender.broadcast_prompt_with_receipt(prompt)?;
            await_ask_loop(
                &cfg,
                &mut *matcher,
                EndpointSender::Huddle {
                    sender,
                    bounced_users: Mutex::new(HashSet::new()),
                },
                inbox,
                receipt,
            )
            .await
        }
    }
}

impl<'a, F, R> Future for Ask<'a, F, R>
where
    F: FnMut(&ChatMessage) -> R + Send + 'a,
    R: IntoVerdict + 'a,
    R::Output: Send + 'a,
{
    type Output = Result<R::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if matches!(this.state, AskState::Unstarted { .. }) {
            let AskState::Unstarted {
                cfg,
                source,
                prompt,
                matcher,
            } = std::mem::replace(&mut this.state, AskState::Done)
            else {
                unreachable!()
            };
            let span = make_span(&cfg, &source);
            let fut = run_ask(cfg, source, prompt, matcher).instrument(span);
            this.state = AskState::Running(Box::pin(fut));
        }

        match &mut this.state {
            AskState::Running(fut) => {
                let poll_res = fut.as_mut().poll(cx);
                if poll_res.is_ready() {
                    this.state = AskState::Done;
                }
                poll_res
            }
            AskState::Done => panic!("Ask polled after completion"),
            AskState::Unstarted { .. } => unreachable!(),
        }
    }
}
