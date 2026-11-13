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

use crate::ask::{Ask, AskConfig, AskSource, IntoVerdict};
use crate::bot::BotCore;
use crate::casemap::{CaseMap, FoldedStr};
use crate::error::{DisconnectReason, Error, Result};
use crate::inbox::{Filter, Inbox};
use crate::message::ChatMessage;
use crate::outbound::{OutboundChatKind, WriteReceipt};
use crate::room::Room;

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
}

#[derive(Default)]
struct RegistryState {
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
    casemap: Arc<AtomicU8>,
    next_id: AtomicU64,
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
            next_id: AtomicU64::new(1),
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

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
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
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            Arc::new(UserTrack::new(
                id,
                new_nick.to_owned(),
                Arc::clone(&self.casemap),
            ))
        };

        track.set_nick(new_nick.to_owned());
        state.tracks.insert(new_folded, Arc::downgrade(&track));
        track
    }

    /// Updates `account` if `nick` is currently tracked (or creates track if `create_if_missing`).
    pub(crate) fn update_account(
        &self,
        nick: &str,
        raw_account: &str,
        create_if_missing: bool,
    ) -> Option<Arc<UserTrack>> {
        let acct = normalize_account(raw_account);
        let track = if create_if_missing {
            Some(self.get_or_create(nick))
        } else {
            self.get_existing(nick)
        }?;
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
        state.last_live_count = rebuilt.len();
        state.tracks = rebuilt;
    }

    pub(crate) fn clear(&self) {
        let mut state = self.state.lock().unwrap();
        state.tracks.clear();
        state.last_live_count = 0;
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
        R: IntoVerdict + 'a,
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
        let filter = Filter::UserDm(Arc::clone(&self.track));
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
        let filter = Filter::UserInRoom {
            track: Arc::clone(&self.track),
            channel_name: room.name().to_owned(),
        };
        match self.core.upgrade() {
            Some(core) if Arc::ptr_eq(&core, room.core()) => {
                let cm = core.casemap();
                core.inboxes.subscribe(core.clone(), filter, caller, cm)
            }
            _ => Inbox::disconnected_bot_dropped(filter, caller),
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
        Filter::UserDm(Arc::clone(&user.track)),
        caller,
        cm,
    );
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
        Arc::make_mut(&mut self.ask_config).ignore_commands = true;
        self.inbox.set_ignore_commands(true);
        self
    }

    pub fn with_accept_notices(mut self) -> Self {
        Arc::make_mut(&mut self.ask_config).accept_notices = true;
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
        R: IntoVerdict + 'a,
        R::Output: Send + 'a,
    {
        let base_cfg = (*self.ask_config).clone();
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
mod tests {
    use super::*;

    #[test]
    fn rfc1459_identity_rename_and_quit_rejoin() {
        let reg = UserRegistry::new(CaseMap::Rfc1459);
        let t1 = reg.get_or_create("[Alice]");
        let t2 = reg.get_or_create("{alice}");
        assert_eq!(t1.id, t2.id);

        // Survives /nick rename:
        let renamed = reg.rename("{alice}", "Alice2");
        assert_eq!(renamed.id, t1.id);
        assert_eq!(t1.nick(), "Alice2");
        assert!(reg.get_existing("[Alice]").is_none());
        assert_eq!(reg.get_or_create("alice2").id, t1.id);

        // QUIT + rejoin allocates a fresh UserTrack::id:
        let (quit_track, netsplit) = reg
            .mark_quit("Alice2", 42, Some("hub.eu.net leaf.us.net".into()), true)
            .unwrap();
        assert_eq!(quit_track.id, t1.id);
        assert!(netsplit);
        assert!(!t1.is_online());
        assert_eq!(
            t1.status(),
            UserStatus::Quit {
                at_seq: 42,
                nick: "Alice2".into(),
                reason: Some("hub.eu.net leaf.us.net".into()),
                netsplit: true,
            }
        );

        let rejoined = reg.get_or_create("Alice2");
        assert_ne!(rejoined.id, t1.id);
        assert!(rejoined.is_online());
    }

    #[test]
    fn netsplit_detection_and_account_transitions() {
        assert!(is_netsplit_reason("hub.eu.net leaf.us.net"));
        assert!(is_netsplit_reason("*.net *.split"));
        assert!(!is_netsplit_reason("Quit: Leaving"));
        assert!(!is_netsplit_reason("Ping timeout: 240 seconds"));
        assert!(!is_netsplit_reason("single.server.net"));
        assert!(!is_netsplit_reason("1.2 3.4"));

        let reg = UserRegistry::default();
        let u = reg.get_or_create("Alice");
        assert_eq!(u.account(), None);

        reg.update_account("alice", "alice_acct", false);
        assert_eq!(u.account().as_deref(), Some("alice_acct"));

        reg.update_account("ALICE", "*", false);
        assert_eq!(u.account(), None);

        reg.update_account("ALICE", "0", false);
        assert_eq!(u.account(), None);
    }

    #[test]
    fn amortized_dead_weak_gc_without_drop_reentrancy() {
        let reg = UserRegistry::default();
        // Create and immediately drop 65 transient users (> 2 * 32 = 64):
        for i in 0..65 {
            let _temp = reg.get_or_create(&format!("Transient{i}"));
        }
        // Trigger one more access so `len > 2 * 32` sweeps dead Weak entries:
        let _keep = reg.get_or_create("Survivor");
        assert_eq!(reg.tracked_map_len(), 1);
    }

    #[test]
    fn dynamic_casemapping_rekey() {
        let reg = UserRegistry::new(CaseMap::Rfc1459);
        let a = reg.get_or_create("[Alice]");
        assert_eq!(reg.get_or_create("{alice}").id, a.id);

        reg.rekey_casemap(CaseMap::Ascii);
        assert_eq!(a.casemap(), CaseMap::Ascii);
        assert_eq!(reg.get_or_create("[alice]").id, a.id);
        // Under Ascii, `{alice}` is now a distinct user from `[Alice]`:
        let b = reg.get_or_create("{alice}");
        assert_ne!(b.id, a.id);
    }
}
