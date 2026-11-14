use std::collections::HashMap;
use std::fmt;
use std::panic::Location;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

use irc_colors::Text;
use tokio::sync::watch;

use crate::casemap::{AtomicCaseMap, CaseMap, FoldedStr};
use crate::core::BotCore;
use crate::error::{DisconnectReason, Error, Result};
use crate::inbox::{Filter, Inbox};
use crate::outbound::OutboundChatKind;

static NEXT_USER_ID: AtomicU64 = AtomicU64::new(1);

/// Live connection state of a tracked IRC user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UserStatus {
    Online,
    Quit {
        at_seq: u64,
        nick: String,
        reason: Option<String>,
    },
}

/// Normalizes IRCv3 account strings (`"*"`, `"0"`, or `""` mean unauthenticated -> `None`).
pub(crate) fn normalize_account(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "*" || trimmed == "0" {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

#[derive(Debug)]
pub(crate) struct UserTrack {
    pub(crate) id: u64,
    pub(crate) current_nick: RwLock<String>,
    pub(crate) account: RwLock<Option<String>>,
    pub(crate) status_tx: watch::Sender<UserStatus>,
    pub(crate) dm_mutex: Arc<tokio::sync::Mutex<()>>,
    pub(crate) lock_holder: Mutex<Option<&'static Location<'static>>>,
    pub(crate) casemap: AtomicCaseMap,
}

impl UserTrack {
    fn new(id: u64, nick: String, casemap: AtomicCaseMap) -> Self {
        let (status_tx, _) = watch::channel(UserStatus::Online);
        Self {
            id,
            current_nick: RwLock::new(nick),
            account: RwLock::new(None),
            status_tx,
            dm_mutex: Arc::new(tokio::sync::Mutex::new(())),
            lock_holder: Mutex::new(None),
            casemap,
        }
    }

    pub(crate) fn nick(&self) -> String {
        self.current_nick.read().unwrap().clone()
    }

    pub(crate) fn set_nick(&self, new_nick: String) {
        *self.current_nick.write().unwrap() = new_nick;
    }

    pub(crate) fn account(&self) -> Option<String> {
        self.account.read().unwrap().clone()
    }

    pub(crate) fn set_account(&self, acct: Option<String>) {
        *self.account.write().unwrap() = acct;
    }

    pub(crate) fn casemap(&self) -> CaseMap {
        self.casemap.load()
    }

    pub(crate) fn is_online(&self) -> bool {
        matches!(*self.status_tx.borrow(), UserStatus::Online)
    }

    pub(crate) fn status(&self) -> UserStatus {
        self.status_tx.borrow().clone()
    }
}

#[derive(Default)]
struct RegistryState {
    casemap: CaseMap,
    tracks: HashMap<FoldedStr, Weak<UserTrack>>,
    last_live_count: usize,
}

impl RegistryState {
    /// Amortized dead-`Weak` sweep: runs during normal registry access whenever map capacity/len
    /// exceeds `2 * last_live_count.max(32)`. Never called from `Drop`!
    fn maybe_sweep(&mut self) {
        let threshold = 2 * self.last_live_count.max(32);
        if self.tracks.len() > threshold {
            self.tracks.retain(|_, weak| weak.strong_count() > 0);
            self.last_live_count = self.tracks.len();
        }
    }
}

pub(crate) struct UserRegistry {
    state: Mutex<RegistryState>,
    casemap: AtomicCaseMap,
}

impl Default for UserRegistry {
    fn default() -> Self {
        Self::new(AtomicCaseMap::default())
    }
}

impl UserRegistry {
    pub(crate) fn new(casemap: AtomicCaseMap) -> Self {
        let initial = casemap.load();
        Self {
            state: Mutex::new(RegistryState {
                casemap: initial,
                ..RegistryState::default()
            }),
            casemap,
        }
    }

    /// Looks up an already-live `UserTrack` without allocating a new one.
    pub(crate) fn get_existing(&self, nick: &str) -> Option<Arc<UserTrack>> {
        let mut state = self.state.lock().unwrap();
        state.maybe_sweep();
        let folded = state.casemap.fold(nick);
        state.tracks.get(&folded).and_then(Weak::upgrade)
    }

    /// Resolves `nick` to an existing live `UserTrack`, or allocates a new online track.
    pub(crate) fn get_or_create(&self, nick: &str) -> Arc<UserTrack> {
        let mut state = self.state.lock().unwrap();
        state.maybe_sweep();
        let folded = state.casemap.fold(nick);

        if let Some(existing) = state.tracks.get(&folded).and_then(Weak::upgrade) {
            return existing;
        }

        let id = NEXT_USER_ID.fetch_add(1, Ordering::Relaxed);
        let track = Arc::new(UserTrack::new(id, nick.to_owned(), self.casemap.clone()));
        state.tracks.insert(folded, Arc::downgrade(&track));
        track
    }

    /// Re-keys `old_nick -> new_nick` on `NICK` and updates the track's live nickname.
    /// Returns the affected `Arc<UserTrack>`.
    pub(crate) fn rename(&self, old_nick: &str, new_nick: &str) -> Arc<UserTrack> {
        let mut state = self.state.lock().unwrap();
        state.maybe_sweep();
        let old_folded = state.casemap.fold(old_nick);
        let new_folded = state.casemap.fold(new_nick);

        let track = if let Some(existing) = state.tracks.remove(&old_folded).and_then(|w| w.upgrade())
        {
            existing
        } else if let Some(existing) = state.tracks.get(&new_folded).and_then(Weak::upgrade) {
            existing
        } else {
            let id = NEXT_USER_ID.fetch_add(1, Ordering::Relaxed);
            Arc::new(UserTrack::new(
                id,
                new_nick.to_owned(),
                self.casemap.clone(),
            ))
        };

        track.set_nick(new_nick.to_owned());
        state.tracks.insert(new_folded, Arc::downgrade(&track));
        track
    }

    /// Updates `account` if `nick` is currently tracked.
    pub(crate) fn update_account(&self, nick: &str, raw_account: &str) -> Option<Arc<UserTrack>> {
        let acct = normalize_account(raw_account);
        let track = self.get_existing(nick)?;
        track.set_account(acct);
        Some(track)
    }

    /// Marks `nick` offline (`Quit { at_seq, nick, reason }`) and removes it from the
    /// active nick table so a future rejoin allocates a fresh `UserTrack`.
    pub(crate) fn mark_quit(
        &self,
        nick: &str,
        at_seq: u64,
        reason: Option<String>,
    ) -> Option<Arc<UserTrack>> {
        let track = {
            let mut state = self.state.lock().unwrap();
            state.maybe_sweep();
            let folded = state.casemap.fold(nick);
            state.tracks.remove(&folded).and_then(|w| w.upgrade())
        }?;

        let live_nick = track.nick();
        let _ = track.status_tx.send_replace(UserStatus::Quit {
            at_seq,
            nick: live_nick,
            reason,
        });
        Some(track)
    }

    /// Resolves a non-channel send-refusal / PM-block target (`401`, `486`, `716`, etc.) to
    /// `(target_user_id, quit_track_id)`.
    pub(crate) fn resolve_send_error(
        &self,
        target: &str,
        code: u16,
        at_seq: u64,
        message: &str,
    ) -> (Option<u64>, Option<u64>) {
        if code == 401 {
            match self.mark_quit(target, at_seq, Some(message.to_owned())) {
                Some(track) => (Some(track.id), Some(track.id)),
                None => (None, None),
            }
        } else {
            (self.get_existing(target).map(|t| t.id), None)
        }
    }

    /// Rebuilds all `FoldedStr` keys when `005 CASEMAPPING` changes dynamically.
    pub(crate) fn rekey_casemap(&self, new_map: CaseMap) {
        let mut state = self.state.lock().unwrap();
        state.casemap = new_map;
        self.casemap.store(new_map);
        let old_tracks = std::mem::take(&mut state.tracks);
        let mut rebuilt = HashMap::with_capacity(old_tracks.len());
        for (_, weak) in old_tracks {
            if let Some(track) = weak.upgrade() {
                let folded = new_map.fold(&track.nick());
                rebuilt.insert(folded, weak);
            }
        }
        state.last_live_count = rebuilt.len();
        state.tracks = rebuilt;
    }

    pub(crate) fn clear(&self) {
        let mut state = self.state.lock().unwrap();
        state.tracks.clear();
        state.last_live_count = 0;
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn tracked_map_len(&self) -> usize {
        self.state.lock().unwrap().tracks.len()
    }
}

/// Lightweight, `Copy`, immutable session identifier for a [`User`] ([`User::id`]).
///
/// Suitable as a key in `HashMap<UserId, V>`, `HashSet<UserId>`, `BTreeMap<UserId, V>`,
/// or `BTreeSet<UserId>` without interior-mutability lints (`clippy::mutable_key_type`),
/// handle cloning on lookups, or keeping offline [`User`] allocations alive in score/vote tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UserId(pub(crate) u64);

impl UserId {
    /// Returns the raw session-unique monotonic `u64` identifier.
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Config-free, cycle-free, `NICK`/`QUIT`-aware session identity handle.
///
/// Equality uses the session-unique [`UserId`] (`UserTrack::id`), surviving `/nick` changes
/// and RFC 1459 case-folding. `User` deliberately omits `Hash` and `Ord` in favor of
/// [`User::id`]: `UserTrack` contains interior mutability (`RwLock`/`Mutex`/`AtomicU8`), which
/// would trigger `clippy::mutable_key_type` on every caller's `HashMap<User, _>` / `HashSet<User>`,
/// and keying long-lived tables by `UserId` avoids pinning departed `UserTrack` allocations.
#[derive(Clone, Debug)]
pub struct User {
    core: Weak<BotCore>,
    track: Arc<UserTrack>,
}

impl PartialEq for User {
    fn eq(&self, other: &Self) -> bool {
        self.track.id == other.track.id
    }
}

impl Eq for User {}

impl User {
    pub(crate) fn from_parts(core: Weak<BotCore>, track: Arc<UserTrack>) -> Self {
        Self { core, track }
    }

    pub(crate) fn track_id(&self) -> u64 {
        self.track.id
    }

    /// Returns this user's lightweight `Copy` session identifier (`Hash + Eq + Ord`).
    pub fn id(&self) -> UserId {
        UserId(self.track.id)
    }

    pub(crate) fn track(&self) -> &Arc<UserTrack> {
        &self.track
    }

    pub(crate) fn track_arc(&self) -> Arc<UserTrack> {
        Arc::clone(&self.track)
    }

    pub(crate) fn belongs_to(&self, core: &Arc<BotCore>) -> bool {
        self.core
            .upgrade()
            .is_some_and(|mine| Arc::ptr_eq(&mine, core))
    }

    pub(crate) fn upgrade_core(&self) -> Result<Arc<BotCore>> {
        let core = self
            .core
            .upgrade()
            .ok_or(Error::Disconnected(DisconnectReason::BotDropped))?;
        core.check_connected()?;
        Ok(core)
    }

    pub(crate) fn quit_error(&self) -> Option<Error> {
        match self.track.status() {
            UserStatus::Online => None,
            UserStatus::Quit { nick, reason, .. } => Some(Error::UserLeft { nick, reason }),
        }
    }

    pub(crate) fn send_chat(&self, kind: OutboundChatKind, text: Text) -> Result<()> {
        let core = self.upgrade_core()?;
        if let Some(err) = self.quit_error() {
            return Err(err);
        }
        let nick = self.track.nick();
        core.send_chat(kind, &nick, text)
    }

    /// Current nickname (live, automatically updated on `NICK`).
    pub fn nick(&self) -> String {
        self.track.nick()
    }

    /// Compares `nick` against this user's live nickname under the bot's active [`CaseMap`].
    pub fn eq_nick(&self, nick: &str) -> bool {
        self.track.casemap().str_eq(&self.track.nick(), nick)
    }

    /// Authenticated IRC Services account name (`None` if unauthenticated).
    pub fn account(&self) -> Option<String> {
        self.track.account()
    }

    pub fn is_online(&self) -> bool {
        self.track.is_online()
    }

    /// Non-blocking one-way outbound DM (does NOT acquire `dm_mutex`).
    pub fn say(&self, text: impl Into<Text>) -> Result<()> {
        self.send_chat(OutboundChatKind::Privmsg, text.into())
    }

    pub fn notice(&self, text: impl Into<Text>) -> Result<()> {
        self.send_chat(OutboundChatKind::Notice, text.into())
    }

    pub fn action(&self, text: impl Into<Text>) -> Result<()> {
        self.send_chat(OutboundChatKind::Action, text.into())
    }

    /// Opens an unserialized DM [`Inbox`] tracking `NICK` changes and failing with
    /// `Err(Error::UserLeft { .. })` on `QUIT` or `401`.
    #[track_caller]
    pub fn inbox(&self) -> Inbox {
        let caller = Location::caller();
        self.inbox_at(caller)
    }

    pub(crate) fn inbox_at(&self, caller: &'static Location<'static>) -> Inbox {
        let filter = Filter::Scoped {
            rooms: Vec::new(),
            dm_users: vec![Arc::clone(&self.track)],
        };
        match self.core.upgrade() {
            Some(core) => core.inboxes.subscribe(core.clone(), filter, caller),
            None => Inbox::disconnected_bot_dropped(filter, caller),
        }
    }
}

#[cfg(test)]
#[path = "user_test.rs"]
mod test;
