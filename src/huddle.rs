use std::sync::Arc;

use irc_colors::Text;

use crate::ask::{Ask, AskConfig, AskSource, IntoVerdict};
use crate::bot::BotCore;
use crate::error::{Error, Result};
use crate::inbox::Inbox;
use crate::message::ChatMessage;
use crate::outbound::{OutboundChatKind, WriteReceipt};
use crate::user::{ExclusiveDm, User};

/// RAII channel-free group DM guard (`bot.huddle(&users)` / `bot.huddle_available(&users)`).
///
/// Holds ordered [`ExclusiveDm`] locks for all members plus a wire-ordered multi-user [`Inbox`],
/// releasing all locks on `Drop`.
pub struct Huddle {
    core: Arc<BotCore>,
    guards: Vec<ExclusiveDm>,
    inbox: Inbox,
    ask_config: Arc<AskConfig>,
    relay_prefix: Option<Text>,
}

impl Huddle {
    pub(crate) fn new(
        core: Arc<BotCore>,
        guards: Vec<ExclusiveDm>,
        mut inbox: Inbox,
        ask_config: Arc<AskConfig>,
    ) -> Self {
        ask_config.apply_to_inbox(&mut inbox);
        Self {
            core,
            guards,
            inbox,
            ask_config,
            relay_prefix: None,
        }
    }

    pub(crate) fn core(&self) -> Option<&Arc<BotCore>> {
        Some(&self.core)
    }

    pub(crate) fn has_relay(&self) -> bool {
        self.relay_prefix.is_some()
    }

    pub(crate) fn contains_user_id(&self, uid: u64) -> bool {
        self.guards.iter().any(|g| g.user().track_id() == uid)
    }

    pub(crate) fn broadcast_prompt_with_receipt(&self, prompt: Text) -> Result<WriteReceipt> {
        self.core.check_connected()?;
        let mut last_receipt: Option<WriteReceipt> = None;
        let mut last_offline_err: Option<Error> = None;

        for g in &self.guards {
            match g.send_chat_with_receipt(OutboundChatKind::Privmsg, prompt.clone()) {
                Ok(r) => last_receipt = Some(r),
                Err(e @ Error::UserLeft { .. }) => last_offline_err = Some(e),
                Err(e) => return Err(e),
            }
        }

        match last_receipt {
            Some(r) => Ok(r),
            None => Err(last_offline_err.unwrap_or(Error::UserLeft {
                nick: String::new(),
                reason: None,
            })),
        }
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
        Arc::make_mut(&mut self.ask_config).ignore_commands = true;
        self.inbox.set_ignore_commands(true);
        self
    }

    pub fn with_accept_notices(mut self) -> Self {
        Arc::make_mut(&mut self.ask_config).accept_notices = true;
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

    fn broadcast_kind(&self, kind: OutboundChatKind, text: Text) -> Result<()> {
        self.core.check_connected()?;
        for g in &self.guards {
            match g.send_chat_with_receipt(kind, text.clone()) {
                Ok(_) | Err(Error::UserLeft { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Relays `msg` to all other online huddle users (`<prefix><<msg.nick>> <msg.to_styled()>`).
    pub fn relay_to_others(&self, msg: &ChatMessage) -> Result<()> {
        self.core.check_connected()?;
        let base = match &self.relay_prefix {
            Some(p) => p.clone(),
            None => Text::new(),
        };
        let rendered = base + "<" + &msg.nick + "> " + msg.to_styled();
        let sender_id = msg.sender.track_id();
        for g in &self.guards {
            if g.user().track_id() == sender_id {
                continue;
            }
            match g.send_chat_with_receipt(OutboundChatKind::Privmsg, rendered.clone()) {
                Ok(_) | Err(Error::UserLeft { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub fn ask<'a, F, R>(&'a mut self, prompt: impl Into<Text>, matcher: F) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoVerdict + 'a,
        R::Output: Send + 'a,
    {
        let base_cfg = (*self.ask_config).clone();
        Ask::new(
            AskSource::BorrowedHuddle { huddle: self },
            base_cfg,
            prompt.into(),
            matcher,
        )
    }

    pub fn inbox(&mut self) -> &mut Inbox {
        &mut self.inbox
    }
}
