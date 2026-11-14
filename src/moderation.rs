use std::borrow::Borrow;
use std::collections::HashSet;

use irc_colors::Text;

use crate::error::{Error, Result};
use crate::room::Room;
use crate::state::BotPresence;
use crate::user::{User, UserId};

impl Room {
    /// Creates a cancellation-safe [`ModerationGuard`] snapshotting this room's current topic and
    /// voiced (`+v`) occupants so temporary game moderation (`+m`, `+v`/`-v`, topic updates) is
    /// automatically reverted on `Drop` if the bot still holds chanop.
    ///
    /// Fails immediately if the bot is disconnected, not joined to this room
    /// ([`Error::BotLeftRoom`]), lacks chanop / is in a modeless `+` channel
    /// ([`Error::NotChannelOp`]), or the server's `005 CHANMODES` does not support `+m`
    /// ([`Error::InvalidInput`]).
    pub fn moderation_guard(&self) -> Result<ModerationGuard> {
        let core = self.core();
        core.check_connected()?;
        {
            let ws = core.state.read().unwrap();
            ws.check_bot_in_channel(self.name(), BotPresence::Op)?;
            if !ws.chan_modes.supports_flag('m') {
                return Err(Error::InvalidInput(
                    "server 005 CHANMODES does not support +m moderation",
                ));
            }
        }
        Ok(ModerationGuard::new(self.clone()))
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
            .core()
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
#[path = "moderation_test.rs"]
mod test;
