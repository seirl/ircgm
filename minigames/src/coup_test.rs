use ircgm::testing::test_harness;

use super::{ambassador_exchange_or_cancel, resolve_steal_attempt, Role, StealResolution};

#[tokio::test(start_paused = true)]
async fn concurrent_steal_queues_behind_ambassador_exchange_without_interleaving() {
    let (bot, mut srv) = test_harness("GM");
    srv.complete_join("#lobby", &["Alice", "Bob"]);
    srv.settle().await;

    let lobby = bot.room("#lobby").unwrap();
    let alice = bot.user("Alice").unwrap();
    let bob = bot.user("Bob").unwrap();

    let round = tokio::spawn({
        let lobby = lobby.clone();
        let alice = alice.clone();
        let bob = bob.clone();
        async move {
            tokio::try_join!(
                ambassador_exchange_or_cancel(
                    &alice,
                    [Role::Duke, Role::Contessa],
                    [Role::Captain, Role::Ambassador]
                ),
                resolve_steal_attempt(&lobby, &bob, &alice),
            )
        }
    });

    srv.flush().await;

    // Only the Ambassador Exchange prompt was sent to Alice; Bob's steal DM is queued on ExclusiveDm!
    let initial_alice_dms = srv.take_messages_to("Alice");
    assert_eq!(initial_alice_dms.len(), 1);
    assert!(initial_alice_dms[0].contains("Ambassador Exchange"));

    // Invalid card name triggers inline retry; then Alice picks Captain + Ambassador:
    srv.dm("Alice", "Wizard");
    srv.settle().await;
    assert!(srv
        .take_messages_to("Alice")
        .iter()
        .any(|m| m.contains("Choose an available card")));

    srv.dm("Alice", "Captain");
    srv.dm("Alice", "Ambassador");
    srv.settle().await;

    // Now that Ambassador Exchange released ExclusiveDm, the queued Steal prompt arrives!
    let next_alice_dms = srv.take_messages_to("Alice");
    assert!(next_alice_dms
        .iter()
        .any(|m| m.contains("Bob is stealing 2 coins")));

    srv.dm("Alice", "block ambassador");
    let (kept, steal) = round.await.unwrap().unwrap();
    assert_eq!(kept, Some([Role::Captain, Role::Ambassador]));
    assert_eq!(steal, StealResolution::Blocked(Role::Ambassador));
    assert!(srv
        .take_messages_to("#lobby")
        .iter()
        .any(|m| m.contains("blocks the steal claiming Ambassador")));
}

#[tokio::test(start_paused = true)]
async fn canceling_ambassador_exchange_releases_dm_lock_immediately_for_queued_steal() {
    let (bot, mut srv) = test_harness("GM");
    srv.complete_join("#lobby", &["Alice", "Bob"]);
    srv.settle().await;

    let lobby = bot.room("#lobby").unwrap();
    let alice = bot.user("Alice").unwrap();
    let bob = bot.user("Bob").unwrap();

    let round = tokio::spawn({
        let lobby = lobby.clone();
        let alice = alice.clone();
        let bob = bob.clone();
        async move {
            tokio::try_join!(
                ambassador_exchange_or_cancel(
                    &alice,
                    [Role::Duke, Role::Contessa],
                    [Role::Captain, Role::Ambassador]
                ),
                resolve_steal_attempt(&lobby, &bob, &alice),
            )
        }
    });

    srv.flush().await;
    let _ = srv.take_messages_to("Alice");

    // Alice sends `!cancel`: `.with_ignore_commands()` skips `next_chat()` and trips `.until_ok(...)`,
    // dropping `ExclusiveDm` so Bob's queued steal prompt is sent immediately!
    srv.dm("Alice", "!cancel");
    srv.settle().await;

    let alice_dms = srv.take_messages_to("Alice");
    assert!(alice_dms
        .iter()
        .any(|m| m.contains("Ambassador exchange cancelled")));
    assert!(alice_dms
        .iter()
        .any(|m| m.contains("Bob is stealing 2 coins")));

    srv.dm("Alice", "allow");
    let (kept, steal) = round.await.unwrap().unwrap();
    assert_eq!(kept, None);
    assert_eq!(steal, StealResolution::Stolen { coins: 2 });
}
