//! Direct-style `async`/`await` IRC game master and interactive bot library.
//!
//! `ircgm` replaces state-machine enums and callback spaghetti with structured direct-style futures:
//! - [`Bot`] manages the IRC connection pump, registration barrier, priority outbound flood scheduler,
//!   and session-scoped [`Room`], [`User`], [`ExclusiveDm`], [`Huddle`], [`Breakout`], and [`Inbox`] handles.
//! - [`Ask`] and [`AskConfig`] provide ergonomic prompts with post-write-receipt reply clocks,
//!   inline [`AskMatch::reject`] validation, [`AskMatch::accept_say`] confirmations, countdown
//!   warnings, per-player timeout callouts, and explicit [`Answer`] outcomes by default.
//! - [`GameFutureExt`] composes any prompt or multi-step `async fn` with `.until()`, `.or_until()`,
//!   `.with_timeout()`, `.none_on_timeout()`, and `.run_for()`.
//!
//! See `examples/` for standalone bot/API pattern showcases and the companion `minigames`
//! workspace crate for 8 playable multi-player games (`blackjack`, `heist`, `cold_war`,
//! `mafia`, `quiplash`, `coup`, `auction`, `simon_says`) plus `minigames/examples/game_bot.rs`.
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use std::time::Duration;
//! use ircgm::prelude::*;
//!
//! async fn run_lobby(bot: Bot, room: Room) -> Result<()> {
//!     loop {
//!         let trigger = match room
//!             .inbox()
//!             .into_wait_cmd("!guess")
//!             .until(bot.shutdown_requested())
//!             .await
//!         {
//!             Outcome::Completed(res) => res?,
//!             Outcome::Stopped(()) => break,
//!         };
//!
//!         let player = &trigger.sender;
//!         match room
//!             .ask_user(
//!                 player,
//!                 irc_format!("{}: pick a number 1..=5!", player.nick().bold()),
//!                 |m| match m.text.trim().parse::<u8>() {
//!                     Ok(n @ 1..=5) => AskMatch::accept(n),
//!                     _ => AskMatch::reject("Enter a number between 1 and 5!".yellow()),
//!                 },
//!             )
//!             .reply_timeout(Duration::from_secs(15))
//!             .await?
//!         {
//!             Answer::Ok(3) => room.say("🎉 Spot on!".green().bold())?,
//!             Answer::Ok(n) => room.say(format_args!("Nope ({n})—it was 3!"))?,
//!             Answer::TimedOut => {
//!                 room.say(irc_format!("{} {}", player.nick().bold(), "ran out of time!".yellow()))?
//!             }
//!             Answer::Left { .. } | Answer::Unreachable { .. } => {}
//!         }
//!     }
//!     Ok(())
//! }
//! # let _ = run_lobby;
//! ```

#![forbid(unsafe_code)]

pub use irc;
pub use irc_colors::{self, irc_format, Text};

mod ask;
mod bot;
mod breakout;
mod casemap;
mod error;
mod exclusive_dm;
pub mod future;
mod huddle;
mod inbox;
mod message;
mod outbound;
mod pump;
mod room;
mod state;
pub mod testing;
mod user;

pub mod fmt {
    pub use irc_colors::{
        irc_format, Color, ColorValue, Colorize, ExtendedColor, Span, Style, Text,
    };
}

pub mod prelude {
    pub use crate::{
        fmt::Colorize as _, irc_format, Answer, AskConfig, AskMatch, Bot, Breakout, ChatMessage,
        Error, Event, ExclusiveDm, GameFutureExt as _, Huddle, Inbox, InboxScope, MessageKind,
        Outcome, Result, Room, Text, User, UserId,
    };
}

pub use crate::{
    ask::{Answer, Ask, AskConfig, AskMatch, IntoAskMatch, TimeoutCtx},
    bot::{Bot, BotConfig, ShutdownRequested},
    breakout::{Breakout, BreakoutOptions},
    casemap::{CaseMap, FoldedStr},
    error::{DisconnectReason, Error, Result},
    exclusive_dm::ExclusiveDm,
    future::{race_all, GameFutureExt, Outcome},
    huddle::Huddle,
    inbox::{Inbox, InboxScope},
    message::{ChatMessage, Event, MessageKind, Target},
    room::{Member, Room},
    user::{User, UserId},
};

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Bot>();
    assert_send_sync::<Room>();
    assert_send_sync::<User>();
    assert_send_sync::<UserId>();
    assert_send_sync::<Member>();
    assert_send_sync::<AskConfig>();
    assert_send_sync::<InboxScope>();
    assert_send_sync::<MessageKind>();
    assert_send_sync::<ChatMessage>();
    assert_send_sync::<Event>();
    assert_send_sync::<Breakout>();
    assert_send_sync::<ExclusiveDm>();
    assert_send_sync::<Huddle>();
    assert_send_sync::<Inbox>();
    assert_send_sync::<Answer<()>>();
    assert_send_sync::<AskMatch<()>>();
};
