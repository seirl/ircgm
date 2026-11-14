use std::fmt;
use std::sync::Arc;

use irc_colors::{irc_format, Text};

use crate::ask::{Ask, AskConfig, AskMany, IntoAskMatch};
use crate::bot::BotCore;
use crate::chat::{Chat, GroupChat, InboxRef};
use crate::error::{Error, Result};
use crate::exclusive_dm::DmLease;
use crate::inbox::Inbox;
use crate::message::{ChatMessage, Target};
use crate::outbound::OutboundChatKind;
use crate::user::User;

/// RAII channel-free group DM guard (`bot.huddle(name, &users)` / `bot.huddle_available(name, &users)`).
///
/// Holds the DM prompt locks of all members (the same per-user lock [`crate::ExclusiveDm`]
/// takes) plus one wire-ordered [`Inbox`] over all members' DMs that automatically relays
/// `[<name>] <<nick>> <msg>` to fellow members, releasing all locks on `Drop`.
pub struct Huddle {
    core: Arc<BotCore>,
    name: Text,
    leases: Vec<DmLease>,
    initial_offline: Option<(String, Option<String>)>,
    inbox: Inbox,
    ask_config: AskConfig,
}

impl fmt::Debug for Huddle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Huddle")
            .field("name", &self.name.plain())
            .field("users", &self.leases)
            .finish_non_exhaustive()
    }
}

impl Huddle {
    pub(crate) fn new(
        core: Arc<BotCore>,
        name: Text,
        leases: Vec<DmLease>,
        initial_offline: Option<(String, Option<String>)>,
        mut inbox: Inbox,
        ask_config: AskConfig,
    ) -> Self {
        ask_config.apply_to_locked_inbox(&mut inbox);
        let users: Vec<User> = leases.iter().map(|l| l.user().clone()).collect();
        let relay_core = Arc::clone(&core);
        let relay_name = name.clone();
        inbox.on_chat = Some(Arc::new(move |msg: &ChatMessage| {
            if relay_core.check_connected().is_err() {
                return;
            }
            let rendered = if relay_name.is_empty() {
                irc_format!("<{}> {}", msg.nick, msg.to_styled())
            } else {
                irc_format!("[{relay_name}] <{}> {}", msg.nick, msg.to_styled())
            };
            let sender_id = msg.sender.id();
            for user in &users {
                if user.id() != sender_id {
                    let _ = user.send_chat(OutboundChatKind::Privmsg, rendered.clone());
                }
            }
        }));
        Self {
            core,
            name,
            leases,
            initial_offline,
            inbox,
            ask_config,
        }
    }

    fn broadcast_kind(&self, kind: OutboundChatKind, text: Text) -> Result<()> {
        self.core.check_connected()?;
        let mut sent_any = false;
        let mut last_offline_err: Option<Error> = None;

        for lease in &self.leases {
            match lease.user().send_chat(kind, text.clone()) {
                Ok(()) => sent_any = true,
                Err(e @ Error::UserLeft { .. }) => last_offline_err = Some(e),
                Err(e) => return Err(e),
            }
        }

        if sent_any {
            Ok(())
        } else {
            Err(last_offline_err
                .or_else(|| {
                    self.initial_offline
                        .clone()
                        .map(|(nick, reason)| Error::UserLeft { nick, reason })
                })
                .unwrap_or(Error::InvalidInput("Huddle has no members")))
        }
    }

    pub fn name(&self) -> &Text {
        &self.name
    }

    pub fn users(&self) -> Vec<User> {
        self.leases.iter().map(|l| l.user().clone()).collect()
    }

    pub fn online_users(&self) -> Vec<User> {
        self.leases
            .iter()
            .map(DmLease::user)
            .filter(|u| u.is_online())
            .cloned()
            .collect()
    }

    /// Returns `true` if `user` is a member of this `Huddle`.
    pub fn has_user(&self, user: &User) -> bool {
        user.belongs_to(&self.core) && self.leases.iter().any(|l| l.user() == user)
    }

    pub fn len(&self) -> usize {
        self.leases.len()
    }

    pub fn is_empty(&self) -> bool {
        self.leases.is_empty()
    }

    /// The [`AskConfig`] this huddle's prompts and inbox reads use. Clone it to derive a tweaked
    /// one for [`Huddle::set_ask_config`].
    pub fn ask_config(&self) -> &AskConfig {
        &self.ask_config
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

    /// Broadcasts to all currently-online huddle users (skipping any user who disconnected mid-huddle).
    pub fn say(&self, text: impl Into<Text>) -> Result<()> {
        self.broadcast_kind(OutboundChatKind::Privmsg, text.into())
    }

    pub fn notice(&self, text: impl Into<Text>) -> Result<()> {
        self.broadcast_kind(OutboundChatKind::Notice, text.into())
    }

    pub fn action(&self, text: impl Into<Text>) -> Result<()> {
        self.broadcast_kind(OutboundChatKind::Action, text.into())
    }

    #[track_caller]
    pub fn ask<'a, F, R>(&'a mut self, prompt: impl Into<Text>, matcher: F) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoAskMatch + 'a,
        R::Output: Send + 'a,
    {
        let cfg = self.ask_config.clone();
        Ask::from_group_with_config(self, cfg, prompt.into(), matcher)
    }

    /// Broadcasts `prompt` to all online huddle members and collects up to one answer per member
    /// (or restricted subset via [`AskMany::from_users`]) while relaying member chat.
    #[track_caller]
    pub fn ask_many<'a, F, R>(
        &'a mut self,
        prompt: impl Into<Text>,
        matcher: F,
    ) -> AskMany<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoAskMatch + 'a,
        R::Output: Send + 'a,
    {
        let cfg = self.ask_config.clone();
        AskMany::from_group_with_config(self, cfg, prompt.into(), matcher)
    }

    pub fn inbox(&mut self) -> &mut Inbox {
        &mut self.inbox
    }
}

impl Chat for Huddle {
    fn say(&self, text: impl Into<Text>) -> Result<()> {
        Huddle::say(self, text)
    }

    fn notice(&self, text: impl Into<Text>) -> Result<()> {
        Huddle::notice(self, text)
    }

    fn action(&self, text: impl Into<Text>) -> Result<()> {
        Huddle::action(self, text)
    }

    fn inbox(&mut self) -> InboxRef<'_> {
        InboxRef::Borrowed(&mut self.inbox)
    }

    fn target(&self) -> Target {
        Target::Dm
    }

    fn participants(&self) -> Option<Vec<User>> {
        Some(self.users())
    }
}

impl GroupChat for Huddle {
    fn has_user(&self, user: &User) -> bool {
        Huddle::has_user(self, user)
    }
}

#[cfg(test)]
#[path = "huddle_test.rs"]
mod test;
