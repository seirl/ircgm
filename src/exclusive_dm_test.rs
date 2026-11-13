use irc::proto::Command;

use super::*;
use crate::testing::test_harness;

#[tokio::test]
async fn exclusive_dm_records_and_clears_lock_holder_and_syncs_inbox_flags() {
    let (bot, mut server) = test_harness("gm");
    let alice = bot.user("alice").unwrap();

    assert!(alice.track().lock_holder.lock().unwrap().is_none());

    let mut dm = alice
        .exclusive()
        .await
        .unwrap()
        .with_ignore_commands()
        .with_accept_notices();
    assert!(alice.track().lock_holder.lock().unwrap().is_some());
    assert_eq!(dm.user(), &alice);
    assert!(format!("{dm:?}").contains("ExclusiveDm"));

    dm.say("hi").unwrap();
    dm.notice("hud").unwrap();
    dm.action("waves").unwrap();
    assert_eq!(
        server.recv_sent().await,
        Some(Command::PRIVMSG("alice".into(), "hi".into()))
    );
    assert_eq!(
        server.recv_sent().await,
        Some(Command::NOTICE("alice".into(), "hud".into()))
    );
    assert_eq!(
        server.recv_sent().await,
        Some(Command::PRIVMSG(
            "alice".into(),
            "\x01ACTION waves\x01".into()
        ))
    );

    server.dm("alice", "!cmd_skipped");
    server.send_line(":alice!~a@h NOTICE gm :notice_accepted\r\n");
    server.settle().await;
    assert_eq!(dm.inbox().next_chat().await.unwrap().text, "notice_accepted");

    drop(dm);
    assert!(alice.track().lock_holder.lock().unwrap().is_none());
}

#[tokio::test]
async fn acquire_many_and_available_reject_foreign_and_duplicate_users() {
    let (bot1, _srv1) = test_harness("gm1");
    let (bot2, _srv2) = test_harness("gm2");

    let alice1 = bot1.user("alice").unwrap();
    let foreign = bot2.user("alice").unwrap();

    assert!(matches!(
        bot1.exclusive_many([&alice1, &foreign]).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        bot1.exclusive_available([&alice1, &foreign]).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        bot1.exclusive_many([&alice1, &alice1]).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        bot1.exclusive_available([&alice1, &alice1]).await,
        Err(Error::InvalidInput(_))
    ));
}
