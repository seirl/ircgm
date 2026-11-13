//! # Co-op Vault Heist (2 Players: Hacker + Safecracker)
//!
//! ## How to Play
//! 1. Two thieves join in `#lobby` (**Hacker** + **Safecracker**) and accept an `INVITE` into a
//!    private `#vault-xxxxxx` [`Breakout`] room (`30s`).
//! 2. **Phase 1 — Vault Door Breach Race (`tokio::select!`):** Both specialists work simultaneously;
//!    whichever cracks their mechanism first opens the blast door and cancels the other's puzzle:
//!    - **Hacker (Private DM [`ExclusiveDm`]):** Converts `0x2A` to decimal (`42`), then enters
//!      `override` (`30s` per step, with [`Verdict::reject`] hints on wrong answers).
//!    - **Safecracker (Vault Channel [`Room::ask_user`]):** Sets 3 pins in order (`!tension`,
//!      `!rake`, `!set`, `20s` per pin).
//! 3. **Phase 2 — Live Laser Alarm & Dual-Key Extraction (`20s` virtual fuse):**
//!    - Breaching the vault trips a `20s` laser alarm (`1.0x` burn rate)!
//!    - Inside `#vault`, either thief can type `!cool` (once, halving alarm speed to `0.5x`) or
//!      gamble on `!cut <red|blue>` (cutting the safe wire disarms the vault immediately; cutting
//!      the trap wire **doubles** alarm speed to `2.0x`!), **or** both thieves can synchronize
//!      `!turn alpha` (Hacker) + `!turn omega` (Safecracker) before the alarm expires!
//! 4. Whether the crew escapes, trips the alarm, or an **external** caller cancels the heist future,
//!    dropping [`Breakout`] automatically `KICK`s everyone out and `PART`s `#vault-xxxxxx`.
//!
//! ## Library Mechanics Showcased
//! - **Pure composable game future (no internal admin/shutdown logic):** `run_heist` knows nothing
//!   about `!stop` or [`Bot::shutdown_requested`] — external callers wrap it with `.until(...)` and
//!   RAII `Drop` on [`Breakout`], [`ExclusiveDm`], and [`Inbox`] cleans up `#vault-xxxxxx` automatically.
//! - **RAII [`Breakout`] + scoped [`AskConfig`]:** Ephemeral `#vault-xxxxxx` creation,
//!   [`Breakout::invite_and_wait`], and room-level `AskConfig` defaults.
//! - **Asymmetric DM + Channel race (`tokio::select!`) & synced barrier (`tokio::try_join!`):**
//!   Races a multi-step [`ExclusiveDm`] puzzle against a 3-step [`Room::ask_user`] sequence, then
//!   synchronizes two distinct players' key turns.
//! - **Live mid-countdown alarm scaling (`tokio::sync::watch`):** Dynamically scales the active
//!   extraction fuse (`0.5x` `!cool` / `2.0x` wrong `!cut`) mid-sleep.

use std::time::Duration;

use ircgm::prelude::*;
use tokio::sync::watch;
use tokio::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wire {
    Red,
    Blue,
}

impl Wire {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "red" => Some(Self::Red),
            "blue" => Some(Self::Blue),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeistOutcome {
    ExtractedDualKeys,
    DisarmedWire,
    CaughtByAlarm,
}

async fn scaled_alarm(virtual_dur: Duration, rate_rx: &mut watch::Receiver<f64>) {
    let mut remaining = virtual_dur.as_secs_f64();
    while remaining > 0.0 {
        let rate = (*rate_rx.borrow_and_update()).max(0.01);
        let start = Instant::now();
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs_f64(remaining / rate)) => break,
            Ok(()) = rate_rx.changed() => {
                remaining = (remaining - start.elapsed().as_secs_f64() * rate).max(0.0);
            }
        }
    }
}

/// Hacker's 2-step private DM bypass puzzle.
pub async fn hack_terminal(hacker: &User) -> Result<()> {
    let mut dm = hacker
        .exclusive()
        .await?
        .with_ask_config(
            &AskConfig::new()
                .reply_timeout(Duration::from_secs(30))
                .ignore_commands(),
        );

    dm.ask("💻 Step 1/2 — Convert `0x2A` to decimal:", |m| {
        match m.text.trim() {
            "42" => Verdict::accept(()),
            _ => Verdict::reject("Hint: 2 * 16 + 10.".yellow()),
        }
    })
    .await?;

    dm.ask("💻 Step 2/2 — Type `override` to cut door power:", |m| {
        if m.text.trim().eq_ignore_ascii_case("override") {
            Verdict::accept_ack(
                (),
                "🔓 Door power severed! Return to the vault channel!".green(),
            )
        } else {
            Verdict::reject("Type `override` to sever door power.".yellow())
        }
    })
    .await
}

/// Safecracker's 3-step vault channel lockpick sequence.
pub async fn crack_tumblers(vault: &Room, safecracker: &User) -> Result<()> {
    for (idx, cmd) in ["!tension", "!rake", "!set"].into_iter().enumerate() {
        vault
            .ask_user(
                safecracker,
                format!("🔓 Pin {}/3 — {} type `{cmd}`:", idx + 1, safecracker.nick()),
                |m| m.is_cmd(cmd).then_some(()),
            )
            .reply_timeout(Duration::from_secs(20))
            .await?;
    }
    Ok(())
}

/// Lobby entrypoint: recruits 2 thieves (`Hacker` + `Safecracker`) and runs Co-op Vault Heist.
pub async fn play(bot: &Bot, lobby: &Room) -> Result< crate::PlayOutcome > {
    let Some(players) =
        crate::recruit_players(lobby, "Co-op Vault Heist", 2..=2, Duration::from_secs(30)).await?
    else {
        return Ok(crate::PlayOutcome::NotEnoughPlayers);
    };
    let safe_wire = if fastrand::bool() {
        Wire::Red
    } else {
        Wire::Blue
    };
    let _ = run_heist(bot, lobby, &players[0], &players[1], safe_wire).await?;
    Ok(crate::PlayOutcome::Completed)
}

/// Runs a complete 2-player Co-op Vault Heist inside a temporary `#vault-xxxxxx` [`Breakout`].
pub async fn run_heist(
    bot: &Bot,
    lobby: &Room,
    hacker: &User,
    safecracker: &User,
    safe_wire: Wire,
) -> Result<HeistOutcome> {
    let vault = bot
        .breakout(&crate::breakout_slug(lobby, "vault"))
        .await?
        .with_ask_config(
            &AskConfig::new()
                .reply_timeout(Duration::from_secs(20))
                .timeout_message("🚨 Too slow on the tumblers!".yellow()),
        );

    lobby.say(
        "🏦 Vault located! ".bold()
            + hacker.nick().bold()
            + " & "
            + safecracker.nick().bold()
            + ": accept your invite to "
            + vault.name().cyan().bold()
            + "!",
    )?;
    vault
        .invite_and_wait([hacker, safecracker], Duration::from_secs(30))
        .await?;

    // Phase 1: Hacker (DM) vs Safecracker (#vault) race to open the outer blast door!
    // Using `.settled()?` isolates per-player timeouts/departures while propagating fatal
    // `Error::Disconnected` / `Error::BotLeftRoom` immediately.
    let breached = tokio::select! {
        res = hack_terminal(hacker).settled() => {
            if matches!(res?, Turn::Reply(())) {
                vault.say("🔓 Hacker bypassed the outer vault door via DM!".green().bold())?;
                true
            } else {
                matches!(crack_tumblers(&vault, safecracker).settled().await?, Turn::Reply(()))
            }
        }
        res = crack_tumblers(&vault, safecracker).settled() => {
            if matches!(res?, Turn::Reply(())) {
                vault.say("🔓 Safecracker picked the outer vault door!".green().bold())?;
                true
            } else {
                matches!(hack_terminal(hacker).settled().await?, Turn::Reply(()))
            }
        }
    };

    if !breached {
        lobby.say("🚨 The heist crew failed to breach the outer vault door!".red().bold())?;
        return Ok(HeistOutcome::CaughtByAlarm);
    }

    // Phase 2: 20s variable-rate laser alarm + synchronized dual-key extraction!
    vault.say(
        "🚨 LASER ALARM (20s)! Sync `!turn alpha` ("
            + hacker.nick().bold()
            + ") + `!turn omega` ("
            + safecracker.nick().bold()
            + "), or use `!cool` / `!cut red|blue`!",
    )?;

    let (rate_tx, mut rate_rx) = watch::channel(1.0_f64);
    let dual_keys = async {
        let turn_alpha = async {
            let mut inbox = vault.inbox_user(hacker);
            loop {
                let (_msg, key) = inbox.wait_cmd_arg("!turn").await?;
                if key.eq_ignore_ascii_case("alpha") {
                    vault.say("🔑 Alpha key engaged!".cyan().bold())?;
                    return Ok::<(), Error>(());
                }
                vault.say(
                    hacker.nick().bold() + ": your cylinder takes `!turn alpha`!".yellow(),
                )?;
            }
        };
        let turn_omega = async {
            let mut inbox = vault.inbox_user(safecracker);
            loop {
                let (_msg, key) = inbox.wait_cmd_arg("!turn").await?;
                if key.eq_ignore_ascii_case("omega") {
                    vault.say("🔑 Omega key engaged!".cyan().bold())?;
                    return Ok::<(), Error>(());
                }
                vault.say(
                    safecracker.nick().bold() + ": your cylinder takes `!turn omega`!".yellow(),
                )?;
            }
        };
        tokio::try_join!(turn_alpha, turn_omega)?;
        Ok::<HeistOutcome, Error>(HeistOutcome::ExtractedDualKeys)
    };

    let alarm_tamper = async {
        let mut inbox = vault.inbox_users([hacker, safecracker]);
        let mut current_rate = 1.0_f64;
        let mut cooled = false;
        loop {
            let msg = inbox.next_chat().await?;
            if msg.is_cmd("!cool") && !cooled {
                cooled = true;
                current_rate *= 0.5;
                let _ = rate_tx.send(current_rate);
                vault.say(
                    "❄️ Liquid nitrogen sprayed on the sensor — alarm slowed to "
                        + format!("{current_rate}x!").cyan().bold(),
                )?;
            } else if let Some(arg) = msg.cmd_arg("!cut") {
                if let Some(wire) = Wire::parse(arg) {
                    if wire == safe_wire {
                        return Ok::<HeistOutcome, Error>(HeistOutcome::DisarmedWire);
                    }
                    current_rate *= 2.0;
                    let _ = rate_tx.send(current_rate);
                    vault.say(
                        "⚡ TRIPWIRE! Alarm speed doubled to ".red().bold()
                            + format!("{current_rate}x!").yellow().bold(),
                    )?;
                }
            }
        }
    };

    let outcome = tokio::select! {
        () = scaled_alarm(Duration::from_secs(20), &mut rate_rx) => HeistOutcome::CaughtByAlarm,
        res = dual_keys => res?,
        res = alarm_tamper => res?,
    };

    match outcome {
        HeistOutcome::ExtractedDualKeys => {
            lobby.say(
                "💎 HEIST COMPLETE! ".green().bold()
                    + hacker.nick().bold()
                    + " & "
                    + safecracker.nick().bold()
                    + " turned both keys and escaped with the diamonds!",
            )?;
        }
        HeistOutcome::DisarmedWire => {
            lobby.say(
                "✂️ HEIST COMPLETE! ".green().bold()
                    + hacker.nick().bold()
                    + " & "
                    + safecracker.nick().bold()
                    + " cut the alarm wire and emptied the vault!",
            )?;
        }
        HeistOutcome::CaughtByAlarm => {
            lobby.say(
                "🚓 BUSTED! Security locked down the vault on "
                    .red()
                    .bold()
                    + hacker.nick().bold()
                    + " & "
                    + safecracker.nick().bold()
                    + "!",
            )?;
        }
    }

    Ok(outcome)
}

#[cfg(test)]
#[path = "heist_test.rs"]
mod test;
