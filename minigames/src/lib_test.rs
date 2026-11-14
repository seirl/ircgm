use std::time::Duration;

use futures::FutureExt as _;
use irc::proto::Command;
use ircgm::prelude::*;
use ircgm::testing::test_harness;

use crate::heist::{run_heist, Wire};
use crate::{find_game, recruit_players, GAMES};

#[test]
fn game_catalog_lookup_normalizes_names_and_summaries_show_player_counts() {
    let slug = |name: &str| find_game(name).map(|g| g.slug);
    assert_eq!(slug("Blackjack"), Some("blackjack"));
    assert_eq!(slug("bj"), Some("blackjack"));
    assert_eq!(slug("cold-war"), Some("cold_war"));
    assert_eq!(slug("coldwar"), Some("cold_war"));
    assert_eq!(slug("!simon-says now"), Some("simon_says"));
    assert_eq!(slug("werewolf"), Some("mafia"));
    assert_eq!(slug("nonexistent"), None);
    assert_eq!(slug(""), None);

    assert_eq!(GAMES.len(), 8);
    for (i, game) in GAMES.iter().enumerate() {
        assert!(GAMES[..i].iter().all(|other| other.slug != game.slug));
        assert_eq!(slug(game.slug), Some(game.slug));
        assert!(game.aliases.iter().all(|alias| slug(alias) == Some(game.slug)));
    }
    let summary = |name: &str| find_game(name).unwrap().summary;
    assert_eq!(summary("blackjack"), "1-4p turn-based channel Blackjack");
    assert_eq!(summary("heist"), "2p co-op vault breach + live-scaled laser alarm");
}

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
async fn recruit_players_drops_recruits_who_part_get_kicked_or_quit() {
    let (bot, srv) = test_harness("GM");
    srv.complete_join("#lobby", &["Alice", "Bob", "Carol", "Dave"]);
    srv.settle().await;
    let lobby = bot.room("#lobby").unwrap();

    let task = tokio::spawn({
        let lobby = lobby.clone();
        async move { recruit_players(&lobby, "TestGame", 2..=4, Duration::from_secs(10)).await }
    });
    srv.flush().await;

    for nick in ["Alice", "Bob", "Carol"] {
        srv.chat(nick, "#lobby", "!join");
    }
    srv.part_user("Alice", "#lobby", None);
    srv.kick_user("GM", "#lobby", "Bob", Some("no idling"));
    srv.chat("Dave", "#lobby", "!join");
    srv.chat("Dave", "#lobby", "!leave");
    srv.chat("Dave", "#lobby", "!join");
    srv.quit_user("Carol", Some("bye"));
    srv.advance(Duration::from_secs(10)).await;

    // Only Dave is still seated, so the 2-player minimum is not met:
    assert!(task.await.unwrap().unwrap().is_none());

    // A recruit who left and came back counts again, once:
    let task = tokio::spawn({
        let lobby = lobby.clone();
        async move { recruit_players(&lobby, "TestGame", 2..=2, Duration::from_secs(10)).await }
    });
    srv.flush().await;
    srv.chat("Dave", "#lobby", "!join");
    srv.part_user("Dave", "#lobby", None);
    srv.join_user("Dave", "#lobby");
    srv.chat("Dave", "#lobby", "!join");
    srv.join_user("Eve", "#lobby");
    srv.chat("Eve", "#lobby", "!join");
    let players = task.await.unwrap().unwrap().expect("2 players");
    let nicks: Vec<String> = players.iter().map(User::nick).collect();
    assert_eq!(nicks, ["Dave", "Eve"]);
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
            let admin_stop = bot.inbox().on_map(|m| {
                (m.account.as_deref() == Some("staff") && m.is_cmd("!stop"))
                    .then_some(ExternalStop::AdminStop)
            });

            let starter_abort = alice
                .inbox()
                .on_cmd("!abort")
                .map(|_| ExternalStop::StarterAbort);

            run_heist(&bot, &lobby, &alice, &bob, Wire::Red)
                .until(bot.shutdown_requested().map(|()| ExternalStop::Shutdown))
                .or_until(admin_stop)
                .or_until(starter_abort)
                .await
        }
    });

    // Wait for `run_heist` to open `#lobby-vault-xxxxxx` and invite Alice & Bob inside:
    let vault = srv
        .accept_breakout_invite("#lobby-vault", &["Alice", "Bob"])
        .await;
    assert!(vault.starts_with("#lobby-vault-"));

    // Mid-heist, an authenticated staff member DMs `!stop` to the bot:
    srv.set_account("Mod", Some("staff"));
    srv.dm("Mod", "!stop");
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
