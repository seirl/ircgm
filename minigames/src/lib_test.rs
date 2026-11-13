use std::time::Duration;

use irc::proto::Command;
use ircgm::prelude::*;
use ircgm::testing::test_harness;

use crate::heist::{run_heist, Wire};
use crate::recruit_players;

#[tokio::test(start_paused = true)]
async fn recruit_players_deduplicates_across_nick_change_and_times_out_when_short() {
    let (bot, srv) = test_harness("GM");
    srv.complete_join("#lobby", &["Alice", "Bob"]);
    srv.settle().await;

    let lobby = bot.room("#lobby").unwrap();

    // 1. Alice joins, renames to `Alicia`, tries joining again (ignored!), then Bob joins -> 2 distinct!
    let join_task = tokio::spawn({
        let lobby = lobby.clone();
        async move { recruit_players(&lobby, "TestGame", 2, Duration::from_secs(15)).await }
    });
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }

    srv.chat("Alice", "#lobby", "!join");
    srv.send_line(":Alice!~Alice@test.host NICK :Alicia\r\n");
    srv.chat("Alicia", "#lobby", "!join");
    srv.chat("Bob", "#lobby", "!join");

    let players = join_task.await.unwrap().unwrap().expect("2 players");
    assert_eq!(players.len(), 2);
    assert_eq!(players[0].nick(), "Alicia");
    assert_eq!(players[1].nick(), "Bob");

    // 2. Timeout returns `Ok(None)` cleanly when fewer than `exact_count` join:
    let short_task = tokio::spawn(async move {
        recruit_players(&lobby, "TestGame", 3, Duration::from_secs(10)).await
    });
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(10)).await;
    assert!(short_task.await.unwrap().unwrap().is_none());
}

#[tokio::test(start_paused = true)]
async fn external_supervisor_cancels_running_minigame_and_tears_down_breakout_on_drop() {
    #[derive(Debug, PartialEq, Eq)]
    enum ExternalStop {
        Shutdown,
        AdminStop,
        StarterAbort,
    }

    let (bot, mut srv) = test_harness("GM");
    srv.complete_join("#lobby", &["Alice", "Bob", "Mod"]);
    srv.settle().await;

    let lobby = bot.room("#lobby").unwrap();
    let alice = bot.user("Alice").unwrap();
    let bob = bot.user("Bob").unwrap();

    // Wrap `run_heist` COMPLETELY EXTERNALLY with shutdown + admin `!stop` + starter `!abort`:
    let supervised = tokio::spawn({
        let bot = bot.clone();
        let lobby = lobby.clone();
        let alice = alice.clone();
        let bob = bob.clone();
        async move {
            let admin_stop = bot
                .inbox_all()
                .into_wait_for(|m| m.account.as_deref() == Some("staff") && m.is_cmd("!stop"))
                .ok_or_pending()
                .map_output(|_| ExternalStop::AdminStop);

            let starter_abort = alice
                .inbox()
                .into_wait_cmd("!abort")
                .ok_or_pending()
                .map_output(|_| ExternalStop::StarterAbort);

            run_heist(&bot, &lobby, &alice, &bob, Wire::Red)
                .until(bot.shutdown_requested().map_output(|()| ExternalStop::Shutdown))
                .or_until(admin_stop)
                .or_until(starter_abort)
                .await
        }
    });

    // Wait for `run_heist` to open `#vault-xxxxxx` and invite Alice & Bob inside:
    let vault = loop {
        if let Some(Command::JOIN(chan, _, _)) = srv.recv_sent().await {
            if chan.starts_with("#vault-") {
                srv.complete_join(&chan, &[]);
                break chan;
            }
        }
    };
    srv.settle().await;
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    srv.send_line(&format!(":Alice!~Alice@test.host JOIN {vault}\r\n"));
    srv.send_line(&format!(":Bob!~Bob@test.host JOIN {vault}\r\n"));
    srv.settle().await;
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }

    // Mid-heist, an authenticated staff member DMs `!stop` to the bot:
    srv.send_line("@account=staff :Mod!~Mod@test.host PRIVMSG GM :!stop\r\n");
    let outcome = supervised.await.unwrap();
    assert!(matches!(outcome, Outcome::Stopped(ExternalStop::AdminStop)));

    // Dropping the `run_heist` future dropped `Breakout`, automatically emitting `KICK` + `PART`!
    let mut saw_kick = false;
    let mut saw_part = false;
    while let Some(cmd) = srv.try_recv_sent() {
        match cmd {
            Command::KICK(ch, _, _) if ch == vault => saw_kick = true,
            Command::PART(ch, _) if ch == vault => saw_part = true,
            _ => {}
        }
    }
    assert!(saw_kick && saw_part, "expected external cancel to KICK and PART {vault}");
}
