use std::time::Duration;

use irc::proto::Command;
use ircgm::testing::test_harness;

use super::*;

#[tokio::test(start_paused = true)]
async fn full_mafia_round_runs_night_moderation_wolf_huddle_graveyard_and_day_vote() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#town", &["Wolf1", "Wolf2", "Villager1", "Villager2"]);
    server.settle().await;

    let room = bot.room("#town").unwrap();
    let w1 = bot.user("Wolf1").unwrap();
    let w2 = bot.user("Wolf2").unwrap();
    let v1 = bot.user("Villager1").unwrap();
    let v2 = bot.user("Villager2").unwrap();

    // Zero bodyguards (`&[]`) exercises `race_all` empty-iterator Pending safety!
    let game = tokio::spawn({
        let bot = bot.clone();
        let room = room.clone();
        let wolves = vec![w1.clone(), w2.clone()];
        let villagers = vec![v1.clone(), v2.clone()];
        async move { run_mafia_round(&bot, &room, &wolves, &[], &villagers).await }
    });

    let _graveyard = server.accept_breakout("#town-graveyard", &[]).await;

    // Nightfall moderation commands:
    let mut saw_topic = false;
    let mut saw_plus_m = false;
    for _ in 0..6 {
        if let Some(cmd) = server.try_recv_sent() {
            match cmd {
                Command::TOPIC(ch, Some(t)) if ch == "#town" && t.contains("Night 1 (4 alive)") => {
                    saw_topic = true;
                }
                Command::Raw(m, args) if m == "MODE" && args == vec!["#town", "+m"] => {
                    saw_plus_m = true;
                }
                _ => {}
            }
        }
    }
    assert!(saw_topic && saw_plus_m);

    let _ = server.take_messages_to("Wolf1");
    let _ = server.take_messages_to("Wolf2");

    // Wolves deliberate via Huddle and bite Villager1:
    server.dm("Wolf1", "let's bite Villager1");
    server.dm("Wolf1", "!kill Wolf2"); // Invalid -> private reject to Wolf1
    server.dm("Wolf2", "!kill Villager1");
    server.settle().await;

    let w1_dms = server.take_messages_to("Wolf1");
    assert!(w1_dms
        .iter()
        .any(|m| m.contains("Pick a living non-Werewolf townsfolk!")));
    assert!(w1_dms
        .iter()
        .any(|m| m.contains("Wolf2 locked in Villager1!")));
    let w2_dms = server.take_messages_to("Wolf2");
    assert!(w2_dms
        .iter()
        .any(|m| m.contains("[Wolves] <Wolf1> let's bite Villager1")));
    assert!(w2_dms
        .iter()
        .any(|m| m.contains("Wolf2 locked in Villager1!")));

    // 5s bodyguard window (`0` guards) + 3s AFK `#graveyard` `invite_available` window:
    tokio::time::advance(Duration::from_secs(5)).await;
    server.flush().await;
    tokio::time::advance(Duration::from_secs(3)).await;
    server.flush().await;

    // Day vote among survivors (Wolf1, Wolf2, Villager2):
    // Villager2 typos `Wolv2` first — gets a retry hint rather than losing their ballot!
    let _ = server.take_messages_to("Villager2");
    server.dm("Villager2", "!vote Wolv2");
    server.settle().await;
    assert!(server
        .take_messages_to("Villager2")
        .iter()
        .any(|m| m.contains("Pick a living player's nick")));

    server.dm("Villager2", "!vote Wolf2");
    server.dm("Wolf1", "!vote Wolf2");
    server.settle().await;
    assert!(server
        .take_messages_to("Villager2")
        .iter()
        .any(|m| m.contains("Secret ballot cast for Wolf2!")));
    tokio::time::advance(Duration::from_secs(45)).await;

    let winner = game.await.unwrap().unwrap();
    assert_eq!(winner, MafiaWinner::Town);
    let town_msgs = server.take_messages_to("#town");
    assert!(town_msgs
        .iter()
        .any(|m| m.contains("Villager1 was devoured in the night!")));
    assert!(town_msgs
        .iter()
        .any(|m| m.contains("exiled Wolf2 (a Werewolf)! TOWN WINS!")));
}

#[tokio::test(start_paused = true)]
async fn external_cancel_mid_night_restores_room_modes_voices_and_topic_via_raii_drop() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#town", &["Wolf1", "Villager1"]);
    server.send_line(":irc.srv 332 GM #town :Welcome to #town!\r\n");
    server.settle().await;

    let room = bot.room("#town").unwrap();
    let w1 = bot.user("Wolf1").unwrap();
    let v1 = bot.user("Villager1").unwrap();

    let supervised = tokio::spawn({
        let bot = bot.clone();
        let room = room.clone();
        async move {
            run_mafia_round(&bot, &room, &[w1], &[], &[v1])
                .until(room.inbox().on_cmd("!abort"))
                .await
        }
    });

    let graveyard = server.accept_breakout("#town-graveyard", &[]).await;
    server.flush().await;
    while server.try_recv_sent().is_some() {}

    // Cancel mid-night via `!abort`!
    server.chat("Villager1", "#town", "!abort");
    let out = supervised.await.unwrap();
    assert!(matches!(out, Outcome::Stopped(_)));

    let mut saw_minus_m = false;
    let mut saw_devoice = false;
    let mut saw_restored_topic = false;
    let mut saw_graveyard_part = false;
    while let Some(cmd) = server.try_recv_sent() {
        match cmd {
            Command::Raw(m, args) if m == "MODE" && args == vec!["#town", "-m"] => {
                saw_minus_m = true;
            }
            Command::Raw(m, args)
                if m == "MODE"
                    && args.first().map(String::as_str) == Some("#town")
                    && args.get(1).is_some_and(|s| s.starts_with("-v")) =>
            {
                saw_devoice = true;
            }
            Command::TOPIC(ch, Some(t)) if ch == "#town" && t == "Welcome to #town!" => {
                saw_restored_topic = true;
            }
            Command::PART(ch, _) if ch == graveyard => {
                saw_graveyard_part = true;
            }
            _ => {}
        }
    }
    assert!(
        saw_minus_m && saw_devoice && saw_restored_topic && saw_graveyard_part,
        "expected RAII Drop cleanup (-m={saw_minus_m}, -v={saw_devoice}, topic={saw_restored_topic}, part={saw_graveyard_part})"
    );
}

#[tokio::test(start_paused = true)]
async fn offline_bodyguard_skipped_by_available_and_online_bodyguard_saves_wolf_target() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#town", &["Wolf1", "Guard1", "Guard2", "Villager1"]);
    server.settle().await;

    let room = bot.room("#town").unwrap();
    let w1 = bot.user("Wolf1").unwrap();
    let g1 = bot.user("Guard1").unwrap();
    let g2 = bot.user("Guard2").unwrap();
    let v1 = bot.user("Villager1").unwrap();

    // Guard1 disconnects before nightfall (`exclusive_available` skips Guard1 cleanly!):
    server.quit_user("Guard1", Some("AFK"));
    server.settle().await;

    let game = tokio::spawn({
        let bot = bot.clone();
        let room = room.clone();
        let wolves = vec![w1];
        let guards = vec![g1, g2];
        let villagers = vec![v1];
        async move { run_mafia_round(&bot, &room, &wolves, &guards, &villagers).await }
    });

    let _graveyard = server.accept_breakout("#town-graveyard", &[]).await;

    // Wolf1 bites Villager1, and online Guard2 DMs `!protect Villager1` within 5s!
    server.dm("Wolf1", "!kill Villager1");
    server.settle().await;
    let _ = server.take_messages_to("Guard2");
    server.dm("Guard2", "!protect Villager1");
    server.settle().await;
    assert!(server
        .take_messages_to("Guard2")
        .iter()
        .any(|m| m.contains("Moving to protect Villager1!")));

    // Both Guard2 and saved Villager1 vote out Wolf1!
    server.dm("Guard2", "!vote Wolf1");
    server.dm("Villager1", "!vote Wolf1");
    server.settle().await;
    tokio::time::advance(Duration::from_secs(45)).await;

    let winner = game.await.unwrap().unwrap();
    assert_eq!(winner, MafiaWinner::Town);
    assert!(server
        .take_messages_to("#town")
        .iter()
        .any(|m| m.contains("Bodyguard thwarted the Werewolves' attack")));
}
