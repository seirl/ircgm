use std::fmt;
use std::panic::Location;
use std::sync::Arc;

use irc_colors::{irc_format, Text};

use crate::chat::{Chat, GroupChat, InboxRef};
use crate::core::BotCore;
use crate::error::{Error, Result};
use crate::exclusive_dm::DmLease;
use crate::inbox::{Filter, Inbox, OnChatHook};
use crate::message::{ChatMessage, Target};
use crate::outbound::OutboundChatKind;
use crate::user::{User, UserTrack};

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
        caller: &'static Location<'static>,
    ) -> Self {
        let dm_users: Vec<Arc<UserTrack>> = leases.iter().map(|l| l.user().track_arc()).collect();
        let users: Vec<User> = leases.iter().map(|l| l.user().clone()).collect();
        let relay_core = Arc::clone(&core);
        let relay_name = name.clone();
        let on_chat: OnChatHook = Arc::new(move |msg: &ChatMessage| {
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
        });
        let inbox = core.inboxes.subscribe_with_on_chat(
            Arc::clone(&core),
            Filter::Scoped {
                rooms: Vec::new(),
                dm_users,
            },
            caller,
            Some(on_chat),
        );
        Self {
            core,
            name,
            leases,
            initial_offline,
            inbox,
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
