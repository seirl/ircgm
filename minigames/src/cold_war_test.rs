use ircgm::testing::test_harness;

use super::*;

#[tokio::test]
async fn cold_war_relays_hotline_banter_and_resolves_betrayal_in_breakout_rooms() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#lobby", &["Kennedy", "Khrushchev"]);
    server.settle().await;

    let lobby = bot.room("#lobby").unwrap();
    let kennedy = bot.user("Kennedy").unwrap();
    let khrushchev = bot.user("Khrushchev").unwrap();

    let game = tokio::spawn({
        let bot = bot.clone();
        let lobby = lobby.clone();
        async move { run_cold_war(&bot, &lobby, &kennedy, &khrushchev).await }
    });

    let usa_chan = server.accept_breakout("#lobby-usa", &[]).await;
    let ussr_chan = server.accept_breakout("#lobby-ussr", &[]).await;
    server.join_user("Kennedy", &usa_chan);
    server.join_user("Khrushchev", &ussr_chan);
    server.settle().await;
    let _ = server.take_messages_to(&usa_chan);
    let _ = server.take_messages_to(&ussr_chan);

    // Negotiate across the hotline, then both type `!ready`:
    server.chat("Kennedy", &usa_chan, "Let's both stand down.");
    server.chat("Khrushchev", &ussr_chan, "Agreed, comrade!");
    server.chat("Kennedy", &usa_chan, "!ready");
    server.chat("Khrushchev", &ussr_chan, "!ready");
    server.settle().await;

    assert!(server
        .take_messages_to(&ussr_chan)
        .iter()
        .any(|m| m == "[USA] <Kennedy> Let's both stand down."));
    assert!(server
        .take_messages_to(&usa_chan)
        .iter()
        .any(|m| m == "[USSR] <Khrushchev> Agreed, comrade!"));

    // Kennedy stands down while Khrushchev launches!
    server.chat("Kennedy", &usa_chan, "!stand_down");
    server.chat("Khrushchev", &ussr_chan, "!launch");
    server.settle().await;

    let outcome = game.await.unwrap().unwrap();
    assert_eq!(outcome, ColdWarOutcome::UssrFirstStrike);
    assert!(server
        .take_messages_to("#lobby")
        .iter()
        .any(|m| m.contains("FIRST STRIKE! Khrushchev launched while Kennedy stood down!")));
}
