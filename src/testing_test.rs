use irc::proto::Command;
use irc_colors::Colorize;

use super::*;
use crate::error::{DisconnectReason, Error};

#[tokio::test]
async fn take_messages_to_strips_styles_case_folds_and_preserves_other_commands() {
    let (bot, mut server) = test_harness("gm");
    let join_fut = tokio::spawn({
        let bot = bot.clone();
        async move { bot.join("#Lobby").await }
    });
    assert!(matches!(server.recv_sent().await, Some(Command::JOIN(_, _, _))));
    server.complete_join("#Lobby", &["alice"]);
    let room = join_fut.await.unwrap().unwrap();

    let alice = bot.user("alice").unwrap();
    room.voice(&alice).unwrap();
    room.say("Hello lobby!".bold().green()).unwrap();
    alice.say("Secret DM").unwrap();
    room.notice("Heads up").unwrap();
    server.settle().await;

    // Drain only `#lobby` (case-insensitively), leaving `MODE` and `alice` DM intact:
    let lobby_msgs = server.take_messages_to("#lobby");
    assert_eq!(lobby_msgs, vec!["Hello lobby!", "Heads up"]);

    // `MODE` was preserved in `recv_sent` order:
    assert_eq!(
        server.recv_sent().await,
        Some(Command::Raw(
            "MODE".into(),
            vec!["#Lobby".into(), "+v".into(), "alice".into()]
        ))
    );
    // And `alice`'s DM is still intact:
    assert_eq!(server.take_messages_to("ALICE"), vec!["Secret DM"]);
    assert!(server.try_recv_sent().is_none());
}

#[tokio::test]
async fn test_server_disconnect_triggers_eof_on_open_inbox() {
    let (bot, server) = test_harness("gm");
    let mut inbox = bot.inbox_all();
    server.disconnect();

    let res = inbox.next_event().await;
    assert!(matches!(
        res,
        Err(Error::Disconnected(DisconnectReason::Eof))
    ));
}
