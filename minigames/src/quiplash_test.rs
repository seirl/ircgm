use ircgm::testing::test_harness;

use super::*;

#[tokio::test(start_paused = true)]
async fn quiplash_handles_validation_reject_pm_block_716_warning_timeout_and_voting() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#quip", &["Alice", "Bob", "Carol", "Dan"]);
    server.settle().await;

    let room = bot.room("#quip").unwrap();
    let alice = bot.user("Alice").unwrap();
    let bob = bot.user("Bob").unwrap();
    let carol = bot.user("Carol").unwrap();
    let dan = bot.user("Dan").unwrap();

    let round = tokio::spawn({
        let bot = bot.clone();
        let room = room.clone();
        let alice = alice.clone();
        let bob = bob.clone();
        let carol = carol.clone();
        let dan = dan.clone();
        async move {
            let assignments = [
                (alice, "Worst thing to say at a wedding"),
                (bob, "Rejected superhero name"),
                (carol, "Secret ingredient in mystery meat"),
                (dan, "Unlikely Fortune Cookie message"),
            ];
            let quips = collect_quips(&room, &assignments).await?;
            vote_on_quips(&bot, &room, &quips).await
        }
    });
    server.flush().await;
    let _ = server.take_messages_to("Alice");
    let _ = server.take_messages_to("Carol");
    let _ = server.take_messages_to("Dan");

    // Bob has `+g` callerid enabled (`716`):
    server.send_line(":irc.srv 716 GM Bob :is in +g mode\r\n");

    // Alice sends >80 chars (rejected!), then a valid quip; Dan submits a valid quip:
    server.dm("Alice", &"x".repeat(85));
    server.dm("Alice", "See you at the next one!");
    server.dm("Dan", "Help I am trapped in a bakery");
    server.settle().await;

    assert!(server
        .take_messages_to("Alice")
        .iter()
        .any(|m| m.contains("between 1 and 80 characters")));

    // Advance 35s -> Carol gets 10s warning; advance 10s -> Carol times out and voting begins!
    tokio::time::advance(Duration::from_secs(35)).await;
    server.flush().await;
    assert_eq!(
        server.take_messages_to("Carol"),
        vec!["⏳ 10 seconds left to submit!"]
    );

    tokio::time::advance(Duration::from_secs(10)).await;
    server.flush().await;

    let room_msgs = server.take_messages_to("#quip");
    assert!(room_msgs
        .iter()
        .any(|m| m.contains("Bob couldn't receive DMs (716")));
    assert!(room_msgs.iter().any(|m| m == "Carol ran out of time!"));

    // Voting phase (20s): Alice tries self-voting #1 (ignored!), Bob and Carol vote #2 (Dan's quip):
    server.chat("Alice", "#quip", "!pick 1");
    server.chat("Bob", "#quip", "!pick 2");
    server.chat("Carol", "#quip", "!pick 2");
    server.settle().await;
    tokio::time::advance(Duration::from_secs(20)).await;

    let winner = round.await.unwrap().unwrap().unwrap();
    assert_eq!(winner.0, dan);
    assert_eq!(winner.1, "Help I am trapped in a bakery");
    assert!(server
        .take_messages_to("#quip")
        .iter()
        .any(|m| m.contains("Dan wins Quiplash (2 votes)")));
}

#[tokio::test(start_paused = true)]
async fn tied_quips_trigger_wire_fair_dm_tiebreaker_race() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#quip", &["Alice", "Dan", "Bob", "Carol"]);
    server.settle().await;

    let room = bot.room("#quip").unwrap();
    let alice = bot.user("Alice").unwrap();
    let dan = bot.user("Dan").unwrap();

    let submissions = vec![
        (alice.clone(), "First quip".to_owned()),
        (dan.clone(), "Second quip".to_owned()),
    ];

    let vote = tokio::spawn({
        let bot = bot.clone();
        let room = room.clone();
        async move { vote_on_quips(&bot, &room, &submissions).await }
    });

    server.flush().await;

    // 1-1 tie (#1 gets Bob's vote, #2 gets Carol's vote):
    server.chat("Bob", "#quip", "!pick 1");
    server.chat("Carol", "#quip", "!pick 2");
    server.settle().await;
    tokio::time::advance(Duration::from_secs(20)).await;
    server.flush().await;

    // Tiebreaker fires (`17 * 6 = 102`); Dan answers before Alice in the same wire burst:
    server.dm("Dan", "102");
    server.dm("Alice", "102");

    let winner = vote.await.unwrap().unwrap().unwrap();
    assert_eq!(winner.0, dan);
    assert!(server
        .take_messages_to("#quip")
        .iter()
        .any(|m| m.contains("TIEBREAKER!")));
}
