use std::borrow::Borrow;
use std::collections::HashSet;
use std::fmt;
use std::future::poll_fn;
use std::ops::Deref;
use std::panic::Location;
use std::sync::Arc;
use std::time::Duration;

use irc::proto::Command;
use irc_colors::Text;

use crate::error::{Error, Result};
use crate::inbox::Filter;
use crate::message::{Event, WireKind};
use crate::outbound::{sanitize_reason, sanitize_text};
use crate::room::Room;
use crate::user::{User, UserStatus};

/// Channel naming strategy for [`BreakoutOptions`].
#[derive(Clone, Debug)]
pub(crate) enum BreakoutNaming {
    /// Generates `<prefix>-<6 lowercase alphanumeric>` (`prefix` must start with `'#'` or `'&'`,
    /// e.g. `"#vault"` or `"##vault"`).
    RandomSuffix { prefix: String },
    /// Uses the exact channel name provided (must start with `'#'` or `'&'`).
    Exact(String),
}

impl BreakoutNaming {
    pub(crate) fn generate_channel_name(&self) -> Result<String> {
        match self {
            BreakoutNaming::Exact(exact) => {
                if !exact.starts_with(['#', '&']) {
                    return Err(Error::InvalidInput(
                        "exact breakout channel name must start with '#' or '&'",
                    ));
                }
                Ok(exact.clone())
            }
            BreakoutNaming::RandomSuffix { prefix } => {
                if !prefix.starts_with(['#', '&']) {
                    return Err(Error::InvalidInput(
                        "breakout prefix must explicitly start with '#' or '&'",
                    ));
                }
                const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
                let mut suffix = String::with_capacity(6);
                for _ in 0..6 {
                    let idx = fastrand::usize(..ALPHABET.len());
                    suffix.push(ALPHABET[idx] as char);
                }
                Ok(format!("{prefix}-{suffix}"))
            }
        }
    }
}

/// Configuration options for [`crate::Bot::breakout_with`].
#[derive(Clone, Debug)]
pub struct BreakoutOptions {
    pub(crate) naming: BreakoutNaming,
    /// Modes applied upon creation (default: `Some("+is".into())`).
    pub modes: Option<String>,
    /// Verifies `@` chanop after `JOIN`/`NAMES` (default: `true`).
    pub require_op: bool,
    pub kick_on_drop: bool,
    pub kick_reason: Text,
    pub setup_timeout: Duration,
}

impl BreakoutOptions {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            naming: BreakoutNaming::RandomSuffix {
                prefix: prefix.into(),
            },
            modes: Some("+is".into()),
            require_op: true,
            kick_on_drop: true,
            kick_reason: Text::from("Session ended"),
            setup_timeout: Duration::from_secs(15),
        }
    }

    pub fn exact(channel: impl Into<String>) -> Self {
        Self {
            naming: BreakoutNaming::Exact(channel.into()),
            modes: Some("+is".into()),
            require_op: true,
            kick_on_drop: true,
            kick_reason: Text::from("Session ended"),
            setup_timeout: Duration::from_secs(15),
        }
    }

    /// Sets the modes applied upon creation (e.g. `"+im"`); see [`BreakoutOptions::no_modes`]
    /// to apply none.
    pub fn modes(mut self, modes: impl Into<String>) -> Self {
        self.modes = Some(modes.into());
        self
    }

    /// Disables automatic mode setting on breakout room creation (`modes = None`).
    pub fn no_modes(mut self) -> Self {
        self.modes = None;
        self
    }

    pub fn require_op(mut self, require: bool) -> Self {
        self.require_op = require;
        self
    }

    pub fn kick_on_drop(mut self, kick: bool) -> Self {
        self.kick_on_drop = kick;
        self
    }

    pub fn kick_reason(mut self, reason: impl Into<Text>) -> Self {
        self.kick_reason = reason.into();
        self
    }

    pub fn setup_timeout(mut self, timeout: Duration) -> Self {
        self.setup_timeout = timeout;
        self
    }
}

/// RAII ephemeral channel handle (`bot.breakout(prefix)` / `bot.breakout_with(opts)`).
#[must_use = "dropping a Breakout immediately parts the channel; bind it to a variable"]
pub struct Breakout {
    room: Room,
    opts: BreakoutOptions,
    armed: bool,
}

impl fmt::Debug for Breakout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Breakout")
            .field("room", &self.room)
            .field("armed", &self.armed)
            .finish_non_exhaustive()
    }
}

impl Deref for Breakout {
    type Target = Room;

    fn deref(&self) -> &Room {
        &self.room
    }
}

impl Drop for Breakout {
    fn drop(&mut self) {
        if self.armed {
            self.armed = false;
            let cmds = self.build_teardown_commands();
            self.room
                .core()
                .enqueue_part(self.room.name(), cmds, true);
        }
    }
}

impl Breakout {
    pub(crate) fn new(room: Room, opts: BreakoutOptions) -> Self {
        Self {
            room,
            opts,
            armed: true,
        }
    }

    fn build_teardown_commands(&self) -> Vec<Command> {
        let core = self.room.core();
        let ws = core.state.read().unwrap();
        let cm = ws.casemap;
        let chan_name = self.room.name().to_owned();

        let mut cmds = Vec::new();
        if self.opts.kick_on_drop {
            if let Some(roster) = ws.channel(&chan_name).and_then(|c| c.roster()) {
                let mut targets: Vec<String> = roster
                    .values()
                    .map(|occ| occ.track.nick())
                    .filter(|nick| !cm.str_eq(nick, &ws.current_nick))
                    .collect();
                targets.sort();

                if !targets.is_empty() {
                    let raw_reason = sanitize_reason(
                        &sanitize_text(self.opts.kick_reason.clone()).to_irc_string(),
                    );
                    let reason_opt = if raw_reason.is_empty() {
                        None
                    } else {
                        Some(raw_reason)
                    };
                    let batch_size = ws.targmax_kick.max(1);
                    for chunk in targets.chunks(batch_size) {
                        let joined_targets = chunk.join(",");
                        cmds.push(Command::KICK(
                            chan_name.clone(),
                            joined_targets,
                            reason_opt.clone(),
                        ));
                    }
                }
            }
        }

        cmds.push(Command::PART(chan_name, None));
        cmds
    }

    /// Borrows the underlying [`Room`] without moving or disarming `Breakout`.
    pub fn room(&self) -> &Room {
        &self.room
    }

    /// Strict all-or-nothing invite handshake.
    pub async fn invite_and_wait<I, U>(&self, users: I, timeout: Duration) -> Result<()>
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        self.invite_handshake(users, true, timeout).await.map(|_| ())
    }

    /// Partial/pack-resilient invite handshake: skips offline users, invites online ones,
    /// finishes early once all invited users have either joined or quit, and returns whichever
    /// users joined and remained present before `timeout` elapsed.
    pub async fn invite_available<I, U>(&self, users: I, timeout: Duration) -> Result<Vec<User>>
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        self.invite_handshake(users, false, timeout).await
    }

    async fn invite_handshake<I, U>(
        &self,
        users: I,
        strict: bool,
        timeout: Duration,
    ) -> Result<Vec<User>>
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        self.room.ensure_bot_in_room_if_tracked()?;
        let core = self.room.core();

        let mut tracked_users: Vec<User> = Vec::new();
        let mut seen_ids: HashSet<u64> = HashSet::new();
        for item in users {
            let u = item.borrow();
            if !u.belongs_to(core) {
                return Err(Error::InvalidInput(
                    "User handle belongs to a different Bot session",
                ));
            }
            if strict {
                if let Some(err) = u.quit_error() {
                    return Err(err);
                }
                if seen_ids.insert(u.track_id()) {
                    tracked_users.push(u.clone());
                }
            } else if u.is_online() && seen_ids.insert(u.track_id()) {
                tracked_users.push(u.clone());
            }
        }

        if tracked_users.is_empty() {
            return Ok(Vec::new());
        }

        let dm_tracks = tracked_users.iter().map(User::track_arc).collect();
        let mut inbox = core.inboxes.subscribe(
            Arc::clone(core),
            Filter::Scoped {
                rooms: vec![self.room.name().to_owned()],
                dm_users: dm_tracks,
            },
            Location::caller(),
        );

        let mut joined_ids: HashSet<u64> = HashSet::new();
        let mut gone_ids: HashSet<u64> = HashSet::new();
        let mut invite_cmds: Vec<Command> = Vec::new();

        for u in &tracked_users {
            if let Some(err) = u.quit_error() {
                if strict {
                    return Err(err);
                }
                gone_ids.insert(u.track_id());
            } else if self.room.has_user(u) {
                joined_ids.insert(u.track_id());
            } else {
                let nick = u.nick();
                if !nick.is_empty() {
                    invite_cmds.push(Command::INVITE(nick, self.room.name().to_owned()));
                }
            }
        }

        if !invite_cmds.is_empty() {
            core.outbound.enqueue_batch(invite_cmds);
        }

        if joined_ids.len() + gone_ids.len() >= tracked_users.len() {
            return Ok(tracked_users
                .into_iter()
                .filter(|u| joined_ids.contains(&u.track_id()))
                .collect());
        }

        let sleep = tokio::time::sleep(timeout);
        tokio::pin!(sleep);

        loop {
            let wire_res = tokio::select! {
                biased;
                res = poll_fn(|cx| inbox.poll_next_wire(cx)) => res,
                _ = &mut sleep => {
                    if strict {
                        return Err(Error::HandshakeTimeout {
                            operation: "breakout invite_and_wait",
                        });
                    }
                    break;
                }
            };

            match wire_res {
                Ok(wire) => {
                    let live_cm = core.casemap();
                    match &wire.kind {
                        WireKind::Event(Event::Joined { channel, user, .. })
                            if live_cm.str_eq(channel, self.room.name()) =>
                        {
                            if seen_ids.contains(&user.track_id()) && user.is_online() {
                                gone_ids.remove(&user.track_id());
                                joined_ids.insert(user.track_id());
                            }
                        }
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
                        ) if live_cm.str_eq(channel, self.room.name()) => {
                            if seen_ids.contains(&user.track_id()) {
                                joined_ids.remove(&user.track_id());
                                if strict {
                                    return Err(Error::UserLeft {
                                        nick: nick.clone(),
                                        reason: reason.clone(),
                                    });
                                }
                                gone_ids.insert(user.track_id());
                            }
                        }
                        WireKind::Event(Event::Quit {
                            user, nick, reason, ..
                        }) => {
                            if seen_ids.contains(&user.track_id()) {
                                joined_ids.remove(&user.track_id());
                                if strict {
                                    return Err(Error::UserLeft {
                                        nick: nick.clone(),
                                        reason: reason.clone(),
                                    });
                                }
                                gone_ids.insert(user.track_id());
                            }
                        }
                        WireKind::Event(Event::DeliveryFailed {
                            target,
                            code: 401,
                            message,
                            ..
                        }) => {
                            if let Some(uid) = wire.bounce_user_id() {
                                if seen_ids.contains(&uid) && !joined_ids.contains(&uid) {
                                    if strict {
                                        return Err(Error::UserLeft {
                                            nick: target.clone(),
                                            reason: Some(message.clone()),
                                        });
                                    }
                                    gone_ids.insert(uid);
                                }
                            }
                        }
                        WireKind::BotLeftRoom { channel, reason }
                            if live_cm.str_eq(channel, self.room.name()) =>
                        {
                            return Err(Error::BotLeftRoom {
                                channel: channel.clone(),
                                reason: reason.clone(),
                            });
                        }
                        _ => {}
                    }

                    for u in &tracked_users {
                        if let UserStatus::Quit { nick, reason, .. } = u.track_arc().status() {
                            joined_ids.remove(&u.track_id());
                            if strict {
                                return Err(Error::UserLeft { nick, reason });
                            }
                            gone_ids.insert(u.track_id());
                        }
                    }

                    if joined_ids.len() + gone_ids.len() >= tracked_users.len() {
                        break;
                    }
                }
                Err(e @ Error::UserLeft { .. }) => {
                    if strict {
                        return Err(e);
                    }
                    for u in &tracked_users {
                        if !u.is_online() {
                            joined_ids.remove(&u.track_id());
                        }
                    }
                    break;
                }
                Err(e) => return Err(e),
            }
        }

        Ok(tracked_users
            .into_iter()
            .filter(|u| joined_ids.contains(&u.track_id()))
            .collect())
    }

    /// Awaitable eviction & teardown: kicks remaining non-bot occupants (if `kick_on_drop`),
    /// sends `PART`, awaits server confirmation of self `PART`, and disarms `Drop`.
    pub async fn close(mut self) -> Result<()> {
        self.armed = false;
        let core = self.room.core();
        core.check_connected()?;

        // Subscribe before sending PART so we never miss the server's PART echo:
        let mut inbox = core.inboxes.subscribe(
            Arc::clone(core),
            Filter::Scoped {
                rooms: vec![self.room.name().to_owned()],
                dm_users: Vec::new(),
            },
            Location::caller(),
        );

        let cmds = self.build_teardown_commands();
        core.enqueue_part(self.room.name(), cmds, true);

        // If the channel wasn't even joined in WorldState anymore, return immediately:
        if core
            .state
            .read()
            .unwrap()
            .active_channel(self.room.name())
            .is_none()
        {
            return Ok(());
        }

        let sleep = tokio::time::sleep(self.opts.setup_timeout);
        tokio::pin!(sleep);

        loop {
            let res = tokio::select! {
                biased;
                r = poll_fn(|cx| inbox.poll_next_wire(cx)) => r,
                _ = &mut sleep => {
                    return Err(Error::HandshakeTimeout {
                        operation: "breakout close (PART)",
                    });
                }
            };
            match res {
                Ok(wire) => {
                    if matches!(wire.kind, WireKind::BotLeftRoom { .. }) {
                        return Ok(());
                    }
                }
                Err(Error::BotLeftRoom { .. }) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
#[path = "breakout_test.rs"]
mod test;

