use std::collections::HashSet;
use std::sync::Arc;

use irc_colors::{irc_format, Text};

use crate::ask::{Ask, AskConfig, AskSource, IntoVerdict};
use crate::bot::BotCore;
use crate::error::{Error, Result};
use crate::inbox::Inbox;
use crate::message::ChatMessage;
use crate::outbound::{OutboundChatKind, WriteReceipt};
use crate::user::{ExclusiveDm, User, UserId};

/// Disjoint outbound view of a [`Huddle`] so [`Ask`] can send prompts, countdown warnings,
/// rejects, and banter relays while mutably polling `Huddle::inbox`.
pub(crate) struct HuddleSender<'a> {
    core: &'a Arc<BotCore>,
    guards: &'a [ExclusiveDm],
    initial_offline: &'a Option<(String, Option<String>)>,
    relay_prefix: &'a Option<Text>,
    relay_commands: bool,
}

impl<'a> HuddleSender<'a> {
    pub(crate) fn core(&self) -> &'a Arc<BotCore> {
        self.core
    }

    pub(crate) fn has_relay(&self) -> bool {
        self.relay_prefix.is_some()
    }

    pub(crate) fn broadcast_prompt_with_receipt(&self, prompt: Text) -> Result<WriteReceipt> {
        self.core.check_connected()?;
        let mut last_receipt: Option<WriteReceipt> = None;
        let mut last_offline_err: Option<Error> = None;

        for g in self.guards {
            match g.send_chat_with_receipt(OutboundChatKind::Privmsg, prompt.clone()) {
                Ok(r) => last_receipt = Some(r),
                Err(e @ Error::UserLeft { .. }) => last_offline_err = Some(e),
                Err(e) => return Err(e),
            }
        }

        last_receipt.ok_or_else(|| {
            last_offline_err
                .or_else(|| {
                    self.initial_offline
                        .clone()
                        .map(|(nick, reason)| Error::UserLeft { nick, reason })
                })
                .unwrap_or(Error::InvalidInput("Huddle has no members"))
        })
    }

    pub(crate) fn broadcast_kind(&self, kind: OutboundChatKind, text: Text) -> Result<()> {
        self.core.check_connected()?;
        for g in self.guards {
            match g.send_chat_with_receipt(kind, text.clone()) {
                Ok(_) | Err(Error::UserLeft { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub(crate) fn relay_to_others(&self, msg: &ChatMessage) -> Result<()> {
        self.core.check_connected()?;
        if !self.relay_commands && self.core.is_command_text(&msg.text) {
            return Ok(());
        }
        let empty = Text::new();
        let prefix = self.relay_prefix.as_ref().unwrap_or(&empty);
        let rendered = irc_format!("{prefix}<{}> {}", msg.nick, msg.to_styled());
        let sender_id = msg.sender.id();
        for g in self.guards {
            if g.user().id() == sender_id {
                continue;
            }
            match g.send_chat_with_receipt(OutboundChatKind::Privmsg, rendered.clone()) {
                Ok(_) | Err(Error::UserLeft { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub(crate) fn find_matching_user(&self, target_user_id: Option<u64>) -> Option<&'a User> {
        let uid = UserId(target_user_id?);
        self.guards
            .iter()
            .map(ExclusiveDm::user)
            .find(|u| u.id() == uid)
    }

    pub(crate) fn any_reachable_online(&self, bounced_users: &HashSet<UserId>) -> bool {
        self.guards
            .iter()
            .map(ExclusiveDm::user)
            .any(|u| u.is_online() && !bounced_users.contains(&u.id()))
    }
}

/// RAII channel-free group DM guard (`bot.huddle(&users)` / `bot.huddle_available(&users)`).
///
/// Holds ordered [`ExclusiveDm`] locks for all members plus a wire-ordered multi-user [`Inbox`],
/// releasing all locks on `Drop`.
pub struct Huddle {
    core: Arc<BotCore>,
    guards: Vec<ExclusiveDm>,
    initial_offline: Option<(String, Option<String>)>,
    inbox: Inbox,
    ask_config: Arc<AskConfig>,
    relay_prefix: Option<Text>,
    relay_commands: bool,
}

impl Huddle {
    pub(crate) fn new(
        core: Arc<BotCore>,
        guards: Vec<ExclusiveDm>,
        initial_offline: Option<(String, Option<String>)>,
        mut inbox: Inbox,
        ask_config: Arc<AskConfig>,
    ) -> Self {
        inbox.idle_timeout = Some(std::time::Duration::from_secs(300));
        ask_config.apply_to_inbox(&mut inbox);
        Self {
            core,
            guards,
            initial_offline,
            inbox,
            ask_config,
            relay_prefix: None,
            relay_commands: false,
        }
    }

    pub(crate) fn check_connected(&self) -> Result<()> {
        self.core.check_connected()
    }

    fn sender_view(&self) -> HuddleSender<'_> {
        HuddleSender {
            core: &self.core,
            guards: &self.guards,
            initial_offline: &self.initial_offline,
            relay_prefix: &self.relay_prefix,
            relay_commands: self.relay_commands,
        }
    }

    pub(crate) fn split_for_ask(&mut self) -> (HuddleSender<'_>, &mut Inbox) {
        let sender = HuddleSender {
            core: &self.core,
            guards: &self.guards,
            initial_offline: &self.initial_offline,
            relay_prefix: &self.relay_prefix,
            relay_commands: self.relay_commands,
        };
        (sender, &mut self.inbox)
    }

    pub fn users(&self) -> Vec<User> {
        self.guards.iter().map(|g| g.user().clone()).collect()
    }

    pub fn online_users(&self) -> Vec<User> {
        self.guards
            .iter()
            .map(|g| g.user())
            .filter(|u| u.is_online())
            .cloned()
            .collect()
    }

    /// Returns `true` if `user` holds an [`ExclusiveDm`] slot inside this `Huddle`.
    pub fn has_user(&self, user: &User) -> bool {
        user.belongs_to(&self.core) && self.guards.iter().any(|g| g.user() == user)
    }

    /// Borrows `user`'s already-locked [`ExclusiveDm`] inside this `Huddle`, avoiding
    /// self-deadlock on `user.exclusive()` / `user.ask(...)`.
    pub fn dm_mut(&mut self, user: &User) -> Option<&mut ExclusiveDm> {
        if !user.belongs_to(&self.core) {
            return None;
        }
        self.guards.iter_mut().find(|g| g.user() == user)
    }

    pub fn len(&self) -> usize {
        self.guards.len()
    }

    pub fn is_empty(&self) -> bool {
        self.guards.is_empty()
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

    pub fn with_relay(mut self, prefix: impl Into<Text>) -> Self {
        self.relay_prefix = Some(prefix.into());
        self
    }

    pub fn without_relay(mut self) -> Self {
        self.relay_prefix = None;
        self
    }

    pub fn set_relay(&mut self, prefix: Option<Text>) {
        self.relay_prefix = prefix;
    }

    /// Controls whether lines starting with `BotConfig::command_prefixes` (default `"!"`)
    /// are relayed to fellow huddle members (default: `false`, so `!help` / `!status` or
    /// mistyped commands aren't echoed to teammates even when `.ignore_commands()` is off).
    pub fn with_relay_commands(mut self, enabled: bool) -> Self {
        self.relay_commands = enabled;
        self
    }

    pub fn set_relay_commands(&mut self, enabled: bool) {
        self.relay_commands = enabled;
    }

    /// Broadcasts to all currently-online huddle users (skipping any user who disconnected mid-huddle).
    pub fn say(&self, text: impl Into<Text>) -> Result<()> {
        self.sender_view()
            .broadcast_kind(OutboundChatKind::Privmsg, text.into())
    }

    pub fn notice(&self, text: impl Into<Text>) -> Result<()> {
        self.sender_view()
            .broadcast_kind(OutboundChatKind::Notice, text.into())
    }

    pub fn action(&self, text: impl Into<Text>) -> Result<()> {
        self.sender_view()
            .broadcast_kind(OutboundChatKind::Action, text.into())
    }

    /// Relays `msg` to all other online huddle users (`<prefix><<msg.nick>> <msg.to_styled()>`),
    /// skipping command-prefixed messages unless [`Huddle::with_relay_commands`] is enabled.
    pub fn relay_to_others(&self, msg: &ChatMessage) -> Result<()> {
        self.sender_view().relay_to_others(msg)
    }

    pub fn ask<'a, F, R>(&'a mut self, prompt: impl Into<Text>, matcher: F) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoVerdict + 'a,
        R::Output: Send + 'a,
    {
        let mut base_cfg = (*self.ask_config).clone();
        self.inbox.sync_flags_to_ask_config(&mut base_cfg);
        Ask::new(
            AskSource::BorrowedHuddle { huddle: self },
            base_cfg,
            prompt.into(),
            matcher,
        )
    }

    /// Prompts a single `Huddle` member over their already-held [`ExclusiveDm`] guard without
    /// self-deadlocking `dm_mutex` or leaking the prompt to teammates.
    pub fn ask_user<'a, F, R>(
        &'a mut self,
        user: &User,
        prompt: impl Into<Text>,
        matcher: F,
    ) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoVerdict + 'a,
        R::Output: Send + 'a,
    {
        let mut base_cfg = (*self.ask_config).clone();
        self.inbox.sync_flags_to_ask_config(&mut base_cfg);
        let source = if !user.belongs_to(&self.core) {
            AskSource::ImmediateErr(Error::InvalidInput(
                "User handle belongs to a different Bot session",
            ))
        } else if let Some(idx) = self.guards.iter().position(|g| g.user() == user) {
            AskSource::BorrowedDm {
                dm: &mut self.guards[idx],
            }
        } else {
            let err = user
                .quit_error()
                .unwrap_or(Error::InvalidInput("User is not a member of this Huddle"));
            AskSource::ImmediateErr(err)
        };
        Ask::new(source, base_cfg, prompt.into(), matcher)
    }

    pub fn inbox(&mut self) -> &mut Inbox {
        &mut self.inbox
    }
}

#[cfg(test)]
#[path = "huddle_test.rs"]
mod test;

