use std::collections::HashSet;
use std::future::poll_fn;
use std::ops::Deref;
use std::panic::Location;
use std::sync::Arc;
use std::time::Duration;

use irc::proto::Command;
use irc_colors::Text;

use crate::ask::AskConfig;
use crate::error::{Error, Result};
use crate::inbox::Filter;
use crate::message::WireKind;
use crate::outbound::{sanitize_reason, sanitize_text, Priority};
pub use crate::room::InviteTarget;
use crate::room::Room;
use crate::user::{User, UserStatus, UserTrack};

/// Channel naming strategy for [`BreakoutOptions`].
#[derive(Clone, Debug)]
pub enum BreakoutNaming {
    /// Generates `#<prefix>-<6 lowercase alphanumeric>` (default).
    RandomSuffix { prefix: String },
    /// Uses the exact channel name provided.
    Exact(String),
}

impl BreakoutNaming {
    pub(crate) fn generate_channel_name(&self) -> String {
        match self {
            BreakoutNaming::Exact(exact) => exact.clone(),
            BreakoutNaming::RandomSuffix { prefix } => {
                const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
                let mut suffix = String::with_capacity(6);
                for _ in 0..6 {
                    let idx = fastrand::usize(..ALPHABET.len());
                    suffix.push(ALPHABET[idx] as char);
                }
                if prefix.starts_with(['#', '&']) {
                    format!("{prefix}-{suffix}")
                } else {
                    format!("#{prefix}-{suffix}")
                }
            }
        }
    }
}

/// Configuration options for [`crate::Bot::breakout_with`].
#[derive(Clone, Debug)]
pub struct BreakoutOptions {
    pub naming: BreakoutNaming,
    /// Modes applied upon creation (default: `Some("+is".into())`).
    pub modes: Option<String>,
    /// Verifies `@` chanop after `JOIN`/`NAMES` (default: `true`).
    pub require_op: bool,
    pub kick_on_drop: bool,
    pub kick_reason: Text,
    pub setup_timeout: Duration,
    /// Optional local [`AskConfig`] applied to the created [`Breakout`].
    pub ask_config: Option<AskConfig>,
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
            ask_config: None,
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
            ask_config: None,
        }
    }

    pub fn modes(mut self, modes: Option<impl Into<String>>) -> Self {
        self.modes = modes.map(Into::into);
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

    pub fn ask_config(mut self, cfg: &AskConfig) -> Self {
        self.ask_config = Some(cfg.clone());
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
            let (cmds, cm) = self.build_teardown_commands();
            let _ = self
                .room
                .core()
                .outbound
                .enqueue_batch(Priority::Background, cmds, cm);
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

    fn build_teardown_commands(&self) -> (Vec<Command>, crate::casemap::CaseMap) {
        let core = self.room.core();
        let ws = core.state.read().unwrap();
        let cm = ws.casemap;
        let chan_name = self.room.name().to_owned();
        let chan_key = cm.fold(&chan_name);

        let mut cmds = Vec::new();
        if self.opts.kick_on_drop {
            if let Some(chan) = ws.channels.get(&chan_key) {
                let mut targets: Vec<String> = chan
                    .members
                    .values()
                    .filter(|occ| !cm.str_eq(&occ.nick, &ws.current_nick))
                    .map(|occ| occ.nick.clone())
                    .collect();
                targets.sort();

                if !targets.is_empty() {
                    let raw_reason = sanitize_reason(&sanitize_text(self.opts.kick_reason.clone()).to_irc_string());
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
        (cmds, cm)
    }

    /// Clones the underlying [`Room`] view without moving or disarming `Breakout`.
    pub fn to_room(&self) -> Room {
        self.room.clone()
    }

    /// In-place [`AskConfig`] override (`mut self -> Self`), shadowing `Room::with_ask_config`
    /// so chaining on a fresh `Breakout` never drops it.
    pub fn with_ask_config(mut self, cfg: &AskConfig) -> Self {
        self.set_ask_config(cfg);
        self
    }

    pub fn set_ask_config(&mut self, cfg: &AskConfig) {
        self.room = self.room.with_ask_config(cfg);
    }

    /// Strict all-or-nothing invite handshake.
    pub async fn invite_and_wait<'a, I, P>(&self, targets: I, timeout: Duration) -> Result<()>
    where
        I: IntoIterator<Item = &'a P>,
        P: InviteTarget + ?Sized + 'a,
    {
        self.room.ensure_bot_in_room_if_tracked()?;
        let core = self.room.core();

        enum PendingTarget {
            Tracked(Arc<UserTrack>),
            Nick(String),
        }

        let mut resolved: Vec<(String, Option<Arc<UserTrack>>)> = Vec::new();
        for t in targets {
            resolved.push(crate::room::resolve_invite_target(t, core)?);
        }
        if resolved.is_empty() {
            return Ok(());
        }

        let tracked_users: Vec<Arc<UserTrack>> = resolved
            .iter()
            .filter_map(|(_, tr)| tr.clone())
            .collect();
        let cm = core.casemap();
        let mut inbox = core.inboxes.subscribe(
            Arc::clone(core),
            Filter::Scoped {
                rooms: vec![self.room.name().to_owned()],
                dm_users: tracked_users,
            },
            Location::caller(),
            cm,
        );

        let mut pending: Vec<PendingTarget> = Vec::new();
        let mut invite_cmds: Vec<Command> = Vec::new();
        for (nick, track_opt) in resolved {
            match track_opt {
                Some(track) => {
                    let already_in = {
                        let ws = core.state.read().unwrap();
                        let ckey = ws.casemap.fold(self.room.name());
                        ws.channels.get(&ckey).is_some_and(|c| {
                            c.members.values().any(|o| o.track.id == track.id)
                        })
                    };
                    if !already_in && !pending.iter().any(|p| matches!(p, PendingTarget::Tracked(t) if t.id == track.id)) {
                        invite_cmds.push(Command::INVITE(nick, self.room.name().to_owned()));
                        pending.push(PendingTarget::Tracked(track));
                    }
                }
                None => {
                    if !self.room.has_nick(&nick)
                        && !pending.iter().any(|p| matches!(p, PendingTarget::Nick(n) if cm.str_eq(n, &nick)))
                    {
                        invite_cmds.push(Command::INVITE(nick.clone(), self.room.name().to_owned()));
                        pending.push(PendingTarget::Nick(nick));
                    }
                }
            }
        }

        if pending.is_empty() {
            return Ok(());
        }

        let _ = core
            .outbound
            .enqueue_batch(Priority::Interactive, invite_cmds, cm);

        let sleep = tokio::time::sleep(timeout);
        tokio::pin!(sleep);

        loop {
            let wire_res = tokio::select! {
                biased;
                res = poll_fn(|cx| inbox.poll_next_wire(cx, false)) => res,
                _ = &mut sleep => {
                    return Err(Error::HandshakeTimeout {
                        operation: "breakout invite_and_wait",
                    });
                }
            };
            let wire = wire_res?;
            let live_cm = core.casemap();

            match &wire.kind {
                WireKind::Joined {
                    channel,
                    user,
                    nick,
                    ..
                } if live_cm.str_eq(channel, self.room.name()) => {
                    pending.retain(|p| match p {
                        PendingTarget::Tracked(t) => t.id != user.track_id(),
                        PendingTarget::Nick(n) => !live_cm.str_eq(n, nick),
                    });
                    if pending.is_empty() {
                        return Ok(());
                    }
                }
                WireKind::Quit {
                    user, nick, reason, ..
                } => {
                    let matched = pending.iter().any(|p| match p {
                        PendingTarget::Tracked(t) => t.id == user.track_id(),
                        PendingTarget::Nick(n) => live_cm.str_eq(n, nick),
                    });
                    if matched {
                        return Err(Error::UserLeft {
                            nick: nick.clone(),
                            reason: reason.clone(),
                        });
                    }
                }
                WireKind::SendError {
                    target,
                    target_user_id,
                    code: 401,
                    message,
                } => {
                    let matched = pending.iter().any(|p| match p {
                        PendingTarget::Tracked(t) => {
                            Some(t.id) == *target_user_id || live_cm.str_eq(&t.nick(), target)
                        }
                        PendingTarget::Nick(n) => live_cm.str_eq(n, target),
                    });
                    if matched {
                        return Err(Error::UserLeft {
                            nick: target.clone(),
                            reason: Some(message.clone()),
                        });
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

            // Also double-check tracked statuses in case a `Quit` updated `UserTrack` directly:
            for p in &pending {
                if let PendingTarget::Tracked(t) = p {
                    if let UserStatus::Quit { nick, reason, .. } = t.status() {
                        return Err(Error::UserLeft { nick, reason });
                    }
                }
            }
        }
    }

    /// Partial/pack-resilient invite handshake: skips offline users, invites online ones,
    /// finishes early once all invited users have either joined or quit, and returns whichever
    /// users joined before `timeout` elapsed.
    pub async fn invite_available<'a, I>(&self, users: I, timeout: Duration) -> Result<Vec<User>>
    where
        I: IntoIterator<Item = &'a User>,
    {
        self.room.ensure_bot_in_room_if_tracked()?;
        let core = self.room.core();

        let mut online_users: Vec<User> = Vec::new();
        let mut seen_ids: HashSet<u64> = HashSet::new();
        for u in users {
            if !u.belongs_to(core) {
                return Err(Error::InvalidInput(
                    "User handle belongs to a different Bot session",
                ));
            }
            if u.is_online() && seen_ids.insert(u.track_id()) {
                online_users.push(u.clone());
            }
        }

        if online_users.is_empty() {
            return Ok(Vec::new());
        }

        let dm_tracks: Vec<Arc<UserTrack>> = online_users.iter().map(User::track_arc).collect();
        let cm = core.casemap();
        let mut inbox = core.inboxes.subscribe(
            Arc::clone(core),
            Filter::Scoped {
                rooms: vec![self.room.name().to_owned()],
                dm_users: dm_tracks,
            },
            Location::caller(),
            cm,
        );

        let mut joined_ids: HashSet<u64> = HashSet::new();
        let mut gone_ids: HashSet<u64> = HashSet::new();
        let mut invite_cmds: Vec<Command> = Vec::new();

        for u in &online_users {
            if self.room.has_user(u) {
                joined_ids.insert(u.track_id());
            } else if u.is_online() {
                let nick = u.nick();
                if !nick.is_empty() {
                    invite_cmds.push(Command::INVITE(nick, self.room.name().to_owned()));
                }
            } else {
                gone_ids.insert(u.track_id());
            }
        }

        if !invite_cmds.is_empty() {
            let _ = core
                .outbound
                .enqueue_batch(Priority::Interactive, invite_cmds, cm);
        }

        if joined_ids.len() + gone_ids.len() >= online_users.len() {
            return Ok(online_users
                .into_iter()
                .filter(|u| joined_ids.contains(&u.track_id()))
                .collect());
        }

        let sleep = tokio::time::sleep(timeout);
        tokio::pin!(sleep);

        loop {
            let wire_res = tokio::select! {
                biased;
                res = poll_fn(|cx| inbox.poll_next_wire(cx, false)) => res,
                _ = &mut sleep => break,
            };

            match wire_res {
                Ok(wire) => {
                    let live_cm = core.casemap();
                    match &wire.kind {
                        WireKind::Joined { channel, user, .. }
                            if live_cm.str_eq(channel, self.room.name()) =>
                        {
                            if seen_ids.contains(&user.track_id()) {
                                joined_ids.insert(user.track_id());
                            }
                        }
                        WireKind::Quit { user, .. } => {
                            if seen_ids.contains(&user.track_id())
                                && !joined_ids.contains(&user.track_id())
                            {
                                gone_ids.insert(user.track_id());
                            }
                        }
                        WireKind::Parted { channel, user, .. }
                        | WireKind::Kicked { channel, user, .. }
                            if live_cm.str_eq(channel, self.room.name()) =>
                        {
                            joined_ids.remove(&user.track_id());
                            gone_ids.insert(user.track_id());
                        }
                        WireKind::SendError {
                            target_user_id: Some(uid),
                            code: 401,
                            ..
                        } => {
                            if seen_ids.contains(uid) && !joined_ids.contains(uid) {
                                gone_ids.insert(*uid);
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

                    for u in &online_users {
                        if !u.is_online() && !joined_ids.contains(&u.track_id()) {
                            gone_ids.insert(u.track_id());
                        }
                    }

                    if joined_ids.len() + gone_ids.len() >= online_users.len() {
                        break;
                    }
                }
                Err(Error::UserLeft { .. }) => break,
                Err(e) => return Err(e),
            }
        }

        Ok(online_users
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
        let cm = core.casemap();
        let mut inbox = core.inboxes.subscribe(
            Arc::clone(core),
            Filter::Scoped {
                rooms: vec![self.room.name().to_owned()],
                dm_users: Vec::new(),
            },
            Location::caller(),
            cm,
        );

        let (cmds, cmd_cm) = self.build_teardown_commands();
        let _ = core
            .outbound
            .enqueue_batch(Priority::Background, cmds, cmd_cm);

        // If the channel wasn't even joined in WorldState anymore, return immediately:
        {
            let ws = core.state.read().unwrap();
            let key = ws.casemap.fold(self.room.name());
            if !ws.channels.contains_key(&key) {
                return Ok(());
            }
        }

        let sleep = tokio::time::sleep(self.opts.setup_timeout);
        tokio::pin!(sleep);

        loop {
            let res = tokio::select! {
                biased;
                r = poll_fn(|cx| inbox.poll_next_wire(cx, false)) => r,
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
