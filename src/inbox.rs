use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::future::poll_fn;
use std::panic::Location;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use irc::proto::Message;
use tokio::sync::mpsc;

use crate::bot::BotCore;
use crate::casemap::{CaseMap, FoldedStr};
use crate::error::{DisconnectReason, Error, Result};
use crate::message::{ChatMessage, Event, Target, WireEvent, WireKind};
use crate::room::Room;
use crate::user::{User, UserStatus, UserTrack};

/// Multi-source filter builder for [`crate::Bot::inbox_scope`] (delivers matching channels
/// and/or DMs into one wire-ordered [`Inbox`]).
#[derive(Clone, Debug, Default)]
pub struct InboxScope {
    pub(crate) rooms: Vec<(String, u64)>,
    pub(crate) dm_users: Vec<User>,
}

impl InboxScope {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn room(mut self, room: &Room) -> Self {
        let cm = room.casemap();
        let sid = room.core().session_id;
        if !self
            .rooms
            .iter()
            .any(|(r, existing_sid)| *existing_sid == sid && cm.str_eq(r, room.name()))
        {
            self.rooms.push((room.name().to_owned(), sid));
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
        if !self.dm_users.iter().any(|u| u == user) {
            self.dm_users.push(user.clone());
        }
        self
    }

    pub fn dms_from<I, U>(mut self, users: I) -> Self
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        for u in users {
            self = self.dm_from(u.borrow());
        }
        self
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Filter {
    All,
    AllDms,
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
                    WireKind::RawOnly => {
                        if is_channel_target {
                            !rooms.is_empty()
                        } else {
                            !dm_users.is_empty() || !rooms.is_empty()
                        }
                    }
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

        let departed_users = snapshot_departed_room_users(&core, &filter);
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
            idle_timeout: None,
            latched_departure: None,
            last_user_departure: None,
            departed_users,
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

    /// Returns `true` if any active `Inbox` listens to `folded_chan` (either via `Bot::inbox`
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

fn snapshot_departed_room_users(core: &BotCore, filter: &Filter) -> HashSet<u64> {
    let Filter::UsersInRoom {
        tracks,
        channel_name,
    } = filter
    else {
        return HashSet::new();
    };
    let ws = core.state.read().unwrap();
    let chan_key = ws.casemap.fold(channel_name);
    let chan = ws.channels.get(&chan_key);
    let mut departed = HashSet::new();
    for track in tracks {
        if !track.is_online()
            || chan.is_some_and(|c| c.names_synced && !c.members.contains_key(&track.id))
        {
            departed.insert(track.id);
        }
    }
    departed
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
    pub(crate) ignore_commands: bool,
    pub(crate) accept_notices: bool,
    pub(crate) idle_timeout: Option<Duration>,
    latched_departure: Option<Error>,
    last_user_departure: Option<(String, Option<String>)>,
    departed_users: HashSet<u64>,
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
            idle_timeout: None,
            latched_departure: None,
            last_user_departure: None,
            departed_users: HashSet::new(),
        }
    }

    pub(crate) fn invalid_session(
        filter: Filter,
        opened_at: &'static Location<'static>,
    ) -> Self {
        let mut inbox = Self::disconnected_bot_dropped(filter, opened_at);
        inbox.latched_departure = Some(Error::InvalidInput(
            "handle belongs to a different Bot session",
        ));
        inbox
    }

    pub fn ignore_commands(mut self) -> Self {
        self.ignore_commands = true;
        self
    }

    pub fn accept_notices(mut self) -> Self {
        self.accept_notices = true;
        self
    }

    pub fn lossy(mut self) -> Self {
        self.lag_policy = LagPolicy::Lossy;
        self
    }

    /// Discards all messages currently buffered in this inbox and clears any pending lag or
    /// latched departure state.
    pub fn clear(&mut self) {
        while self.rx.try_recv().is_ok() {}
        self.dropped.store(0, Ordering::Release);
        self.warned_75.store(false, Ordering::Relaxed);
        self.latched_departure = None;
        self.last_user_departure = None;
        if let Some(core) = &self.core {
            self.departed_users = snapshot_departed_room_users(core, &self.filter);
        } else {
            self.departed_users.clear();
        }
    }

    async fn next_wire(&mut self, use_idle_timeout: bool) -> Result<Arc<WireEvent>> {
        match self.idle_timeout.filter(|_| use_idle_timeout) {
            None => poll_fn(|cx| self.poll_next_wire(cx)).await,
            Some(dur) => tokio::time::timeout(dur, poll_fn(|cx| self.poll_next_wire(cx)))
                .await
                .map_err(|_| Error::TimedOut)?,
        }
    }

    /// Cancel-safe core reply/chat loop shared by [`Inbox::wait_map`] and [`crate::Ask`].
    pub(crate) async fn wait_reply_map<B, F, T>(
        &mut self,
        ignore_commands: bool,
        accept_notices: bool,
        check_bounce: B,
        f: F,
    ) -> Result<T>
    where
        B: FnMut(&str, Option<u64>, u16, &str) -> Option<Error>,
        F: FnMut(&ChatMessage) -> Option<T>,
    {
        self.wait_reply_map_inner(false, ignore_commands, accept_notices, check_bounce, f)
            .await
    }

    async fn wait_reply_map_inner<B, F, T>(
        &mut self,
        use_idle_timeout: bool,
        ignore_commands: bool,
        accept_notices: bool,
        mut check_bounce: B,
        mut f: F,
    ) -> Result<T>
    where
        B: FnMut(&str, Option<u64>, u16, &str) -> Option<Error>,
        F: FnMut(&ChatMessage) -> Option<T>,
    {
        loop {
            let wire = self.next_wire(use_idle_timeout).await?;
            match &wire.kind {
                WireKind::SendError {
                    target,
                    target_user_id,
                    code,
                    message,
                } => {
                    if let Some(err) = check_bounce(target, *target_user_id, *code, message) {
                        return Err(err);
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
                        return Ok(val);
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
                        return Ok(val);
                    }
                }
                _ => {}
            }
        }
    }

    pub async fn wait_map<F, T>(&mut self, f: F) -> Result<T>
    where
        F: FnMut(&ChatMessage) -> Option<T>,
    {
        self.wait_reply_map_inner(
            true,
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

    /// Waits for a word-boundary command `cmd` (ASCII case-insensitive), ignoring
    /// `self.ignore_commands` so explicit command waits never deadlock on command-skipping inboxes.
    pub async fn wait_cmd(&mut self, cmd: &str) -> Result<ChatMessage> {
        self.wait_reply_map_inner(
            true,
            false,
            self.accept_notices,
            |_, _, _, _| None,
            |m| m.is_cmd(cmd).then(|| m.clone()),
        )
        .await
    }

    /// Waits for `cmd` followed by non-empty arguments, returning `(msg, trimmed_arg_string)`.
    pub async fn wait_cmd_arg(&mut self, cmd: &str) -> Result<(ChatMessage, String)> {
        self.wait_reply_map_inner(
            true,
            false,
            self.accept_notices,
            |_, _, _, _| None,
            |m| m.cmd_arg(cmd).map(|arg| (m.clone(), arg.to_owned())),
        )
        .await
    }

    pub async fn next_chat(&mut self) -> Result<ChatMessage> {
        self.wait_map(|m| Some(m.clone())).await
    }

    pub async fn next_event(&mut self) -> Result<Event> {
        self.wait_event_map(|ev| Some(ev.clone())).await
    }

    /// Consuming one-shot variant of [`Inbox::wait_map`] that owns this [`Inbox`] inside the
    /// returned future.
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

    /// Lossy supervision stop-trigger: enables `.lossy()` internally (so burst lag logs a warning
    /// and keeps listening rather than killing the trigger) and parks `Pending` if the watched
    /// scope closes or disconnects (so a departing user never triggers a false stop in `.until()`).
    pub async fn on_map<F, T>(mut self, f: F) -> T
    where
        F: FnMut(&ChatMessage) -> Option<T>,
    {
        self.lag_policy = LagPolicy::Lossy;
        match self
            .wait_reply_map_inner(
                false,
                self.ignore_commands,
                self.accept_notices,
                |_, _, _, _| None,
                f,
            )
            .await
        {
            Ok(val) => val,
            Err(_) => {
                drop(self);
                std::future::pending::<T>().await
            }
        }
    }

    /// Lossy supervision stop-trigger matching `pred` (for `.until(...)` / `.or_until(...)`).
    pub async fn on_for<P>(self, mut pred: P) -> ChatMessage
    where
        P: FnMut(&ChatMessage) -> bool,
    {
        self.on_map(move |m| pred(m).then(|| m.clone())).await
    }

    /// Lossy supervision stop-trigger matching word-boundary `cmd` (ASCII case-insensitive),
    /// always accepting commands even if `ignore_commands` was set.
    pub async fn on_cmd(mut self, cmd: &str) -> ChatMessage {
        self.ignore_commands = false;
        self.on_for(|m| m.is_cmd(cmd)).await
    }

    /// Waits for and transforms the next matching high-level [`Event`] in this inbox.
    pub async fn wait_event_map<F, T>(&mut self, mut f: F) -> Result<T>
    where
        F: FnMut(&Event) -> Option<T>,
    {
        loop {
            let wire = self.next_wire(true).await?;
            if let Some(ev) = wire.to_public_event() {
                if let Some(val) = f(&ev) {
                    return Ok(val);
                }
            }
        }
    }

    pub async fn next_raw(&mut self) -> Result<Arc<Message>> {
        let wire = self.next_wire(true).await?;
        Ok(Arc::clone(&wire.raw))
    }

    pub async fn wait_raw_map<F, T>(&mut self, mut f: F) -> Result<T>
    where
        F: FnMut(&Message) -> Option<T>,
    {
        loop {
            let wire = self.next_wire(true).await?;
            if let Some(val) = f(&wire.raw) {
                return Ok(val);
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
            Filter::All | Filter::AllDms => {}
            Filter::UsersInRoom {
                tracks,
                channel_name,
            } => {
                if tracks.is_empty() {
                    return;
                }
                let has_user = |uid: u64| tracks.iter().any(|t| t.id == uid);
                match &wire.kind {
                    WireKind::BotLeftRoom { channel, reason }
                        if cm.str_eq(channel, channel_name) =>
                    {
                        self.latched_departure = Some(Error::BotLeftRoom {
                            channel: channel.clone(),
                            reason: reason.clone(),
                        });
                    }
                    WireKind::Joined { channel, user, .. }
                        if has_user(user.track_id()) && cm.str_eq(channel, channel_name) =>
                    {
                        self.departed_users.remove(&user.track_id());
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
                        self.departed_users.insert(user.track_id());
                        self.last_user_departure = Some((nick.clone(), reason.clone()));
                        let any_live = tracks.iter().any(|t| {
                            !self.departed_users.contains(&t.id)
                                && !matches!(
                                    t.status(),
                                    UserStatus::Quit { at_seq, .. } if at_seq <= wire.seq
                                )
                        });
                        if !any_live {
                            self.latched_departure = Some(Error::UserLeft {
                                nick: nick.clone(),
                                reason: reason.clone(),
                            });
                        }
                    }
                    WireKind::Quit {
                        user, nick, reason, ..
                    } if has_user(user.track_id()) => {
                        self.departed_users.insert(user.track_id());
                        self.last_user_departure = Some((nick.clone(), reason.clone()));
                        let any_live = tracks.iter().any(|t| {
                            !self.departed_users.contains(&t.id)
                                && !matches!(
                                    t.status(),
                                    UserStatus::Quit { at_seq, .. } if at_seq <= wire.seq
                                )
                        });
                        if !any_live {
                            self.latched_departure = Some(Error::UserLeft {
                                nick: nick.clone(),
                                reason: reason.clone(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Filter::Scoped { rooms, dm_users } => {
                if rooms.is_empty() && dm_users.is_empty() {
                    return;
                }
                match &wire.kind {
                    WireKind::BotLeftRoom { channel, reason }
                        if rooms.iter().any(|r| cm.str_eq(r, channel)) =>
                    {
                        let any_live_room = {
                            let ws = core.state.read().unwrap();
                            rooms.iter().any(|r| {
                                !ws.casemap.str_eq(r, channel)
                                    && ws
                                        .lost_channels
                                        .get(&ws.casemap.fold(r))
                                        .is_none_or(|lost| lost.at_seq > wire.seq)
                            })
                        };
                        let any_live_user = dm_users.iter().any(|t| {
                            !matches!(
                                t.status(),
                                UserStatus::Quit { at_seq, .. } if at_seq <= wire.seq
                            )
                        });
                        if !any_live_room && !any_live_user {
                            self.latched_departure = Some(Error::BotLeftRoom {
                                channel: channel.clone(),
                                reason: reason.clone(),
                            });
                        }
                    }
                    WireKind::Quit {
                        user, nick, reason, ..
                    } if dm_users.iter().any(|t| t.id == user.track_id()) => {
                        let any_live_room = {
                            let ws = core.state.read().unwrap();
                            rooms.iter().any(|r| {
                                ws.lost_channels
                                    .get(&ws.casemap.fold(r))
                                    .is_none_or(|lost| lost.at_seq > wire.seq)
                            })
                        };
                        let any_live_user = dm_users.iter().any(|t| {
                            t.id != user.track_id()
                                && !matches!(
                                    t.status(),
                                    UserStatus::Quit { at_seq, .. } if at_seq <= wire.seq
                                )
                        });
                        if !any_live_room && !any_live_user {
                            self.latched_departure = Some(Error::UserLeft {
                                nick: nick.clone(),
                                reason: reason.clone(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn check_scope_exhausted(&self) -> Option<Error> {
        let core = self.core.as_ref()?;
        if let Some(reason) = core.disconnect_reason() {
            return Some(Error::Disconnected(reason));
        }

        match &self.filter {
            Filter::All | Filter::AllDms => None,
            Filter::UsersInRoom {
                tracks,
                channel_name,
            } => {
                if tracks.is_empty() {
                    return None;
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
                    .expect("non-empty tracks with 0 live_tracks must have a departed user");
                Some(Error::UserLeft { nick, reason })
            }
            Filter::Scoped { rooms, dm_users } => {
                if rooms.is_empty() && dm_users.is_empty() {
                    return None;
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
                } else {
                    let (nick, reason) =
                        last_quit_user.expect("exhausted non-empty Scoped filter has quit user");
                    Some(Error::UserLeft { nick, reason })
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "inbox_test.rs"]
mod test;

