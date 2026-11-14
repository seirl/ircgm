use std::borrow::Borrow;
use std::collections::HashSet;
use std::panic::Location;
use std::sync::Arc;

use irc::proto::Command;
use irc_colors::Text;

use crate::ask::{Ask, AskConfig, AskSource, IntoAskMatch};
use crate::bot::BotCore;
use crate::casemap::CaseMap;
use crate::error::{Error, Result};
use crate::inbox::{Filter, Inbox};
use crate::message::ChatMessage;
use crate::outbound::{
    sanitize_reason, sanitize_text, validate_param, validate_target, OutboundChatKind, Priority,
    WriteReceipt,
};
use crate::state::BotPresence;
use crate::user::{User, UserId};

/// Snapshot of a channel occupant in `WorldState`, preserving [`User`] identity and status prefixes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub user: User,
    pub nick: String,
    /// `true` when the member holds `@` (`+o`) or higher channel privilege (`&`, `~`).
    pub is_op: bool,
    /// `true` when the member holds `+` (`+v`).
    pub is_voiced: bool,
    /// Raw status prefix symbols ordered from highest to lowest per `005 PREFIX` (e.g. `['@', '+']`).
    pub prefixes: Vec<char>,
}

/// Lightweight handle to an IRC channel (`#lobby`), carrying a local [`AskConfig`].
#[derive(Clone, Debug)]
pub struct Room {
    core: Arc<BotCore>,
    channel_name: String,
    ask_config: Arc<AskConfig>,
}

impl Room {
    pub(crate) fn new(
        core: Arc<BotCore>,
        channel_name: String,
        ask_config: Arc<AskConfig>,
    ) -> Self {
        Self {
            core,
            channel_name,
            ask_config,
        }
    }

    pub(crate) fn core(&self) -> &Arc<BotCore> {
        &self.core
    }

    pub(crate) fn belongs_to(&self, core: &Arc<BotCore>) -> bool {
        Arc::ptr_eq(&self.core, core)
    }

    pub(crate) fn into_name(self) -> String {
        self.channel_name
    }

    pub(crate) fn ensure_bot_in_room_if_tracked(&self) -> Result<()> {
        self.core.check_connected()?;
        let ws = self.core.state.read().unwrap();
        ws.check_bot_in_channel(&self.channel_name, BotPresence::Tracked)
    }

    pub(crate) fn ensure_user_askable(&self, user: &User) -> Result<()> {
        self.core.check_connected()?;
        if let Some(err) = user.quit_error() {
            return Err(err);
        }
        let ws = self.core.state.read().unwrap();
        ws.check_bot_in_channel(&self.channel_name, BotPresence::Tracked)?;
        let synced = ws.channel(&self.channel_name).and_then(|c| c.members());
        if synced.is_some_and(|members| !members.contains_key(&user.track_id())) {
            return Err(Error::UserLeft {
                nick: user.nick(),
                reason: None,
            });
        }
        Ok(())
    }

    pub(crate) fn ensure_any_user_askable(&self, users: &[User]) -> Result<()> {
        self.ensure_bot_in_room_if_tracked()?;
        if users.is_empty() {
            return Err(Error::InvalidInput("ask_users requires at least one User"));
        }
        for u in users {
            if !u.belongs_to(&self.core) {
                return Err(Error::InvalidInput(
                    "User handle belongs to a different Bot session",
                ));
            }
        }
        let mut last_offline_err: Option<Error> = None;
        for u in users {
            match self.ensure_user_askable(u) {
                Ok(()) => return Ok(()),
                Err(e @ Error::UserLeft { .. }) => last_offline_err = Some(e),
                Err(fatal) => return Err(fatal),
            }
        }
        Err(last_offline_err.expect("non-empty users with no askable user must have UserLeft"))
    }

    fn ensure_can_moderate(&self) -> Result<usize> {
        self.core.check_connected()?;
        let ws = self.core.state.read().unwrap();
        ws.check_bot_in_channel(&self.channel_name, BotPresence::Op)?;
        Ok(ws.max_modes.max(1))
    }

    pub(crate) fn send_chat(&self, kind: OutboundChatKind, text: Text) -> Result<WriteReceipt> {
        self.ensure_bot_in_room_if_tracked()?;
        self.core.send_chat(kind, &self.channel_name, text)
    }

    pub fn name(&self) -> &str {
        &self.channel_name
    }

    pub fn casemap(&self) -> CaseMap {
        self.core.casemap()
    }

    /// The [`AskConfig`] this room's prompts and inboxes start from. Clone it to derive a
    /// tweaked view: `room.with_ask_config(&room.ask_config().clone().reply_timeout(dur))`.
    pub fn ask_config(&self) -> &AskConfig {
        &self.ask_config
    }

    /// Derives a lightweight `Room` view carrying `cfg` as its local [`AskConfig`].
    pub fn with_ask_config(&self, cfg: &AskConfig) -> Self {
        Self {
            core: Arc::clone(&self.core),
            channel_name: self.channel_name.clone(),
            ask_config: Arc::new(cfg.clone()),
        }
    }

    pub fn say(&self, text: impl Into<Text>) -> Result<()> {
        self.send_chat(OutboundChatKind::Privmsg, text.into())
            .map(|_| ())
    }

    pub fn notice(&self, text: impl Into<Text>) -> Result<()> {
        self.send_chat(OutboundChatKind::Notice, text.into())
            .map(|_| ())
    }

    pub fn action(&self, text: impl Into<Text>) -> Result<()> {
        self.send_chat(OutboundChatKind::Action, text.into())
            .map(|_| ())
    }

    /// Sends `INVITE <user> <channel>`. Fails immediately with `Err(Error::UserLeft { .. })`
    /// without sending traffic if `user` is already known offline.
    pub fn invite(&self, user: &User) -> Result<()> {
        if !user.belongs_to(&self.core) {
            return Err(Error::InvalidInput(
                "User handle belongs to a different Bot session",
            ));
        }
        self.ensure_bot_in_room_if_tracked()?;
        if let Some(err) = user.quit_error() {
            return Err(err);
        }
        let nick = user.nick();
        validate_target(&nick, "invite target")?;
        self.core.outbound.enqueue_one(
            Priority::Interactive,
            Command::INVITE(nick, self.channel_name.clone()),
        );
        Ok(())
    }

    pub fn part(&self, reason: Option<&str>) -> Result<()> {
        self.core.check_connected()?;
        let clean = reason.map(sanitize_reason);
        self.core.enqueue_part(
            &self.channel_name,
            Priority::Interactive,
            vec![Command::PART(self.channel_name.clone(), clean)],
            false,
        );
        Ok(())
    }

    pub fn members(&self) -> Vec<Member> {
        let ws = self.core.state.read().unwrap();
        let Some(roster) = ws.channel(&self.channel_name).and_then(|c| c.roster()) else {
            return Vec::new();
        };
        let weak = Arc::downgrade(&self.core);
        let mut out: Vec<Member> = roster
            .values()
            .map(|occ| {
                let nick = occ.track.nick();
                Member {
                    user: User::from_parts(weak.clone(), Arc::clone(&occ.track)),
                    nick,
                    is_op: ws.occupant_is_op(occ),
                    is_voiced: ws.occupant_is_voiced(occ),
                    prefixes: occ.prefixes.clone(),
                }
            })
            .collect();
        out.sort_by(|a, b| a.nick.cmp(&b.nick));
        out
    }

    pub fn member(&self, user: &User) -> Option<Member> {
        if !user.belongs_to(&self.core) {
            return None;
        }
        let ws = self.core.state.read().unwrap();
        let occ = ws
            .channel(&self.channel_name)?
            .roster()?
            .get(&user.track_id())?;
        Some(Member {
            user: User::from_parts(Arc::downgrade(&self.core), Arc::clone(&occ.track)),
            nick: occ.track.nick(),
            is_op: ws.occupant_is_op(occ),
            is_voiced: ws.occupant_is_voiced(occ),
            prefixes: occ.prefixes.clone(),
        })
    }

    /// Returns the sorted current nicknames of all occupants in this room.
    pub fn nicks(&self) -> Vec<String> {
        self.members().into_iter().map(|m| m.nick).collect()
    }

    pub fn has_user(&self, user: &User) -> bool {
        if !user.belongs_to(&self.core) {
            return false;
        }
        let ws = self.core.state.read().unwrap();
        ws.channel(&self.channel_name)
            .and_then(|c| c.roster())
            .is_some_and(|roster| roster.contains_key(&user.track_id()))
    }

    pub fn has_nick(&self, nick: &str) -> bool {
        let ws = self.core.state.read().unwrap();
        ws.channel(&self.channel_name)
            .is_some_and(|chan| chan.member_by_nick(nick).is_some())
    }

    pub fn bot_is_op(&self) -> bool {
        let ws = self.core.state.read().unwrap();
        ws.channel(&self.channel_name)
            .and_then(|chan| ws.self_occupant(chan))
            .is_some_and(|occ| ws.occupant_is_op(occ))
    }

    pub fn topic(&self) -> Option<String> {
        let ws = self.core.state.read().unwrap();
        ws.channel(&self.channel_name).and_then(|c| c.topic.clone())
    }

    pub fn kick(&self, user: &User, reason: Option<&str>) -> Result<()> {
        if !user.belongs_to(&self.core) {
            return Err(Error::InvalidInput(
                "User handle belongs to a different Bot session",
            ));
        }
        self.ensure_can_moderate()?;
        if let Some(err) = user.quit_error() {
            return Err(err);
        }
        let nick = user.nick();
        validate_target(&nick, "kick target")?;
        let clean = reason.map(sanitize_reason);
        self.core.outbound.enqueue_one(
            Priority::Interactive,
            Command::KICK(self.channel_name.clone(), nick, clean),
        );
        Ok(())
    }

    pub fn set_mode(&self, modes: &str, args: &[&str]) -> Result<()> {
        self.ensure_can_moderate()?;
        validate_param(modes, "mode flags")?;
        for arg in args {
            validate_param(arg, "mode argument")?;
        }
        let mut raw_args = Vec::with_capacity(2 + args.len());
        raw_args.push(self.channel_name.clone());
        raw_args.push(modes.to_owned());
        for arg in args {
            raw_args.push((*arg).to_owned());
        }
        self.core
            .outbound
            .enqueue_one(Priority::Interactive, Command::Raw("MODE".into(), raw_args));
        Ok(())
    }

    pub fn voice(&self, user: &User) -> Result<()> {
        self.voice_many([user])
    }

    pub fn voice_many<I, U>(&self, users: I) -> Result<()>
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        self.apply_status_many('+', 'v', users)
    }

    pub fn devoice(&self, user: &User) -> Result<()> {
        self.devoice_many([user])
    }

    pub fn devoice_many<I, U>(&self, users: I) -> Result<()>
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        self.apply_status_many('-', 'v', users)
    }

    fn apply_status_many<I, U>(&self, sign: char, mode_char: char, users: I) -> Result<()>
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        let max_modes = self.ensure_can_moderate()?;
        let mut nicks = Vec::new();
        for u_item in users {
            let u = u_item.borrow();
            if !u.belongs_to(&self.core) {
                return Err(Error::InvalidInput(
                    "User handle belongs to a different Bot session",
                ));
            }
            if !u.is_online() {
                continue;
            }
            let nick = u.nick();
            validate_target(&nick, "mode target")?;
            nicks.push(nick);
        }
        if nicks.is_empty() {
            return Ok(());
        }

        let mut cmds = Vec::with_capacity(nicks.len().div_ceil(max_modes));
        for chunk in nicks.chunks(max_modes) {
            let mut flags = String::with_capacity(1 + chunk.len());
            flags.push(sign);
            for _ in chunk {
                flags.push(mode_char);
            }
            let mut raw_args = Vec::with_capacity(2 + chunk.len());
            raw_args.push(self.channel_name.clone());
            raw_args.push(flags);
            raw_args.extend(chunk.iter().cloned());
            cmds.push(Command::Raw("MODE".into(), raw_args));
        }

        self.core.outbound.enqueue_batch(Priority::Interactive, cmds);
        Ok(())
    }

    pub fn set_topic(&self, topic: impl Into<Text>) -> Result<()> {
        self.ensure_can_moderate()?;
        let cleaned = sanitize_text(topic.into());
        let wire_topic = {
            let ws = self.core.state.read().unwrap();
            let budget = ws.payload_budget(
                "TOPIC",
                &self.channel_name,
                self.core.max_payload_bytes,
                false,
            );
            let mut chunks = cleaned.wrap(budget);
            if chunks.is_empty() {
                String::new()
            } else {
                chunks.remove(0).to_irc_string()
            }
        };
        self.core.outbound.enqueue_one(
            Priority::Interactive,
            Command::TOPIC(self.channel_name.clone(), Some(wire_topic)),
        );
        Ok(())
    }

    /// Creates a cancellation-safe [`ModerationGuard`] snapshotting this room's current topic and
    /// voiced (`+v`) occupants so temporary game moderation (`+m`, `+v`/`-v`, topic updates) is
    /// automatically reverted on `Drop` if the bot still holds chanop.
    ///
    /// Fails immediately if the bot is disconnected, not joined to this room
    /// ([`Error::BotLeftRoom`]), lacks chanop / is in a modeless `+` channel
    /// ([`Error::NotChannelOp`]), or the server's `005 CHANMODES` does not support `+m`
    /// ([`Error::InvalidInput`]).
    pub fn moderation_guard(&self) -> Result<ModerationGuard> {
        self.core.check_connected()?;
        {
            let ws = self.core.state.read().unwrap();
            ws.check_bot_in_channel(&self.channel_name, BotPresence::Op)?;
            if !ws.chan_modes.supports_flag('m') {
                return Err(Error::InvalidInput(
                    "server 005 CHANMODES does not support +m moderation",
                ));
            }
        }
        Ok(ModerationGuard::new(self.clone()))
    }

    #[track_caller]
    pub fn ask<'a, F, R>(&'a self, prompt: impl Into<Text>, matcher: F) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoAskMatch + 'a,
        R::Output: Send + 'a,
    {
        let caller = Location::caller();
        Ask::new(
            AskSource::RoomOpen {
                room: self,
                caller,
            },
            (*self.ask_config).clone(),
            prompt.into(),
            matcher,
        )
    }

    #[track_caller]
    pub fn ask_user<'a, F, R>(
        &'a self,
        user: &'a User,
        prompt: impl Into<Text>,
        matcher: F,
    ) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoAskMatch + 'a,
        R::Output: Send + 'a,
    {
        let caller = Location::caller();
        Ask::new(
            AskSource::RoomUser {
                room: self,
                user,
                caller,
            },
            (*self.ask_config).clone(),
            prompt.into(),
            matcher,
        )
    }

    /// Prompts the channel and accepts replies only from `users` (ignoring spectators and
    /// resolving with [`crate::Answer::Left`] only if *every* listed user leaves/quits).
    #[track_caller]
    pub fn ask_users<'a, I, U, F, R>(
        &'a self,
        users: I,
        prompt: impl Into<Text>,
        matcher: F,
    ) -> Ask<'a, F, R>
    where
        I: IntoIterator<Item = U>,
        U: std::borrow::Borrow<User>,
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoAskMatch + 'a,
        R::Output: Send + 'a,
    {
        let caller = Location::caller();
        let collected: Vec<User> = users.into_iter().map(|u| u.borrow().clone()).collect();
        Ask::new(
            AskSource::RoomUsers {
                room: self,
                users: collected,
                caller,
            },
            (*self.ask_config).clone(),
            prompt.into(),
            matcher,
        )
    }

    #[track_caller]
    pub fn inbox(&self) -> Inbox {
        let caller = Location::caller();
        self.inbox_at(caller)
    }

    /// Opens a channel [`Inbox`] matching only messages and presence events from `user` inside
    /// this room (symmetric 1-player counterpart to [`Room::inbox_users`] and [`Room::ask_user`]).
    #[track_caller]
    pub fn inbox_user(&self, user: &User) -> Inbox {
        let caller = Location::caller();
        self.inbox_users_at(std::slice::from_ref(user), caller)
    }

    /// Opens a channel [`Inbox`] matching only messages and presence events from `users`
    /// inside this room (ignoring spectators, and terminating with [`Error::UserLeft`] once
    /// all listed users have left or quit).
    #[track_caller]
    pub fn inbox_users<I, U>(&self, users: I) -> Inbox
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        let caller = Location::caller();
        let collected: Vec<User> = users.into_iter().map(|u| u.borrow().clone()).collect();
        self.inbox_users_at(&collected, caller)
    }

    pub(crate) fn inbox_users_at(
        &self,
        users: &[User],
        caller: &'static Location<'static>,
    ) -> Inbox {
        let mut tracks: Vec<Arc<crate::user::UserTrack>> = Vec::with_capacity(users.len());
        let mut all_belong = true;
        for u in users {
            if !u.belongs_to(&self.core) {
                all_belong = false;
            }
            let track = u.track_arc();
            if !tracks.iter().any(|t| t.id == track.id) {
                tracks.push(track);
            }
        }
        let filter = Filter::UsersInRoom {
            tracks,
            channel_name: self.channel_name.clone(),
        };
        if !all_belong {
            return Inbox::invalid_session(filter, caller);
        }
        let mut inbox = self
            .core
            .inboxes
            .subscribe(Arc::clone(&self.core), filter, caller);
        self.ask_config.apply_flags_to_inbox(&mut inbox);
        inbox
    }

    pub(crate) fn inbox_at(&self, caller: &'static Location<'static>) -> Inbox {
        let filter = Filter::Scoped {
            rooms: vec![self.channel_name.clone()],
            dm_users: Vec::new(),
        };
        let mut inbox = self
            .core
            .inboxes
            .subscribe(Arc::clone(&self.core), filter, caller);
        // Conversation scopes (`Room`, `Breakout`, `ExclusiveDm`, `Huddle`) propagate their
        // `AskConfig` stream flags (`ignore_commands`, `accept_notices`) onto `.inbox*()` so
        // `room.with_ask_config` sets policy room-wide across both prompts and listeners (while
        // `wait_cmd` / `on_cmd` explicitly override `ignore_commands = false`). `Bot` and `User`
        // stay config-free so session routers and stop triggers start unfiltered.
        self.ask_config.apply_flags_to_inbox(&mut inbox);
        inbox
    }
}

/// RAII channel moderation guard returned by [`Room::moderation_guard`].
///
/// Snapshots the room's topic and voiced (`+v`) occupants on creation and tracks mode flags,
/// voice grants/revocations, and topic updates performed through the guard. Dropping the guard
/// (including when a game future is cancelled via `.until(...)`) automatically restores any
/// modified flags, voices, and topic if the bot still holds chanop in the room.
#[derive(Debug)]
pub struct ModerationGuard {
    room: Room,
    initial_topic: Option<String>,
    initially_voiced: HashSet<UserId>,
    desired_voice: Vec<(User, bool)>,
    active_flags: Vec<char>,
    topic_changed: bool,
    disarmed: bool,
}

impl ModerationGuard {
    fn new(room: Room) -> Self {
        let initial_topic = room.topic();
        let initially_voiced = room
            .members()
            .into_iter()
            .filter(|m| m.is_voiced)
            .map(|m| m.user.id())
            .collect();
        Self {
            room,
            initial_topic,
            initially_voiced,
            desired_voice: Vec::new(),
            active_flags: Vec::new(),
            topic_changed: false,
            disarmed: false,
        }
    }

    pub fn room(&self) -> &Room {
        &self.room
    }

    /// Sets or clears a parameterless channel mode flag (e.g. `'m'` or `'N'`), scheduling `-<flag>`
    /// on drop while enabled. Rejects parameterized modes (`k`, `l`, `b`, ...) and flags omitted
    /// from the server's `005 CHANMODES` category D.
    pub fn set_flag(&mut self, flag: char, enabled: bool) -> Result<()> {
        let supported = self
            .room
            .core
            .state
            .read()
            .unwrap()
            .chan_modes
            .supports_flag(flag);
        if !supported {
            return Err(Error::InvalidInput(
                "ModerationGuard::set_flag requires a supported parameterless channel mode flag",
            ));
        }
        let spec = format!("{}{flag}", if enabled { '+' } else { '-' });
        self.room.set_mode(&spec, &[])?;
        if enabled {
            if !self.active_flags.contains(&flag) {
                self.active_flags.push(flag);
            }
        } else {
            self.active_flags.retain(|&f| f != flag);
        }
        Ok(())
    }

    /// Toggles `+m` / `-m` (moderated channel), automatically restoring `-m` on drop if left on.
    pub fn set_moderated(&mut self, enabled: bool) -> Result<()> {
        self.set_flag('m', enabled)
    }

    /// Sets the channel topic and marks the initial topic to be restored on drop.
    pub fn set_topic(&mut self, topic: impl Into<Text>) -> Result<()> {
        self.room.set_topic(topic)?;
        self.topic_changed = true;
        Ok(())
    }

    pub fn voice(&mut self, user: &User) -> Result<()> {
        self.voice_many([user])
    }

    pub fn voice_many<I, U>(&mut self, users: I) -> Result<()>
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        let collected: Vec<User> = users.into_iter().map(|u| u.borrow().clone()).collect();
        self.room.voice_many(&collected)?;
        self.record_voice(&collected, true);
        Ok(())
    }

    pub fn devoice(&mut self, user: &User) -> Result<()> {
        self.devoice_many([user])
    }

    pub fn devoice_many<I, U>(&mut self, users: I) -> Result<()>
    where
        I: IntoIterator<Item = U>,
        U: Borrow<User>,
    {
        let collected: Vec<User> = users.into_iter().map(|u| u.borrow().clone()).collect();
        self.room.devoice_many(&collected)?;
        self.record_voice(&collected, false);
        Ok(())
    }

    fn record_voice(&mut self, users: &[User], voiced: bool) {
        for user in users {
            if !user.is_online() {
                continue;
            }
            if let Some(slot) = self.desired_voice.iter_mut().find(|(u, _)| u == user) {
                slot.1 = voiced;
            } else {
                self.desired_voice.push((user.clone(), voiced));
            }
        }
    }

    /// Explicitly restores all tracked moderation changes now and disarms `Drop`.
    pub fn restore(mut self) -> Result<()> {
        self.disarmed = true;
        self.restore_inner()
    }

    /// Consumes the guard without restoring modes, voices, or topic on `Drop`.
    pub fn disarm(mut self) {
        self.disarmed = true;
    }

    fn restore_inner(&mut self) -> Result<()> {
        let mut first_err: Option<Error> = None;

        if !self.active_flags.is_empty() {
            let mut spec = String::with_capacity(1 + self.active_flags.len());
            spec.push('-');
            for &flag in &self.active_flags {
                spec.push(flag);
            }
            self.active_flags.clear();
            if let Err(e) = self.room.set_mode(&spec, &[]) {
                first_err.get_or_insert(e);
            }
        }

        if !self.desired_voice.is_empty() {
            let current_voiced: HashSet<UserId> = self
                .room
                .members()
                .into_iter()
                .filter(|m| m.is_voiced)
                .map(|m| m.user.id())
                .collect();
            let mut to_devoice = Vec::new();
            let mut to_revoice = Vec::new();
            for (user, desired) in self.desired_voice.drain(..) {
                let uid = user.id();
                let was_voiced = self.initially_voiced.contains(&uid);
                let is_voiced_now = current_voiced.contains(&uid);
                if was_voiced && (!desired || !is_voiced_now) {
                    to_revoice.push(user);
                } else if !was_voiced && (desired || is_voiced_now) {
                    to_devoice.push(user);
                }
            }
            if !to_devoice.is_empty() {
                if let Err(e) = self.room.devoice_many(&to_devoice) {
                    first_err.get_or_insert(e);
                }
            }
            if !to_revoice.is_empty() {
                if let Err(e) = self.room.voice_many(&to_revoice) {
                    first_err.get_or_insert(e);
                }
            }
        }

        if self.topic_changed {
            self.topic_changed = false;
            if let Err(e) = self
                .room
                .set_topic(self.initial_topic.as_deref().unwrap_or(""))
            {
                first_err.get_or_insert(e);
            }
        }

        match first_err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

impl Drop for ModerationGuard {
    fn drop(&mut self) {
        if !self.disarmed && self.room.bot_is_op() {
            let _ = self.restore_inner();
        }
    }
}

#[cfg(test)]
#[path = "room_test.rs"]
mod test;

