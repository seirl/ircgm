use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::{poll_fn, Future};
use std::panic::Location;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use irc::proto::Message;
use tokio::sync::mpsc;

use crate::casemap::{AtomicCaseMap, CaseMap, FoldedStr};
use crate::core::BotCore;
use crate::error::{DisconnectReason, Error, Result};
use crate::message::{ChatMessage, Event, Target, WireEvent, WireKind};
use crate::state::WorldState;
use crate::user::{UserStatus, UserTrack};

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
}

pub(crate) type OnChatHook = Arc<dyn Fn(&ChatMessage) + Send + Sync>;

struct InboxSlot {
    filter: Filter,
    /// Exact-role buckets this slot is registered in (cached so `unsubscribe` touches only them).
    keys: Vec<BucketKey>,
    scope_label: String,
    opened_at: &'static Location<'static>,
    tx: mpsc::Sender<Arc<WireEvent>>,
    dropped: Arc<AtomicU64>,
    warned_75: Arc<AtomicBool>,
    max_capacity: usize,
    last_dispatched_seq: u64,
    on_chat: Option<OnChatHook>,
}

impl InboxSlot {
    /// Delivers `event` once per wire `seq` (a slot can sit in several buckets an event hits).
    /// Returns `true` when `event` is newly delivered to a still-open slot.
    fn offer(&mut self, event: &Arc<WireEvent>) -> bool {
        if self.last_dispatched_seq == event.seq {
            return false;
        }
        self.last_dispatched_seq = event.seq;
        match self.tx.try_send(Arc::clone(event)) {
            Ok(()) => {
                let cap = self.max_capacity;
                let queued = cap.saturating_sub(self.tx.capacity());
                if cap > 0
                    && queued.saturating_mul(4) >= cap.saturating_mul(3)
                    && !self.warned_75.swap(true, Ordering::Relaxed)
                {
                    tracing::warn!(
                        scope = %self.scope_label,
                        opened_at = %self.opened_at,
                        queued,
                        capacity = cap,
                        "inbox buffer reached >=75% capacity"
                    );
                }
                true
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::AcqRel);
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

/// One exact role a slot plays: every slot in a bucket wants every event routed to it, so
/// dispatch never re-filters (keeping `O(1)` lookup even with ~1,000 `UsersInRoom` slots in a
/// single channel, and `O(1)` `swap_remove` on `Drop` via cached `InboxSlot::keys`).
///
/// Dispatch is intentionally pure fan-out with no shadowing: `ExclusiveDm` and `Huddle`
/// serialize cooperative DM prompts via `UserTrack::dm_mutex`, while unserialized `user.inbox()`
/// and `bot.inbox*()` continue receiving DMs so external `.until(...)` cancel/stop supervisors
/// keep working while a prompt holds the DM lock.
#[derive(Clone, Debug, PartialEq, Eq)]
enum BucketKey {
    /// `Filter::All`: every event.
    All,
    /// `Filter::AllDms`: DM chat/notices, DM bounces, and non-channel raw frames.
    AllDms,
    /// `Filter::Scoped` room: everything about the channel.
    RoomAll(FoldedStr),
    /// `Filter::UsersInRoom` channel: channel bounces, `BotLeftRoom`, channel raw frames.
    RoomMeta(FoldedStr),
    /// `Filter::UsersInRoom` track: that user's chat/notices/joins/parts/kicks in the channel.
    RoomUser(FoldedStr, u64),
    /// `Filter::Scoped` DM user: DM chat/notices from, bounces to, and quits/renames of the user.
    UserDm(u64),
    /// `Filter::UsersInRoom` track: quits/renames/`401` bounces of the user (any channel).
    UserPresence(u64),
}

impl BucketKey {
    fn for_filter(filter: &Filter, cm: CaseMap) -> Vec<BucketKey> {
        let mut keys = Vec::new();
        let mut push = |key: BucketKey| {
            if !keys.contains(&key) {
                keys.push(key);
            }
        };
        match filter {
            Filter::All => push(BucketKey::All),
            Filter::AllDms => push(BucketKey::AllDms),
            Filter::UsersInRoom {
                tracks,
                channel_name,
            } => {
                let chan = cm.fold(channel_name);
                push(BucketKey::RoomMeta(chan.clone()));
                for track in tracks {
                    push(BucketKey::RoomUser(chan.clone(), track.id));
                    push(BucketKey::UserPresence(track.id));
                }
            }
            Filter::Scoped { rooms, dm_users } => {
                for room in rooms {
                    push(BucketKey::RoomAll(cm.fold(room)));
                }
                for track in dm_users {
                    push(BucketKey::UserDm(track.id));
                }
            }
        }
        keys
    }
}

#[derive(Default)]
struct ChannelBucket {
    all_events: Vec<u64>,
    room_meta: Vec<u64>,
    by_user: HashMap<u64, Vec<u64>>,
}

impl ChannelBucket {
    fn is_empty(&self) -> bool {
        self.all_events.is_empty() && self.room_meta.is_empty() && self.by_user.is_empty()
    }
}

#[derive(Default)]
struct UserBucket {
    dm: Vec<u64>,
    presence: Vec<u64>,
}

#[derive(Default)]
struct RegistryInner {
    casemap: CaseMap,
    slots: HashMap<u64, InboxSlot>,
    all: Vec<u64>,
    all_dms: Vec<u64>,
    channels: HashMap<FoldedStr, ChannelBucket>,
    users: HashMap<u64, UserBucket>,
}

impl RegistryInner {
    fn add_to_buckets(&mut self, slot_id: u64, keys: &[BucketKey]) {
        for key in keys {
            let ids = match key {
                BucketKey::All => &mut self.all,
                BucketKey::AllDms => &mut self.all_dms,
                BucketKey::RoomAll(chan) => &mut self.channels.entry(chan.clone()).or_default().all_events,
                BucketKey::RoomMeta(chan) => &mut self.channels.entry(chan.clone()).or_default().room_meta,
                BucketKey::RoomUser(chan, uid) => self
                    .channels
                    .entry(chan.clone())
                    .or_default()
                    .by_user
                    .entry(*uid)
                    .or_default(),
                BucketKey::UserDm(uid) => &mut self.users.entry(*uid).or_default().dm,
                BucketKey::UserPresence(uid) => &mut self.users.entry(*uid).or_default().presence,
            };
            ids.push(slot_id);
        }
    }

    fn remove_from_buckets(&mut self, slot_id: u64, keys: &[BucketKey]) {
        fn take(ids: &mut Vec<u64>, slot_id: u64) {
            if let Some(pos) = ids.iter().position(|&id| id == slot_id) {
                ids.swap_remove(pos);
            }
        }
        for key in keys {
            match key {
                BucketKey::All => take(&mut self.all, slot_id),
                BucketKey::AllDms => take(&mut self.all_dms, slot_id),
                BucketKey::RoomAll(chan) | BucketKey::RoomMeta(chan) | BucketKey::RoomUser(chan, _) => {
                    let Some(bucket) = self.channels.get_mut(chan) else {
                        continue;
                    };
                    match key {
                        BucketKey::RoomAll(_) => take(&mut bucket.all_events, slot_id),
                        BucketKey::RoomMeta(_) => take(&mut bucket.room_meta, slot_id),
                        BucketKey::RoomUser(_, uid) => {
                            if let Some(ids) = bucket.by_user.get_mut(uid) {
                                take(ids, slot_id);
                                if ids.is_empty() {
                                    bucket.by_user.remove(uid);
                                }
                            }
                        }
                        _ => unreachable!("outer match arm only admits room keys"),
                    }
                    if bucket.is_empty() {
                        self.channels.remove(chan);
                    }
                }
                BucketKey::UserDm(uid) | BucketKey::UserPresence(uid) => {
                    let Some(bucket) = self.users.get_mut(uid) else {
                        continue;
                    };
                    match key {
                        BucketKey::UserDm(_) => take(&mut bucket.dm, slot_id),
                        _ => take(&mut bucket.presence, slot_id),
                    }
                    if bucket.dm.is_empty() && bucket.presence.is_empty() {
                        self.users.remove(uid);
                    }
                }
            }
        }
    }
}

pub(crate) struct InboxRegistry {
    inner: Mutex<RegistryInner>,
    next_slot_id: AtomicU64,
}

impl Default for InboxRegistry {
    fn default() -> Self {
        Self::new(AtomicCaseMap::default())
    }
}

impl InboxRegistry {
    pub(crate) fn new(casemap: AtomicCaseMap) -> Self {
        Self {
            inner: Mutex::new(RegistryInner {
                casemap: casemap.load(),
                ..RegistryInner::default()
            }),
            next_slot_id: AtomicU64::new(1),
        }
    }

    pub(crate) fn subscribe(
        &self,
        core: Arc<BotCore>,
        filter: Filter,
        opened_at: &'static Location<'static>,
    ) -> Inbox {
        self.subscribe_with_on_chat(core, filter, opened_at, None)
    }

    pub(crate) fn subscribe_with_on_chat(
        &self,
        core: Arc<BotCore>,
        filter: Filter,
        opened_at: &'static Location<'static>,
        on_chat: Option<OnChatHook>,
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
                let keys = BucketKey::for_filter(&filter, inner.casemap);
                inner.add_to_buckets(slot_id, &keys);
                inner.slots.insert(
                    slot_id,
                    InboxSlot {
                        filter: filter.clone(),
                        keys,
                        scope_label: scope_label.clone(),
                        opened_at,
                        tx,
                        dropped: Arc::clone(&dropped),
                        warned_75: Arc::clone(&warned_75),
                        max_capacity: cap,
                        last_dispatched_seq: 0,
                        on_chat,
                    },
                );
            }
        }

        let departed_users = snapshot_departed_room_users(&core, &filter);
        Inbox {
            sub: InboxSub::Active { core, slot_id, rx },
            filter,
            scope_label,
            opened_at,
            dropped,
            warned_75,
            lag_policy: LagPolicy::Strict,
            ignore_commands: false,
            accept_notices: false,
            read_timeout: None,
            latched_departure: None,
            last_user_departure: None,
            departed_users,
        }
    }

    pub(crate) fn unsubscribe(&self, slot_id: u64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(slot) = inner.slots.remove(&slot_id) {
            inner.remove_from_buckets(slot_id, &slot.keys);
        }
    }

    /// Returns `true` if any active `Inbox` listens to `channel` (either via `Bot::inbox`
    /// or a channel-scoped subscription).
    pub(crate) fn has_interest_in_channel(&self, channel: &str) -> bool {
        let inner = self.inner.lock().unwrap();
        !inner.all.is_empty()
            || (!inner.channels.is_empty()
                && inner.channels.contains_key(&inner.casemap.fold(channel)))
    }

    /// Returns `true` if a channel-scoped `Inbox` (`Room::inbox*`, `InboxScope::room`) watches
    /// `channel`, i.e. someone still needs its `Left` state to report `BotLeftRoom`.
    pub(crate) fn has_channel_listener(&self, channel: &str) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.channels.contains_key(&inner.casemap.fold(channel))
    }

    pub(crate) fn rekey_casemap(&self, new_map: CaseMap) {
        let mut guard = self.inner.lock().unwrap();
        let inner = &mut *guard;
        inner.casemap = new_map;
        inner.all.clear();
        inner.all_dms.clear();
        inner.channels.clear();
        inner.users.clear();
        let mut rekeyed = Vec::with_capacity(inner.slots.len());
        for (&slot_id, slot) in &mut inner.slots {
            slot.keys = BucketKey::for_filter(&slot.filter, new_map);
            rekeyed.push((slot_id, slot.keys.clone()));
        }
        for (slot_id, keys) in rekeyed {
            inner.add_to_buckets(slot_id, &keys);
        }
    }

    pub(crate) fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.slots.clear();
        inner.all.clear();
        inner.all_dms.clear();
        inner.channels.clear();
        inner.users.clear();
    }

    /// Routes `event` to exactly the buckets whose role it matches: O(1) bucket lookups plus one
    /// `offer` per interested slot, with no per-event allocation or scan over all slots.
    pub(crate) fn dispatch(&self, event: &Arc<WireEvent>, channel_hint: Option<&str>) {
        let is_chat = matches!(&event.kind, WireKind::Event(Event::Chat(_)));
        let mut hooks: Vec<OnChatHook> = Vec::new();
        let mut guard = self.inner.lock().unwrap();
        let RegistryInner {
            casemap,
            slots,
            all,
            all_dms,
            channels,
            users,
        } = &mut *guard;
        if slots.is_empty() {
            return;
        }
        let cm = *casemap;
        let mut send = |ids: &[u64]| {
            for id in ids {
                if let Some(slot) = slots.get_mut(id) {
                    if slot.offer(event) && is_chat {
                        if let Some(ref hook) = slot.on_chat {
                            hooks.push(Arc::clone(hook));
                        }
                    }
                }
            }
        };
        let room = |name: &str| channels.get(&cm.fold(name));
        let user = |uid: u64| users.get(&uid);

        send(all);
        match &event.kind {
            WireKind::Event(Event::Chat(msg)) => match &msg.target {
                Target::Channel(ch) => {
                    if let Some(bucket) = room(ch) {
                        send(&bucket.all_events);
                        if let Some(ids) = bucket.by_user.get(&msg.sender.track_id()) {
                            send(ids);
                        }
                    }
                }
                Target::Dm => {
                    send(all_dms);
                    if let Some(bucket) = user(msg.sender.track_id()) {
                        send(&bucket.dm);
                    }
                }
            },
            WireKind::Event(Event::Notice { sender, target, .. }) => match target {
                Target::Channel(ch) => {
                    if let Some(bucket) = room(ch) {
                        send(&bucket.all_events);
                        if let Some(ids) = sender
                            .as_ref()
                            .and_then(|s| bucket.by_user.get(&s.track_id()))
                        {
                            send(ids);
                        }
                    }
                }
                Target::Dm => {
                    send(all_dms);
                    if let Some(bucket) = sender.as_ref().and_then(|s| user(s.track_id())) {
                        send(&bucket.dm);
                    }
                }
            },
            WireKind::Event(
                Event::Joined { channel, user: who, .. }
                | Event::Parted { channel, user: who, .. }
                | Event::Kicked { channel, user: who, .. },
            ) => {
                if let Some(bucket) = room(channel) {
                    send(&bucket.all_events);
                    if let Some(ids) = bucket.by_user.get(&who.track_id()) {
                        send(ids);
                    }
                }
            }
            WireKind::Event(
                Event::Quit {
                    user: who,
                    channels: occupied,
                    ..
                }
                | Event::NickChanged {
                    user: who,
                    channels: occupied,
                    ..
                },
            ) => {
                if let Some(bucket) = user(who.track_id()) {
                    send(&bucket.dm);
                    send(&bucket.presence);
                }
                for ch in occupied {
                    if let Some(bucket) = room(ch) {
                        send(&bucket.all_events);
                    }
                }
            }
            WireKind::Event(Event::DeliveryFailed { target, .. }) => {
                if channel_hint.is_some() {
                    if let Some(bucket) = room(target) {
                        send(&bucket.all_events);
                        send(&bucket.room_meta);
                    }
                } else {
                    send(all_dms);
                    // A user bounce (`401`) also concerns `UsersInRoom` watchers of that user, so
                    // parked `Room::inbox_user` / `Room::ask_user` wake and see the departure:
                    if let Some(bucket) = event.bounce_user_id().and_then(user) {
                        send(&bucket.dm);
                        send(&bucket.presence);
                    }
                }
            }
            WireKind::BotLeftRoom { channel, .. } => {
                if let Some(bucket) = room(channel) {
                    send(&bucket.all_events);
                    send(&bucket.room_meta);
                }
            }
            WireKind::RawOnly => match channel_hint {
                Some(ch) => {
                    if let Some(bucket) = room(ch) {
                        send(&bucket.all_events);
                        send(&bucket.room_meta);
                    }
                }
                None => send(all_dms),
            },
        }
        drop(guard);
        if !hooks.is_empty() {
            if let WireKind::Event(Event::Chat(msg)) = &event.kind {
                for hook in hooks {
                    hook(msg);
                }
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
    let members = ws.channel(channel_name).and_then(|c| c.members());
    let mut departed = HashSet::new();
    for track in tracks {
        if !track.is_online() || members.is_some_and(|m| !m.contains_key(&track.id)) {
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

/// Where an [`Inbox`]'s events come from.
enum InboxSub {
    /// Registered in `core.inboxes` under `slot_id` (the sender may already be gone if the
    /// connection was down at subscribe time; reads then report the disconnect reason).
    Active {
        core: Arc<BotCore>,
        slot_id: u64,
        rx: mpsc::Receiver<Arc<WireEvent>>,
    },
    /// Opened from a handle whose `Bot` was dropped: every read fails with
    /// `Disconnected(BotDropped)`.
    Disconnected,
    /// Built from handles of another `Bot` session: every read fails with `InvalidInput`.
    InvalidSession,
}

impl InboxSub {
    fn core(&self) -> Option<&Arc<BotCore>> {
        match self {
            InboxSub::Active { core, .. } => Some(core),
            InboxSub::Disconnected | InboxSub::InvalidSession => None,
        }
    }
}

/// Scoped, cancel-safe, wire-ordered listener for chat messages, high-level [`Event`]s, and raw IRC frames.
pub struct Inbox {
    sub: InboxSub,
    filter: Filter,
    scope_label: String,
    opened_at: &'static Location<'static>,
    dropped: Arc<AtomicU64>,
    warned_75: Arc<AtomicBool>,
    lag_policy: LagPolicy,
    pub(crate) ignore_commands: bool,
    pub(crate) accept_notices: bool,
    read_timeout: Option<Duration>,
    latched_departure: Option<Error>,
    last_user_departure: Option<(String, Option<String>)>,
    departed_users: HashSet<u64>,
}

impl fmt::Debug for Inbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let slot_id = match &self.sub {
            InboxSub::Active { slot_id, .. } => Some(*slot_id),
            InboxSub::Disconnected | InboxSub::InvalidSession => None,
        };
        f.debug_struct("Inbox")
            .field("scope", &self.scope_label)
            .field("slot_id", &slot_id)
            .field("opened_at", &self.opened_at)
            .finish_non_exhaustive()
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        if let InboxSub::Active { core, slot_id, .. } = &self.sub {
            core.inboxes.unsubscribe(*slot_id);
        }
    }
}

impl Inbox {
    fn detached(sub: InboxSub, filter: Filter, opened_at: &'static Location<'static>) -> Self {
        let scope_label = filter.scope_label();
        Self {
            sub,
            filter,
            scope_label,
            opened_at,
            dropped: Arc::new(AtomicU64::new(0)),
            warned_75: Arc::new(AtomicBool::new(false)),
            lag_policy: LagPolicy::Strict,
            ignore_commands: false,
            accept_notices: false,
            read_timeout: None,
            latched_departure: None,
            last_user_departure: None,
            departed_users: HashSet::new(),
        }
    }

    pub(crate) fn core(&self) -> Option<&Arc<BotCore>> {
        match &self.sub {
            InboxSub::Active { core, .. } => Some(core),
            InboxSub::Disconnected | InboxSub::InvalidSession => None,
        }
    }

    pub(crate) fn is_invalid_session(&self) -> bool {
        matches!(self.sub, InboxSub::InvalidSession)
    }

    pub(crate) fn is_departed_in_room(&self, track_id: u64) -> bool {
        self.departed_users.contains(&track_id)
    }

    #[cfg(test)]
    pub(crate) fn scope_label(&self) -> &str {
        &self.scope_label
    }

    pub(crate) fn disconnected_bot_dropped(
        filter: Filter,
        opened_at: &'static Location<'static>,
    ) -> Self {
        Self::detached(InboxSub::Disconnected, filter, opened_at)
    }

    pub(crate) fn invalid_session(
        filter: Filter,
        opened_at: &'static Location<'static>,
    ) -> Self {
        Self::detached(InboxSub::InvalidSession, filter, opened_at)
    }

    pub fn ignore_commands(mut self) -> Self {
        self.ignore_commands = true;
        self
    }

    pub fn accept_notices(mut self) -> Self {
        self.accept_notices = true;
        self
    }

    /// In-place form of [`Inbox::ignore_commands`], usable on a borrowed inbox
    /// (`dm.inbox()`, `huddle.inbox()`) and able to switch skipping back off.
    pub fn set_ignore_commands(&mut self, ignore: bool) {
        self.ignore_commands = ignore;
    }

    /// In-place form of [`Inbox::accept_notices`], usable on a borrowed inbox and able to switch
    /// notice delivery back off.
    pub fn set_accept_notices(&mut self, accept: bool) {
        self.accept_notices = accept;
    }

    /// Sets a per-read timeout (`Err(Error::TimedOut)`) applied to async `wait_*` / `next_*` calls
    /// on this inbox (`None` by default; `Some(300s)` on [`crate::ExclusiveDm`] and
    /// [`crate::Huddle`] inboxes so AFK bouncer clients cannot hold a DM lock forever).
    pub fn read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = Some(timeout);
        self
    }

    /// Disables any per-read timeout on this inbox.
    pub fn no_read_timeout(mut self) -> Self {
        self.read_timeout = None;
        self
    }

    /// In-place form of [`Inbox::read_timeout`] / [`Inbox::no_read_timeout`], usable on a borrowed
    /// inbox (`dm.inbox().set_read_timeout(None)` disables the default 300s lock-safety timeout).
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.read_timeout = timeout;
    }

    pub fn lossy(mut self) -> Self {
        self.lag_policy = LagPolicy::Lossy;
        self
    }

    /// Discards all messages currently buffered in this inbox and clears any pending lag or
    /// latched departure state (a detached `Disconnected` / `InvalidSession` inbox keeps
    /// failing).
    pub fn clear(&mut self) {
        self.dropped.store(0, Ordering::Release);
        self.warned_75.store(false, Ordering::Relaxed);
        self.latched_departure = None;
        self.last_user_departure = None;
        match &mut self.sub {
            InboxSub::Active { core, rx, .. } => {
                while rx.try_recv().is_ok() {}
                self.departed_users = snapshot_departed_room_users(core, &self.filter);
            }
            InboxSub::Disconnected | InboxSub::InvalidSession => self.departed_users.clear(),
        }
    }

    async fn next_wire(&mut self) -> Result<Arc<WireEvent>> {
        poll_fn(|cx| self.poll_next_wire(cx)).await
    }

    async fn run_read<Fut, T>(timeout: Option<Duration>, fut: Fut) -> Result<T>
    where
        Fut: Future<Output = Result<T>>,
    {
        match timeout {
            None => fut.await,
            Some(dur) => {
                match tokio::time::timeout_at(crate::future::deadline_after(dur), fut).await {
                    Ok(res) => res,
                    Err(_) => Err(Error::TimedOut),
                }
            }
        }
    }

    /// Cancel-safe core reply/chat loop shared by [`Inbox::wait_map`], [`Inbox::wait_cmd`], and
    /// [`Inbox::next_chat`].
    async fn wait_reply_map_inner<F, T>(
        &mut self,
        ignore_commands: bool,
        accept_notices: bool,
        mut f: F,
    ) -> Result<T>
    where
        F: FnMut(&ChatMessage) -> Option<T>,
    {
        loop {
            let wire = self.next_wire().await?;
            let candidate = match &wire.kind {
                WireKind::Event(Event::Chat(msg)) => Some(msg),
                WireKind::Event(Event::Notice { .. }) if accept_notices => wire.human_notice(),
                _ => None,
            };
            let Some(msg) = candidate else {
                continue;
            };
            if ignore_commands
                && self
                    .sub
                    .core()
                    .is_some_and(|c| c.is_command_text(&msg.text))
            {
                continue;
            }
            if let Some(val) = f(msg) {
                return Ok(val);
            }
        }
    }

    pub async fn wait_map<F, T>(&mut self, f: F) -> Result<T>
    where
        F: FnMut(&ChatMessage) -> Option<T>,
    {
        let timeout = self.read_timeout;
        Self::run_read(
            timeout,
            self.wait_reply_map_inner(self.ignore_commands, self.accept_notices, f),
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
        let timeout = self.read_timeout;
        Self::run_read(
            timeout,
            self.wait_reply_map_inner(false, self.accept_notices, |m| {
                m.is_cmd(cmd).then(|| m.clone())
            }),
        )
        .await
    }

    /// Waits for `cmd` followed by non-empty arguments, returning `(msg, trimmed_arg_string)`.
    pub async fn wait_cmd_arg(&mut self, cmd: &str) -> Result<(ChatMessage, String)> {
        let timeout = self.read_timeout;
        Self::run_read(
            timeout,
            self.wait_reply_map_inner(false, self.accept_notices, |m| {
                m.cmd_arg(cmd).map(|arg| (m.clone(), arg.to_owned()))
            }),
        )
        .await
    }

    pub async fn next_chat(&mut self) -> Result<ChatMessage> {
        self.wait_map(|m| Some(m.clone())).await
    }

    /// Non-blocking counterpart to [`Inbox::next_chat`]: returns `Ok(Some(msg))` if a matching
    /// chat message (honoring [`Inbox::ignore_commands`] and [`Inbox::accept_notices`]) is already
    /// buffered, `Ok(None)` if no matching message is currently queued, or `Err(...)` on lag,
    /// departure, or disconnect.
    pub fn try_next_chat(&mut self) -> Result<Option<ChatMessage>> {
        let ignore_commands = self.ignore_commands;
        let accept_notices = self.accept_notices;
        while let Some(wire) = self.try_next_wire()? {
            let candidate = match &wire.kind {
                WireKind::Event(Event::Chat(msg)) => Some(msg),
                WireKind::Event(Event::Notice { .. }) if accept_notices => wire.human_notice(),
                _ => None,
            };
            let Some(msg) = candidate else {
                continue;
            };
            if ignore_commands
                && self
                    .sub
                    .core()
                    .is_some_and(|c| c.is_command_text(&msg.text))
            {
                continue;
            }
            return Ok(Some(msg.clone()));
        }
        Ok(None)
    }

    pub async fn next_event(&mut self) -> Result<Event> {
        self.wait_event_map(|ev| Some(ev.clone())).await
    }

    /// Non-blocking counterpart to [`Inbox::next_event`]: returns `Ok(Some(event))` if a public
    /// [`Event`] is already buffered, `Ok(None)` if the queue is currently empty, or `Err(...)` on
    /// lag, departure, or disconnect.
    pub fn try_next_event(&mut self) -> Result<Option<Event>> {
        while let Some(wire) = self.try_next_wire()? {
            if let Some(ev) = wire.to_public_event() {
                return Ok(Some(ev));
            }
        }
        Ok(None)
    }

    /// Lossy supervision stop-trigger: enables `.lossy()` internally (so burst lag logs a warning
    /// and keeps listening rather than killing the trigger) and parks `Pending` if the watched
    /// scope closes or disconnects (so a departing user never triggers a false stop in `.until()`).
    pub async fn on_map<F, T>(mut self, f: F) -> T
    where
        F: FnMut(&ChatMessage) -> Option<T>,
    {
        self.lag_policy = LagPolicy::Lossy;
        self.read_timeout = None;
        match self
            .wait_reply_map_inner(self.ignore_commands, self.accept_notices, f)
            .await
        {
            Ok(val) => val,
            Err(_) => {
                drop(self);
                std::future::pending::<T>().await
            }
        }
    }

    /// Lossy supervision stop-trigger matching word-boundary `cmd` (ASCII case-insensitive),
    /// always accepting commands even if `ignore_commands` was set.
    pub async fn on_cmd(mut self, cmd: &str) -> ChatMessage {
        self.ignore_commands = false;
        self.on_map(|m| m.is_cmd(cmd).then(|| m.clone())).await
    }

    /// Waits for and transforms the next matching high-level [`Event`] in this inbox.
    pub async fn wait_event_map<F, T>(&mut self, mut f: F) -> Result<T>
    where
        F: FnMut(&Event) -> Option<T>,
    {
        let timeout = self.read_timeout;
        Self::run_read(timeout, async {
            loop {
                let wire = self.next_wire().await?;
                if let Some(ev) = wire.to_public_event() {
                    if let Some(val) = f(&ev) {
                        return Ok(val);
                    }
                }
            }
        })
        .await
    }

    pub async fn next_raw(&mut self) -> Result<Arc<Message>> {
        let timeout = self.read_timeout;
        let wire = Self::run_read(timeout, self.next_wire()).await?;
        Ok(Arc::clone(&wire.raw))
    }

    /// Non-blocking counterpart to [`Inbox::next_raw`].
    pub fn try_next_raw(&mut self) -> Result<Option<Arc<Message>>> {
        Ok(self.try_next_wire()?.map(|wire| Arc::clone(&wire.raw)))
    }

    pub async fn wait_raw_map<F, T>(&mut self, mut f: F) -> Result<T>
    where
        F: FnMut(&Message) -> Option<T>,
    {
        let timeout = self.read_timeout;
        Self::run_read(timeout, async {
            loop {
                let wire = self.next_wire().await?;
                if let Some(val) = f(&wire.raw) {
                    return Ok(val);
                }
            }
        })
        .await
    }

    fn pre_recv_check(&mut self) -> Result<()> {
        let skipped = self.dropped.swap(0, Ordering::AcqRel);
        if skipped > 0 {
            match self.lag_policy {
                LagPolicy::Strict => {
                    return Err(Error::Lagged {
                        scope: self.scope_label.clone(),
                        skipped,
                        opened_at: self.opened_at,
                    });
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
            return Err(err);
        }

        match &self.sub {
            InboxSub::Active { .. } => Ok(()),
            InboxSub::Disconnected => Err(Error::Disconnected(DisconnectReason::BotDropped)),
            InboxSub::InvalidSession => Err(Error::InvalidInput(
                "handle belongs to a different Bot session",
            )),
        }
    }

    fn disconnected_error(&self) -> Error {
        let reason = self
            .sub
            .core()
            .and_then(|c| c.disconnect_reason())
            .unwrap_or(DisconnectReason::BotDropped);
        Error::Disconnected(reason)
    }

    /// Non-blocking wire frame popper sharing lag, latched-departure, and exhaustion semantics with
    /// [`Inbox::poll_next_wire`].
    pub(crate) fn try_next_wire(&mut self) -> Result<Option<Arc<WireEvent>>> {
        self.pre_recv_check()?;
        let InboxSub::Active { rx, .. } = &mut self.sub else {
            unreachable!("checked by pre_recv_check")
        };
        match rx.try_recv() {
            Ok(wire) => {
                self.latch_in_stream_departure(&wire);
                Ok(Some(wire))
            }
            Err(mpsc::error::TryRecvError::Disconnected) => Err(self.disconnected_error()),
            Err(mpsc::error::TryRecvError::Empty) => match self.check_scope_exhausted() {
                Some(err) => Err(err),
                None => Ok(None),
            },
        }
    }

    /// Cancel-safe raw wire frame poller.
    pub(crate) fn poll_next_wire(&mut self, cx: &mut Context<'_>) -> Poll<Result<Arc<WireEvent>>> {
        if let Err(err) = self.pre_recv_check() {
            return Poll::Ready(Err(err));
        }
        let InboxSub::Active { rx, .. } = &mut self.sub else {
            unreachable!("checked by pre_recv_check")
        };
        match rx.poll_recv(cx) {
            Poll::Ready(Some(wire)) => {
                self.latch_in_stream_departure(&wire);
                Poll::Ready(Ok(wire))
            }
            Poll::Ready(None) => Poll::Ready(Err(self.disconnected_error())),
            Poll::Pending => {
                if let Some(err) = self.check_scope_exhausted() {
                    return Poll::Ready(Err(err));
                }
                Poll::Pending
            }
        }
    }

    /// In-stream half of the exhaustion rule: updates `UsersInRoom` roster bookkeeping for `wire`
    /// and, when `wire` is a departure that leaves no live source as of `wire.seq`, latches that
    /// departure's error (returned after this frame is delivered).
    fn latch_in_stream_departure(&mut self, wire: &WireEvent) {
        let Some(core) = self.sub.core() else {
            return;
        };
        let cm = core.casemap();
        let (trigger, left_room) = match (&self.filter, &wire.kind) {
            (
                Filter::UsersInRoom { channel_name: scope, .. },
                WireKind::BotLeftRoom { channel, reason },
            ) if cm.str_eq(channel, scope) => (
                Error::BotLeftRoom {
                    channel: channel.clone(),
                    reason: reason.clone(),
                },
                Some(channel.as_str()),
            ),
            (Filter::Scoped { rooms, .. }, WireKind::BotLeftRoom { channel, reason })
                if rooms.iter().any(|r| cm.str_eq(r, channel)) =>
            {
                (
                    Error::BotLeftRoom {
                        channel: channel.clone(),
                        reason: reason.clone(),
                    },
                    Some(channel.as_str()),
                )
            }
            (
                Filter::UsersInRoom {
                    tracks,
                    channel_name: scope,
                },
                WireKind::Event(Event::Joined { channel, user, .. }),
            ) if tracks.iter().any(|t| t.id == user.track_id()) && cm.str_eq(channel, scope) => {
                self.departed_users.remove(&user.track_id());
                return;
            }
            (
                Filter::UsersInRoom {
                    tracks,
                    channel_name: scope,
                },
                WireKind::Event(
                    Event::Parted {
                        channel,
                        user,
                        nick,
                        reason,
                    }
                    | Event::Kicked {
                        channel,
                        user,
                        nick,
                        reason,
                        ..
                    },
                ),
            ) if tracks.iter().any(|t| t.id == user.track_id()) && cm.str_eq(channel, scope) => {
                self.departed_users.insert(user.track_id());
                self.last_user_departure = Some((nick.clone(), reason.clone()));
                (
                    Error::UserLeft {
                        nick: nick.clone(),
                        reason: reason.clone(),
                    },
                    None,
                )
            }
            (
                Filter::UsersInRoom { tracks, .. } | Filter::Scoped { dm_users: tracks, .. },
                WireKind::Event(Event::Quit {
                    user, nick, reason, ..
                }),
            ) if tracks.iter().any(|t| t.id == user.track_id()) => {
                if matches!(self.filter, Filter::UsersInRoom { .. }) {
                    self.departed_users.insert(user.track_id());
                    self.last_user_departure = Some((nick.clone(), reason.clone()));
                }
                (
                    Error::UserLeft {
                        nick: nick.clone(),
                        reason: reason.clone(),
                    },
                    None,
                )
            }
            _ => return,
        };
        let ctx = ExhaustCtx {
            seq: Some(wire.seq),
            left_room,
            departed: &self.departed_users,
            last_departure: self.last_user_departure.as_ref(),
        };
        let exhausted = self
            .filter
            .check_exhausted(&core.state.read().unwrap(), ctx)
            .is_some();
        if exhausted {
            self.latched_departure = Some(trigger);
        }
    }

    /// Idle half of the exhaustion rule: once the buffer is drained, judges the scope against
    /// the current `WorldState` (or reports the disconnect).
    fn check_scope_exhausted(&self) -> Option<Error> {
        let core = self.sub.core()?;
        if let Some(reason) = core.disconnect_reason() {
            return Some(Error::Disconnected(reason));
        }
        let ctx = ExhaustCtx {
            seq: None,
            left_room: None,
            departed: &self.departed_users,
            last_departure: self.last_user_departure.as_ref(),
        };
        self.filter
            .check_exhausted(&core.state.read().unwrap(), ctx)
    }
}

/// What [`Filter::check_exhausted`] judges against: the wire position being latched
/// (`seq: Some`) or the current `WorldState` (`seq: None`), plus the inbox's in-stream state.
#[derive(Clone, Copy)]
struct ExhaustCtx<'a> {
    seq: Option<u64>,
    /// Channel whose `BotLeftRoom` is being latched: dead even if `WorldState` already moved on
    /// (rejoined or evicted it).
    left_room: Option<&'a str>,
    departed: &'a HashSet<u64>,
    last_departure: Option<&'a (String, Option<String>)>,
}

impl Filter {
    /// Unified scope-exhaustion rule shared by in-stream latching and idle polling: returns the
    /// error to surface once no source of a scoped filter (`UsersInRoom` tracks, `Scoped` rooms
    /// and DM users) is live as of `ctx`, or `None` while any source is live.
    fn check_exhausted(&self, ws: &WorldState, ctx: ExhaustCtx<'_>) -> Option<Error> {
        let quit = |t: &UserTrack| match t.status() {
            UserStatus::Quit { nick, reason, at_seq } => {
                ctx.seq.is_none_or(|s| at_seq <= s).then_some((nick, reason))
            }
            UserStatus::Online => None,
        };
        // `Some(BotLeftRoom)` if the bot is out of `room` as of `ctx`:
        let room_gone = |room: &str| -> Option<Error> {
            if ctx.left_room.is_some_and(|l| ws.casemap.str_eq(l, room)) {
                return Some(Error::BotLeftRoom {
                    channel: room.to_owned(),
                    reason: None,
                });
            }
            let chan = ws.channel(room)?;
            let left_at = chan.left_at_seq()?;
            if ctx.seq.is_none_or(|s| left_at <= s) {
                chan.left_error()
            } else {
                None
            }
        };

        match self {
            Filter::All | Filter::AllDms => None,
            Filter::UsersInRoom {
                tracks,
                channel_name,
            } => {
                if tracks.is_empty() {
                    return None;
                }
                if let Some(err) = room_gone(channel_name) {
                    return Some(err);
                }
                // Idle polling also trusts the synced roster (e.g. a `NAMES` resync dropping a user):
                let members = match ctx.seq {
                    None => ws.channel(channel_name).and_then(|c| c.members()),
                    Some(_) => None,
                };
                let mut fallback = None;
                for track in tracks {
                    if let Some(gone) = quit(track) {
                        fallback = Some(gone);
                    } else if ctx.departed.contains(&track.id)
                        || members.is_some_and(|m| !m.contains_key(&track.id))
                    {
                        fallback.get_or_insert_with(|| (track.nick(), None));
                    } else {
                        return None;
                    }
                }
                let (nick, reason) = ctx
                    .last_departure
                    .cloned()
                    .or(fallback)
                    .expect("non-empty tracks with no live track must have a departed user");
                Some(Error::UserLeft { nick, reason })
            }
            Filter::Scoped { rooms, dm_users } => {
                if rooms.is_empty() && dm_users.is_empty() {
                    return None;
                }
                let mut lost_room = None;
                for room in rooms {
                    match room_gone(room) {
                        Some(err) => lost_room = Some(err),
                        None => return None,
                    }
                }
                let mut quit_user = None;
                for track in dm_users {
                    match quit(track) {
                        Some(gone) => quit_user = Some(gone),
                        None => return None,
                    }
                }
                lost_room.or_else(|| {
                    quit_user.map(|(nick, reason)| Error::UserLeft { nick, reason })
                })
            }
        }
    }
}

#[cfg(test)]
#[path = "inbox_test.rs"]
mod test;

