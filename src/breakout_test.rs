use irc::proto::Command;

use super::*;
use crate::testing::test_harness;

#[test]
fn breakout_naming_generates_six_char_lowercase_alnum_or_exact() {
    let random = BreakoutNaming::RandomSuffix {
        prefix: "heist".into(),
    }
    .generate_channel_name();
    assert!(random.starts_with("#heist-"));
    let suffix = random.strip_prefix("#heist-").unwrap();
    assert_eq!(suffix.len(), 6);
    assert!(suffix
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));

    let already_prefixed = BreakoutNaming::RandomSuffix {
        prefix: "&local".into(),
    }
    .generate_channel_name();
    assert!(already_prefixed.starts_with("&local-"));
    assert_eq!(already_prefixed.len(), "&local-".len() + 6);

    let exact = BreakoutNaming::Exact("#mafia-wolves".into()).generate_channel_name();
    assert_eq!(exact, "#mafia-wolves");
}

#[tokio::test]
async fn build_teardown_commands_honors_kick_on_drop_and_targmax_kick() {
    let (bot, mut server) = test_harness("gm");
    server.send_line(":srv 005 gm TARGMAX=KICK:2 :are supported\r\n");

    let br_task = tokio::spawn({
        let bot = bot.clone();
        async move {
            bot.breakout_with(
                BreakoutOptions::exact("#vault")
                    .modes(None::<String>)
                    .kick_reason("Vault sealed"),
            )
            .await
        }
    });
    assert!(matches!(server.recv_sent().await, Some(Command::JOIN(ch, _, _)) if ch == "#vault"));
    server.complete_join("#vault", &["alice", "bob", "carol"]);
    server.settle().await;

    let mut br = br_task.await.unwrap().unwrap();
    let (cmds, _) = br.build_teardown_commands();
    assert_eq!(
        cmds,
        vec![
            Command::KICK(
                "#vault".into(),
                "alice,bob".into(),
                Some("Vault sealed".into())
            ),
            Command::KICK("#vault".into(), "carol".into(), Some("Vault sealed".into())),
            Command::PART("#vault".into(), None),
        ]
    );

    // Disarm so `Drop` doesn't enqueue duplicate commands, then check `kick_on_drop(false)`:
    br.armed = false;
    let br_no_kick = Breakout {
        room: br.to_room(),
        opts: BreakoutOptions::exact("#vault").kick_on_drop(false),
        armed: false,
    };
    let (no_kick_cmds, _) = br_no_kick.build_teardown_commands();
    assert_eq!(no_kick_cmds, vec![Command::PART("#vault".into(), None)]);
}
