use irc::proto::Command;

use super::*;
use crate::testing::test_harness;

#[tokio::test]
async fn moderation_guard_restores_flags_pre_voiced_users_and_only_touched_topic() {
    let (bot, mut server) = test_harness("gm");
    server.complete_join("#town", &["+alice", "bob", "carol"]);
    server.initial_topic("#town", "Lobby Open");
    server.settle().await;

    let room = bot.room("#town").unwrap();
    let alice = bot.user("alice").unwrap();
    let bob = bot.user("bob").unwrap();
    let carol = bot.user("carol").unwrap();

    // 1. Guard that modifies +m, +N, voices bob+carol, and devoices pre-voiced alice (WITHOUT
    // changing topic) restores -mN, -v bob, +v alice (after carol was already devoiced mid-game),
    // and leaves topic untouched:
    {
        let mut guard = room.moderation_guard().unwrap();
        // Parameterized modes (`k`) are rejected by `set_flag`:
        assert!(matches!(
            guard.set_flag('k', true),
            Err(Error::InvalidInput(_))
        ));
        guard.set_moderated(true).unwrap();
        guard.set_flag('N', true).unwrap();
        guard.voice_many([&bob, &carol]).unwrap();
        guard.devoice(&alice).unwrap();
        guard.devoice(&carol).unwrap();
        while server.try_recv_sent().is_some() {}
    }

    let mut teardown = Vec::new();
    while let Some(cmd) = server.try_recv_sent() {
        teardown.push(cmd);
    }
    assert_eq!(
        teardown,
        vec![
            Command::Raw("MODE".into(), vec!["#town".into(), "-mN".into()]),
            Command::Raw(
                "MODE".into(),
                vec!["#town".into(), "-v".into(), "bob".into()]
            ),
            Command::Raw(
                "MODE".into(),
                vec!["#town".into(), "+v".into(), "alice".into()]
            ),
        ]
    );

    // 2. Guard that updates topic restores the initial topic on explicit `restore()`, while
    // `disarm()` suppresses teardown:
    let mut g2 = room.moderation_guard().unwrap();
    g2.set_topic("Night 1").unwrap();
    let _ = server.recv_sent().await;
    g2.restore().unwrap();
    assert_eq!(
        server.recv_sent().await,
        Some(Command::TOPIC("#town".into(), Some("Lobby Open".into())))
    );

    let mut g3 = room.moderation_guard().unwrap();
    g3.set_moderated(true).unwrap();
    let _ = server.recv_sent().await;
    g3.disarm();
    assert!(server.try_recv_sent().is_none());

    // 3. Rejects unjoined rooms (`BotLeftRoom`), non-op rooms (`NotChannelOp`), modeless `+`
    // channels (`NotChannelOp`), and servers whose `005 CHANMODES` omits `m` (`InvalidInput`):
    let unjoined = bot.room("#unjoined").unwrap();
    assert!(matches!(
        unjoined.moderation_guard(),
        Err(Error::BotLeftRoom { .. })
    ));

    server.mode("ChanServ", "#town", "-o gm");
    server.settle().await;
    assert!(matches!(
        room.moderation_guard(),
        Err(Error::NotChannelOp { ref channel }) if channel == "#town"
    ));

    server.isupport("CHANTYPES=#&+ CHANMODES=b,k,l,it");
    server.complete_join("+modeless", &["alice"]);
    server.complete_join("#nomod", &["alice"]);
    server.settle().await;
    assert!(matches!(
        bot.room("+modeless").unwrap().moderation_guard(),
        Err(Error::NotChannelOp { .. })
    ));
    assert!(matches!(
        bot.room("#nomod").unwrap().moderation_guard(),
        Err(Error::InvalidInput(_))
    ));
}
