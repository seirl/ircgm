use std::collections::HashSet;
use std::time::Duration;

use futures::future::try_join_all;
use futures::stream::{FuturesUnordered, StreamExt};
use ircgm::prelude::*;
use ircgm::testing::test_harness;
use tokio::sync::watch;
use tokio::time::Instant;

// 5.1 The Heist
pub enum Stop {
    Shutdown,
    Admin(ChatMessage),
    LeaderAbort(ChatMessage),
}

pub fn is_admin(msg: &ChatMessage) -> bool {
    msg.account.as_deref() == Some("seirl") || msg.host.as_deref() == Some("staff/seirl")
}

pub async fn lobby(bot: Bot, room: Room) -> Result<()> {
    loop {
        let trigger = match room
            .inbox()
            .wait_cmd("!heist")
            .until(bot.shutdown_requested())
            .await
        {
            Outcome::Completed(res) => res?,
            Outcome::Stopped(()) => break,
        };
        let leader = &trigger.sender;

        let mut admin_inbox = bot.inbox_all();
        let admin_stop = admin_inbox
            .wait_for(|m| is_admin(m) && m.is_cmd("!stop") && (m.is_dm() || m.is_in(&room)))
            .ok_or_pending()
            .map_output(Stop::Admin);

        let mut abort_inbox = leader.inbox();
        let leader_abort = abort_inbox
            .wait_cmd("!abort")
            .ok_or_pending()
            .map_output(Stop::LeaderAbort);

        match heist_mission(bot.clone(), room.clone(), leader)
            .until(bot.shutdown_requested().map_output(|()| Stop::Shutdown))
            .or_until(admin_stop)
            .or_until(leader_abort)
            .await
        {
            Outcome::Completed(Ok(())) => {}
            Outcome::Completed(Err(Error::TimedOut)) => {
                room.say(leader.nick().bold() + " hesitated too long and got caught!")?;
            }
            Outcome::Completed(Err(Error::HandshakeTimeout { .. })) => {
                room.say(leader.nick().bold() + " never showed up at the vault!")?;
            }
            Outcome::Completed(Err(e)) => room.say("Heist ended early: ".red() + e.to_string())?,
            Outcome::Stopped(Stop::Shutdown) => {
                room.say("Server restarting—heist called off!".yellow())?;
                break;
            }
            Outcome::Stopped(Stop::Admin(admin_msg)) => {
                bot.reply(&admin_msg, "Heist aborted.".yellow())?;
                if admin_msg.is_dm() {
                    room.say("ABORTED".bold().white().on_red() + " Heist stopped by an admin.")?;
                }
            }
            Outcome::Stopped(Stop::LeaderAbort(_)) => {
                room.say(leader.nick().bold() + " called off the heist!".yellow())?;
            }
        }
    }
    Ok(())
}

pub async fn heist_mission(bot: Bot, lobby: Room, leader: &User) -> Result<()> {
    let vault = bot
        .breakout("vault")
        .await?
        .with_ask_config(
            &AskConfig::new()
                .reply_timeout(Duration::from_secs(45))
                .timeout_message(
                    "ALERT".bold().white().on_red() + " Too slow—security spotted you!",
                ),
        );

    lobby.say(leader.nick().bold().yellow() + ": check your invite to " + vault.name().cyan() + "!")?;
    vault
        .invite_and_wait([leader], Duration::from_secs(30))
        .await?;

    let plan = vault
        .ask_user(
            leader,
            "Choose approach: "
                + "!plan stealth".bold().light_cyan()
                + " | "
                + "!plan loud".bold().orange(),
            |m| match m.cmd_args("!plan") {
                Some(choice @ ("stealth" | "loud")) => Some(choice.to_owned()),
                _ => None,
            },
        )
        .await?;

    let wire = vault
        .ask_user(
            leader,
            "Cut " + "red".red().bold() + " or " + "blue".light_blue().bold() + " wire?! (10s)",
            |m| match m.text.trim() {
                w @ ("red" | "blue") => Some(w.to_owned()),
                _ => None,
            },
        )
        .reply_timeout(Duration::from_secs(10))
        .await?;

    lobby.say(
        leader.nick().bold().green()
            + " cut the "
            + wire
            + " wire and pulled off the "
            + plan.bold()
            + " heist!",
    )?;
    Ok(())
}

// 5.2 Blackjack
#[derive(PartialEq, Eq)]
pub enum Move {
    Hit,
    Stand,
}

pub async fn blackjack_round(room: &Room, players: &[User]) -> Result<()> {
    let room = room.with_ask_config(
        &AskConfig::new()
            .reply_timeout(Duration::from_secs(30))
            .timeout_message_fn(|ctx| match ctx.user {
                Some(u) => u.nick().bold().yellow() + ": ⏳ too slow, auto-standing!",
                None => "⏳ Time's up!".yellow(),
            }),
    );

    room.ask("Anyone type `!deal` to cut the deck!", |m| {
        m.is_cmd("!deal").then_some(())
    })
    .await?;

    for player in players {
        loop {
            match room
                .ask_user(
                    player,
                    player.nick().bold() + ": `hit` or `stand`?",
                    |m| match m.text.trim() {
                        "hit" => Some(Move::Hit),
                        "stand" => Some(Move::Stand),
                        _ => None,
                    },
                )
                .settled()
                .await?
            {
                Turn::Reply(Move::Hit) => room.say(player.nick().bold() + " draws a card!")?,
                Turn::Reply(Move::Stand) | Turn::TimedOut => break,
                Turn::Left { nick, .. } => {
                    room.say(nick.bold() + " left the table—hand folded.")?;
                    break;
                }
                Turn::Unreachable { .. } => break,
            }
        }
    }
    Ok(())
}

// 5.3 Quiz (`User` hashes only its immutable session `u64` ID)
#[allow(clippy::mutable_key_type)]
pub async fn quiz_game(bot: Bot, room: Room) -> Result<()> {
    let players: Vec<User> = {
        let mut inbox = room.inbox();
        room.say("First 3 to type " + "!join".bold().green() + " get to play!")?;

        let mut set = HashSet::with_capacity(3);
        while set.len() < 3 {
            let msg = inbox.wait_cmd("!join").await?;
            if set.insert(msg.sender.clone()) {
                room.say(msg.sender.nick().bold() + format!(" joined! ({}/3)", set.len()))?;
            }
        }
        set.into_iter().collect()
    };

    let _locks = bot.exclusive_many(&players).await?;
    let mut race_in = bot.inbox_users(&players);
    for p in &players {
        p.say("Quick! What is 17 * 6? (20s)")?;
    }

    match race_in
        .wait_map(|m| (m.text.trim() == "102").then(|| (m.sender.clone(), m.text.clone())))
        .with_timeout(Duration::from_secs(20))
        .settled()
        .await?
    {
        Turn::Reply((winner, answer)) => {
            room.say(winner.nick().bold().green() + " won with " + answer.bold() + "!")?;
        }
        Turn::TimedOut => {
            for p in &players {
                let _ = p.say("⏳ Time's up!".yellow());
            }
            room.say("Nobody answered in time!".yellow())?;
        }
        Turn::Left { .. } | Turn::Unreachable { .. } => {
            room.say("All players disconnected—quiz cancelled.".yellow())?;
        }
    }
    Ok(())
}

// 5.4 Mafia
pub fn start_mafia_night(room: &Room, alive: &[User], dead: &[User], night: u32) -> Result<()> {
    room.set_topic(format!("🌙 Mafia — Night {night} ({} alive)", alive.len()))?;
    room.set_mode("+m", &[])?;
    room.devoice_many(dead)?;
    room.voice_many(alive)?;
    Ok(())
}

pub async fn werewolf_pack_kill(
    bot: &Bot,
    wolves: &[User],
    townsfolk: &[User],
) -> Result<Option<User>> {
    let mut pack = bot
        .huddle_available(wolves)
        .await?
        .with_relay("[Wolves] ".red().bold())
        .with_ask_config(
            &AskConfig::new()
                .reply_timeout(Duration::from_secs(30))
                .timeout_message("⏳ Dawn breaks—the pack missed its chance!".yellow()),
        );

    let pick = pack
        .ask(
            "🐺 Whisper here to deliberate, or `!kill <nick>` to strike (30s):",
            |m| {
                let Some(target) = m.cmd_args("!kill") else {
                    return Verdict::Ignore;
                };
                match townsfolk.iter().find(|u| u.eq_nick(target)) {
                    Some(victim) => Verdict::Accept((m.sender.clone(), victim.clone())),
                    None => Verdict::reject("Pick a living non-Werewolf villager!".yellow()),
                }
            },
        )
        .settled()
        .await?;

    match pick {
        Turn::Reply((killer, victim)) => {
            pack.say(
                "🐺 ".bold()
                    + killer.nick().bold()
                    + " locked in "
                    + victim.nick().bold().red()
                    + "!",
            )?;
            Ok(Some(victim))
        }
        Turn::TimedOut | Turn::Left { .. } | Turn::Unreachable { .. } => Ok(None),
    }
}

pub async fn pick_loadout_or_cancel(alice: &User) -> Result<Option<[String; 3]>> {
    let flow = async {
        let mut dm = alice.exclusive().await?.with_ignore_commands();
        dm.say("Name 3 items to bring into the vault (send 3 messages, or `!cancel`):")?;
        Ok::<_, ircgm::Error>([
            dm.inbox().next_chat().await?.text,
            dm.inbox().next_chat().await?.text,
            dm.inbox().next_chat().await?.text,
        ])
    };

    let mut cancel_in = alice.inbox();
    match flow.until(cancel_in.wait_cmd("!cancel").ok_or_pending()).await {
        Outcome::Completed(res) => res.map(Some),
        Outcome::Stopped(_) => {
            alice.say("Loadout selection cancelled.".yellow())?;
            Ok(None)
        }
    }
}

pub async fn resolve_simultaneous_triggers(alice: &User) -> Result<(bool, Option<usize>)> {
    let prompt_cfg = AskConfig::new()
        .reply_timeout(Duration::from_secs(20))
        .ignore_commands()
        .no_timeout_message();
    let (opt_block, opt_card) = tokio::try_join!(
        alice
            .ask(
                "Bob played Steal on you! Play Block? (yes/no, 20s)",
                |m| match m.text.trim() {
                    "yes" => Some(true),
                    "no" => Some(false),
                    _ => None,
                }
            )
            .config(&prompt_cfg)
            .warn_remaining(Duration::from_secs(5), "⏳ 5 seconds left!".yellow())
            .optional(),
        alice
            .ask(
                "Discard a card (1, 2, or 3):",
                |m| match m.text.trim().parse::<usize>() {
                    Ok(n @ 1..=3) => Verdict::Accept(n),
                    _ => Verdict::reject("Please reply with 1, 2, or 3.".yellow()),
                }
            )
            .config(&prompt_cfg)
            .optional(),
    )?;
    Ok((opt_block.unwrap_or(false), opt_card))
}

pub async fn collect_secret_votes(
    room: &Room,
    alive: &[User],
) -> Result<Vec<(User, Option<String>)>> {
    let mut dms: Vec<_> = alive.iter().map(|u| (u.clone(), u.inbox().lossy())).collect();
    room.say("Night falls! Everyone DM me `!vote <nick>` within 45 seconds.")?;
    let deadline = Instant::now() + Duration::from_secs(45);

    try_join_all(dms.iter_mut().map(|(voter, inbox)| async move {
        let pick = inbox
            .wait_map(|m| m.cmd_args("!vote").filter(|s| !s.is_empty()).map(str::to_owned))
            .with_deadline(deadline)
            .settled()
            .await?
            .into_reply();
        Ok::<_, ircgm::Error>((voter.clone(), pick))
    }))
    .await
}

// 5.5 Live Admin Control Panel
pub enum AdminCmd {
    Speed(f64),
    Stop,
}

fn parse_admin_dm(m: &ChatMessage) -> Option<(ChatMessage, AdminCmd)> {
    if !is_admin(m) {
        return None;
    }
    if m.is_cmd("!stop") {
        return Some((m.clone(), AdminCmd::Stop));
    }
    let factor = m.cmd_args("!speed")?.parse::<f64>().ok().filter(|&x| x > 0.0)?;
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

pub async fn run_supervised_game(bot: Bot, room: Room) -> Result<()> {
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
                    bot.reply(&msg, "Stopping game.")?;
                    return Ok::<_, ircgm::Error>(());
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

// 5.6 Multi-Room Bridge & Ping/Pong
pub async fn diplomat_bridge(bot: Bot) -> Result<()> {
    let usa = bot.join("#usa").await?;
    let ussr = bot.join("#ussr").await?;
    let mut bridge_in = bot.inbox_rooms([&usa, &ussr]);

    loop {
        let msg = bridge_in.next_chat().await?;
        if msg.is_in(&usa) {
            ussr.say("[USA] ".light_blue().bold() + "<" + (&msg.nick).bold() + "> " + msg.to_styled())?;
        } else {
            usa.say("[USSR] ".red().bold() + "<" + (&msg.nick).bold() + "> " + msg.to_styled())?;
        }
    }
}

pub async fn ping_pong(bot: Bot) -> Result<()> {
    let mut inbox = bot.inbox_all();
    loop {
        let msg = inbox.wait_cmd("!ping").await?;
        bot.reply(&msg, "pong".green().bold())?;
    }
}

// 5.7 Quiplash
pub async fn quiplash_round(
    room: &Room,
    assignments: &[(User, &str)],
) -> Result<Vec<(User, String)>> {
    let total = assignments.len();
    let cfg = AskConfig::new()
        .reply_timeout(Duration::from_secs(45))
        .timeout_message("⏳ Pencils down—time's up!".yellow())
        .ignore_commands();

    let mut tasks: FuturesUnordered<_> = assignments
        .iter()
        .map(|(player, secret_prompt)| {
            let cfg = &cfg;
            async move {
                let turn = player
                    .ask(
                        "Prompt: ".bold().cyan() + *secret_prompt + " (45s, max 80 chars)",
                        |m| {
                            let quip = m.text.trim();
                            if quip.is_empty() || quip.len() > 80 {
                                Verdict::reject(
                                    "Keep your answer between 1 and 80 characters!".yellow(),
                                )
                            } else {
                                Verdict::Accept(quip.to_owned())
                            }
                        },
                    )
                    .config(cfg)
                    .warn_remaining(
                        Duration::from_secs(10),
                        "⏳ 10 seconds left to submit!".yellow(),
                    )
                    .settled()
                    .await?;
                Ok::<_, ircgm::Error>((player.clone(), turn))
            }
        })
        .collect();

    let mut submissions = Vec::with_capacity(total);
    while let Some(res) = tasks.next().await {
        let (player, turn) = res?;
        match turn {
            Turn::Reply(quip) => {
                submissions.push((player.clone(), quip));
                room.say(
                    "✔ ".green().bold()
                        + player.nick().bold()
                        + format!(" locked in! ({}/{total})", submissions.len()),
                )?;
            }
            Turn::TimedOut => {
                room.say(player.nick().bold().yellow() + " ran out of time!")?;
            }
            Turn::Left { nick, .. } => {
                room.say(nick.bold() + " disconnected.")?;
            }
            Turn::Unreachable { code, .. } => {
                room.say(
                    player.nick().bold().red()
                        + format!(
                            " couldn't receive DMs ({code:?} — check `/umode -g` / NickServ)!"
                        ),
                )?;
            }
        }
    }
    Ok(submissions)
}

// 5.8 Co-op Bomb Defusal
pub async fn hack_terminal(hacker: &User) -> Result<()> {
    let mut dm = hacker
        .exclusive()
        .await?
        .with_ask_config(
            &AskConfig::new()
                .reply_timeout(Duration::from_secs(30))
                .ignore_commands(),
        );

    dm.ask("Step 1/2 — Convert `0x2A` to decimal:", |m| match m.text.trim() {
        "42" => Verdict::Accept(()),
        _ => Verdict::reject("Wrong checksum—try again!".red()),
    })
    .await?;

    dm.ask("Step 2/2 — Type `override` to cut blast-door power:", |m| {
        (m.text.trim() == "override").then_some(())
    })
    .await
}

pub async fn pick_pins(room: &Room, lockpick: &User) -> Result<()> {
    let room = room.with_ask_config(&AskConfig::new().reply_timeout(Duration::from_secs(20)));
    for (pin, expected) in [(1, "!tension"), (2, "!rake"), (3, "!set")] {
        room.ask_user(
            lockpick,
            lockpick.nick().bold() + format!(": Pin {pin}/3 (`!tension`, `!rake`, or `!set`)?"),
            |m| m.is_cmd(expected).then_some(()),
        )
        .await?;
    }
    Ok(())
}

pub async fn coop_bomb_defusal(room: &Room, hacker: &User, lockpick: &User) -> Result<()> {
    room.say(
        "🚨 Blast door sealed! Hacker (check DM) and Lockpick (here)—first bypass opens the core!",
    )?;

    let method = {
        let hack_fut = hack_terminal(hacker).settled();
        let pick_fut = pick_pins(room, lockpick).settled();
        tokio::pin!(hack_fut, pick_fut);
        let (mut hack_active, mut pick_active) = (true, true);

        loop {
            if !hack_active && !pick_active {
                room.say("💥 Neither specialist breached the door in time!".red().bold())?;
                return Ok(());
            }
            tokio::select! {
                res = &mut hack_fut, if hack_active => match res? {
                    Turn::Reply(()) => break "hacked the terminal",
                    Turn::TimedOut | Turn::Left { .. } | Turn::Unreachable { .. } => hack_active = false,
                },
                res = &mut pick_fut, if pick_active => match res? {
                    Turn::Reply(()) => break "picked the vault lock",
                    Turn::TimedOut | Turn::Left { .. } | Turn::Unreachable { .. } => pick_active = false,
                },
            }
        }
    };

    room.say(
        "🔓 Door open ("
            + method.green().bold()
            + ")! BOTH turn keys within 15s (`!turn alpha` / `!turn omega`)!",
    )?;

    let defused = async {
        tokio::try_join!(
            room.ask_user(hacker, hacker.nick().bold() + ": `!turn alpha`!", |m| {
                (m.cmd_args("!turn") == Some("alpha")).then_some(())
            })
            .no_reply_timeout(),
            room.ask_user(lockpick, lockpick.nick().bold() + ": `!turn omega`!", |m| {
                (m.cmd_args("!turn") == Some("omega")).then_some(())
            })
            .no_reply_timeout(),
        )
    }
    .with_timeout(Duration::from_secs(15))
    .optional()
    .await?;

    if defused.is_some() {
        room.say("🎉 BOMB DEFUSED with seconds to spare!".green().bold())?;
    } else {
        room.say("💥 BOOM! Keys weren't turned in sync!".bold().white().on_red())?;
    }
    Ok(())
}

#[tokio::test]
async fn cookbook_smoke_execution_end_to_end() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#vault", &["Hacker", "Lockpick"]);
    server.settle().await;

    let room = bot.room("#vault").unwrap();
    let hacker = bot.user("Hacker").unwrap();
    let lockpick = bot.user("Lockpick").unwrap();

    // Run Cookbook §5.8 Co-op Bomb Defusal end-to-end against `TestServer`!
    let r = room.clone();
    let h = hacker.clone();
    let l = lockpick.clone();
    let game_task = tokio::spawn(async move { coop_bomb_defusal(&r, &h, &l).await });

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    // Hacker completes 2-step `ExclusiveDm` bypass, canceling `pick_pins`:
    server.dm("Hacker", "42");
    server.settle().await;
    tokio::task::yield_now().await;
    server.dm("Hacker", "override");
    server.settle().await;
    tokio::task::yield_now().await;

    // Both turn keys in sync in `#vault`:
    server.chat("Hacker", "#vault", "!turn alpha");
    server.chat("Lockpick", "#vault", "!turn omega");
    server.settle().await;

    game_task.await.unwrap().unwrap();
    let room_msgs = server.take_messages_to("#vault");
    assert!(
        room_msgs.iter().any(|m| m.contains("BOMB DEFUSED")),
        "expected BOMB DEFUSED in {room_msgs:?}"
    );
}
