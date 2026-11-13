//! Demonstrates two advanced per-user `ExclusiveDm` patterns:
//!
//! 1. `pick_loadout_or_cancel`: A multi-step free-text DM wizard using
//!    `.ignore_commands()` so `!cancel` skips the free-text reader and trips
//!    the outer `.until(...)` supervisor, immediately dropping the `ExclusiveDm` lock.
//! 2. `resolve_simultaneous_triggers`: Two concurrent `user.ask(...)` futures run
//!    inside `tokio::try_join!` — `ExclusiveDm` serializes them on the wire and
//!    [`AskMatch::accept_say`] enqueues Prompt #1's confirmation *before* releasing
//!    `ExclusiveDm` to Prompt #2.

use std::time::Duration;

use ircgm::prelude::*;

pub async fn pick_loadout_or_cancel(player: &User) -> Result<Option<[String; 3]>> {
    let flow = async {
        let mut dm = player.exclusive().await?.ignore_commands();
        dm.say("Name 3 items to bring into the vault (send 3 messages, or `!cancel`):")?;
        Ok::<_, Error>([
            dm.inbox().next_chat().await?.text,
            dm.inbox().next_chat().await?.text,
            dm.inbox().next_chat().await?.text,
        ])
    }
    .with_timeout(Duration::from_secs(60));

    match flow.until(player.inbox().on_cmd("!cancel")).await {
        Outcome::Completed(res) => res.map(Some),
        Outcome::Stopped(_) => {
            player.say("Loadout selection cancelled.".yellow())?;
            Ok(None)
        }
    }
}

pub async fn resolve_simultaneous_triggers(player: &User) -> Result<(bool, Option<usize>)> {
    let prompt_cfg = AskConfig::new()
        .reply_timeout(Duration::from_secs(20))
        .ignore_commands()
        .no_timeout_message();

    let (ans_block, ans_card) = tokio::try_join!(
        player
            .ask(
                "Bob played Steal on you! Play Block? (yes/no, 20s)",
                |m| match m.text.trim() {
                    "yes" => AskMatch::accept_say(true, "✔ Playing Block!".green()),
                    "no" => AskMatch::accept_say(false, "✔ Allowing Steal.".yellow()),
                    _ => AskMatch::ignore(),
                }
            )
            .config(&prompt_cfg)
            .warn_remaining(Duration::from_secs(5), "⏳ 5 seconds left!".yellow()),
        player
            .ask(
                "Discard a card (1, 2, or 3):",
                |m| match m.text.trim().parse::<usize>() {
                    Ok(n @ 1..=3) => {
                        AskMatch::accept_say(n, format_args!("✔ Discarded card #{n}.").green())
                    }
                    _ => AskMatch::reject("Please reply with 1, 2, or 3.".yellow()),
                }
            )
            .config(&prompt_cfg),
    )?;

    Ok((ans_block.ok().unwrap_or(false), ans_card.ok()))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let irc_cfg = irc::client::data::Config {
        nickname: Some("WizardBot".into()),
        server: Some("127.0.0.1".into()),
        port: Some(6667),
        use_tls: Some(false),
        channels: vec!["#lobby".into()],
        ..Default::default()
    };

    let bot = Bot::connect(irc_cfg).await?;
    let lobby = bot.room("#lobby")?;

    let trigger = lobby.inbox().wait_cmd("!loadout").await?;
    if let Some(items) = pick_loadout_or_cancel(&trigger.sender).await? {
        lobby.say(irc_format!(
            "{} packed: {}",
            trigger.sender.nick().bold(),
            items.join(", "),
        ))?;
    }

    let _ = resolve_simultaneous_triggers(&trigger.sender).await?;
    bot.quit(Some("Demo complete")).await
}
