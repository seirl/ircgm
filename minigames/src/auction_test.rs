use std::time::Duration;

use ircgm::testing::test_harness;

use super::run_auction;

#[tokio::test(start_paused = true)]
async fn mixed_channel_bids_and_secret_dm_appraise_and_proxy_resolve_in_wire_order() {
    let (bot, mut srv) = test_harness("GM");
    srv.complete_join("#lobby", &["Alice", "Bob", "Cara"]);
    srv.settle().await;

    let lobby = bot.room("#lobby").unwrap();
    let alice = bot.user("Alice").unwrap();
    let bob = bot.user("Bob").unwrap();
    let cara = bot.user("Cara").unwrap();
    let players = vec![alice.clone(), bob.clone(), cara.clone()];

    let game = tokio::spawn({
        let bot = bot.clone();
        let lobby = lobby.clone();
        let players = players.clone();
        async move {
            run_auction(
                &bot,
                &lobby,
                &players,
                "Jade Idol",
                100,
                Duration::from_secs(20),
            )
            .await
        }
    });

    srv.flush().await;

    // 1. Alice secretly appraises via DM while Bob opens bidding in #lobby;
    //    Cara registers a secret `!proxy 60` in DM right *before* Bob raises to 50 in the same burst!
    srv.dm("Alice", "!appraise");
    srv.chat("Bob", "#lobby", "!bid 30");
    srv.dm("Cara", "!proxy 60");
    srv.chat("Bob", "#lobby", "!bid 50");
    srv.settle().await;

    // Alice received her secret appraisal DM:
    assert!(srv
        .take_messages_to("Alice")
        .iter()
        .any(|m| m.contains("worth 100g")));

    // Let the 20s gavel expire: Cara's proxy (`60g`) auto-outbid Bob's `50g` to `55g`!
    tokio::time::advance(Duration::from_secs(20)).await;
    let res = game.await.unwrap().unwrap().expect("winner");
    assert_eq!(res.winner.id(), cara.id());
    assert_eq!(res.winning_bid, 55);
    assert_eq!(res.true_value, 100);
    assert_eq!(res.net_profit, 45);
    assert!(srv
        .take_messages_to("#lobby")
        .iter()
        .any(|m| m.contains("SOLD to Cara for 55g")));
}
