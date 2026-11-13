//! Direct-style `async`/`await` IRC game master and interactive bot library.
//!
//! `ircgm` replaces state-machine enums and callback spaghetti with structured direct-style futures:
//! - [`Bot`] manages the IRC connection pump, registration barrier, priority outbound flood scheduler,
//!   and session-scoped [`Room`], [`User`], [`ExclusiveDm`], [`Huddle`], [`Breakout`], and [`Inbox`] handles.
//! - [`Ask`] and [`AskConfig`] provide ergonomic prompts with post-write-receipt reply clocks,
//!   inline [`Verdict::Reject`] validation, countdown warnings, and per-player timeout callouts.
//! - [`GameFutureExt`] composes any prompt or multi-step `async fn` with `.until()`, `.or_until()`,
//!   `.with_timeout()`, `.optional()`, and `.settled()` ([`Turn`]).
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
//!             .ask_user(player, player.nick().bold() + ": pick a number 1..=5!", |m| {
//!                 match m.text.trim().parse::<u8>() {
//!                     Ok(n @ 1..=5) => Verdict::Accept(n),
//!                     _ => Verdict::reject("Enter a number between 1 and 5!".yellow()),
//!                 }
//!             })
//!             .reply_timeout(Duration::from_secs(15))
//!             .settled()
//!             .await?
//!         {
//!             Turn::Reply(3) => room.say("🎉 Spot on!".green().bold())?,
//!             Turn::Reply(n) => room.say(format!("Nope ({n})—it was 3!"))?,
//!             Turn::TimedOut => room.say(player.nick().bold() + " ran out of time!".yellow())?,
//!             Turn::Left { .. } | Turn::Unreachable { .. } => {}
//!         }
//!     }
//!     Ok(())
//! }
//! # let _ = run_lobby;
//! ```

#![forbid(unsafe_code)]

pub use irc;
pub use irc_colors::{self, Text};

mod ask;
mod bot;
mod breakout;
mod casemap;
mod error;
mod huddle;
mod inbox;
mod message;
mod outbound;
mod pump;
mod room;
mod state;
mod supervise;
pub mod testing;
mod user;

pub mod fmt {
    pub use irc_colors::{Color, ColorValue, Colorize, Span, Style, Text};
}

pub mod future {
    pub use crate::supervise::{
        MapOutput, Optional, OrUntil, RaceAll, RunFor, RunUntilDeadline, Settled, Until,
        WithDeadline, WithTimeout,
    };
}

pub mod prelude {
    pub use crate::{
        fmt::Colorize as _, AskConfig, Bot, Breakout, ChatMessage, Error, Event, ExclusiveDm,
        GameFutureExt as _, Huddle, Inbox, InboxScope, MessageKind, Outcome, Result, Room, Text,
        Turn, User, UserId, Verdict,
    };
}

pub use crate::{
    ask::{Ask, AskConfig, IntoVerdict, TimeoutCtx, Verdict},
    bot::{Bot, BotConfig, ShutdownRequested},
    breakout::{Breakout, BreakoutOptions},
    casemap::{CaseMap, FoldedStr},
    error::{DisconnectReason, Error, Result},
    huddle::Huddle,
    inbox::{Inbox, InboxScope},
    message::{ChatMessage, Event, MessageKind, Target},
    room::{Member, Room},
    supervise::{race_all, GameFutureExt, Outcome, Turn},
    user::{ExclusiveDm, User, UserId},
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
};
