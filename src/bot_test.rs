use irc::proto::Command;

use super::*;
use crate::testing::{test_harness, test_harness_with};

#[tokio::test]
async fn bot_config_custom_command_prefixes_control_is_command_text() {
    let cfg = BotConfig::default().with_command_prefixes(["!", "."]);
    let (bot, _server) = test_harness_with("gm", cfg);
    let core = &bot.owner.core;

    assert!(core.is_command_text("!join"));
    assert!(core.is_command_text("   .status now"));
    assert!(!core.is_command_text("hello !world"));
    assert!(!core.is_command_text("   "));
}

#[tokio::test]
async fn bot_reply_mirrors_message_kind_and_destination() {
    let (bot, mut server) = test_harness("gm");
    let mut all = bot.inbox_all().accept_notices();

    // 1. Channel PRIVMSG -> channel PRIVMSG:
    server.chat("alice", "#lobby", "hello room");
    let ch_msg = all.next_chat().await.unwrap();
    bot.reply(&ch_msg, "hi channel").unwrap();
    assert_eq!(
        server.recv_sent().await,
        Some(Command::PRIVMSG("#lobby".into(), "hi channel".into()))
    );

    // 2. DM ACTION -> DM PRIVMSG:
    server.send_line(":alice!~a@h PRIVMSG gm :\x01ACTION waves\x01\r\n");
    let act_msg = all.next_chat().await.unwrap();
    assert!(act_msg.is_action());
    bot.reply(&act_msg, "waves back").unwrap();
    assert_eq!(
        server.recv_sent().await,
        Some(Command::PRIVMSG("alice".into(), "waves back".into()))
    );

    // 3. Human NOTICE -> NOTICE:
    server.send_line(":alice!~a@h NOTICE gm :psst\r\n");
    let not_msg = all.next_chat().await.unwrap();
    assert!(not_msg.is_notice());
    bot.reply(&not_msg, "quiet reply").unwrap();
    assert_eq!(
        server.recv_sent().await,
        Some(Command::NOTICE("alice".into(), "quiet reply".into()))
    );
}

#[tokio::test]
async fn shutdown_requested_resolves_before_and_after_request_shutdown() {
    let (bot, _server) = test_harness("gm");
    assert!(!bot.is_shutting_down());

    let waiter = bot.shutdown_requested();
    bot.request_shutdown();
    assert!(bot.is_shutting_down());
    waiter.await;

    // Subsequent calls resolve immediately:
    bot.shutdown_requested().await;
}

#[tokio::test]
async fn bot_reply_tracks_sender_rename_and_refuses_impostor_after_quit() {
    let (bot, mut server) = test_harness("gm");
    server.complete_join("#lobby", &["alice", "mallory"]);
    server.settle().await;

    let mut dms = bot.inbox_dms();
    server.dm("alice", "secret question");
    server.settle().await;
    let dm_msg = dms.next_chat().await.unwrap();

    // Alice renames -> `bot.reply` targets her new live nickname:
    server.rename_user("alice", "alicia");
    server.settle().await;
    bot.reply(&dm_msg, "secret answer").unwrap();
    assert_eq!(
        server.recv_sent().await,
        Some(Command::PRIVMSG("alicia".into(), "secret answer".into()))
    );

    // Alicia quits and Mallory steals `"alicia"` -> `bot.reply` refuses with `UserLeft`:
    server.quit_user("alicia", Some("gone"));
    server.rename_user("mallory", "alicia");
    server.settle().await;
    assert!(matches!(
        bot.reply(&dm_msg, "do not leak"),
        Err(Error::UserLeft { ref nick, .. }) if nick == "alicia"
    ));
    assert!(server.try_recv_sent().is_none());
}
