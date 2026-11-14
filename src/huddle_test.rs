use super::*;
use crate::testing::test_harness;

#[tokio::test]
async fn huddle_relay_forwards_banter_and_commands_to_other_members() {
    let (bot, mut server) = test_harness("gm");
    let w1 = bot.user("wolf1").unwrap();
    let w2 = bot.user("wolf2").unwrap();

    let mut pack = bot.huddle("Pack", [&w1, &w2]).await.unwrap();
    assert_eq!(pack.name().plain(), "Pack");
    assert_eq!(pack.len(), 2);
    assert!(!pack.is_empty());

    // Reading from `pack.inbox()` automatically relays banter and commands:
    server.dm("wolf1", "let's get alice");
    let _chat = pack.inbox().next_chat().await.unwrap();
    server.settle().await;
    assert_eq!(
        server.take_messages_to("wolf2"),
        vec!["[Pack] <wolf1> let's get alice"]
    );
    assert!(server.take_messages_to("wolf1").is_empty());

    server.dm("wolf1", "!help");
    let _cmd = pack.inbox().next_chat().await.unwrap();
    server.settle().await;
    assert_eq!(
        server.take_messages_to("wolf2"),
        vec!["[Pack] <wolf1> !help"]
    );
}

#[tokio::test]
async fn empty_vs_all_offline_huddle_and_from_user_subset() {
    let (bot, server) = test_harness("gm");

    // 1. Truly 0-argument huddle fails with `InvalidInput` (no fabricated `nick: ""`!):
    let mut empty = bot
        .huddle_available("Pack", std::iter::empty::<&User>())
        .await
        .unwrap();
    assert!(empty.is_empty());
    let res = empty.ask("Anyone here?", |m| Some(m.text.clone())).await;
    assert!(matches!(res, Err(Error::InvalidInput(_))));

    // 2. Huddle whose candidates were offline preserves real quitter nick in `Answer::Left`:
    let gone = bot.user("ghostwolf").unwrap();
    server.quit_user("ghostwolf", Some("away"));
    server.settle().await;
    let mut all_offline = bot.huddle_available("Pack", [&gone]).await.unwrap();
    assert!(all_offline.is_empty());
    assert!(matches!(
        all_offline.ask("Anyone here?", |m| Some(m.text.clone())).await,
        Ok(crate::Answer::Left { ref nick, ref reason }) if nick == "ghostwolf" && reason.as_deref() == Some("away")
    ));

    // 3. `from_user` rejects outsiders and preserves unread backlog when target already quit:
    let w1 = bot.user("wolf1").unwrap();
    let w2 = bot.user("wolf2").unwrap();
    let outsider = bot.user("villager").unwrap();
    let mut pack = bot.huddle("Pack", [&w1, &w2]).await.unwrap();
    assert!(pack.has_user(&w1));
    assert!(!pack.has_user(&outsider));
    assert!(matches!(
        pack.ask("Hello?", |m| Some(m.text.clone())).from_user(&outsider).await,
        Err(Error::InvalidInput(_))
    ));

    server.dm("wolf1", "still here");
    server.quit_user("wolf2", Some("bye"));
    server.settle().await;
    assert!(matches!(
        pack.ask("Confirm?", |m| Some(m.text.clone())).from_user(&w2).await,
        Ok(crate::Answer::Left { ref nick, .. }) if nick == "wolf2"
    ));
    assert_eq!(pack.inbox().next_chat().await.unwrap().text, "still here");
}

#[tokio::test(start_paused = true)]
async fn huddle_ask_from_user_and_ask_many_consensus_with_banter_relay() {
    let (bot, mut server) = test_harness("gm");
    let w1 = bot.user("wolf1").unwrap();
    let w2 = bot.user("wolf2").unwrap();
    let w3 = bot.user("wolf3").unwrap();
    let mut pack = bot.huddle("Pack", [&w1, &w2, &w3]).await.unwrap();

    // 1. `huddle.ask(...).from_user(&w1)` broadcasts to all 3 wolves, relays w2's banter, and
    // accepts only w1's answer:
    let alpha_ask = tokio::spawn({
        let w1 = w1.clone();
        async move {
            let pick = pack
                .ask("Alpha wolf1, call the play:", |m| {
                    m.cmd_arg("!call").map(str::to_owned)
                })
                .from_user(&w1)
                .await
                .unwrap();
            (pack, pick)
        }
    });
    assert_eq!(server.recv_message_to("wolf1").await, "Alpha wolf1, call the play:");
    assert_eq!(server.recv_message_to("wolf2").await, "Alpha wolf1, call the play:");
    assert_eq!(server.recv_message_to("wolf3").await, "Alpha wolf1, call the play:");

    server.dm("wolf2", "take bob!");
    server.dm("wolf2", "!call bob"); // ignored: only w1 may answer
    server.dm("wolf1", "!call carol");
    server.settle().await;

    let (mut pack, alpha_pick) = alpha_ask.await.unwrap();
    assert_eq!(alpha_pick, crate::Answer::Ok("carol".into()));
    assert_eq!(
        server.take_messages_to("wolf1"),
        vec!["[Pack] <wolf2> take bob!", "[Pack] <wolf2> !call bob"]
    );
    let _ = server.take_messages_to("wolf2");
    let _ = server.take_messages_to("wolf3");

    // 2. `huddle.ask_many` consensus vote: wolves discuss and can change votes until all remaining
    // reachable wolves agree on the same target:
    let consensus_task = tokio::spawn(async move {
        pack.ask_many("Vote `!kill <target>` until unanimous:", |m| {
            let Some(arg) = m.cmd_arg("!kill") else {
                return crate::AskMatch::ignore();
            };
            if arg == "wolf1" {
                crate::AskMatch::reject("Cannot bite a wolf!")
            } else {
                crate::AskMatch::accept(arg.to_owned())
            }
        })
        .allow_change(true)
        .config(crate::AskConfig::timeout(std::time::Duration::from_secs(30)))
        .stop_when(|a| {
            // Unanimous once at least 2 wolves have voted and every active vote matches:
            a.len() >= 2 && {
                let mut vals = a.values();
                let first = vals.next().unwrap();
                vals.all(|v| v == first)
            }
        })
        .await
    });

    assert_eq!(server.recv_message_to("wolf1").await, "Vote `!kill <target>` until unanimous:");
    assert_eq!(server.recv_message_to("wolf2").await, "Vote `!kill <target>` until unanimous:");
    assert_eq!(server.recv_message_to("wolf3").await, "Vote `!kill <target>` until unanimous:");

    server.dm("wolf1", "!kill wolf1"); // broadcast reject to the whole huddle!
    server.dm("wolf1", "!kill bob");
    server.dm("wolf2", "!kill carol");
    server.dm("wolf1", "wait ok let's do carol"); // relayed banter while already voted!
    server.settle().await;
    assert!(!consensus_task.is_finished());
    assert_eq!(
        server.take_messages_to("wolf1"),
        vec!["Cannot bite a wolf!", "[Pack] <wolf2> !kill carol"]
    );
    assert_eq!(
        server.take_messages_to("wolf2"),
        vec![
            "[Pack] <wolf1> !kill wolf1",
            "Cannot bite a wolf!",
            "[Pack] <wolf1> !kill bob",
            "[Pack] <wolf1> wait ok let's do carol",
        ]
    );

    // Wolf1 switches to `carol` -> 2/2 voted wolves agree on `carol` -> stops immediately!
    server.dm("wolf1", "!kill carol");
    server.settle().await;
    let votes = consensus_task.await.unwrap().unwrap();
    assert_eq!(votes.get(&w1), Some(&"carol".to_owned()));
    assert_eq!(votes.get(&w2), Some(&"carol".to_owned()));
    assert_eq!(votes.get(&w3), None);
}
