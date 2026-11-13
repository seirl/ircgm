use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, UNIX_EPOCH};

use ircgm::prelude::*;
use ircgm::testing::test_harness;
use ircgm::CaseMap;

#[tokio::test]
async fn single_batch_privmsg_rename_steal_quit_preserves_stamped_identity() {
    let (bot, mut server) = test_harness("GM");
    server.complete_join("#r", &["Alice", "Mallory"]);
    server.settle().await;
    let _ = server.take_messages_to("#r");

    let room = bot.room("#r").unwrap();
    let alice = bot.user("Alice").unwrap();
    let mallory = bot.user("Mallory").unwrap();
    assert_ne!(alice, mallory);

    let mut all_in = bot.inbox();

    let ask_room = room.clone();
    let ask_alice = alice.clone();
    let ask_task = tokio::spawn(async move {
        ask_room
            .ask_user(&ask_alice, "Your move?", |m| {
                (m.text.trim() == "hit").then(|| m.clone())
            })
            .await
    });
    tokio::task::yield_now().await;
    assert_eq!(server.take_messages_to("#r"), vec!["Your move?"]);

    // Single batch: Alice answers "hit", renames to Alice2, Mallory steals "Alice", Alice2 quits!
    server.send_line(":Alice!u@h PRIVMSG #r :hit\r\n");
    server.send_line(":Alice!u@h NICK :Alice2\r\n");
    server.send_line(":Mallory!m@h NICK :Alice\r\n");
    server.send_line(":Alice2!u@h QUIT :bye\r\n");
    server.settle().await;

    let hit_msg = ask_task.await.unwrap().unwrap().unwrap();
    assert_eq!(hit_msg.text, "hit");
    assert_eq!(hit_msg.nick, "Alice");
    assert_eq!(hit_msg.sender, alice);
    assert_eq!(hit_msg.sender.nick(), "Alice2");

    let current_alice = bot.user("Alice").unwrap();
    assert_eq!(current_alice, mallory);
    assert_ne!(hit_msg.sender, current_alice);

    let first_chat = all_in.next_chat().await.unwrap();
    assert_eq!(first_chat.sender, alice);
    assert_ne!(first_chat.sender, current_alice);
}

#[tokio::test]
async fn event_fields_server_time_ident_and_channels() {
    let (bot, server) = test_harness("GM");
    server.complete_join("#arena", &["@GM"]);
    server.settle().await;

    let mut inbox = bot.inbox();
    let before = tokio::time::Instant::now();

    server.send_line(":Cara!~cara@cloak.example JOIN #arena * :Cara\r\n");
    server.send_line(
        "@time=2026-05-01T12:30:00.250Z :Cara!~cara@cloak.example PRIVMSG #arena :hello\r\n",
    );
    server.send_line(":Cara!~cara@cloak.example NICK :Cara_afk\r\n");
    server.send_line(":GM!~GM@test.host KICK #arena Cara_afk :idle\r\n");
    server.send_line(":Dan!~dan@host.example JOIN #arena\r\n");
    server.send_line(":Dan!~dan@host.example PART #arena :brb\r\n");
    server.settle().await;

    let cara = match inbox.next_event().await.unwrap() {
        Event::Joined {
            channel,
            user,
            nick,
            ident,
            host,
        } => {
            assert_eq!(channel, "#arena");
            assert_eq!(nick, "Cara");
            assert_eq!(ident.as_deref(), Some("~cara"));
            assert_eq!(host.as_deref(), Some("cloak.example"));
            user
        }
        other => panic!("expected Joined, got {other:?}"),
    };

    match inbox.next_event().await.unwrap() {
        Event::Chat(msg) => {
            assert_eq!(msg.sender, cara);
            assert_eq!(msg.ident.as_deref(), Some("~cara"));
            assert_eq!(msg.host.as_deref(), Some("cloak.example"));
            assert!(msg.received_at >= before);
            let ts = msg.server_time.expect("parsed @time");
            let dur = ts.duration_since(UNIX_EPOCH).unwrap();
            assert_eq!(dur.subsec_millis(), 250);
        }
        other => panic!("expected Chat, got {other:?}"),
    }

    match inbox.next_event().await.unwrap() {
        Event::NickChanged {
            user,
            old_nick,
            new_nick,
            channels,
        } => {
            assert_eq!(user, cara);
            assert_eq!(old_nick, "Cara");
            assert_eq!(new_nick, "Cara_afk");
            assert_eq!(channels, vec!["#arena".to_owned()]);
        }
        other => panic!("expected NickChanged, got {other:?}"),
    }

    match inbox.next_event().await.unwrap() {
        Event::Kicked {
            channel,
            user,
            nick,
            by,
            reason,
        } => {
            assert_eq!(channel, "#arena");
            assert_eq!(user, cara);
            assert_eq!(nick, "Cara_afk");
            assert_eq!(by, "GM");
            assert_eq!(reason.as_deref(), Some("idle"));
        }
        other => panic!("expected Kicked, got {other:?}"),
    }

    let dan = match inbox.next_event().await.unwrap() {
        Event::Joined { user, nick, .. } => {
            assert_eq!(nick, "Dan");
            user
        }
        other => panic!("expected Dan Joined, got {other:?}"),
    };
    match inbox.next_event().await.unwrap() {
        Event::Parted {
            channel,
            user,
            nick,
            reason,
        } => {
            assert_eq!(channel, "#arena");
            assert_eq!(user, dan);
            assert_eq!(nick, "Dan");
            assert_eq!(reason.as_deref(), Some("brb"));
        }
        other => panic!("expected Dan Parted, got {other:?}"),
    }
}

#[tokio::test]
async fn user_id_keys_and_consuming_inbox_combinators() {
    let (bot, server) = test_harness("GM");
    server.complete_join("#lobby", &["[Alice]", "Bob"]);
    server.settle().await;

    let room = bot.room("#lobby").unwrap();
    let alice_1 = bot.user("[Alice]").unwrap();
    let alice_folded = bot.user("{alice}").unwrap();
    let bob = bot.user("Bob").unwrap();

    let alice_id: UserId = alice_1.id();
    assert_eq!(alice_id, alice_folded.id());
    assert_ne!(alice_id, bob.id());
    assert!(alice_id.as_u64() > 0);
    assert_eq!(alice_id.to_string(), alice_id.as_u64().to_string());

    let mut scores: HashMap<UserId, usize> = HashMap::new();
    scores.insert(alice_id, 10);
    let mut seen: HashSet<UserId> = HashSet::new();
    assert!(seen.insert(alice_id));
    let mut ordered: BTreeSet<UserId> = BTreeSet::new();
    ordered.insert(bob.id());
    ordered.insert(alice_id);
    assert_eq!(ordered.len(), 2);

    server.send_line(":[Alice]!u@h NICK :AliceRenamed\r\n");
    server.settle().await;
    assert_eq!(bot.user("AliceRenamed").unwrap().id(), alice_id);
    assert_eq!(scores[&alice_1.id()], 10);

    server.quit_user("AliceRenamed", Some("reconnecting"));
    server.send_line(":AliceRenamed!u@h JOIN #lobby\r\n");
    server.settle().await;
    let alice_rejoined = bot.user("AliceRenamed").unwrap();
    assert_ne!(alice_rejoined.id(), alice_id);
    assert!(seen.insert(alice_rejoined.id()));

    let stop_fut = bot.inbox().on_cmd("!stop");
    let abort_fut = bob.inbox().on_cmd("!abort");

    let work = async move {
        let first = room.inbox().next_chat().await?;
        let mapped = room
            .inbox()
            .wait_map(|m| m.cmd_args("!pick").map(str::to_owned))
            .await?;
        Ok::<_, Error>((first.text, mapped))
    };

    let race_task = tokio::spawn(async move { work.until(stop_fut).or_until(abort_fut).await });
    tokio::task::yield_now().await;

    server.dm("Bob", "!abort");
    server.settle().await;

    match race_task.await.unwrap() {
        Outcome::Stopped(msg) => assert_eq!(msg.text, "!abort"),
        other => panic!("expected Outcome::Stopped, got {other:?}"),
    }
}

#[tokio::test]
async fn dynamic_casemapping_rekey_rfc1459_netsplit_and_accounts() {
    let (bot, server) = test_harness("GM");
    server.complete_join("#[Lobby]", &["[Alice]"]);
    server.settle().await;

    assert_eq!(bot.casemap(), CaseMap::Rfc1459);

    let u1 = bot.user("[Alice]").unwrap();
    let u2 = bot.user("{alice}").unwrap();
    assert_eq!(u1, u2);
    assert!(u1.eq_nick("{alice}"));

    let room_brace = bot.room("#{lobby}").unwrap();
    let mut room_in = room_brace.inbox();

    server.send_line(":irc.srv 005 GM CASEMAPPING=ascii :are supported\r\n");
    server.settle().await;
    assert_eq!(bot.casemap(), CaseMap::Ascii);
    assert!(u1.eq_nick("[alice]"));
    assert!(!u1.eq_nick("{alice}"));

    assert_eq!(bot.user("[alice]").unwrap(), u1);
    let u_curly = bot.user("{alice}").unwrap();
    assert_ne!(u1, u_curly);

    let room_bracket = bot.room("#[lobby]").unwrap();
    assert!(room_bracket.has_user(&u1));
    let mut bracket_in = room_bracket.inbox();
    server.chat("[ALICE]", "#[LOBBY]", "ascii_works");
    server.settle().await;
    let m = bracket_in.next_chat().await.unwrap();
    assert_eq!(m.sender, u1);
    assert_eq!(m.text, "ascii_works");

    let no_match = room_in
        .next_chat()
        .with_timeout(Duration::from_millis(10))
        .await
        .unwrap_err();
    assert!(matches!(no_match, Error::TimedOut));

    let mut all_events = bot.inbox();
    server.send_line(":Bob!~b@h JOIN #[lobby] bob_acct :Bob Real\r\n");
    server.settle().await;
    let bob = bot.user("Bob").unwrap();
    assert_eq!(bob.account().as_deref(), Some("bob_acct"));

    server.send_line(":Bob!~b@h ACCOUNT *\r\n");
    server.settle().await;
    assert_eq!(bob.account(), None);

    server.send_line("@account=bob_v2 :Bob!~b@h PRIVMSG #[lobby] :reauthed\r\n");
    server.settle().await;
    assert_eq!(bob.account().as_deref(), Some("bob_v2"));

    all_events.clear();
    server.quit_user("Bob", Some("hub.irc.org leaf.irc.org"));
    server.quit_user("[Alice]", Some("Client Quit"));
    server.settle().await;

    let ev1 = all_events.next_event().await.unwrap();
    let ev2 = all_events.next_event().await.unwrap();
    assert!(matches!(ev1, Event::Quit { netsplit: true, ref nick, .. } if nick == "Bob"));
    assert!(matches!(ev2, Event::Quit { netsplit: false, ref nick, .. } if nick == "[Alice]"));

    server.send_line(":Bob!~b@h JOIN #[lobby] * :Bob\r\n");
    server.settle().await;
    let bob_rejoined = bot.user("Bob").unwrap();
    assert_ne!(bob, bob_rejoined);
    assert!(!bob.is_online());
    assert!(bob_rejoined.is_online());
}
