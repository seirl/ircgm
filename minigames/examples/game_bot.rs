//! Playable multi-minigame IRC bot (`cargo run -p minigames --example game_bot`).
//!
//! Demonstrates **external supervision**: none of the minigames in [`minigames`] know about
//! admins, `!stop`, `!abort`, or [`Bot::shutdown_requested`]. Instead, [`game_lobby_loop`] wraps
//! [`minigames::play_by_name`] from the outside with `.until(...)` / `.or_until(...)`, relying on
//! RAII `Drop` (`Breakout`, `Huddle`, `ExclusiveDm`, `Inbox`) to clean up any cancelled game!

use ircgm::prelude::*;
use minigames::{play_by_name, GAMES};

enum StopReason {
    Shutdown,
    Admin(Box<ChatMessage>),
    StarterAbort,
}

fn is_admin(msg: &ChatMessage) -> bool {
    msg.account.as_deref() == Some("admin")
}

async fn game_lobby_loop(bot: &Bot, lobby: &Room) -> Result<()> {
    let mut inbox = lobby.inbox();
    while let Outcome::Completed(msg) = inbox
        .next_chat()
        .until(bot.shutdown_requested())
        .await
    {
        let msg = msg?;
        if msg.is_cmd("!games") {
            let list: Vec<String> = GAMES
                .iter()
                .map(|(slug, desc)| format!("{slug} ({desc})"))
                .collect();
            lobby.say("🎮 Available games (`!play <name>`): ".bold() + list.join(" | "))?;
            continue;
        }

        let Some(slug) = msg.cmd_args("!play").filter(|s| !s.is_empty()) else {
            continue;
        };

        let starter = msg.sender.clone();
        let admin_stop = bot
            .inbox_all()
            .into_wait_for(|m| is_admin(m) && m.is_cmd("!stop") && (m.is_dm() || m.is_in(lobby)))
            .ok_or_pending()
            .map_output(|m| StopReason::Admin(Box::new(m)));

        let starter_abort = starter
            .inbox()
            .into_wait_cmd("!abort")
            .ok_or_pending()
            .map_output(|_| StopReason::StarterAbort);

        match play_by_name(slug, bot, lobby)
            .until(bot.shutdown_requested().map_output(|()| StopReason::Shutdown))
            .or_until(admin_stop)
            .or_until(starter_abort)
            .await
        {
            Outcome::Completed(Ok(true)) => {}
            Outcome::Completed(Ok(false)) => {
                lobby.say(format!("Unknown game `{slug}` — try `!games`.").yellow())?;
            }
            Outcome::Completed(Err(e)) => {
                lobby.say(format!("Game `{slug}` ended early: {e}").red())?;
            }
            Outcome::Stopped(StopReason::Shutdown) => break,
            Outcome::Stopped(StopReason::Admin(admin_msg)) => {
                bot.reply(&admin_msg, format!("Stopped `{slug}`.").yellow())?;
                if admin_msg.is_dm() {
                    lobby.say(format!("🛑 `{slug}` was stopped by an admin.").yellow())?;
                }
            }
            Outcome::Stopped(StopReason::StarterAbort) => {
                lobby.say(starter.nick().bold() + format!(" aborted `{slug}`!").yellow())?;
            }
        }
    }
    Ok(())
}

async fn admin_quit_loop(bot: &Bot, lobby: &Room) -> Result<()> {
    let mut inbox = lobby.inbox();
    while let Outcome::Completed(msg) = inbox
        .wait_cmd("!quit")
        .until(bot.shutdown_requested())
        .await
    {
        let msg = msg?;
        if is_admin(&msg) {
            bot.reply(&msg, "Shutting down GameBot!".yellow())?;
            bot.quit(Some("Requested by admin")).await?;
            break;
        }
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let irc_cfg = irc::client::data::Config {
        nickname: Some("GameBot".into()),
        server: Some("127.0.0.1".into()),
        port: Some(6667),
        use_tls: Some(false),
        ..Default::default()
    };

    let bot = Bot::connect(irc_cfg).await?;
    let lobby = bot.join("#lobby").await?;

    tokio::try_join!(game_lobby_loop(&bot, &lobby), admin_quit_loop(&bot, &lobby))?;
    Ok(())
}
