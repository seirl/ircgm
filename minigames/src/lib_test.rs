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
        async move { recruit_players(&lobby, "TestGame", 2..=2, Duration::from_secs(15)).await }
    });
    srv.flush().await;

    srv.chat("Alice", "#lobby", "!join");
    srv.rename_user("Alice", "Alicia");
    srv.chat("Alicia", "#lobby", "!join");
    srv.chat("Bob", "#lobby", "!join");

    let players = join_task.await.unwrap().unwrap().expect("2 players");
    assert_eq!(players.len(), 2);
    assert_eq!(players[0].nick(), "Alicia");
    assert_eq!(players[1].nick(), "Bob");

    // 2. Flexible `2..=4` range returns `Ok(Some(2))` when window expires with >= `min` players:
    let range_task = tokio::spawn({
        let lobby = lobby.clone();
        async move { recruit_players(&lobby, "RangeGame", 2..=4, Duration::from_secs(10)).await }
    });
    srv.flush().await;
    srv.chat("Alicia", "#lobby", "!join");
    srv.chat("Bob", "#lobby", "!join");
    srv.settle().await;
    tokio::time::advance(Duration::from_secs(10)).await;
    let range_players = range_task.await.unwrap().unwrap().expect("2..=4 satisfied");
    assert_eq!(range_players.len(), 2);

    // 3. Timeout returns `Ok(None)` cleanly when fewer than `min` join:
    let short_task = tokio::spawn(async move {
        recruit_players(&lobby, "TestGame", 3..=3, Duration::from_secs(10)).await
    });
    srv.flush().await;
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
                .inbox()
                .on_for(|m| m.account.as_deref() == Some("staff") && m.is_cmd("!stop"))
                .map_output(|_| ExternalStop::AdminStop);

            let starter_abort = alice
                .inbox()
                .on_cmd("!abort")
                .map_output(|_| ExternalStop::StarterAbort);

            run_heist(&bot, &lobby, &alice, &bob, Wire::Red)
                .until(bot.shutdown_requested().map_output(|()| ExternalStop::Shutdown))
                .or_until(admin_stop)
                .or_until(starter_abort)
                .await
        }
    });

    // Wait for `run_heist` to open `#lobby-vault-xxxxxx` and invite Alice & Bob inside:
    let vault = srv.accept_breakout("#lobby-vault", &[]).await;
    assert!(vault.starts_with("#lobby-vault-"));
    srv.join_user("Alice", &vault);
    srv.join_user("Bob", &vault);
    srv.settle().await;

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
