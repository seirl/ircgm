use std::collections::HashMap;
use std::future::poll_fn;
use std::panic::Location;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures_core::Stream;
use irc::proto::Message;
use tokio::sync::mpsc;

use crate::bot::BotCore;
use crate::casemap::{CaseMap, FoldedStr};
use crate::error::{DisconnectReason, Error, Result};
use crate::message::{ChatMessage, Event, Target, WireEvent, WireKind};
use crate::room::Room;
use crate::user::{User, UserStatus, UserTrack};

/// Multi-source filter builder for [` crate::Bot::inbox_scope `] (delivers matching channels
/// and/or DMs into one wire-ordered [`Inbox`]).
#[derive(Clone, Debug, Default)]
pub struct InboxScope {
    pub(crate) rooms: Vec<String>,
    pub(crate) dm_users: Vec<Arc<UserTrack>>,
}

impl InboxScope {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn room(mut self, room: &Room) -> Self {
        let cm = room.casemap();
        if !self.rooms.iter().any(|r| cm.str_eq(r, room.name())) {
            self.rooms.push(room.name().to_owned());
        }
        self
    }

    pub fn rooms<'a, I: IntoIterator<Item = &'a Room>>(mut self, rooms: I) -> Self {
        for r in rooms {
            self = self.room(r);
        }
        self
    }

    pub fn dm_from(mut self, user: &User) -> Self {
        let track = user.track_arc();
        if !self.dm_users.iter().any(|t| t.id == track.id) {
            self.dm_users.push(track);
        }
        self
    }

    pub fn dms_from<'a, I: IntoIterator<Item = &'a User>>(mut self, users: I) -> Self {
        for u in users {
            self = self.dm_from(u);
        }
        self
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Filter {
    All,
    AllDms,
    UserDm(Arc<UserTrack>),
    UsersInRoom {
        tracks: Vec<Arc<UserTrack>>,
        channel_name: String,
    },
    Scoped {
        rooms: Vec<String>,
        dm_users: Vec<Arc<UserTrack>>,
    },
}

impl Filter {
    pub(crate) fn scope_label(&self) -> String {
        match self {
            Filter::All => "bot".into(),
            Filter::AllDms => "dm".into(),
            Filter::UserDm(track) => format!("user {}", track.nick()),
            Filter::UsersInRoom {
                tracks,
                channel_name,
            } => match tracks.as_slice() {
                [track] => format!("user {} in {}", track.nick(), channel_name),
                ts => {
                    let names: Vec<String> = ts.iter().map(|t| t.nick()).collect();
                    format!("users [{}] in {}", names.join(", "), channel_name)
                }
            },
            Filter::Scoped { rooms, dm_users } => match (rooms.as_slice(), dm_users.as_slice()) {
                ([r], []) => format!("room {r}"),
                ([], [u]) => format!("user {}", u.nick()),
                (rs, []) if !rs.is_empty() => format!("rooms {}", rs.join(", ")),
                ([], us) if !us.is_empty() => {
                    let names: Vec<String> = us.iter().map(|u| u.nick()).collect();
                    format!("users {}", names.join(", "))
                }
                (rs, us) => {
                    let names: Vec<String> = us.iter().map(|u| u.nick()).collect();
                    format!("scope(rooms: [{}], users: [{}])", rs.join(", "), names.join(", "))
                }
            },
        }
    }

    fn matches(&self, ev: &WireEvent, is_channel_target: bool, cm: CaseMap) -> bool {
        match self {
            Filter::All => true,
            Filter::AllDms => match &ev.kind {
                WireKind::Chat(m) => m.target.is_dm(),
                WireKind::Notice { target, .. } => target.is_dm(),
                WireKind::SendError { .. } => !is_channel_target,
                WireKind::RawOnly => !is_channel_target,
                _ => false,
            },
            Filter::UserDm(track) => match &ev.kind {
                WireKind::Chat(m) => m.target.is_dm() && m.sender.track_id() == track.id,
                WireKind::Notice {
                    sender: Some(u),
                    target: Target::Dm,
                    ..
                } => u.track_id() == track.id,
                WireKind::Quit { user, .. } | WireKind::NickChanged { user, .. } => {
                    user.track_id() == track.id
                }
                WireKind::SendError { target_user_id, .. } => {
                    !is_channel_target && *target_user_id == Some(track.id)
                }
                _ => false,
            },
            Filter::UsersInRoom {
                tracks,
                channel_name,
            } => {
                let has_user = |uid: u64| tracks.iter().any(|t| t.id == uid);
                match &ev.kind {
                    WireKind::Chat(m) => {
                        has_user(m.sender.track_id())
                            && matches!(&m.target, Target::Channel(ch) if cm.str_eq(ch, channel_name))
                    }
                    WireKind::Notice {
                        sender: Some(u),
                        target: Target::Channel(ch),
                        ..
                    } => has_user(u.track_id()) && cm.str_eq(ch, channel_name),
                    WireKind::Joined { channel, user, .. }
                    | WireKind::Parted { channel, user, .. }
                    | WireKind::Kicked { channel, user, .. } => {
                        has_user(user.track_id()) && cm.str_eq(channel, channel_name)
                    }
                    WireKind::Quit { user, .. } | WireKind::NickChanged { user, .. } => {
                        has_user(user.track_id())
                    }
                    WireKind::SendError { target, .. } => {
                        is_channel_target && cm.str_eq(target, channel_name)
                    }
                    WireKind::BotLeftRoom { channel, .. } => cm.str_eq(channel, channel_name),
                    WireKind::RawOnly => is_channel_target,
                    _ => false,
                }
            }
            Filter::Scoped { rooms, dm_users } => {
                let has_room = |ch: &str| rooms.iter().any(|r| cm.str_eq(r, ch));
                let has_user = |uid: u64| dm_users.iter().any(|t| t.id == uid);
                match &ev.kind {
                    WireKind::Chat(m) => match &m.target {
                        Target::Channel(ch) => has_room(ch),
                        Target::Dm => has_user(m.sender.track_id()),
                    },
                    WireKind::Notice { sender, target, .. } => match target {
                        Target::Channel(ch) => has_room(ch),
                        Target::Dm => sender.as_ref().is_some_and(|u| has_user(u.track_id())),
                    },
                    WireKind::Joined { channel, .. }
                    | WireKind::Parted { channel, .. }
                    | WireKind::Kicked { channel, .. } => has_room(channel),
                    WireKind::Quit { user, channels, .. }
                    | WireKind::NickChanged { user, channels, .. } => {
                        has_user(user.track_id()) || channels.iter().any(|c| has_room(c))
                    }
                    WireKind::SendError {
                        target,
                        target_user_id,
                        ..
                    } => {
                        if is_channel_target {
                            has_room(target)
                        } else {
                            target_user_id.is_some_and(has_user)
                        }
                    }
                    WireKind::BotLeftRoom { channel, .. } => has_room(channel),
                    WireKind::RawOnly => true,
                }
            }
        }
    }
}

struct InboxSlot {
    filter: Filter,
    scope_label: String,
    opened_at: &'static Location<'static>,
    tx: mpsc::Sender<Arc<WireEvent>>,
    dropped: Arc<AtomicU64>,
    warned_75: Arc<AtomicBool>,
    max_capacity: usize,
    last_dispatched_seq: u64,
}

#[derive(Default)]
struct RegistryInner {
    slots: HashMap<u64, InboxSlot>,
    all_ids: Vec<u64>,
    dm_ids: Vec<u64>,
    by_channel: HashMap<FoldedStr, Vec<u64>>,
    by_user: HashMap<u64, Vec<u64>>,
}

pub(crate) struct InboxRegistry {
    inner: Mutex<RegistryInner>,
    next_slot_id: AtomicU64,
}

impl Default for InboxRegistry {
    fn default() -> Self {
        Self {
            inner: Mutex::new(RegistryInner::default()),
            next_slot_id: AtomicU64::new(1),
        }
    }
}

impl InboxRegistry {
    pub(crate) fn subscribe(
        &self,
        core: Arc<BotCore>,
        filter: Filter,
        opened_at: &'static Location<'static>,
        casemap: CaseMap,
    ) -> Inbox {
        let slot_id = self.next_slot_id.fetch_add(1, Ordering::Relaxed);
        let cap = core.inbox_capacity.max(1);
        let (tx, rx) = mpsc::channel(cap);
        let dropped = Arc::new(AtomicU64::new(0));
        let warned_75 = Arc::new(AtomicBool::new(false));
        let scope_label = filter.scope_label();

        {
            let mut inner = self.inner.lock().unwrap();
            // Only register if the connection is still live:
            if core.disconnect_reason().is_none() {
                match &filter {
                    Filter::All => inner.all_ids.push(slot_id),
                    Filter::AllDms => inner.dm_ids.push(slot_id),
                    Filter::UserDm(track) => {
                        inner.by_user.entry(track.id).or_default().push(slot_id);
                    }
                    Filter::UsersInRoom {
                        tracks,
                        channel_name,
                    } => {
                        inner
                            .by_channel
                            .entry(casemap.fold(channel_name))
                            .or_default()
                            .push(slot_id);
                        for track in tracks {
                            let list = inner.by_user.entry(track.id).or_default();
                            if !list.contains(&slot_id) {
                                list.push(slot_id);
                            }
                        }
                    }
                    Filter::Scoped { rooms, dm_users } => {
                        for r in rooms {
                            inner
                                .by_channel
                                .entry(casemap.fold(r))
                                .or_default()
                                .push(slot_id);
                        }
                        for u in dm_users {
                            inner.by_user.entry(u.id).or_default().push(slot_id);
                        }
                    }
                }
                inner.slots.insert(
                    slot_id,
                    InboxSlot {
                        filter: filter.clone(),
                        scope_label: scope_label.clone(),
                        opened_at,
                        tx,
                        dropped: Arc::clone(&dropped),
                        warned_75: Arc::clone(&warned_75),
                        max_capacity: cap,
                        last_dispatched_seq: 0,
                    },
                );
            }
        }

        Inbox {
            core: Some(core),
            slot_id,
            filter,
            scope_label,
            opened_at,
            rx,
            dropped,
            warned_75,
            lag_policy: LagPolicy::Strict,
            ignore_commands: false,
            accept_notices: false,
            latched_departure: None,
            last_user_departure: None,
        }
    }

    pub(crate) fn unsubscribe(&self, slot_id: u64) {
        let mut inner = self.inner.lock().unwrap();
        let Some(slot) = inner.slots.remove(&slot_id) else {
            return;
        };
        match &slot.filter {
            Filter::All => inner.all_ids.retain(|&id| id != slot_id),
            Filter::AllDms => inner.dm_ids.retain(|&id| id != slot_id),
            Filter::UserDm(track) => {
                if let Some(vec) = inner.by_user.get_mut(&track.id) {
                    vec.retain(|&id| id != slot_id);
                    if vec.is_empty() {
                        inner.by_user.remove(&track.id);
                    }
                }
            }
            Filter::UsersInRoom { tracks, .. } => {
                for track in tracks {
                    if let Some(vec) = inner.by_user.get_mut(&track.id) {
                        vec.retain(|&id| id != slot_id);
                        if vec.is_empty() {
                            inner.by_user.remove(&track.id);
                        }
                    }
                }
                inner.by_channel.retain(|_, vec| {
                    vec.retain(|&id| id != slot_id);
                    !vec.is_empty()
                });
            }
            Filter::Scoped { dm_users, .. } => {
                for u in dm_users {
                    if let Some(vec) = inner.by_user.get_mut(&u.id) {
                        vec.retain(|&id| id != slot_id);
                        if vec.is_empty() {
                            inner.by_user.remove(&u.id);
                        }
                    }
                }
                inner.by_channel.retain(|_, vec| {
                    vec.retain(|&id| id != slot_id);
                    !vec.is_empty()
                });
            }
        }
    }

    /// Returns `true` if any active `Inbox` listens to `folded_chan` (either via `inbox_all`
    /// or a channel-scoped subscription).
    pub(crate) fn has_interest_in_channel(&self, folded_chan: &FoldedStr) -> bool {
        let inner = self.inner.lock().unwrap();
        !inner.all_ids.is_empty()
            || inner
                .by_channel
                .get(folded_chan)
                .is_some_and(|v| !v.is_empty())
    }

    pub(crate) fn rekey_casemap(&self, new_map: CaseMap) {
        let mut inner = self.inner.lock().unwrap();
        let mut rebuilt: HashMap<FoldedStr, Vec<u64>> = HashMap::new();
        for (&slot_id, slot) in &inner.slots {
            match &slot.filter {
                Filter::UsersInRoom { channel_name, .. } => {
                    rebuilt
                        .entry(new_map.fold(channel_name))
                        .or_default()
                        .push(slot_id);
                }
                Filter::Scoped { rooms, .. } => {
                    for r in rooms {
                        let list = rebuilt.entry(new_map.fold(r)).or_default();
                        if !list.contains(&slot_id) {
                            list.push(slot_id);
                        }
                    }
                }
                _ => {}
            }
        }
        inner.by_channel = rebuilt;
    }

    pub(crate) fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.slots.clear();
        inner.all_ids.clear();
        inner.dm_ids.clear();
        inner.by_channel.clear();
        inner.by_user.clear();
    }

    pub(crate) fn dispatch(
        &self,
        event: &Arc<WireEvent>,
        channel_hint: Option<&str>,
        casemap: CaseMap,
    ) {
        let mut inner = self.inner.lock().unwrap();
        if inner.slots.is_empty() {
            return;
        }

        let mut candidates: Vec<u64> = Vec::with_capacity(inner.all_ids.len() + 4);
        candidates.extend_from_slice(&inner.all_ids);

        match &event.kind {
            WireKind::Chat(msg) => match &msg.target {
                Target::Channel(ch) => {
                    if let Some(ids) = inner.by_channel.get(&casemap.fold(ch)) {
                        candidates.extend_from_slice(ids);
                    }
                }
                Target::Dm => {
                    candidates.extend_from_slice(&inner.dm_ids);
                    if let Some(ids) = inner.by_user.get(&msg.sender.track_id()) {
                        candidates.extend_from_slice(ids);
                    }
                }
            },
            WireKind::Notice { sender, target, .. } => match target {
                Target::Channel(ch) => {
                    if let Some(ids) = inner.by_channel.get(&casemap.fold(ch)) {
                        candidates.extend_from_slice(ids);
                    }
                }
                Target::Dm => {
                    candidates.extend_from_slice(&inner.dm_ids);
                    if let Some(u) = sender {
                        if let Some(ids) = inner.by_user.get(&u.track_id()) {
                            candidates.extend_from_slice(ids);
                        }
                    }
                }
            },
            WireKind::Joined { channel, user, .. }
            | WireKind::Parted { channel, user, .. }
            | WireKind::Kicked { channel, user, .. } => {
                if let Some(ids) = inner.by_channel.get(&casemap.fold(channel)) {
                    candidates.extend_from_slice(ids);
                }
                if let Some(ids) = inner.by_user.get(&user.track_id()) {
                    candidates.extend_from_slice(ids);
                }
            }
            WireKind::Quit { user, channels, .. }
            | WireKind::NickChanged { user, channels, .. } => {
                if let Some(ids) = inner.by_user.get(&user.track_id()) {
                    candidates.extend_from_slice(ids);
                }
                for ch in channels {
                    if let Some(ids) = inner.by_channel.get(&casemap.fold(ch)) {
                        candidates.extend_from_slice(ids);
                    }
                }
            }
            WireKind::SendError {
                target,
                target_user_id,
                ..
            } => {
                if channel_hint.is_some() {
                    if let Some(ids) = inner.by_channel.get(&casemap.fold(target)) {
                        candidates.extend_from_slice(ids);
                    }
                } else {
                    candidates.extend_from_slice(&inner.dm_ids);
                    if let Some(uid) = target_user_id {
                        if let Some(ids) = inner.by_user.get(uid) {
                            candidates.extend_from_slice(ids);
                        }
                    }
                }
            }
            WireKind::BotLeftRoom { channel, .. } => {
                if let Some(ids) = inner.by_channel.get(&casemap.fold(channel)) {
                    candidates.extend_from_slice(ids);
                }
            }
            WireKind::RawOnly => {
                if let Some(ch) = channel_hint {
                    if let Some(ids) = inner.by_channel.get(&casemap.fold(ch)) {
                        candidates.extend_from_slice(ids);
                    }
                } else {
                    candidates.extend_from_slice(&inner.dm_ids);
                }
            }
        }

        let is_chan = channel_hint.is_some();
        for slot_id in candidates {
            let Some(slot) = inner.slots.get_mut(&slot_id) else {
                continue;
            };
            if slot.last_dispatched_seq == event.seq {
                continue;
            }
            if !slot.filter.matches(event, is_chan, casemap) {
                continue;
            }
            slot.last_dispatched_seq = event.seq;

            match slot.tx.try_send(Arc::clone(event)) {
                Ok(()) => {
                    let cap = slot.max_capacity;
                    let rem = slot.tx.capacity();
                    let queued = cap.saturating_sub(rem);
                    if cap > 0
                        && queued.saturating_mul(4) >= cap.saturating_mul(3)
                        && !slot.warned_75.swap(true, Ordering::Relaxed)
                    {
                        tracing::warn!(
                            scope = %slot.scope_label,
                            opened_at = %slot.opened_at,
                            queued,
                            capacity = cap,
                            "inbox buffer reached >=75% capacity"
                        );
                    }
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    slot.dropped.fetch_add(1, Ordering::AcqRel);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LagPolicy {
    Strict,
    Lossy,
}

/// Scoped, cancel-safe, wire-ordered listener for chat messages, high-level [`Event`]s, and raw IRC frames.
pub struct Inbox {
    core: Option<Arc<BotCore>>,
    slot_id: u64,
    filter: Filter,
    scope_label: String,
    opened_at: &'static Location<'static>,
    rx: mpsc::Receiver<Arc<WireEvent>>,
    dropped: Arc<AtomicU64>,
    warned_75: Arc<AtomicBool>,
    lag_policy: LagPolicy,
    ignore_commands: bool,
    accept_notices: bool,
    latched_departure: Option<Error>,
    last_user_departure: Option<(String, Option<String>)>,
}

impl Drop for Inbox {
    fn drop(&mut self) {
        if let Some(core) = &self.core {
            if self.slot_id != 0 {
                core.inboxes.unsubscribe(self.slot_id);
            }
        }
    }
}

impl Inbox {
    pub(crate) fn disconnected_bot_dropped(
        filter: Filter,
        opened_at: &'static Location<'static>,
    ) -> Self {
        let (_tx, rx) = mpsc::channel(1);
        let scope_label = filter.scope_label();
        Self {
            core: None,
            slot_id: 0,
            filter,
            scope_label,
            opened_at,
            rx,
            dropped: Arc::new(AtomicU64::new(0)),
            warned_75: Arc::new(AtomicBool::new(false)),
            lag_policy: LagPolicy::Strict,
            ignore_commands: false,
            accept_notices: false,
            latched_departure: None,
            last_user_departure: None,
        }
    }

    pub fn ignore_commands(mut self) -> Self {
        self.ignore_commands = true;
        self
    }

    pub fn set_ignore_commands(&mut self, enabled: bool) {
        self.ignore_commands = enabled;
    }

    pub fn accept_notices(mut self) -> Self {
        self.accept_notices = true;
        self
    }

    pub fn set_accept_notices(&mut self, enabled: bool) {
        self.accept_notices = enabled;
    }

    pub fn lossy(mut self) -> Self {
        self.lag_policy = LagPolicy::Lossy;
        self
    }

    /// Discards all messages currently buffered in this inbox and clears any pending lag state.
    pub fn clear(&mut self) {
        while self.rx.try_recv().is_ok() {}
        self.dropped.store(0, Ordering::Release);
        self.warned_75.store(false, Ordering::Relaxed);
    }

    /// Cancel-safe core reply/chat loop shared by [`Inbox::wait_map`] and [`crate::Ask`].
    pub(crate) async fn wait_reply_map<B, F, T>(
        &mut self,
        ignore_commands: bool,
        accept_notices: bool,
        mut check_bounce: B,
        mut f: F,
    ) -> Result<T>
    where
        B: FnMut(&str, Option<u64>, u16, &str) -> Option<Error>,
        F: FnMut(&ChatMessage) -> Option<T>,
    {
        poll_fn(|cx| loop {
            match self.poll_next_wire(cx) {
                Poll::Ready(Ok(wire)) => match &wire.kind {
                    WireKind::SendError {
                        target,
                        target_user_id,
                        code,
                        message,
                    } => {
                        if let Some(err) = check_bounce(target, *target_user_id, *code, message) {
                            return Poll::Ready(Err(err));
                        }
                    }
                    WireKind::Chat(msg) => {
                        if ignore_commands
                            && self
                                .core
                                .as_ref()
                                .is_some_and(|c| c.is_command_text(&msg.text))
                        {
                            continue;
                        }
                        if let Some(val) = f(msg) {
                            return Poll::Ready(Ok(val));
                        }
                    }
                    WireKind::Notice {
                        human_chat: Some(msg),
                        ..
                    } if accept_notices => {
                        if ignore_commands
                            && self
                                .core
                                .as_ref()
                                .is_some_and(|c| c.is_command_text(&msg.text))
                        {
                            continue;
                        }
                        if let Some(val) = f(msg) {
                            return Poll::Ready(Ok(val));
                        }
                    }
                    _ => {}
                },
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        })
        .await
    }

    pub async fn wait_map<F, T>(&mut self, f: F) -> Result<T>
    where
        F: FnMut(&ChatMessage) -> Option<T>,
    {
        self.wait_reply_map(
            self.ignore_commands,
            self.accept_notices,
            |_, _, _, _| None,
            f,
        )
        .await
    }

    pub async fn wait_for<P>(&mut self, mut pred: P) -> Result<ChatMessage>
    where
        P: FnMut(&ChatMessage) -> bool,
    {
        self.wait_map(|m| pred(m).then(|| m.clone())).await
    }

    pub async fn wait_cmd(&mut self, cmd: &str) -> Result<ChatMessage> {
        self.wait_for(|m| m.is_cmd(cmd)).await
    }

    pub async fn next_chat(&mut self) -> Result<ChatMessage> {
        poll_fn(|cx| self.poll_next_chat_candidate(cx)).await
    }

    pub async fn next_event(&mut self) -> Result<Event> {
        self.wait_event_map(|ev| Some(ev.clone())).await
    }

    /// Consuming one-shot variant of [`Inbox::wait_map`] that owns this [`Inbox`] inside the
    /// returned future (ideal for `.until(...)` / `.or_until(...)` chains and helper functions).
    pub async fn into_wait_map<F, T>(mut self, f: F) -> Result<T>
    where
        F: FnMut(&ChatMessage) -> Option<T>,
    {
        self.wait_map(f).await
    }

    /// Consuming one-shot variant of [`Inbox::wait_for`].
    pub async fn into_wait_for<P>(mut self, pred: P) -> Result<ChatMessage>
    where
        P: FnMut(&ChatMessage) -> bool,
    {
        self.wait_for(pred).await
    }

    /// Consuming one-shot variant of [`Inbox::wait_cmd`].
    pub async fn into_wait_cmd(mut self, cmd: &str) -> Result<ChatMessage> {
        self.wait_cmd(cmd).await
    }

    /// Consuming one-shot variant of [`Inbox::next_chat`].
    pub async fn into_next_chat(mut self) -> Result<ChatMessage> {
        self.next_chat().await
    }

    /// Consuming one-shot variant of [`Inbox::next_event`].
    pub async fn into_next_event(mut self) -> Result<Event> {
        self.next_event().await
    }

    /// Waits for and transforms the next matching high-level [`Event`] in this inbox.
    pub async fn wait_event_map<F, T>(&mut self, mut f: F) -> Result<T>
    where
        F: FnMut(&Event) -> Option<T>,
    {
        poll_fn(|cx| loop {
            match self.poll_next_wire(cx) {
                Poll::Ready(Ok(wire)) => {
                    if let Some(ev) = wire.to_public_event() {
                        if let Some(val) = f(&ev) {
                            return Poll::Ready(Ok(val));
                        }
                    }
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        })
        .await
    }

    /// Waits for the next high-level [`Event`] in this inbox matching `pred`.
    pub async fn wait_event_for<P>(&mut self, mut pred: P) -> Result<Event>
    where
        P: FnMut(&Event) -> bool,
    {
        poll_fn(|cx| loop {
            match self.poll_next_wire(cx) {
                Poll::Ready(Ok(wire)) => {
                    if let Some(ev) = wire.to_public_event() {
                        if pred(&ev) {
                            return Poll::Ready(Ok(ev));
                        }
                    }
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        })
        .await
    }

    /// Consuming one-shot variant of [`Inbox::wait_event_map`].
    pub async fn into_wait_event_map<F, T>(mut self, f: F) -> Result<T>
    where
        F: FnMut(&Event) -> Option<T>,
    {
        self.wait_event_map(f).await
    }

    /// Consuming one-shot variant of [`Inbox::wait_event_for`].
    pub async fn into_wait_event_for<P>(mut self, pred: P) -> Result<Event>
    where
        P: FnMut(&Event) -> bool,
    {
        self.wait_event_for(pred).await
    }

    pub async fn next_raw(&mut self) -> Result<Arc<Message>> {
        poll_fn(|cx| match self.poll_next_wire(cx) {
            Poll::Ready(Ok(wire)) => Poll::Ready(Ok(Arc::clone(&wire.raw))),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        })
        .await
    }

    pub async fn wait_raw_map<F, T>(&mut self, mut f: F) -> Result<T>
    where
        F: FnMut(&Message) -> Option<T>,
    {
        poll_fn(|cx| loop {
            match self.poll_next_wire(cx) {
                Poll::Ready(Ok(wire)) => {
                    if let Some(val) = f(&wire.raw) {
                        return Poll::Ready(Ok(val));
                    }
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        })
        .await
    }

    fn poll_next_chat_candidate(&mut self, cx: &mut Context<'_>) -> Poll<Result<ChatMessage>> {
        loop {
            match self.poll_next_wire(cx) {
                Poll::Ready(Ok(wire)) => {
                    let msg_opt = match &wire.kind {
                        WireKind::Chat(msg) => Some(msg),
                        WireKind::Notice {
                            human_chat: Some(msg),
                            ..
                        } if self.accept_notices => Some(msg),
                        _ => None,
                    };
                    if let Some(msg) = msg_opt {
                        if self.ignore_commands {
                            if let Some(core) = &self.core {
                                if core.is_command_text(&msg.text) {
                                    continue;
                                }
                            }
                        }
                        return Poll::Ready(Ok(msg.clone()));
                    }
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    /// Cancel-safe raw wire frame poller.
    pub(crate) fn poll_next_wire(&mut self, cx: &mut Context<'_>) -> Poll<Result<Arc<WireEvent>>> {
        let skipped = self.dropped.swap(0, Ordering::AcqRel);
        if skipped > 0 {
            match self.lag_policy {
                LagPolicy::Strict => {
                    return Poll::Ready(Err(Error::Lagged {
                        scope: self.scope_label.clone(),
                        skipped,
                        opened_at: self.opened_at,
                    }));
                }
                LagPolicy::Lossy => {
                    tracing::warn!(
                        scope = %self.scope_label,
                        opened_at = %self.opened_at,
                        skipped,
                        "lossy inbox skipped lagged messages"
                    );
                }
            }
        }

        if let Some(err) = self.latched_departure.take() {
            return Poll::Ready(Err(err));
        }

        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(wire)) => {
                self.maybe_latch_in_stream_departure(&wire);
                Poll::Ready(Ok(wire))
            }
            Poll::Ready(None) => {
                let reason = self
                    .core
                    .as_ref()
                    .and_then(|c| c.disconnect_reason())
                    .unwrap_or(DisconnectReason::BotDropped);
                Poll::Ready(Err(Error::Disconnected(reason)))
            }
            Poll::Pending => {
                if let Some(err) = self.check_scope_exhausted() {
                    return Poll::Ready(Err(err));
                }
                Poll::Pending
            }
        }
    }

    fn maybe_latch_in_stream_departure(&mut self, wire: &WireEvent) {
        let Some(core) = &self.core else {
            return;
        };
        let cm = core.casemap();
        match &self.filter {
            Filter::UserDm(track) => {
                if let WireKind::Quit {
                    user, nick, reason, ..
                } = &wire.kind
                {
                    if user.track_id() == track.id {
                        self.latched_departure = Some(Error::UserLeft {
                            nick: nick.clone(),
                            reason: reason.clone(),
                        });
                    }
                }
            }
            Filter::UsersInRoom {
                tracks,
                channel_name,
            } => {
                let has_user = |uid: u64| tracks.iter().any(|t| t.id == uid);
                let record_departure = |nick: &str, reason: &Option<String>| {
                    let info = (nick.to_owned(), reason.clone());
                    if tracks.len() == 1 {
                        (
                            Some(Error::UserLeft {
                                nick: info.0,
                                reason: info.1,
                            }),
                            None,
                        )
                    } else {
                        (None, Some(info))
                    }
                };
                match &wire.kind {
                    WireKind::BotLeftRoom { channel, reason }
                        if cm.str_eq(channel, channel_name) =>
                    {
                        self.latched_departure = Some(Error::BotLeftRoom {
                            channel: channel.clone(),
                            reason: reason.clone(),
                        });
                    }
                    WireKind::Parted {
                        channel,
                        user,
                        nick,
                        reason,
                    }
                    | WireKind::Kicked {
                        channel,
                        user,
                        nick,
                        reason,
                        ..
                    } if has_user(user.track_id()) && cm.str_eq(channel, channel_name) => {
                        let (latched, last) = record_departure(nick, reason);
                        if let Some(err) = latched {
                            self.latched_departure = Some(err);
                        }
                        if let Some(info) = last {
                            self.last_user_departure = Some(info);
                        }
                    }
                    WireKind::Quit {
                        user, nick, reason, ..
                    } if has_user(user.track_id()) => {
                        let (latched, last) = record_departure(nick, reason);
                        if let Some(err) = latched {
                            self.latched_departure = Some(err);
                        }
                        if let Some(info) = last {
                            self.last_user_departure = Some(info);
                        }
                    }
                    _ => {}
                }
            }
            Filter::All | Filter::AllDms | Filter::Scoped { .. } => {}
        }
    }

    fn check_scope_exhausted(&self) -> Option<Error> {
        let core = self.core.as_ref()?;
        if let Some(reason) = core.disconnect_reason() {
            return Some(Error::Disconnected(reason));
        }

        match &self.filter {
            Filter::All | Filter::AllDms => None,
            Filter::UserDm(track) => match track.status() {
                UserStatus::Online => None,
                UserStatus::Quit { nick, reason, .. } => Some(Error::UserLeft { nick, reason }),
            },
            Filter::UsersInRoom {
                tracks,
                channel_name,
            } => {
                if tracks.is_empty() {
                    return Some(Error::UserLeft {
                        nick: String::new(),
                        reason: None,
                    });
                }
                let ws = core.state.read().unwrap();
                let chan_key = ws.casemap.fold(channel_name);
                if let Some(lost) = ws.lost_channels.get(&chan_key) {
                    return Some(Error::BotLeftRoom {
                        channel: lost.name.clone(),
                        reason: lost.reason.clone(),
                    });
                }
                let chan = ws.channels.get(&chan_key);
                let mut live_tracks = 0usize;
                let mut fallback_left: Option<(String, Option<String>)> = None;
                for track in tracks {
                    if let UserStatus::Quit { nick, reason, .. } = track.status() {
                        fallback_left = Some((nick, reason));
                        continue;
                    }
                    if let Some(c) = chan {
                        if c.names_synced && !c.members.contains_key(&track.id) {
                            if fallback_left.is_none() {
                                fallback_left = Some((track.nick(), None));
                            }
                            continue;
                        }
                    }
                    live_tracks += 1;
                }
                if live_tracks > 0 {
                    return None;
                }
                let (nick, reason) = self
                    .last_user_departure
                    .clone()
                    .or(fallback_left)
                    .unwrap_or_else(|| (String::new(), None));
                Some(Error::UserLeft { nick, reason })
            }
            Filter::Scoped { rooms, dm_users } => {
                if rooms.is_empty() && dm_users.is_empty() {
                    return Some(Error::UserLeft {
                        nick: String::new(),
                        reason: None,
                    });
                }

                let ws = core.state.read().unwrap();
                let mut live_rooms = 0usize;
                let mut last_lost_room: Option<(String, Option<String>)> = None;
                for r in rooms {
                    let key = ws.casemap.fold(r);
                    if let Some(lost) = ws.lost_channels.get(&key) {
                        last_lost_room = Some((lost.name.clone(), lost.reason.clone()));
                    } else {
                        live_rooms += 1;
                    }
                }

                let mut live_users = 0usize;
                let mut last_quit_user: Option<(String, Option<String>)> = None;
                for t in dm_users {
                    match t.status() {
                        UserStatus::Online => live_users += 1,
                        UserStatus::Quit { nick, reason, .. } => {
                            last_quit_user = Some((nick, reason));
                        }
                    }
                }

                if live_rooms > 0 || live_users > 0 {
                    return None;
                }

                if let Some((channel, reason)) = last_lost_room {
                    Some(Error::BotLeftRoom { channel, reason })
                } else if let Some((nick, reason)) = last_quit_user {
                    Some(Error::UserLeft { nick, reason })
                } else {
                    Some(Error::UserLeft {
                        nick: String::new(),
                        reason: None,
                    })
                }
            }
        }
    }
}

impl Stream for Inbox {
    type Item = Result<ChatMessage>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.poll_next_chat_candidate(cx) {
            Poll::Ready(res) => Poll::Ready(Some(res)),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
#[path = "inbox_test.rs"]
mod test;

