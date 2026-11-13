use ircgm::testing::test_harness;

use super::*;

#[test]
fn hand_value_softens_aces_cleanly() {
    assert_eq!(hand_value(&[11, 10]), 21);
    assert_eq!(hand_value(&[11, 11, 9]), 21);
    assert_eq!(hand_value(&[11, 8, 5]), 14);
}

#[tokio::test(start_paused = true)]
async fn blackjack_round_handles_hit_stand_timeout_dealer_settlement_and_mid_hand_part() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#bj", &["Alice", "Bob", "Carol", "Spectator"]);
    server.settle().await;

    let room = bot.room("#bj").unwrap();
    let players = vec![
        bot.user("Alice").unwrap(),
        bot.user("Bob").unwrap(),
        bot.user("Carol").unwrap(),
    ];

    // Shoe order:
    // Dealer: 10, 8 (= 18)
    // Alice: 9, 7 (= 16), hits 4 (= 20), stands -> WINS vs 18!
    // Bob: 10, 8 (= 18), renames to Bobby, times out -> PUSH vs 18!
    // Carol: 6, 5 (= 11), parts mid-hand -> folded!
    let shoe = VecDeque::from(vec![10, 8, 9, 7, 4, 10, 8, 6, 5]);

    let game = tokio::spawn({
        let room = room.clone();
        async move { run_round_with_shoe(&room, &players, shoe).await }
    });
    server.flush().await;

    // Spectator `!deal` is ignored by `room.ask_users(&players, ...)`:
    server.chat("Spectator", "#bj", "!deal");
    server.chat("Bob", "#bj", "!deal");
    server.settle().await;

    // Alice hits (draws 4 -> 20), then stands:
    server.chat("Alice", "#bj", "hit");
    server.settle().await;
    server.chat("Alice", "#bj", "stand");
    server.settle().await;

    // Bob renames to Bobby, then times out after 30s -> auto-stands at 18:
    server.rename_user("Bob", "Bobby");
    server.settle().await;
    tokio::time::advance(Duration::from_secs(30)).await;
    server.flush().await;

    // Carol parts mid-hand -> folds cleanly:
    server.part_user("Carol", "#bj", Some("dinner"));
    server.settle().await;

    game.await.unwrap().unwrap();
    let msgs = server.take_messages_to("#bj");
    assert!(msgs.iter().any(|m| m == "Alice draws 4 (total 20)!"));
    assert!(msgs.iter().any(|m| m == "Alice stands on 20."));
    assert!(msgs
        .iter()
        .any(|m| m == "Bobby: ⏳ too slow, auto-standing!"));
    assert!(msgs
        .iter()
        .any(|m| m == "Carol left the table—hand folded."));
    assert!(msgs.iter().any(|m| m == "Dealer finishes with 18."));
    assert!(msgs.iter().any(|m| m == "Alice (20) WINS!"));
    assert!(msgs.iter().any(|m| m == "Bobby (18) PUSH."));
}
