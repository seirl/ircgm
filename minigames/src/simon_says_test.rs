use std::time::Duration;

use irc::proto::Command;
use ircgm::testing::test_harness;

use super::{run_simon_says, Directive, Medium};

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

    srv.flush().await;

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
    for nick in ["Alice", "Bob", "Cara", "Eve"] {
        srv.action(nick, "#lobby", "salutes");
    }
    srv.chat("Dan", "#lobby", "salutes");
    tokio::time::advance(Duration::from_secs(5)).await;
    srv.flush().await;

    // Round 2 (`Notice`): Alice, Bob, Cara, Eve all send channel NOTICE -> advances early!
    for nick in ["Alice", "Bob", "Cara", "Eve"] {
        srv.notice(nick, "#lobby", "ready");
    }
    srv.settle().await;

    // Round 3 (`NickPrefix`): All 4 rename to `spy_*`:
    for (old, new) in [
        ("Alice", "spy_alice"),
        ("Bob", "spy_bob"),
        ("Cara", "spy_cara"),
        ("Eve", "spy_eve"),
    ] {
        srv.rename_user(old, new);
    }
    srv.settle().await;

    // Round 4 (Fakeout without "Simon says"): `spy_eve` speaks and is eliminated! (`3` remain!)
    srv.chat("spy_eve", "#lobby", "oops");
    tokio::time::advance(Duration::from_secs(5)).await;

    // Round 5 (`BunkerRush`, 3 survivors -> 2 seats!):
    let bunker = srv.accept_breakout("#lobby-bunker", &[]).await;

    // `spy_bob` joins (1/2) then foolishly PARTs (0/2); `spy_alice` and `spy_cara` claim the 2 seats!
    srv.join_user("spy_bob", &bunker);
    srv.part_user("spy_bob", &bunker, Some("wait"));
    srv.join_user("spy_alice", &bunker);
    srv.join_user("spy_cara", &bunker);

    let survivors = game.await.unwrap().unwrap();
    assert_eq!(survivors.len(), 2);
    assert_eq!(survivors[0].id(), alice.id());
    assert_eq!(survivors[0].nick(), "spy_alice");
    assert_eq!(survivors[1].id(), cara.id());
    assert_eq!(survivors[1].nick(), "spy_cara");
}
