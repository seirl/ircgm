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
                .breakout_with(crate::BreakoutOptions::new("##vault").modes("+im"))
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

#[tokio::test(start_paused = true)]
async fn bot_quit_closes_test_server_connection_without_timeout() {
    let (bot, mut server) = test_harness("gm");
    bot.quit(Some("goodnight")).await.unwrap();
    assert_eq!(
        server.recv_sent().await,
        Some(Command::QUIT(Some("goodnight".into())))
    );
    assert!(matches!(
        bot.inbox().next_event().await,
        Err(Error::Disconnected(DisconnectReason::Quit))
    ));
}

#[tokio::test(start_paused = true)]
async fn peer_identity_survives_rename_resets_on_quit_and_supports_protocol_helpers() {
    let (bot, mut server) = test_harness("gm");
    server.isupport("MODES=3");
    server.complete_join("#lobby", &["alice", "bob"]);
    server.initial_topic("#lobby", "Welcome!");
    server.settle().await;

    let lobby = bot.room("#lobby").unwrap();
    assert_eq!(lobby.topic().as_deref(), Some("Welcome!"));
    let mut events = bot.inbox();

    // Custom hostmask + account survive `rename_user` onto `part_user` and reset on `quit_user`:
    let mut raw_events = bot.inbox();
    server.set_user_host("alice", "~ali", "cloak.test");
    server.set_account("alice", Some("acct_a"));
    server.rename_user("alice", "alicia");
    server.chat("alicia", "#lobby", "tagged hello");
    server.part_user("alicia", "#lobby", Some("brb"));
    server.settle().await;

    let _ = raw_events.next_raw().await.unwrap(); // NICK
    let _ = raw_events.next_raw().await.unwrap(); // PRIVMSG
    let part_raw = raw_events.next_raw().await.unwrap();
    assert_eq!(
        part_raw.prefix,
        Some(irc::proto::Prefix::Nickname(
            "alicia".into(),
            "~ali".into(),
            "cloak.test".into()
        ))
    );

    let _ = events.next_event().await.unwrap(); // NickChanged
    match events.next_event().await.unwrap() {
        crate::Event::Chat(m) => {
            assert_eq!(m.nick, "alicia");
            assert_eq!(m.ident.as_deref(), Some("~ali"));
            assert_eq!(m.host.as_deref(), Some("cloak.test"));
            assert_eq!(m.account.as_deref(), Some("acct_a"));
        }
        other => panic!("expected Chat, got {other:?}"),
    }
    match events.next_event().await.unwrap() {
        crate::Event::Parted { nick, reason, .. } => {
            assert_eq!(nick, "alicia");
            assert_eq!(reason.as_deref(), Some("brb"));
        }
        other => panic!("expected Parted, got {other:?}"),
    }

    // `accept_breakout_invite` + `advance`:
    let alice_u = bot.user("alicia").unwrap();
    let bob_u = bot.user("bob").unwrap();
    let br_task = tokio::spawn({
        let bot = bot.clone();
        async move {
            let v = bot.breakout("#vault").await?;
            v.invite_and_wait([&alice_u, &bob_u], std::time::Duration::from_secs(5))
                .await?;
            Ok::<_, Error>(v.name().to_owned())
        }
    });
    let vault_ch = server
        .accept_breakout_invite("#vault", &["alicia", "bob"])
        .await;
    assert_eq!(br_task.await.unwrap().unwrap(), vault_ch);

    let timed = tokio::spawn({
        let lobby = lobby.clone();
        async move {
            lobby
                .ask("Quick!", |_| Some(()))
                .config(crate::AskConfig::new().reply_timeout(std::time::Duration::from_secs(5)))
                .await
        }
    });
    server.advance(std::time::Duration::from_secs(5)).await;
    assert_eq!(timed.await.unwrap().unwrap(), crate::Answer::TimedOut);
}

#[tokio::test(start_paused = true)]
async fn virtual_clock_advance_settles_inbound_first_and_recv_message_auto_advances_past_5s() {
    let (bot, mut server) = test_harness("gm");
    server.complete_join("#onuw", &["alice", "dave"]);
    server.settle().await;

    let room = bot.room("#onuw").unwrap();
    let dave = bot.user("dave").unwrap();

    // 1. `server.chat` immediately followed by `server.advance(5s)` (no manual `settle()`) pumps
    //    `"!go"` and arms the 5s sleep *before* advancing virtual time:
    let go_task = tokio::spawn({
        let room = room.clone();
        async move {
            let mut inbox = room.inbox();
            inbox.wait_cmd("!go").await?;
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            room.say("5s elapsed!")?;
            Ok::<_, Error>(())
        }
    });
    server.flush().await;
    server.chat("alice", "#onuw", "!go");
    server.advance(std::time::Duration::from_secs(5)).await;
    go_task.await.unwrap().unwrap();
    assert_eq!(server.take_messages_to("#onuw"), vec!["5s elapsed!"]);

    // 2. `recv_message_to` uses a real wall-clock watchdog so Tokio's `start_paused = true`
    //    auto-advances across a 30s virtual sleep (`>= 5s`) without prematurely timing out:
    let delayed_dm = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        dave.say("Wake up, Dave!").unwrap();
    });
    assert_eq!(server.recv_message_to("dave").await, "Wake up, Dave!");
    delayed_dm.await.unwrap();
}
