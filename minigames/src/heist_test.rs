use std::time::Duration;

use irc::proto::Command;
use ircgm::testing::{test_harness, TestServer};

use super::{run_heist, HeistOutcome, Wire};

async fn accept_vault_and_join_crew(srv: &mut TestServer, crew: &[&str]) -> String {
    let vault = srv.accept_breakout("#lobby-vault", &[]).await;
    assert!(vault.starts_with("#lobby-vault-"));
    for nick in crew {
        srv.join_user(nick, &vault);
    }
    srv.settle().await;
    vault
}

#[tokio::test(start_paused = true)]
async fn hacker_breach_cool_nitrogen_and_synced_keys_succeed_and_teardown_vault() {
    let (bot, mut srv) = test_harness("GM");
    srv.complete_join("#lobby", &["Alice", "Bob"]);
    srv.settle().await;

    let lobby = bot.room("#lobby").unwrap();
    let alice = bot.user("Alice").unwrap();
    let bob = bot.user("Bob").unwrap();

    let mut game = tokio::spawn({
        let bot = bot.clone();
        let lobby = lobby.clone();
        let alice = alice.clone();
        let bob = bob.clone();
        async move { run_heist(&bot, &lobby, &alice, &bob, Wire::Red).await }
    });

    let vault = accept_vault_and_join_crew(&mut srv, &["Alice", "Bob"]).await;

    // Hacker (`Alice`) gets a reject hint on wrong hex, then solves `42` + `override`:
    srv.dm("Alice", "99");
    srv.settle().await;
    assert!(srv
        .take_messages_to("Alice")
        .iter()
        .any(|m| m.contains("Hint: 2 * 16 + 10")));

    srv.dm("Alice", "42");
    srv.settle().await;
    srv.dm("Alice", "override");
    srv.settle().await;

    // Phase 2: Bob sprays `!cool` (`0.5x` -> 40s real wall time!), so crew survives past 25s!
    srv.chat("Bob", &vault, "!cool");
    srv.settle().await;
    tokio::time::advance(Duration::from_secs(25)).await;
    assert!(!game.is_finished());

    // Both turn their keys!
    srv.chat("Alice", &vault, "!turn alpha");
    srv.chat("Bob", &vault, "!turn omega");

    let outcome = (&mut game).await.unwrap().unwrap();
    assert_eq!(outcome, HeistOutcome::ExtractedDualKeys);

    // Verify Breakout RAII drop sent KICK + PART for `#vault-xxxxxx`:
    let mut saw_part = false;
    while let Some(cmd) = srv.try_recv_sent() {
        if matches!(cmd, Command::PART(ch, _) if ch == vault) {
            saw_part = true;
        }
    }
    assert!(saw_part, "expected Breakout drop to PART {vault}");
}

#[tokio::test(start_paused = true)]
async fn safecracker_breach_and_wrong_wire_doubles_alarm_rate_to_catch_crew() {
    let (bot, mut srv) = test_harness("GM");
    srv.complete_join("#lobby", &["Alice", "Bob"]);
    srv.settle().await;

    let lobby = bot.room("#lobby").unwrap();
    let alice = bot.user("Alice").unwrap();
    let bob = bot.user("Bob").unwrap();

    let mut game = tokio::spawn({
        let bot = bot.clone();
        let lobby = lobby.clone();
        let alice = alice.clone();
        let bob = bob.clone();
        async move { run_heist(&bot, &lobby, &alice, &bob, Wire::Blue).await }
    });

    let vault = accept_vault_and_join_crew(&mut srv, &["Alice", "Bob"]).await;

    // Safecracker (`Bob`) picks all 3 pins in `#vault`:
    for cmd in ["!tension", "!rake", "!set"] {
        srv.chat("Bob", &vault, cmd);
        srv.settle().await;
    }

    // 4s virtual pass (16s virtual left), then Alice cuts the wrong wire (`red`) -> 2.0x rate (8s real left)!
    tokio::time::advance(Duration::from_secs(4)).await;
    srv.chat("Alice", &vault, "!cut red");
    srv.settle().await;

    tokio::time::advance(Duration::from_secs(7)).await;
    assert!(!game.is_finished());

    tokio::time::advance(Duration::from_secs(2)).await;
    let outcome = (&mut game).await.unwrap().unwrap();
    assert_eq!(outcome, HeistOutcome::CaughtByAlarm);
    assert!(srv
        .take_messages_to("#lobby")
        .iter()
        .any(|m| m.contains("BUSTED!")));
}

#[tokio::test(start_paused = true)]
async fn disconnect_during_breach_propagates_fatal_error_instead_of_swallowing() {
    let (bot, mut srv) = test_harness("GM");
    srv.complete_join("#lobby", &["Alice", "Bob"]);
    srv.settle().await;

    let lobby = bot.room("#lobby").unwrap();
    let alice = bot.user("Alice").unwrap();
    let bob = bot.user("Bob").unwrap();

    let game = tokio::spawn({
        let bot = bot.clone();
        let lobby = lobby.clone();
        async move { run_heist(&bot, &lobby, &alice, &bob, Wire::Red).await }
    });

    let _vault = accept_vault_and_join_crew(&mut srv, &["Alice", "Bob"]).await;
    srv.disconnect();

    let err = game.await.unwrap().unwrap_err();
    assert!(
        matches!(err, ircgm::Error::Disconnected(_)),
        "expected fatal Error::Disconnected, got {err:?}"
    );
}
