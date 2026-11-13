use std::borrow::Borrow;
use std::panic::Location;
use std::sync::Arc;

use irc::proto::Command;
use irc_colors::Text;

use crate::ask::{Ask, AskConfig, AskSource, IntoVerdict};
use crate::bot::BotCore;
use crate::casemap::CaseMap;
use crate::error::{Error, Result};
use crate::inbox::{Filter, Inbox};
use crate::message::ChatMessage;
use crate::outbound::{
    sanitize_reason, sanitize_text, validate_param, validate_target, OutboundChatKind, Priority,
    WriteReceipt,
};
use crate::state::Occupant;
use crate::user::User;

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

    pub(crate) fn ask_config(&self) -> &AskConfig {
        &self.ask_config
    }

    fn self_occupant<'a>(
        ws: &'a crate::state::WorldState,
        chan: &'a crate::state::ChannelState,
    ) -> Option<&'a Occupant> {
        if let Some(self_id) = ws.self_track_id {
            chan.members.get(&self_id)
        } else {
            chan.member_by_nick(&ws.current_nick, ws.casemap)
        }
    }

    pub(crate) fn ensure_bot_in_room_if_tracked(&self) -> Result<()> {
        self.core.check_connected()?;
        let ws = self.core.state.read().unwrap();
        let key = ws.casemap.fold(&self.channel_name);
        if let Some(lost) = ws.lost_channels.get(&key) {
            return Err(Error::BotLeftRoom {
                channel: lost.name.clone(),
                reason: lost.reason.clone(),
            });
        }
        Ok(())
    }

    pub(crate) fn ensure_user_askable(&self, user: &User) -> Result<()> {
        self.core.check_connected()?;
        if let Some(err) = user.quit_error() {
            return Err(err);
        }
        let ws = self.core.state.read().unwrap();
        let key = ws.casemap.fold(&self.channel_name);
        if let Some(lost) = ws.lost_channels.get(&key) {
            return Err(Error::BotLeftRoom {
                channel: lost.name.clone(),
                reason: lost.reason.clone(),
            });
        }
        if let Some(chan) = ws.channels.get(&key) {
            if chan.names_synced {
                if Self::self_occupant(&ws, chan).is_none() {
                    return Err(Error::BotLeftRoom {
                        channel: self.channel_name.clone(),
                        reason: None,
                    });
                }
                if !chan.members.contains_key(&user.track_id()) {
                    return Err(Error::UserLeft {
                        nick: user.nick(),
                        reason: None,
                    });
                }
            }
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

    fn ensure_can_moderate(&self) -> Result<(CaseMap, usize)> {
        self.core.check_connected()?;
        let ws = self.core.state.read().unwrap();
        let key = ws.casemap.fold(&self.channel_name);
        if let Some(lost) = ws.lost_channels.get(&key) {
            return Err(Error::BotLeftRoom {
                channel: lost.name.clone(),
                reason: lost.reason.clone(),
            });
        }
        if let Some(chan) = ws.channels.get(&key) {
            if chan.names_synced {
                let Some(self_occ) = Self::self_occupant(&ws, chan) else {
                    return Err(Error::BotLeftRoom {
                        channel: self.channel_name.clone(),
                        reason: None,
                    });
                };
                if !ws.occupant_is_op(self_occ) {
                    return Err(Error::NotChannelOp {
                        channel: self.channel_name.clone(),
                    });
                }
            }
        }
        Ok((ws.casemap, ws.max_modes.max(1)))
    }

    pub(crate) fn send_chat_with_receipt(
        &self,
        kind: OutboundChatKind,
        text: Text,
    ) -> Result<WriteReceipt> {
        self.ensure_bot_in_room_if_tracked()?;
        self.core
            .send_chat_with_receipt(kind, &self.channel_name, text)
    }

    pub fn name(&self) -> &str {
        &self.channel_name
    }

    pub fn casemap(&self) -> CaseMap {
        self.core.casemap()
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
        let cm = self.core.casemap();
        let _ = self.core.outbound.enqueue_one(
            Priority::Interactive,
            Command::INVITE(nick, self.channel_name.clone()),
            cm,
        );
        Ok(())
    }

    pub fn part(&self, reason: Option<&str>) -> Result<()> {
        self.core.check_connected()?;
        let clean = reason.map(sanitize_reason);
        let cm = self.core.casemap();
        let _ = self.core.outbound.enqueue_one(
            Priority::Interactive,
            Command::PART(self.channel_name.clone(), clean),
            cm,
        );
        Ok(())
    }

    pub fn members(&self) -> Vec<Member> {
        let ws = self.core.state.read().unwrap();
        let key = ws.casemap.fold(&self.channel_name);
        let Some(chan) = ws.channels.get(&key) else {
            return Vec::new();
        };
        let weak = Arc::downgrade(&self.core);
        let mut out: Vec<Member> = chan
            .members
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
        let key = ws.casemap.fold(&self.channel_name);
        let chan = ws.channels.get(&key)?;
        let occ = chan.members.get(&user.track_id())?;
        Some(Member {
            user: User::from_parts(Arc::downgrade(&self.core), Arc::clone(&occ.track)),
            nick: occ.track.nick(),
            is_op: ws.occupant_is_op(occ),
            is_voiced: ws.occupant_is_voiced(occ),
            prefixes: occ.prefixes.clone(),
        })
    }

    pub fn nicks(&self) -> Vec<String> {
        let ws = self.core.state.read().unwrap();
        let key = ws.casemap.fold(&self.channel_name);
        let Some(chan) = ws.channels.get(&key) else {
            return Vec::new();
        };
        let mut out: Vec<String> = chan.members.values().map(|o| o.track.nick()).collect();
        out.sort();
        out
    }

    pub fn has_user(&self, user: &User) -> bool {
        if !user.belongs_to(&self.core) {
            return false;
        }
        let ws = self.core.state.read().unwrap();
        let key = ws.casemap.fold(&self.channel_name);
        ws.channels
            .get(&key)
            .is_some_and(|chan| chan.members.contains_key(&user.track_id()))
    }

    pub fn has_nick(&self, nick: &str) -> bool {
        let ws = self.core.state.read().unwrap();
        let chan_key = ws.casemap.fold(&self.channel_name);
        ws.channels
            .get(&chan_key)
            .is_some_and(|chan| chan.member_by_nick(nick, ws.casemap).is_some())
    }

    pub fn bot_is_op(&self) -> bool {
        let ws = self.core.state.read().unwrap();
        let chan_key = ws.casemap.fold(&self.channel_name);
        let Some(chan) = ws.channels.get(&chan_key) else {
            return false;
        };
        Self::self_occupant(&ws, chan).is_some_and(|occ| ws.occupant_is_op(occ))
    }

    pub fn topic(&self) -> Option<String> {
        let ws = self.core.state.read().unwrap();
        let chan_key = ws.casemap.fold(&self.channel_name);
        ws.channels.get(&chan_key).and_then(|c| c.topic.clone())
    }

    pub fn kick(&self, user: &User, reason: Option<&str>) -> Result<()> {
        if !user.belongs_to(&self.core) {
            return Err(Error::InvalidInput(
                "User handle belongs to a different Bot session",
            ));
        }
        let (cm, _) = self.ensure_can_moderate()?;
        if let Some(err) = user.quit_error() {
            return Err(err);
        }
        let nick = user.nick();
        validate_target(&nick, "kick target")?;
        let clean = reason.map(sanitize_reason);
        let _ = self.core.outbound.enqueue_one(
            Priority::Interactive,
            Command::KICK(self.channel_name.clone(), nick, clean),
            cm,
        );
        Ok(())
    }

    pub fn set_mode(&self, modes: &str, args: &[&str]) -> Result<()> {
        let (cm, _) = self.ensure_can_moderate()?;
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
        let _ = self.core.outbound.enqueue_one(
            Priority::Interactive,
            Command::Raw("MODE".into(), raw_args),
            cm,
        );
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
        let (cm, max_modes) = self.ensure_can_moderate()?;
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

        let _ = self
            .core
            .outbound
            .enqueue_batch(Priority::Interactive, cmds, cm);
        Ok(())
    }

    pub fn set_topic(&self, topic: impl Into<Text>) -> Result<()> {
        let (cm, _) = self.ensure_can_moderate()?;
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
        let _ = self.core.outbound.enqueue_one(
            Priority::Interactive,
            Command::TOPIC(self.channel_name.clone(), Some(wire_topic)),
            cm,
        );
        Ok(())
    }

    #[track_caller]
    pub fn ask<'a, F, R>(&'a self, prompt: impl Into<Text>, matcher: F) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoVerdict + 'a,
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
        R: IntoVerdict + 'a,
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
    /// failing with [`Error::UserLeft`] only if *every* listed user leaves/quits).
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
        R: IntoVerdict + 'a,
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
        user.inbox_in_at(self, caller)
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
        let cm = self.core.casemap();
        let mut inbox = self
            .core
            .inboxes
            .subscribe(Arc::clone(&self.core), filter, caller, cm);
        self.ask_config.apply_to_inbox(&mut inbox);
        inbox
    }

    pub(crate) fn inbox_at(&self, caller: &'static Location<'static>) -> Inbox {
        let cm = self.core.casemap();
        let filter = Filter::Scoped {
            rooms: vec![self.channel_name.clone()],
            dm_users: Vec::new(),
        };
        let mut inbox = self
            .core
            .inboxes
            .subscribe(Arc::clone(&self.core), filter, caller, cm);
        self.ask_config.apply_to_inbox(&mut inbox);
        inbox
    }
}

#[cfg(test)]
#[path = "room_test.rs"]
mod test;

