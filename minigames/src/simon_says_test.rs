use std::time::Duration;

use irc::proto::Command;
use ircgm::testing::{test_harness, TestServer};

use super::{run_simon_says, Directive, Medium};

async fn accept_next_bunker_join(srv: &mut TestServer) -> String {
    loop {
        if let Some(Command::JOIN(chan, _, _)) = srv.recv_sent().await {
            if chan.starts_with("#bunker-") {
                srv.complete_join(&chan, &[]);
                return chan;
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn simon_says_runs_hud_notice_action_notice_rename_fakeout_and_bunker_rush_finale() {
    let (bot, mut srv) = test_harness("GM");
    srv.complete_join("#lobby", &["Alice", "Bob", "Cara", "Dan", "Eve"]);
    srv.settle().await;

    let lobby = bot.room("#lobby").unwrap();
    let alice = bot.user("Alice").unwrap();
    let bob = bot.user("Bob").unwrap();
    let cara = bot.user("Cara").unwrap();
    let dan = bot.user("Dan").unwrap();
    let eve = bot.user("Eve").unwrap();
    let players = vec![alice.clone(), bob.clone(), cara.clone(), dan, eve];

    let directives = vec![
        Directive {
            simon_says: true,
            medium: Medium::Action("salutes"),
        },
        Directive {
            simon_says: true,
            medium: Medium::Notice("ready"),
        },
        Directive {
            simon_says: true,
            medium: Medium::NickPrefix("spy_"),
        },
        Directive {
            simon_says: false,
            medium: Medium::Privmsg("oops"),
        },
        Directive {
            simon_says: true,
            medium: Medium::BunkerRush,
        },
    ];

    let game = tokio::spawn({
        let bot = bot.clone();
        let lobby = lobby.clone();
        let players = players.clone();
        async move { run_simon_says(&bot, &lobby, &players, &directives, Duration::from_secs(5)).await }
    });

    for _ in 0..5 {
        tokio::task::yield_now().await;
    }

    // Verify outbound CTCP `/me` stage direction and inline HUD `NOTICE` went out:
    let mut saw_hud_notice = false;
    let mut saw_stage_action = false;
    while let Some(cmd) = srv.try_recv_sent() {
        match cmd {
            Command::NOTICE(target, text) if target == "Alice" && text.contains("[HUD]") => {
                saw_hud_notice = true;
            }
            Command::PRIVMSG(target, text)
                if target == "#lobby" && text.starts_with("\x01ACTION ") =>
            {
                saw_stage_action = true;
            }
            _ => {}
        }
    }
    assert!(saw_hud_notice && saw_stage_action);

    // Round 1 (`Action`): Alice, Bob, Cara, Eve do `/me salutes`; Dan sends plain text (eliminated!):
    srv.send_line(":Alice!~Alice@test.host PRIVMSG #lobby :\x01ACTION salutes\x01\r\n");
    srv.send_line(":Bob!~Bob@test.host PRIVMSG #lobby :\x01ACTION salutes\x01\r\n");
    srv.send_line(":Cara!~Cara@test.host PRIVMSG #lobby :\x01ACTION salutes\x01\r\n");
    srv.send_line(":Eve!~Eve@test.host PRIVMSG #lobby :\x01ACTION salutes\x01\r\n");
    srv.chat("Dan", "#lobby", "salutes");
    tokio::time::advance(Duration::from_secs(5)).await;
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }

    // Round 2 (`Notice`): Alice, Bob, Cara, Eve all send channel NOTICE -> advances early!
    srv.send_line(":Alice!~Alice@test.host NOTICE #lobby :ready\r\n");
    srv.send_line(":Bob!~Bob@test.host NOTICE #lobby :ready\r\n");
    srv.send_line(":Cara!~Cara@test.host NOTICE #lobby :ready\r\n");
    srv.send_line(":Eve!~Eve@test.host NOTICE #lobby :ready\r\n");
    srv.settle().await;
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }

    // Round 3 (`NickPrefix`): All 4 rename to `spy_*`:
    srv.send_line(":Alice!~Alice@test.host NICK :spy_alice\r\n");
    srv.send_line(":Bob!~Bob@test.host NICK :spy_bob\r\n");
    srv.send_line(":Cara!~Cara@test.host NICK :spy_cara\r\n");
    srv.send_line(":Eve!~Eve@test.host NICK :spy_eve\r\n");
    srv.settle().await;
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }

    // Round 4 (Fakeout without "Simon says"): `spy_eve` speaks and is eliminated! (`3` remain!)
    srv.chat("spy_eve", "#lobby", "oops");
    tokio::time::advance(Duration::from_secs(5)).await;

    // Round 5 (`BunkerRush`, 3 survivors -> 2 seats!):
    let bunker = accept_next_bunker_join(&mut srv).await;
    srv.settle().await;
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }

    // `spy_bob` joins (1/2) then foolishly PARTs (0/2); `spy_alice` and `spy_cara` claim the 2 seats!
    srv.send_line(&format!(":spy_bob!~Bob@test.host JOIN {bunker}\r\n"));
    srv.send_line(&format!(":spy_bob!~Bob@test.host PART {bunker} :wait\r\n"));
    srv.send_line(&format!(":spy_alice!~Alice@test.host JOIN {bunker}\r\n"));
    srv.send_line(&format!(":spy_cara!~Cara@test.host JOIN {bunker}\r\n"));

    let survivors = game.await.unwrap().unwrap();
    assert_eq!(survivors.len(), 2);
    assert_eq!(survivors[0].id(), alice.id());
    assert_eq!(survivors[0].nick(), "spy_alice");
    assert_eq!(survivors[1].id(), cara.id());
    assert_eq!(survivors[1].nick(), "spy_cara");
}
