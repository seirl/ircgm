use std::collections::HashSet;
use std::fmt;
use std::panic::Location;
use std::sync::Arc;
use std::time::Duration;

use irc_colors::Text;
use tokio::sync::OwnedMutexGuard;

use crate::ask::{Ask, IntoAskMatch};
use crate::bot::BotCore;
use crate::chat::{Chat, InboxRef};
use crate::error::{Error, Result};
use crate::inbox::{Filter, Inbox};
use crate::message::{ChatMessage, Target};
use crate::user::{User, UserStatus, UserTrack};

/// Held lock on a [`User`]'s interactive DM prompt slot (`UserTrack::dm_mutex`).
///
/// Records the holder's call site for contention warnings and clears it on `Drop`. Carries no
/// [`Inbox`]: [`ExclusiveDm`] pairs one lease with its own inbox, while [`crate::Huddle`] pairs
/// several leases with one shared inbox.
pub(crate) struct DmLease {
    user: User,
    _guard: OwnedMutexGuard<()>,
}

impl fmt::Debug for DmLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DmLease")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

impl Drop for DmLease {
    fn drop(&mut self) {
        *self.user.track().lock_holder.lock().unwrap() = None;
    }
}

impl DmLease {
    pub(crate) fn user(&self) -> &User {
        &self.user
    }
}

/// RAII guard holding exclusive access to a [`User`]'s interactive DM prompt slot
/// plus a pre-subscribed [`Inbox`].
pub struct ExclusiveDm {
    lease: DmLease,
    inbox: Inbox,
}

impl fmt::Debug for ExclusiveDm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExclusiveDm")
            .field("user", self.user())
            .finish_non_exhaustive()
    }
}

impl ExclusiveDm {
    /// Wraps `lease` with a fresh DM [`Inbox`] for its user.
    pub(crate) fn from_lease(
        core: Arc<BotCore>,
        lease: DmLease,
        caller: &'static Location<'static>,
    ) -> Self {
        let inbox = core.inboxes.subscribe(
            Arc::clone(&core),
            Filter::Scoped {
                rooms: Vec::new(),
                dm_users: vec![lease.user().track_arc()],
            },
            caller,
        );
        Self { lease, inbox }
    }

    pub fn user(&self) -> &User {
        self.lease.user()
    }

    /// Same as [`User::say`]: holding the lock changes who may prompt the user, not how
    /// lines are sent.
    pub fn say(&self, text: impl Into<Text>) -> Result<()> {
        self.user().say(text)
    }

    pub fn notice(&self, text: impl Into<Text>) -> Result<()> {
        self.user().notice(text)
    }

    pub fn action(&self, text: impl Into<Text>) -> Result<()> {
        self.user().action(text)
    }

    #[track_caller]
    pub fn ask<'a, F, R>(&'a mut self, prompt: impl Into<Text>, matcher: F) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoAskMatch + 'a,
        R::Output: Send + 'a,
    {
        crate::ask::ask(self, prompt, matcher)
    }

    pub fn inbox(&mut self) -> &mut Inbox {
        &mut self.inbox
    }
}

impl Chat for ExclusiveDm {
    fn say(&self, text: impl Into<Text>) -> Result<()> {
        ExclusiveDm::say(self, text)
    }

    fn notice(&self, text: impl Into<Text>) -> Result<()> {
        ExclusiveDm::notice(self, text)
    }

    fn action(&self, text: impl Into<Text>) -> Result<()> {
        ExclusiveDm::action(self, text)
    }

    fn inbox(&mut self) -> InboxRef<'_> {
        InboxRef::Borrowed(&mut self.inbox)
    }

    fn target(&self) -> Target {
        Target::Dm
    }

    fn participants(&self) -> Option<Vec<User>> {
        Some(vec![self.user().clone()])
    }
}

pub(crate) async fn acquire_dm_lease(
    core: &Arc<BotCore>,
    user: User,
    caller: &'static Location<'static>,
) -> Result<DmLease> {
    core.check_connected()?;
    if let Some(err) = user.quit_error() {
        return Err(err);
    }

    let mut status_rx = user.track().status_tx.subscribe();
    let lock_fut = Arc::clone(&user.track().dm_mutex).lock_owned();
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
                let holder = *user.track().lock_holder.lock().unwrap();
                tracing::warn!(
                    user_id = user.track().id,
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

    *user.track().lock_holder.lock().unwrap() = Some(caller);

    Ok(DmLease {
        user,
        _guard: guard,
    })
}

/// Locks the DM prompt slots of `users` in canonical `UserTrack::id` order, so concurrent
/// multi-user acquisitions never deadlock, and returns the leases in the caller's order.
///
/// With `strict`, a member who is offline, or quits before every lock is held, fails the whole
/// call with `UserLeft` (releasing the leases already taken). Otherwise such members are
/// skipped and the last one skipped is returned alongside the leases as `(nick, reason)`.
pub(crate) async fn acquire_dm_leases(
    core: &Arc<BotCore>,
    users: Vec<User>,
    strict: bool,
    caller: &'static Location<'static>,
) -> Result<(Vec<DmLease>, Option<(String, Option<String>)>)> {
    core.check_connected()?;

    let mut seen = HashSet::with_capacity(users.len());
    for u in &users {
        if !u.belongs_to(core) {
            return Err(Error::InvalidInput(
                "User handle belongs to a different Bot session",
            ));
        }
        if !seen.insert(u.track_id()) {
            return Err(Error::InvalidInput("duplicate User in multi-user DM lock"));
        }
    }

    let mut skipped = None;
    let mut indexed: Vec<(usize, User)> = Vec::with_capacity(users.len());
    for (idx, u) in users.into_iter().enumerate() {
        match u.quit_error() {
            Some(err) => skip_departed(err, strict, &mut skipped)?,
            None => indexed.push((idx, u)),
        }
    }

    indexed.sort_by_key(|(_, u)| u.track_id());
    let all_tracks: Vec<Arc<UserTrack>> = indexed.iter().map(|(_, u)| u.track_arc()).collect();
    let mut acquired: Vec<(usize, DmLease)> = Vec::with_capacity(indexed.len());
    for (orig_idx, u) in indexed {
        let res = if strict {
            // Any member quitting aborts at once, even while waiting on an earlier member's
            // contested lock:
            tokio::select! {
                biased;
                err = wait_any_track_quit(&all_tracks) => Err(err),
                res = acquire_dm_lease(core, u, caller) => res,
            }
        } else {
            acquire_dm_lease(core, u, caller).await
        };
        match res {
            Ok(lease) => acquired.push((orig_idx, lease)),
            Err(err) => skip_departed(err, strict, &mut skipped)?,
        }
    }

    // Restore caller order, dropping members who quit while later locks were being acquired:
    acquired.sort_by_key(|(orig_idx, _)| *orig_idx);
    let mut leases = Vec::with_capacity(acquired.len());
    for (_, lease) in acquired {
        match lease.user().quit_error() {
            Some(err) => skip_departed(err, strict, &mut skipped)?,
            None => leases.push(lease),
        }
    }
    Ok((leases, skipped))
}

/// Records a departed member in `skipped`, or fails with `err` if `strict` (or if `err` is not a
/// departure).
fn skip_departed(
    err: Error,
    strict: bool,
    skipped: &mut Option<(String, Option<String>)>,
) -> Result<()> {
    match err {
        Error::UserLeft { nick, reason } if !strict => {
            *skipped = Some((nick, reason));
            Ok(())
        }
        err => Err(err),
    }
}

async fn wait_any_track_quit(tracks: &[Arc<UserTrack>]) -> Error {
    let waiters = tracks.iter().map(|t| {
        let mut rx = t.status_tx.subscribe();
        async move {
            loop {
                if let UserStatus::Quit { nick, reason, .. } = rx.borrow_and_update().clone() {
                    return Error::UserLeft { nick, reason };
                }
                if rx.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        }
    });
    crate::future::race_all(waiters).await
}

#[cfg(test)]
#[path = "exclusive_dm_test.rs"]
mod test;
