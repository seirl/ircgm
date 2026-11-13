//! Multi-channel relay (`Bot::inbox_rooms`) that bridges `#usa` and `#ussr` in
//! wire-arrival order while preserving each speaker's IRC colors and styles via
//! `ChatMessage::to_styled()`.

use ircgm::prelude::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let irc_cfg = irc::client::data::Config {
        nickname: Some("DiplomatBot".into()),
        server: Some("127.0.0.1".into()),
        port: Some(6667),
        use_tls: Some(false),
        ..Default::default()
    };

    let bot = Bot::connect(irc_cfg).await?;
    let usa = bot.join("#usa").await?;
    let ussr = bot.join("#ussr").await?;
    let mut bridge_in = bot.inbox_rooms([&usa, &ussr]);

    loop {
        let msg = match bridge_in.next_chat().until(bot.shutdown_requested()).await {
            Outcome::Completed(res) => res?,
            Outcome::Stopped(()) => break,
        };

        if msg.is_in(&usa) {
            ussr.say(
                "[USA] ".light_blue().bold() + "<" + (&msg.nick).bold() + "> " + msg.to_styled(),
            )?;
        } else {
            usa.say("[USSR] ".red().bold() + "<" + (&msg.nick).bold() + "> " + msg.to_styled())?;
        }
    }

    Ok(())
}
