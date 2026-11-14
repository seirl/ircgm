use std::ops::{Deref, DerefMut};

use irc_colors::Text;

use crate::error::Result;
use crate::inbox::Inbox;
use crate::message::Target;
use crate::user::User;

/// Either a freshly-subscribed [`Inbox`] (returned by stateless handles like [`crate::Room`]) or a
/// borrowed `&mut Inbox` (returned by stateful sessions like [`crate::ExclusiveDm`] and
/// [`crate::Huddle`]).
///
/// Dereferences to [`Inbox`] (`Deref` + `DerefMut`) so callers can invoke `.next_chat()`,
/// `.next_event()`, `.wait_cmd()`, `.try_next_chat()`, etc. directly on `chat.inbox()`.
#[derive(Debug)]
pub enum InboxRef<'a> {
    Owned(Inbox),
    Borrowed(&'a mut Inbox),
}

impl Deref for InboxRef<'_> {
    type Target = Inbox;

    fn deref(&self) -> &Self::Target {
        match self {
            InboxRef::Owned(inbox) => inbox,
            InboxRef::Borrowed(inbox) => inbox,
        }
    }
}

impl DerefMut for InboxRef<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            InboxRef::Owned(inbox) => inbox,
            InboxRef::Borrowed(inbox) => inbox,
        }
    }
}

/// Any open conversation scope (`Room`, `Breakout`, `Huddle`, `ExclusiveDm`, or custom wrapper)
/// that can send messages and expose a scoped [`Inbox`].
pub trait Chat: Send + Sync {
    fn say(&self, text: impl Into<Text>) -> Result<()>;
    fn notice(&self, text: impl Into<Text>) -> Result<()>;
    fn action(&self, text: impl Into<Text>) -> Result<()>;

    #[track_caller]
    fn inbox(&mut self) -> InboxRef<'_>;

    fn target(&self) -> Target;

    /// Fixed participant roster when closed ([`crate::ExclusiveDm`] / [`crate::Huddle`]), or
    /// `None` when open to anyone in the channel ([`crate::Room`] / [`crate::Breakout`]).
    fn participants(&self) -> Option<Vec<User>> {
        None
    }
}

/// Multi-party [`Chat`] scope (`Room`, `Breakout`, `Huddle`, or custom wrapper).
pub trait GroupChat: Chat {
    /// Returns `true` if `user` is currently a member/occupant of this group.
    fn has_user(&self, user: &User) -> bool;

    /// Opens or borrows an [`Inbox`] scoped to `users` (overridden by [`crate::Room`] via
    /// [`crate::Room::inbox_users`]).
    #[track_caller]
    fn inbox_users(&mut self, _users: &[User]) -> InboxRef<'_> {
        self.inbox()
    }
}

impl<T: Chat + ?Sized> Chat for &mut T {
    fn say(&self, text: impl Into<Text>) -> Result<()> {
        (**self).say(text)
    }

    fn notice(&self, text: impl Into<Text>) -> Result<()> {
        (**self).notice(text)
    }

    fn action(&self, text: impl Into<Text>) -> Result<()> {
        (**self).action(text)
    }

    #[track_caller]
    fn inbox(&mut self) -> InboxRef<'_> {
        (**self).inbox()
    }

    fn target(&self) -> Target {
        (**self).target()
    }

    fn participants(&self) -> Option<Vec<User>> {
        (**self).participants()
    }
}

impl<T: GroupChat + ?Sized> GroupChat for &mut T {
    fn has_user(&self, user: &User) -> bool {
        (**self).has_user(user)
    }

    #[track_caller]
    fn inbox_users(&mut self, users: &[User]) -> InboxRef<'_> {
        (**self).inbox_users(users)
    }
}

#[cfg(test)]
#[path = "chat_test.rs"]
mod test;
