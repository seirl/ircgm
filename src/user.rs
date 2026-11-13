use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::panic::Location;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use irc_colors::Text;
use tokio::sync::{watch, OwnedMutexGuard};
use tracing::Instrument as _;

use crate::ask::{Ask, AskConfig, AskSource, IntoAskMatch};
use crate::bot::BotCore;
use crate::casemap::{CaseMap, FoldedStr};
use crate::error::{DisconnectReason, Error, Result};
use crate::inbox::{Filter, Inbox};
use crate::message::ChatMessage;
use crate::outbound::{OutboundChatKind, WriteReceipt};
use crate::room::Room;

static NEXT_USER_ID: AtomicU64 = AtomicU64::new(1);
pub(crate) const MAX_RENAME_RETRIES: u8 = 3;

/// Live connection state of a tracked IRC user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UserStatus {
    Online,
    Quit {
        at_seq: u64,
        nick: String,
        reason: Option<String>,
        netsplit: bool,
    },
}

/// Heuristic detector for IRC server-to-server netsplit quit reasons (`"server1.tld server2.tld"`
/// or wildcard masked `"*.net *.split"`).
pub(crate) fn is_netsplit_reason(reason: &str) -> bool {
    let mut parts = reason.split_whitespace();
    let (Some(left), Some(right), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    is_server_mask(left) && is_server_mask(right)
}

fn is_server_mask(tok: &str) -> bool {
    if tok.len() < 3 || tok.starts_with('.') || tok.ends_with('.') {
        return false;
    }
    let Some((_, tld)) = tok.rsplit_once('.') else {
        return false;
    };
    if tld.is_empty() || !tld.chars().any(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    tok.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '*'))
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
    pub(crate) casemap: Arc<AtomicU8>,
}

impl UserTrack {
    fn new(id: u64, nick: String, casemap: Arc<AtomicU8>) -> Self {
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
        CaseMap::from_u8(self.casemap.load(Ordering::Relaxed))
    }

    pub(crate) fn is_online(&self) -> bool {
        matches!(*self.status_tx.borrow(), UserStatus::Online)
    }

    pub(crate) fn status(&self) -> UserStatus {
        self.status_tx.borrow().clone()
    }

    /// Returns `Some(new_nick)` if a `401` bounced an outdated pre-rename nick while
    /// `self` is still online under `new_nick` and retry budget remains.
    pub(crate) fn next_rename_retry_nick(
        &self,
        bounced_target: &str,
        retries: &mut u8,
    ) -> Option<String> {
        let cur_nick = self.nick();
        if self.is_online()
            && !cur_nick.is_empty()
            && !self.casemap().str_eq(&cur_nick, bounced_target)
            && *retries < MAX_RENAME_RETRIES
        {
            *retries += 1;
            Some(cur_nick)
        } else {
            None
        }
    }
}

#[derive(Default)]
struct RegistryState {
    tracks: HashMap<FoldedStr, Weak<UserTrack>>,
    /// Vacated pre-rename nicks (`folded -> (raw_old_nick, Weak<UserTrack>)`) so in-flight
    /// bounces (`401`, `486`, `716`, etc.) crossing with `NICK` still resolve `target_user_id`.
    /// Evicted immediately if another user claims that nick.
    former_tracks: HashMap<FoldedStr, (String, Weak<UserTrack>)>,
    last_live_count: usize,
    last_former_count: usize,
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
        let former_threshold = 2 * self.last_former_count.max(32);
        if self.former_tracks.len() > former_threshold {
            self.former_tracks
                .retain(|_, (_, weak)| weak.upgrade().is_some_and(|t| t.is_online()));
            self.last_former_count = self.former_tracks.len();
        }
    }
}

pub(crate) struct UserRegistry {
    state: Mutex<RegistryState>,
    casemap: Arc<AtomicU8>,
}

impl Default for UserRegistry {
    fn default() -> Self {
        Self::new(CaseMap::default())
    }
}

impl UserRegistry {
    pub(crate) fn new(initial_casemap: CaseMap) -> Self {
        Self {
            state: Mutex::new(RegistryState::default()),
            casemap: Arc::new(AtomicU8::new(initial_casemap.to_u8())),
        }
    }

    pub(crate) fn casemap(&self) -> CaseMap {
        CaseMap::from_u8(self.casemap.load(Ordering::Relaxed))
    }

    /// Looks up an already-live `UserTrack` without allocating a new one.
    pub(crate) fn get_existing(&self, nick: &str) -> Option<Arc<UserTrack>> {
        let cm = self.casemap();
        let folded = cm.fold(nick);
        let mut state = self.state.lock().unwrap();
        state.maybe_sweep();
        state.tracks.get(&folded).and_then(Weak::upgrade)
    }

    /// Resolves `nick` to an existing live `UserTrack`, or allocates a new online track.
    pub(crate) fn get_or_create(&self, nick: &str) -> Arc<UserTrack> {
        let cm = self.casemap();
        let folded = cm.fold(nick);
        let mut state = self.state.lock().unwrap();
        state.maybe_sweep();

        if let Some(existing) = state.tracks.get(&folded).and_then(Weak::upgrade) {
            return existing;
        }

        state.former_tracks.remove(&folded);
        let id = NEXT_USER_ID.fetch_add(1, Ordering::Relaxed);
        let track = Arc::new(UserTrack::new(id, nick.to_owned(), Arc::clone(&self.casemap)));
        state.tracks.insert(folded, Arc::downgrade(&track));
        track
    }

    /// Re-keys `old_nick -> new_nick` on `NICK` and updates the track's live nickname.
    /// Returns the affected `Arc<UserTrack>`.
    pub(crate) fn rename(&self, old_nick: &str, new_nick: &str) -> Arc<UserTrack> {
        let cm = self.casemap();
        let old_folded = cm.fold(old_nick);
        let new_folded = cm.fold(new_nick);
        let mut state = self.state.lock().unwrap();
        state.maybe_sweep();

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
                Arc::clone(&self.casemap),
            ))
        };

        track.set_nick(new_nick.to_owned());
        state.former_tracks.remove(&new_folded);
        if old_folded != new_folded {
            state
                .former_tracks
                .insert(old_folded, (old_nick.to_owned(), Arc::downgrade(&track)));
        }
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

    /// Marks `nick` offline (`Quit { at_seq, nick, reason, netsplit }`) and removes it from the
    /// active nick table so a future rejoin allocates a fresh `UserTrack`.
    pub(crate) fn mark_quit(
        &self,
        nick: &str,
        at_seq: u64,
        reason: Option<String>,
        check_netsplit: bool,
    ) -> Option<(Arc<UserTrack>, bool)> {
        let cm = self.casemap();
        let folded = cm.fold(nick);
        let track = {
            let mut state = self.state.lock().unwrap();
            state.maybe_sweep();
            state.tracks.remove(&folded).and_then(|w| w.upgrade())
        }?;

        let netsplit = check_netsplit
            && reason
                .as_deref()
                .map(is_netsplit_reason)
                .unwrap_or(false);
        let live_nick = track.nick();
        let _ = track.status_tx.send_replace(UserStatus::Quit {
            at_seq,
            nick: live_nick,
            reason,
            netsplit,
        });
        Some((track, netsplit))
    }

    /// Resolves a non-channel send-refusal / PM-block target (`401`, `486`, `716`, etc.) to
    /// `(target_user_id, quit_track_id)`.
    ///
    /// On `401 ERR_NOSUCHNICK`, only an *active* entry in `tracks` is marked `Quit`; if `target`
    /// was vacated by a recent `NICK` (`former_tracks`), the online user keeps their new nick and
    /// `Online` status while still receiving `target_user_id = Some(track.id)`.
    pub(crate) fn resolve_send_error(
        &self,
        target: &str,
        code: u16,
        at_seq: u64,
        message: &str,
    ) -> (Option<u64>, Option<u64>) {
        let cm = self.casemap();
        let folded = cm.fold(target);
        if code == 401 {
            let (active, former_id) = {
                let mut state = self.state.lock().unwrap();
                state.maybe_sweep();
                if let Some(track) = state.tracks.remove(&folded).and_then(|w| w.upgrade()) {
                    (Some(track), None)
                } else {
                    let fid = state
                        .former_tracks
                        .get(&folded)
                        .and_then(|(_, w)| w.upgrade())
                        .filter(|t| t.is_online())
                        .map(|t| t.id);
                    (None, fid)
                }
            };
            if let Some(track) = active {
                let live_nick = track.nick();
                let _ = track.status_tx.send_replace(UserStatus::Quit {
                    at_seq,
                    nick: live_nick,
                    reason: Some(message.to_owned()),
                    netsplit: false,
                });
                (Some(track.id), Some(track.id))
            } else {
                (former_id, None)
            }
        } else {
            let mut state = self.state.lock().unwrap();
            state.maybe_sweep();
            let id = state
                .tracks
                .get(&folded)
                .and_then(Weak::upgrade)
                .or_else(|| {
                    state
                        .former_tracks
                        .get(&folded)
                        .and_then(|(_, w)| w.upgrade())
                        .filter(|t| t.is_online())
                })
                .map(|t| t.id);
            (id, None)
        }
    }

    /// Rebuilds all `FoldedStr` keys when `005 CASEMAPPING` changes dynamically.
    pub(crate) fn rekey_casemap(&self, new_map: CaseMap) {
        self.casemap.store(new_map.to_u8(), Ordering::Relaxed);
        let mut state = self.state.lock().unwrap();
        let old_tracks = std::mem::take(&mut state.tracks);
        let mut rebuilt = HashMap::with_capacity(old_tracks.len());
        for (_, weak) in old_tracks {
            if let Some(track) = weak.upgrade() {
                let folded = new_map.fold(&track.nick());
                rebuilt.insert(folded, weak);
            }
        }
        let old_former = std::mem::take(&mut state.former_tracks);
        let mut rebuilt_former = HashMap::with_capacity(old_former.len());
        for (_, (raw_old, weak)) in old_former {
            if weak.upgrade().is_some_and(|t| t.is_online()) {
                let folded = new_map.fold(&raw_old);
                if !rebuilt.contains_key(&folded) {
                    rebuilt_former.insert(folded, (raw_old, weak));
                }
            }
        }
        state.last_live_count = rebuilt.len();
        state.last_former_count = rebuilt_former.len();
        state.tracks = rebuilt;
        state.former_tracks = rebuilt_former;
    }

    pub(crate) fn clear(&self) {
        let mut state = self.state.lock().unwrap();
        state.tracks.clear();
        state.former_tracks.clear();
        state.last_live_count = 0;
        state.last_former_count = 0;
    }

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
/// and RFC 1459 case-folding. For `HashMap`/`HashSet`/`BTreeMap` keys, use [`User::id`].
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

    pub(crate) fn send_chat_with_receipt(
        &self,
        kind: OutboundChatKind,
        text: Text,
    ) -> Result<WriteReceipt> {
        let core = self.upgrade_core()?;
        if let Some(err) = self.quit_error() {
            return Err(err);
        }
        let nick = self.track.nick();
        core.send_chat_with_receipt(kind, &nick, text)
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
        let _ = self.send_chat_with_receipt(OutboundChatKind::Privmsg, text.into())?;
        Ok(())
    }

    pub fn notice(&self, text: impl Into<Text>) -> Result<()> {
        let _ = self.send_chat_with_receipt(OutboundChatKind::Notice, text.into())?;
        Ok(())
    }

    pub fn action(&self, text: impl Into<Text>) -> Result<()> {
        let _ = self.send_chat_with_receipt(OutboundChatKind::Action, text.into())?;
        Ok(())
    }

    /// Acquires this user's per-user DM mutex and opens a dedicated [`Inbox`] initialized with
    /// `BotConfig::ask_config`.
    #[track_caller]
    pub fn exclusive(&self) -> impl Future<Output = Result<ExclusiveDm>> + Send + '_ {
        let caller = Location::caller();
        self.exclusive_at(caller)
    }

    pub(crate) async fn exclusive_at(
        &self,
        caller: &'static Location<'static>,
    ) -> Result<ExclusiveDm> {
        let core = self.upgrade_core()?;
        let span = tracing::debug_span!(
            "ircgm::exclusive",
            user_id = self.track.id,
            nick = %self.nick()
        );
        acquire_single_dm_guard(core, self.clone(), caller)
            .instrument(span)
            .await
    }

    /// Acquires `exclusive()`, sends `prompt` in DM, starts the reply deadline *after*
    /// `OutboundSink`'s write receipt resolves, and awaits one matching reply.
    #[track_caller]
    pub fn ask<'a, F, R>(&'a self, prompt: impl Into<Text>, matcher: F) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoAskMatch + 'a,
        R::Output: Send + 'a,
    {
        let caller = Location::caller();
        let base_cfg = self
            .core
            .upgrade()
            .map(|c| (*c.ask_config).clone())
            .unwrap_or_default();
        Ask::new(
            AskSource::UserDm { user: self, caller },
            base_cfg,
            prompt.into(),
            matcher,
        )
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
            Some(core) => {
                let cm = core.casemap();
                core.inboxes.subscribe(core.clone(), filter, caller, cm)
            }
            None => Inbox::disconnected_bot_dropped(filter, caller),
        }
    }

    /// Opens a channel [`Inbox`] matching only this user's messages inside `room`.
    #[track_caller]
    pub fn inbox_in(&self, room: &Room) -> Inbox {
        let caller = Location::caller();
        self.inbox_in_at(room, caller)
    }

    pub(crate) fn inbox_in_at(&self, room: &Room, caller: &'static Location<'static>) -> Inbox {
        let filter = Filter::UsersInRoom {
            tracks: vec![Arc::clone(&self.track)],
            channel_name: room.name().to_owned(),
        };
        match self.core.upgrade() {
            Some(core) if Arc::ptr_eq(&core, room.core()) => {
                let cm = core.casemap();
                let mut inbox = core.inboxes.subscribe(core.clone(), filter, caller, cm);
                room.ask_config().apply_to_inbox(&mut inbox);
                inbox
            }
            Some(_) => Inbox::invalid_session(filter, caller),
            None => Inbox::disconnected_bot_dropped(filter, caller),
        }
    }
}

pub(crate) async fn acquire_single_dm_guard(
    core: Arc<BotCore>,
    user: User,
    caller: &'static Location<'static>,
) -> Result<ExclusiveDm> {
    core.check_connected()?;
    if let Some(err) = user.quit_error() {
        return Err(err);
    }

    let mut status_rx = user.track.status_tx.subscribe();
    let lock_fut = Arc::clone(&user.track.dm_mutex).lock_owned();
    tokio::pin!(lock_fut);

    let warn_sleep = tokio::time::sleep(Duration::from_secs(15));
    tokio::pin!(warn_sleep);
    let mut warned = false;

    let guard = loop {
        tokio::select! {
            biased;
            disc = core.wait_disconnected() => {
                return Err(Error::Disconnected(disc));
            }
            st_res = status_rx.changed() => {
                if st_res.is_ok() {
                    let st = status_rx.borrow_and_update().clone();
                    if let UserStatus::Quit { nick, reason, .. } = st {
                        return Err(Error::UserLeft { nick, reason });
                    }
                }
            }
            g = &mut lock_fut => {
                break g;
            }
            _ = &mut warn_sleep, if !warned => {
                warned = true;
                let holder = *user.track.lock_holder.lock().unwrap();
                tracing::warn!(
                    user_id = user.track.id,
                    nick = %user.nick(),
                    waiter_at = %caller,
                    holder_at = ?holder.map(|loc| loc.to_string()),
                    "waiting >15s to acquire User ExclusiveDm mutex"
                );
            }
        }
    };

    core.check_connected()?;
    if let Some(err) = user.quit_error() {
        return Err(err);
    }

    *user.track.lock_holder.lock().unwrap() = Some(caller);

    let cm = core.casemap();
    let mut inbox = core.inboxes.subscribe(
        Arc::clone(&core),
        Filter::Scoped {
            rooms: Vec::new(),
            dm_users: vec![Arc::clone(&user.track)],
        },
        caller,
        cm,
    );
    inbox.idle_timeout = Some(Duration::from_secs(300));
    let ask_config = Arc::clone(&core.ask_config);
    ask_config.apply_to_inbox(&mut inbox);

    Ok(ExclusiveDm {
        core,
        user,
        inbox,
        ask_config,
        _guard: guard,
    })
}

/// RAII guard holding exclusive access to a [`User`]'s interactive DM prompt slot
/// plus a pre-subscribed [`Inbox`] and local [`AskConfig`].
pub struct ExclusiveDm {
    core: Arc<BotCore>,
    user: User,
    inbox: Inbox,
    ask_config: Arc<AskConfig>,
    _guard: OwnedMutexGuard<()>,
}

impl Drop for ExclusiveDm {
    fn drop(&mut self) {
        *self.user.track.lock_holder.lock().unwrap() = None;
    }
}

impl ExclusiveDm {
    pub(crate) fn core(&self) -> &Arc<BotCore> {
        &self.core
    }

    pub(crate) fn split_for_ask(&mut self) -> (&Arc<BotCore>, &User, &mut Inbox) {
        (&self.core, &self.user, &mut self.inbox)
    }

    pub(crate) fn send_chat_with_receipt(
        &self,
        kind: OutboundChatKind,
        text: Text,
    ) -> Result<WriteReceipt> {
        self.core.check_connected()?;
        if let Some(err) = self.user.quit_error() {
            return Err(err);
        }
        let nick = self.user.nick();
        self.core.send_chat_with_receipt(kind, &nick, text)
    }

    pub fn user(&self) -> &User {
        &self.user
    }

    pub fn with_ask_config(mut self, cfg: &AskConfig) -> Self {
        self.set_ask_config(cfg);
        self
    }

    pub fn set_ask_config(&mut self, cfg: &AskConfig) {
        self.ask_config = Arc::new(cfg.clone());
        cfg.apply_to_inbox(&mut self.inbox);
    }

    pub fn with_ignore_commands(mut self) -> Self {
        self.inbox.set_ignore_commands(true);
        self
    }

    pub fn with_accept_notices(mut self) -> Self {
        self.inbox.set_accept_notices(true);
        self
    }

    pub fn say(&self, text: impl Into<Text>) -> Result<()> {
        let _ = self.send_chat_with_receipt(OutboundChatKind::Privmsg, text.into())?;
        Ok(())
    }

    pub fn notice(&self, text: impl Into<Text>) -> Result<()> {
        let _ = self.send_chat_with_receipt(OutboundChatKind::Notice, text.into())?;
        Ok(())
    }

    pub fn action(&self, text: impl Into<Text>) -> Result<()> {
        let _ = self.send_chat_with_receipt(OutboundChatKind::Action, text.into())?;
        Ok(())
    }

    pub fn ask<'a, F, R>(&'a mut self, prompt: impl Into<Text>, matcher: F) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoAskMatch + 'a,
        R::Output: Send + 'a,
    {
        let mut base_cfg = (*self.ask_config).clone();
        self.inbox.sync_flags_to_ask_config(&mut base_cfg);
        Ask::new(
            AskSource::BorrowedDm { dm: self },
            base_cfg,
            prompt.into(),
            matcher,
        )
    }

    pub fn inbox(&mut self) -> &mut Inbox {
        &mut self.inbox
    }
}

#[cfg(test)]
#[path = "user_test.rs"]
mod test;
