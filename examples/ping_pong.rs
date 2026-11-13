//! Minimal `ircgm` bot showcasing independent concurrent loops sharing the same [`Room`]:
//! - [`ping_loop`] owns its own `room.inbox().accept_notices()` and only cares about `!ping`.
//! - [`admin_loop`] owns a separate `room.inbox()` (ignoring `NOTICE`s) and handles `!quit`.
//!
//! Instead of a monolithic `match cmd` router, each feature is an isolated `async fn` composed
//! with `tokio::try_join!` and cooperative [`Bot::shutdown_requested`] supervision.

use ircgm::prelude::*;

async fn ping_loop(room: &Room) -> Result<()> {
    let mut inbox = room.inbox().accept_notices();
    while let Ok(msg) = inbox.wait_cmd("!ping").await {
        msg.reply("pong".green().bold())?;
    }
    Ok(())
}

async fn admin_loop(bot: &Bot, room: &Room) -> Result<()> {
    let mut inbox = room.inbox();
    while let Outcome::Completed(msg) = inbox
        .wait_cmd("!quit")
        .until(bot.shutdown_requested())
        .await
    {
        let msg = msg?;
        if msg.account.as_deref() != Some("admin") {
            msg.reply("Only admins may run !quit.".red())?;
            continue;
        }
        msg.reply("Shutting down!".yellow())?;
        bot.quit(Some("Requested by admin")).await?;
        break;
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let irc_cfg = irc::client::data::Config {
        nickname: Some("PingBot".into()),
        server: Some("127.0.0.1".into()),
        port: Some(6667),
        use_tls: Some(false),
        ..Default::default()
    };

    let bot = Bot::connect(irc_cfg).await?;
    let lobby = bot.join("#lobby").await?;

    tokio::try_join!(
        async {
            let _ = ping_loop(&lobby).until(bot.shutdown_requested()).await;
            Ok(())
        },
        admin_loop(&bot, &lobby),
    )?;
    Ok(())
}
