use irc::proto::Command;
use ircgm::testing::{test_harness, TestServer};

use super::*;

async fn accept_two_war_rooms(
    server: &mut TestServer,
    usa_nick: &str,
    ussr_nick: &str,
) -> (String, String) {
    let mut usa_chan = String::new();
    let mut ussr_chan = String::new();

    while usa_chan.is_empty() || ussr_chan.is_empty() {
        if let Some(Command::JOIN(ch, _, _)) = server.recv_sent().await {
            if ch.starts_with("#usa-") {
                usa_chan = ch.clone();
                server.complete_join(&ch, &[]);
            } else if ch.starts_with("#ussr-") {
                ussr_chan = ch.clone();
                server.complete_join(&ch, &[]);
            }
        }
    }

    let mut invites_seen = 0;
    while invites_seen < 2 {
        if let Some(Command::INVITE(nick, ch)) = server.recv_sent().await {
            if (nick == usa_nick && ch == usa_chan) || (nick == ussr_nick && ch == ussr_chan) {
                server.send_line(&format!(":{nick}!~u@h JOIN {ch}\r\n"));
                invites_seen += 1;
            }
        }
    }
    server.settle().await;
    (usa_chan, ussr_chan)
}

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

    let (usa_chan, ussr_chan) =
        accept_two_war_rooms(&mut server, "Kennedy", "Khrushchev").await;
    tokio::task::yield_now().await;
    let _ = server.take_messages_to(&usa_chan);
    let _ = server.take_messages_to(&ussr_chan);

    // Negotiate across the hotline, then both type `!ready`:
    server.chat("Kennedy", &usa_chan, "Let's both stand down.");
    server.chat("Khrushchev", &ussr_chan, "Agreed, comrade!");
    server.chat("Kennedy", &usa_chan, "!ready");
    server.chat("Khrushchev", &ussr_chan, "!ready");
    server.settle().await;
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

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
