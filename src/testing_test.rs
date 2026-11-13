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
async fn accept_breakout_and_presence_chat_helpers_preserve_unrelated_commands() {
    let (bot, mut server) = test_harness("gm");
    let lobby = bot.room("#lobby").unwrap();

    let task = tokio::spawn({
        let bot = bot.clone();
        let lobby = lobby.clone();
        async move {
            lobby.say("Opening breakout...")?;
            let vault = bot
                .breakout_with(crate::BreakoutOptions::new("##vault").modes(Some("+im")))
                .await?;
            let mut inbox = vault.inbox().accept_notices();
            let act = inbox.next_chat().await?;
            let ntc = inbox.next_chat().await?;
            inbox
                .wait_event_map(|ev| matches!(ev, crate::Event::Joined { .. }).then_some(()))
                .await?;
            inbox
                .wait_event_map(|ev| matches!(ev, crate::Event::NickChanged { .. }).then_some(()))
                .await?;
            inbox
                .wait_event_map(|ev| matches!(ev, crate::Event::Parted { .. }).then_some(()))
                .await?;
            Ok::<_, Error>((
                vault.name().to_owned(),
                act.is_action(),
                act.text,
                ntc.is_notice(),
                ntc.text,
            ))
        }
    });

    let vault_name = server.accept_breakout("##vault", &["alice"]).await;
    assert!(vault_name.starts_with("##vault-"));
    // Earlier `#lobby` message was preserved across `accept_breakout`!
    assert_eq!(
        server.take_messages_to("#lobby"),
        vec!["Opening breakout..."]
    );
    // And `+im` mode from `breakout_with` is already flushed after `accept_breakout`:
    assert_eq!(
        server.try_recv_sent(),
        Some(Command::Raw(
            "MODE".into(),
            vec![vault_name.clone(), "+im".into()]
        ))
    );

    server.action("alice", &vault_name, "sneaks in");
    server.notice("alice", &vault_name, "whisper");
    server.join_user("bob", &vault_name);
    server.rename_user("bob", "bobby");
    server.part_user("bobby", &vault_name, Some("escaped"));

    let (_, is_act, act_txt, is_ntc, ntc_txt) = task.await.unwrap().unwrap();
    assert!(is_act);
    assert_eq!(act_txt, "sneaks in");
    assert!(is_ntc);
    assert_eq!(ntc_txt, "whisper");
}

#[tokio::test]
async fn test_server_disconnect_triggers_eof_on_open_inbox() {
    let (bot, server) = test_harness("gm");
    let mut inbox = bot.inbox();
    server.disconnect();

    let res = inbox.next_event().await;
    assert!(matches!(
        res,
        Err(Error::Disconnected(DisconnectReason::Eof))
    ));
}
