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
use crate::user::{User, UserTrack};

mod private {
    use crate::user::User;

    pub trait Sealed {
        fn as_user_or_nick(&self) -> std::result::Result<&User, &str>;
    }
}

/// Sealed target trait accepted by [`Room::invite`] and [`crate::Breakout::invite_and_wait`]
/// (implemented for [`User`], `str`, and `String`).
pub trait InviteTarget: private::Sealed {}

impl private::Sealed for User {
    fn as_user_or_nick(&self) -> std::result::Result<&User, &str> {
        Ok(self)
    }
}
impl InviteTarget for User {}

impl private::Sealed for str {
    fn as_user_or_nick(&self) -> std::result::Result<&User, &str> {
        Err(self)
    }
}
impl InviteTarget for str {}

impl private::Sealed for String {
    fn as_user_or_nick(&self) -> std::result::Result<&User, &str> {
        Err(self.as_str())
    }
}
impl InviteTarget for String {}

impl<T: InviteTarget + ?Sized> private::Sealed for &T {
    fn as_user_or_nick(&self) -> std::result::Result<&User, &str> {
        (**self).as_user_or_nick()
    }
}
impl<T: InviteTarget + ?Sized> InviteTarget for &T {}

pub(crate) fn resolve_invite_target<T: InviteTarget + ?Sized>(
    target: &T,
    core: &Arc<BotCore>,
) -> Result<(String, Option<Arc<UserTrack>>)> {
    match target.as_user_or_nick() {
        Ok(user) => {
            if !user.belongs_to(core) {
                return Err(Error::InvalidInput(
                    "User handle belongs to a different Bot session",
                ));
            }
            if let Some(err) = user.quit_error() {
                return Err(err);
            }
            let nick = user.nick();
            validate_target(&nick, "invite target")?;
            Ok((nick, Some(user.track_arc())))
        }
        Err(nick) => {
            validate_target(nick, "invite target")?;
            Ok((nick.to_owned(), None))
        }
    }
}

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
                let self_key = ws.casemap.fold(&ws.current_nick);
                if !chan.members.contains_key(&self_key) {
                    return Err(Error::BotLeftRoom {
                        channel: self.channel_name.clone(),
                        reason: None,
                    });
                }
                if !chan
                    .members
                    .values()
                    .any(|occ| occ.track.id == user.track_id())
                {
                    return Err(Error::UserLeft {
                        nick: user.nick(),
                        reason: None,
                    });
                }
            }
        }
        Ok(())
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
                let self_key = ws.casemap.fold(&ws.current_nick);
                let Some(self_occ) = chan.members.get(&self_key) else {
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

    /// Sends `INVITE <target> <channel>`. Fails immediately with `Err(Error::UserLeft { .. })`
    /// without sending traffic if a [`User`] target is already known offline.
    pub fn invite<T: InviteTarget + ?Sized>(&self, target: &T) -> Result<()> {
        self.ensure_bot_in_room_if_tracked()?;
        let (nick, _) = resolve_invite_target(target, &self.core)?;
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
            Priority::Background,
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
            .map(|occ| Member {
                user: User::from_parts(weak.clone(), Arc::clone(&occ.track)),
                nick: occ.nick.clone(),
                is_op: ws.occupant_is_op(occ),
                is_voiced: ws.occupant_is_voiced(occ),
                prefixes: occ.prefixes.clone(),
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
        let occ = chan
            .members
            .values()
            .find(|o| o.track.id == user.track_id())?;
        Some(Member {
            user: User::from_parts(Arc::downgrade(&self.core), Arc::clone(&occ.track)),
            nick: occ.nick.clone(),
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
        let mut out: Vec<String> = chan.members.values().map(|o| o.nick.clone()).collect();
        out.sort();
        out
    }

    pub fn has_user(&self, user: &User) -> bool {
        if !user.belongs_to(&self.core) {
            return false;
        }
        let ws = self.core.state.read().unwrap();
        let key = ws.casemap.fold(&self.channel_name);
        ws.channels.get(&key).is_some_and(|chan| {
            chan.members
                .values()
                .any(|o| o.track.id == user.track_id())
        })
    }

    pub fn has_nick(&self, nick: &str) -> bool {
        let ws = self.core.state.read().unwrap();
        let chan_key = ws.casemap.fold(&self.channel_name);
        let user_key = ws.casemap.fold(nick);
        ws.channels
            .get(&chan_key)
            .is_some_and(|chan| chan.members.contains_key(&user_key))
    }

    pub fn bot_is_op(&self) -> bool {
        let ws = self.core.state.read().unwrap();
        let chan_key = ws.casemap.fold(&self.channel_name);
        let Some(chan) = ws.channels.get(&chan_key) else {
            return false;
        };
        let self_key = ws.casemap.fold(&ws.current_nick);
        chan.members
            .get(&self_key)
            .is_some_and(|occ| ws.occupant_is_op(occ))
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
        let nick = user.nick();
        validate_target(&nick, "kick target")?;
        let clean = reason.map(sanitize_reason);
        let _ = self.core.outbound.enqueue_one(
            Priority::Background,
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

    pub fn voice_many<'a, I: IntoIterator<Item = &'a User>>(&self, users: I) -> Result<()> {
        self.apply_status_many('+', 'v', users)
    }

    pub fn devoice(&self, user: &User) -> Result<()> {
        self.devoice_many([user])
    }

    pub fn devoice_many<'a, I: IntoIterator<Item = &'a User>>(&self, users: I) -> Result<()> {
        self.apply_status_many('-', 'v', users)
    }

    fn apply_status_many<'a, I: IntoIterator<Item = &'a User>>(
        &self,
        sign: char,
        mode_char: char,
        users: I,
    ) -> Result<()> {
        let (cm, max_modes) = self.ensure_can_moderate()?;
        let mut nicks = Vec::new();
        for u in users {
            if !u.belongs_to(&self.core) {
                return Err(Error::InvalidInput(
                    "User handle belongs to a different Bot session",
                ));
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

    #[track_caller]
    pub fn inbox(&self) -> Inbox {
        let caller = Location::caller();
        self.inbox_at(caller)
    }

    pub(crate) fn inbox_at(&self, caller: &'static Location<'static>) -> Inbox {
        let cm = self.core.casemap();
        let filter = Filter::Scoped {
            rooms: vec![self.channel_name.clone()],
            dm_users: Vec::new(),
        };
        self.core
            .inboxes
            .subscribe(Arc::clone(&self.core), filter, caller, cm)
    }
}
