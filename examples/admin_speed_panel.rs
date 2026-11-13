//! Demonstrates a live DM admin control panel (`!speed <factor>` and `!stop`)
//! running concurrently alongside a channel deliberation timer via `tokio::select!`
//! and `tokio::sync::watch`.

use std::time::Duration;

use ircgm::prelude::*;
use tokio::sync::watch;
use tokio::time::Instant;

enum AdminCmd {
    Speed(f64),
    Stop,
}

fn is_admin(msg: &ChatMessage) -> bool {
    msg.account.as_deref() == Some("seirl") || msg.host.as_deref() == Some("staff/seirl")
}

fn parse_admin_dm(m: &ChatMessage) -> Option<(ChatMessage, AdminCmd)> {
    if !is_admin(m) {
        return None;
    }
    if m.is_cmd("!stop") {
        return Some((m.clone(), AdminCmd::Stop));
    }
    let factor = m
        .cmd_args("!speed")?
        .parse::<f64>()
        .ok()
        .filter(|&x| x > 0.0)?;
    Some((m.clone(), AdminCmd::Speed(factor)))
}

async fn scaled_sleep(virtual_dur: Duration, speed_rx: &mut watch::Receiver<f64>) {
    let mut remaining = virtual_dur.as_secs_f64();
    while remaining > 0.0 {
        let rate = (*speed_rx.borrow_and_update()).max(0.01);
        let start = Instant::now();
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs_f64(remaining / rate)) => break,
            Ok(()) = speed_rx.changed() => {
                remaining = (remaining - start.elapsed().as_secs_f64() * rate).max(0.0);
            }
        }
    }
}

pub async fn run_supervised_deliberation(bot: &Bot, room: &Room) -> Result<()> {
    let (speed_tx, mut speed_rx) = watch::channel(1.0_f64);

    let admin_panel = async {
        let mut inbox = bot.inbox_dms();
        loop {
            let (msg, cmd) = inbox.wait_map(parse_admin_dm).await?;
            match cmd {
                AdminCmd::Speed(factor) => {
                    let _ = speed_tx.send(factor);
                    bot.reply(&msg, format!("Speed set to {factor}x."))?;
                }
                AdminCmd::Stop => {
                    bot.reply(&msg, "Stopping deliberation.")?;
                    return Ok::<_, Error>(());
                }
            }
        }
    };

    let game = async {
        room.say("You have 60 seconds to deliberate!")?;
        scaled_sleep(Duration::from_secs(60), &mut speed_rx).await;
        room.say("Time's up!")?;
        Ok(())
    };

    tokio::select! {
        res = game => res,
        res = admin_panel => res,
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let irc_cfg = irc::client::data::Config {
        nickname: Some("AdminPanelBot".into()),
        server: Some("127.0.0.1".into()),
        port: Some(6667),
        use_tls: Some(false),
        channels: vec!["#lobby".into()],
        ..Default::default()
    };

    let bot = Bot::connect(irc_cfg).await?;
    let lobby = bot.room("#lobby")?;
    run_supervised_deliberation(&bot, &lobby).await?;
    bot.quit(Some("Done")).await
}
