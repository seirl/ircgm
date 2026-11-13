use std::collections::HashSet;
use std::fmt;
use std::panic::Location;
use std::sync::Arc;
use std::time::Duration;

use irc_colors::Text;
use tokio::sync::OwnedMutexGuard;

use crate::ask::{Ask, AskConfig, AskSource, IntoAskMatch};
use crate::bot::BotCore;
use crate::error::{Error, Result};
use crate::inbox::{Filter, Inbox};
use crate::message::ChatMessage;
use crate::outbound::{OutboundChatKind, WriteReceipt};
use crate::user::{User, UserStatus, UserTrack};

/// RAII guard holding exclusive access to a [`User`]'s interactive DM prompt slot
/// plus a pre-subscribed [`Inbox`] and local [`AskConfig`].
pub struct ExclusiveDm {
    core: Arc<BotCore>,
    user: User,
    inbox: Inbox,
    ask_config: AskConfig,
    _guard: OwnedMutexGuard<()>,
}

impl fmt::Debug for ExclusiveDm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExclusiveDm")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

impl Drop for ExclusiveDm {
    fn drop(&mut self) {
        *self.user.track().lock_holder.lock().unwrap() = None;
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
        self.ask_config = cfg.clone();
        cfg.apply_to_locked_inbox(&mut self.inbox);
    }

    pub fn ignore_commands(mut self) -> Self {
        self.ask_config.ignore_commands = true;
        self.inbox.ignore_commands = true;
        self
    }

    pub fn accept_notices(mut self) -> Self {
        self.ask_config.accept_notices = true;
        self.inbox.accept_notices = true;
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
        let base_cfg = self.ask_config.clone();
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

pub(crate) async fn acquire_single_dm_guard(
    core: Arc<BotCore>,
    user: User,
    caller: &'static Location<'static>,
) -> Result<ExclusiveDm> {
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

    let cm = core.casemap();
    let mut inbox = core.inboxes.subscribe(
        Arc::clone(&core),
        Filter::Scoped {
            rooms: Vec::new(),
            dm_users: vec![user.track_arc()],
        },
        caller,
        cm,
    );
    let ask_config = (*core.ask_config).clone();
    ask_config.apply_to_locked_inbox(&mut inbox);

    Ok(ExclusiveDm {
        core,
        user,
        inbox,
        ask_config,
        _guard: guard,
    })
}

pub(crate) async fn acquire_many_dm_guards(
    core: &Arc<BotCore>,
    users: Vec<User>,
    caller: &'static Location<'static>,
) -> Result<Vec<ExclusiveDm>> {
    core.check_connected()?;

    let mut seen = HashSet::with_capacity(users.len());
    let mut indexed: Vec<(usize, User)> = Vec::with_capacity(users.len());
    for (idx, u) in users.into_iter().enumerate() {
        if !u.belongs_to(core) {
            return Err(Error::InvalidInput(
                "User handle belongs to a different Bot session",
            ));
        }
        if !seen.insert(u.track_id()) {
            return Err(Error::InvalidInput("duplicate User in exclusive_many"));
        }
        indexed.push((idx, u));
    }

    if indexed.is_empty() {
        return Ok(Vec::new());
    }

    for (_, u) in &indexed {
        if let Some(err) = u.quit_error() {
            return Err(err);
        }
    }

    // Canonical lock acquisition order (`UserTrack::id` ascending):
    indexed.sort_by_key(|(_, u)| u.track_id());

    let all_tracks: Vec<Arc<UserTrack>> = indexed.iter().map(|(_, u)| u.track_arc()).collect();
    let mut acquired: Vec<(usize, ExclusiveDm)> = Vec::with_capacity(indexed.len());

    for (orig_idx, u) in indexed {
        // Watch ALL remaining/acquired members so any member quitting aborts immediately
        // even while waiting on an earlier member's contested mutex!
        let acquire_fut = acquire_single_dm_guard(Arc::clone(core), u, caller);
        let other_quit_fut = wait_any_track_quit(&all_tracks);

        let guard = tokio::select! {
            biased;
            err = other_quit_fut => return Err(err),
            res = acquire_fut => res?,
        };
        acquired.push((orig_idx, guard));

        for (_, g) in &acquired {
            if let Some(err) = g.user().quit_error() {
                return Err(err);
            }
        }
    }

    // Restore caller iteration order:
    acquired.sort_by_key(|(orig_idx, _)| *orig_idx);
    Ok(acquired.into_iter().map(|(_, g)| g).collect())
}

pub(crate) async fn acquire_available_dm_guards(
    core: &Arc<BotCore>,
    users: Vec<User>,
    caller: &'static Location<'static>,
) -> Result<(Vec<ExclusiveDm>, Option<(String, Option<String>)>)> {
    core.check_connected()?;

    let mut seen = HashSet::with_capacity(users.len());
    let mut indexed: Vec<(usize, User)> = Vec::new();
    let mut last_offline: Option<(String, Option<String>)> = None;
    for (idx, u) in users.into_iter().enumerate() {
        if !u.belongs_to(core) {
            return Err(Error::InvalidInput(
                "User handle belongs to a different Bot session",
            ));
        }
        if !seen.insert(u.track_id()) {
            return Err(Error::InvalidInput("duplicate User in exclusive_available"));
        }
        if let Some(Error::UserLeft { nick, reason }) = u.quit_error() {
            last_offline = Some((nick, reason));
        } else {
            indexed.push((idx, u));
        }
    }

    if indexed.is_empty() {
        return Ok((Vec::new(), last_offline));
    }

    indexed.sort_by_key(|(_, u)| u.track_id());

    let mut acquired: Vec<(usize, ExclusiveDm)> = Vec::with_capacity(indexed.len());
    for (orig_idx, u) in indexed {
        match acquire_single_dm_guard(Arc::clone(core), u, caller).await {
            Ok(guard) => acquired.push((orig_idx, guard)),
            Err(Error::UserLeft { nick, reason }) => {
                last_offline = Some((nick, reason));
            }
            Err(fatal) => return Err(fatal),
        }
    }

    // Drop any guard whose user disconnected while later guards were being acquired:
    acquired.retain(|(_, g)| {
        if let Some(Error::UserLeft { nick, reason }) = g.user().quit_error() {
            last_offline = Some((nick, reason));
            false
        } else {
            true
        }
    });
    acquired.sort_by_key(|(orig_idx, _)| *orig_idx);
    Ok((acquired.into_iter().map(|(_, g)| g).collect(), last_offline))
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
