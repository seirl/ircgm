mod common;

use std::time::Duration;

use irc::proto::Command;
use ircgm::prelude::*;
use ircgm::testing::test_harness;
use ircgm::BreakoutOptions;

#[tokio::test]
async fn worldstate_isupport_batched_moderation_targmax_kick_and_not_op() {
    let (logs, _guard) = common::capture_warns();
    let (bot, mut server) = test_harness("GM");

    server.send_line(
        ":irc.srv 005 GM PREFIX=(qaohv)~&@%+ MODES=4 TARGMAX=PRIVMSG:4,KICK:1 CHANMODES=eIbq,k,flj,CFLMPQScgimnprstuz CASEMAPPING=rfc1459 :are supported\r\n",
    );
    server.send_line(":GM!~GM@test.host JOIN #c\r\n");
    server.send_line(":irc.srv 332 GM #c :Initial Mafia Topic\r\n");
    server.send_line(":irc.srv 353 GM = #c :@GM +U1 U2 U3 U4 U5\r\n");
    server.send_line(":irc.srv 353 GM = #c :U6 U7 U8 U9 U10\r\n");
    server.send_line(":irc.srv 366 GM #c :End of /NAMES list.\r\n");
    server.send_line(":GM!~GM@test.host MODE #c +klo secret 50 U2\r\n");
    server.settle().await;

    let room = bot.room("#c").unwrap();
    assert!(room.bot_is_op());
    assert_eq!(room.topic().as_deref(), Some("Initial Mafia Topic"));
    assert_eq!(room.members().len(), 11);

    let u1 = bot.user("U1").unwrap();
    let u2 = bot.user("U2").unwrap();
    assert!(room.has_user(&u1));
    assert!(room.member(&u1).unwrap().is_voiced);
    assert!(!room.member(&u1).unwrap().is_op);
    assert!(room.member(&u2).unwrap().is_op);

    server.send_line(":U2!~U2@test.host NICK :U2_renamed\r\n");
    server.settle().await;
    assert!(room.has_user(&u2));
    assert_eq!(room.member(&u2).unwrap().nick, "U2_renamed");
    assert!(room.member(&u2).unwrap().is_op);

    server.send_line(":U2_renamed!~U2@test.host NICK :U2\r\n");
    server.send_line(":GM!~GM@test.host TOPIC #c :Night 1\r\n");
    server.settle().await;
    assert_eq!(room.topic().as_deref(), Some("Night 1"));

    let ten_users: Vec<User> = (1..=10)
        .map(|i| bot.user(&format!("U{i}")).unwrap())
        .collect();
    room.voice_many(&ten_users).unwrap();
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::Raw(
            "MODE".into(),
            vec![
                "#c".into(),
                "+vvvv".into(),
                "U1".into(),
                "U2".into(),
                "U3".into(),
                "U4".into()
            ]
        )
    );
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::Raw(
            "MODE".into(),
            vec![
                "#c".into(),
                "+vvvv".into(),
                "U5".into(),
                "U6".into(),
                "U7".into(),
                "U8".into()
            ]
        )
    );
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::Raw(
            "MODE".into(),
            vec!["#c".into(), "+vv".into(), "U9".into(), "U10".into()]
        )
    );

    room.devoice_many(&ten_users).unwrap();
    for expected_flags in ["-vvvv", "-vvvv", "-vv"] {
        let cmd = server.recv_sent().await.unwrap();
        assert!(
            matches!(cmd, Command::Raw(ref m, ref a) if m == "MODE" && a[1] == expected_flags)
        );
    }

    let b1_task = tokio::spawn({
        let bot = bot.clone();
        async move {
            bot.breakout_with(BreakoutOptions::exact("#b1").modes(None::<&str>))
                .await
        }
    });
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::JOIN("#b1".into(), None, None)
    );
    server.complete_join("#b1", &["A", "B", "C"]);
    let b1 = b1_task.await.unwrap().unwrap();
    drop(b1);
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::KICK("#b1".into(), "A".into(), Some("Session ended".into()))
    );
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::KICK("#b1".into(), "B".into(), Some("Session ended".into()))
    );
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::KICK("#b1".into(), "C".into(), Some("Session ended".into()))
    );
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::PART("#b1".into(), None)
    );

    server.send_line(":irc.srv 005 GM TARGMAX=KICK:3 :are supported\r\n");
    server.settle().await;
    let b2_task = tokio::spawn({
        let bot = bot.clone();
        async move {
            bot.breakout_with(BreakoutOptions::exact("#b2").modes(None::<&str>))
                .await
        }
    });
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::JOIN("#b2".into(), None, None)
    );
    server.complete_join("#b2", &["A", "B", "C", "D"]);
    let b2 = b2_task.await.unwrap().unwrap();
    drop(b2);
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::KICK("#b2".into(), "A,B,C".into(), Some("Session ended".into()))
    );
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::KICK("#b2".into(), "D".into(), Some("Session ended".into()))
    );
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::PART("#b2".into(), None)
    );

    server.send_line(":ChanServ!s@services MODE #c -o GM\r\n");
    server.settle().await;
    assert!(!room.bot_is_op());
    for res in [
        room.kick(&u1, None),
        room.set_mode("+m", &[]),
        room.voice(&u1),
        room.devoice(&u1),
        room.set_topic("nope"),
    ] {
        assert!(matches!(res, Err(Error::NotChannelOp { ref channel }) if channel == "#c"));
    }

    let mut raw_in = room.inbox();
    let ask_room = room.clone();
    let chat_ask = tokio::spawn(async move {
        ask_room
            .ask("Still listening?", |m| (m.text == "yep").then_some(()))
            .await
    });
    tokio::task::yield_now().await;
    let _ = server.take_messages_to("#c");

    server.send_line(":irc.srv 482 GM #c :You're not a channel operator\r\n");
    server.chat("U1", "#c", "yep");
    server.settle().await;

    assert_eq!(chat_ask.await.unwrap().unwrap(), Answer::Ok(()));
    assert!(logs.text().contains("482 ERR_CHANOPRIVSNEEDED"));
    let saw_482 = raw_in
        .wait_raw_map(|raw| {
            matches!(&raw.command, Command::Response(r, _) if *r as u16 == 482).then_some(())
        })
        .await;
    assert!(saw_482.is_ok());
}

#[tokio::test]
async fn breakout_invite_and_wait_vs_invite_available_and_room_invite() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#lobby", &["OfflineGuy", "U1", "U2", "U3"]);
    server.settle().await;

    let lobby = bot.room("#lobby").unwrap();
    let offline_u = bot.user("OfflineGuy").unwrap();
    let u1 = bot.user("U1").unwrap();
    let u2 = bot.user("U2").unwrap();
    let u3 = bot.user("U3").unwrap();

    server.quit_user("OfflineGuy", Some("gone"));
    server.settle().await;

    let inv_err = lobby.invite(&offline_u).unwrap_err();
    assert!(matches!(inv_err, Error::UserLeft { .. }));
    assert!(server.try_recv_sent().is_none());

    lobby.invite(&u1).unwrap();
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::INVITE("U1".into(), "#lobby".into())
    );

    let vault_task = tokio::spawn({
        let bot = bot.clone();
        async move {
            bot.breakout_with(BreakoutOptions::exact("#vault").modes(None::<&str>))
                .await
        }
    });
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::JOIN("#vault".into(), None, None)
    );
    server.complete_join("#vault", &[]);
    let vault = vault_task.await.unwrap().unwrap();

    let strict_err = vault
        .invite_and_wait([&offline_u, &u1], Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(matches!(strict_err, Error::UserLeft { .. }));
    assert!(server.try_recv_sent().is_none());

    let avail_fut = vault.invite_available([&offline_u, &u1, &u2, &u3], Duration::from_millis(40));
    let drive = async {
        assert_eq!(
            server.recv_sent().await.unwrap(),
            Command::INVITE("U1".into(), "#vault".into())
        );
        assert_eq!(
            server.recv_sent().await.unwrap(),
            Command::INVITE("U2".into(), "#vault".into())
        );
        assert_eq!(
            server.recv_sent().await.unwrap(),
            Command::INVITE("U3".into(), "#vault".into())
        );
        server.send_line(":U1!~U1@test.host JOIN #vault\r\n");
        server.quit_user("U2", Some("nah"));
        server.settle().await;
    };
    let (joined, ()) = tokio::join!(avail_fut, drive);
    assert_eq!(joined.unwrap(), vec![u1.clone()]);

    let none_joined = vault
        .invite_available([&u3], Duration::from_millis(20))
        .await
        .unwrap();
    let _ = server.recv_sent().await;
    assert!(none_joined.is_empty());

    let close_fut = vault.close();
    let ack_part = async {
        assert_eq!(
            server.recv_sent().await.unwrap(),
            Command::KICK("#vault".into(), "U1".into(), Some("Session ended".into()))
        );
        assert_eq!(
            server.recv_sent().await.unwrap(),
            Command::PART("#vault".into(), None)
        );
        server.send_line(":GM!~GM@test.host PART #vault\r\n");
        server.settle().await;
    };
    let (close_res, ()) = tokio::join!(close_fut, ack_part);
    close_res.unwrap();
}

#[tokio::test]
async fn breakout_invite_tracks_nick_renames_quits_and_inflight_401_reinvites() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#lobby", &["Alice", "Cara", "Dan", "Eve"]);
    server.settle().await;

    let alice = bot.user("Alice").unwrap();
    let untracked_bob = bot.user("UntrackedBob").unwrap();
    let cara = bot.user("Cara").unwrap();
    let ghost_user = bot.user("GhostUser").unwrap();
    let dan = bot.user("Dan").unwrap();
    let eve = bot.user("Eve").unwrap();

    let vault_task = tokio::spawn({
        let bot = bot.clone();
        async move { bot.breakout_with(BreakoutOptions::exact("#vault").no_modes()).await }
    });
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::JOIN("#vault".into(), None, None)
    );
    server.complete_join("#vault", &[]);
    let vault = vault_task.await.unwrap().unwrap();

    // 1. Users (`Alice` in `#lobby` + `UntrackedBob` not in `#lobby`) rename between INVITE and JOIN:
    let wait_renamed = vault.invite_and_wait([&alice, &untracked_bob], Duration::from_secs(1));
    let drive_renamed = async {
        assert_eq!(
            server.recv_sent().await.unwrap(),
            Command::INVITE("Alice".into(), "#vault".into())
        );
        assert_eq!(
            server.recv_sent().await.unwrap(),
            Command::INVITE("UntrackedBob".into(), "#vault".into())
        );
        server.rename_user("Alice", "Alicia");
        server.rename_user("UntrackedBob", "Bobby");
        server.join_user("Alicia", "#vault");
        server.join_user("Bobby", "#vault");
        server.settle().await;
    };
    let (res_renamed, ()) = tokio::join!(wait_renamed, drive_renamed);
    res_renamed.unwrap();
    assert!(vault.has_nick("Alicia"));
    assert!(vault.has_nick("Bobby"));

    // 2. User renames then QUITs before joining -> immediate `Error::UserLeft` with new nick:
    let wait_quit = vault.invite_and_wait([&cara], Duration::from_secs(5));
    let drive_quit = async {
        assert_eq!(
            server.recv_sent().await.unwrap(),
            Command::INVITE("Cara".into(), "#vault".into())
        );
        server.rename_user("Cara", "Cara_afk");
        server.quit_user("Cara_afk", Some("dinner"));
        server.settle().await;
    };
    let (res_quit, ()) = tokio::join!(wait_quit, drive_quit);
    assert!(
        matches!(res_quit, Err(Error::UserLeft { ref nick, ref reason }) if nick == "Cara_afk" && reason.as_deref() == Some("dinner")),
        "expected UserLeft(Cara_afk), got {res_quit:?}"
    );

    // 3. Unknown user receives 401 -> immediate `Error::UserLeft` (never hangs to HandshakeTimeout):
    let wait_401 = vault.invite_and_wait([&ghost_user], Duration::from_secs(5));
    let drive_401 = async {
        assert_eq!(
            server.recv_sent().await.unwrap(),
            Command::INVITE("GhostUser".into(), "#vault".into())
        );
        server.send_line(":irc.srv 401 GM GhostUser :No such nick/channel\r\n");
        server.settle().await;
    };
    let (res_401, ()) = tokio::join!(wait_401, drive_401);
    assert!(
        matches!(res_401, Err(Error::UserLeft { ref nick, .. }) if nick == "GhostUser"),
        "expected UserLeft(GhostUser), got {res_401:?}"
    );

    // 4. Cross-in-flight rename on `invite_and_wait`: `Dan` renames to `Danny` before server sees
    //    `INVITE Dan #vault`, causing `401 Dan` -> bot automatically re-invites `Danny`!
    let wait_cross = vault.invite_and_wait([&dan], Duration::from_secs(5));
    let drive_cross = async {
        assert_eq!(
            server.recv_sent().await.unwrap(),
            Command::INVITE("Dan".into(), "#vault".into())
        );
        server.rename_user("Dan", "Danny");
        server.send_line(":irc.srv 401 GM Dan :No such nick/channel\r\n");
        assert_eq!(
            server.recv_sent().await.unwrap(),
            Command::INVITE("Danny".into(), "#vault".into())
        );
        server.join_user("Danny", "#vault");
        server.settle().await;
    };
    let (res_cross, ()) = tokio::join!(wait_cross, drive_cross);
    res_cross.unwrap();
    assert!(dan.is_online());
    assert!(vault.has_user(&dan));

    // 5. Cross-in-flight rename on `invite_available`: `Eve` renames to `Evelyn`, `401 Eve` triggers
    //    re-invite of `Evelyn`, and `Evelyn` joins cleanly:
    let avail_cross = vault.invite_available([&eve], Duration::from_secs(5));
    let drive_avail = async {
        assert_eq!(
            server.recv_sent().await.unwrap(),
            Command::INVITE("Eve".into(), "#vault".into())
        );
        server.rename_user("Eve", "Evelyn");
        server.send_line(":irc.srv 401 GM Eve :No such nick/channel\r\n");
        assert_eq!(
            server.recv_sent().await.unwrap(),
            Command::INVITE("Evelyn".into(), "#vault".into())
        );
        server.join_user("Evelyn", "#vault");
        server.settle().await;
    };
    let (joined_avail, ()) = tokio::join!(avail_cross, drive_avail);
    assert_eq!(joined_avail.unwrap(), vec![eve.clone()]);

    // 6. Early joiner parting before second invitee joins fails `invite_and_wait` with `UserLeft`:
    let p1 = bot.user("Early1").unwrap();
    let p2 = bot.user("Late2").unwrap();
    let wait_early_part = vault.invite_and_wait([&p1, &p2], Duration::from_secs(5));
    let drive_early_part = async {
        let _ = server.recv_sent().await.unwrap();
        let _ = server.recv_sent().await.unwrap();
        server.join_user("Early1", "#vault");
        server.part_user("Early1", "#vault", Some("changed mind"));
        server.join_user("Late2", "#vault");
        server.settle().await;
    };
    let (res_early_part, ()) = tokio::join!(wait_early_part, drive_early_part);
    assert!(matches!(
        res_early_part,
        Err(Error::UserLeft { ref nick, .. }) if nick == "Early1"
    ));

    // 7. Early joiner quitting before handshake finishes is excluded from `invite_available`:
    let q1 = bot.user("EarlyQuit").unwrap();
    let q2 = bot.user("Survivor").unwrap();
    let wait_avail_quit = vault.invite_available([&q1, &q2], Duration::from_secs(5));
    let drive_avail_quit = async {
        let _ = server.recv_sent().await.unwrap();
        let _ = server.recv_sent().await.unwrap();
        server.join_user("EarlyQuit", "#vault");
        server.quit_user("EarlyQuit", Some("lost wifi"));
        server.join_user("Survivor", "#vault");
        server.settle().await;
    };
    let (res_avail_quit, ()) = tokio::join!(wait_avail_quit, drive_avail_quit);
    assert_eq!(res_avail_quit.unwrap(), vec![q2]);
}

#[tokio::test]
async fn breakout_cancelled_mid_join_parts_without_spurious_part_on_join_failed() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#lobby", &["Alice"]);
    server.settle().await;

    let lobby = bot.room("#lobby").unwrap();

    // 1. Cancelling `breakout_with` via `.until(...)` while `JOIN` is in flight before `366`
    //    automatically enqueues `PART` so the bot never leaks an orphaned channel:
    let cancel_trigger = lobby.inbox().on_cmd("!abort");
    let setup_task = tokio::spawn({
        let bot = bot.clone();
        async move {
            bot.breakout_with(BreakoutOptions::exact("#half-open"))
                .until(cancel_trigger)
                .await
        }
    });

    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::JOIN("#half-open".into(), None, None)
    );
    server.chat("Alice", "#lobby", "!abort");
    let out = setup_task.await.unwrap();
    assert!(matches!(out, Outcome::Stopped(_)));
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::PART("#half-open".into(), None)
    );

    // 2. If the server explicitly rejects `JOIN` (`473 ERR_INVITEONLYCHAN`), the guard disarms
    //    without emitting a spurious `PART`:
    let rejected_task = tokio::spawn({
        let bot = bot.clone();
        async move { bot.breakout_with(BreakoutOptions::exact("#locked")).await }
    });
    assert_eq!(
        server.recv_sent().await.unwrap(),
        Command::JOIN("#locked".into(), None, None)
    );
    server.send_line(":irc.srv 473 GM #locked :Cannot join channel (+i)\r\n");
    server.settle().await;
    let err = rejected_task.await.unwrap().unwrap_err();
    assert!(matches!(err, Error::JoinFailed { code: 473, .. }));
    assert!(server.try_recv_sent().is_none());
}
