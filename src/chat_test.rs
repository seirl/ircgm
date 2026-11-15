use std::time::Duration;

use crate::prelude::*;
use crate::testing::test_harness;
use crate::{BreakoutOptions, Target};

/// Reusable single-answer helper built on [`Chat`] + [`ask`].
async fn confirm(mut chat: impl Chat, question: &str) -> Result<Option<bool>> {
    Ok(ask(&mut chat, question, |m| {
        if m.is_cmd("!yes") {
            AskMatch::accept_say(true, "Confirmed!")
        } else if m.is_cmd("!no") {
            AskMatch::accept(false)
        } else {
            AskMatch::ignore()
        }
    })
    .config(AskConfig::new().reply_timeout(Duration::from_secs(10)))
    .await?
    .ok())
}

/// Reusable multi-player helper built on [`GroupChat`] + [`ask_many`].
async fn ready_roll_call(mut group: impl GroupChat, players: &[User]) -> Result<Vec<User>> {
    group.say("Starting ready check...")?;
    let answers = ask_many(&mut group, "Type `!ready`:", |m| {
        m.is_cmd("!ready").then_some(())
    })
    .from_users(players)
    .config(AskConfig::new().reply_timeout(Duration::from_secs(10)))
    .await?;
    Ok(answers.into_iter().map(|(u, ())| u).collect())
}

/// Downstream wrapper implementing [`Chat`] and [`GroupChat`] with zero closure generics.
struct TaggedGroup<G> {
    tag: String,
    inner: G,
}

impl<G: Chat> Chat for TaggedGroup<G> {
    fn say(&self, text: impl Into<Text>) -> Result<()> {
        self.inner.say(irc_format!("[{}] {}", self.tag, text.into()))
    }
    fn notice(&self, text: impl Into<Text>) -> Result<()> {
        self.inner
            .notice(irc_format!("[{}] {}", self.tag, text.into()))
    }
    fn action(&self, text: impl Into<Text>) -> Result<()> {
        self.inner.action(text)
    }
    #[track_caller]
    fn inbox(&mut self) -> InboxRef<'_> {
        self.inner.inbox()
    }
    fn target(&self) -> Target {
        self.inner.target()
    }
    fn participants(&self) -> Option<Vec<User>> {
        self.inner.participants()
    }
}

impl<G: GroupChat> GroupChat for TaggedGroup<G> {
    fn has_user(&self, user: &User) -> bool {
        self.inner.has_user(user)
    }
    #[track_caller]
    fn inbox_users(&mut self, users: &[User]) -> InboxRef<'_> {
        self.inner.inbox_users(users)
    }
}

#[tokio::test(start_paused = true)]
async fn free_ask_and_ask_many_work_across_refs_handles_and_custom_wrappers() {
    let (bot, mut server) = test_harness("gm");
    server.complete_join("#town", &["alice", "bob"]);
    server.settle().await;

    let room = bot.room("#town").unwrap();
    let alice = bot.user("alice").unwrap();
    let bob = bot.user("bob").unwrap();

    // 1. `&Room` passed directly (no `.clone()` or `mut`!) to `confirm(impl Chat)`:
    let r_confirm = tokio::spawn({
        let room = room.clone();
        async move { confirm(&room, "Start game?").await }
    });
    assert_eq!(server.recv_message_to("#town").await, "Start game?");
    server.chat("alice", "#town", "!yes");
    server.settle().await;
    assert_eq!(r_confirm.await.unwrap().unwrap(), Some(true));
    assert_eq!(server.take_messages_to("#town"), vec!["Confirmed!"]);

    // 2. `&mut ExclusiveDm` passed directly to `confirm(impl Chat)`:
    let mut dm = bob.exclusive().await.unwrap();
    let dm_drive = async {
        assert_eq!(server.recv_message_to("bob").await, "Stay?");
        server.dm("bob", "!no");
        server.settle().await;
    };
    let (dm_res, ()) = tokio::join!(confirm(&mut dm, "Stay?"), dm_drive);
    assert_eq!(dm_res.unwrap(), Some(false));
    drop(dm);

    // 3. `ready_roll_call(impl GroupChat)` works with `&Room` directly (no `.clone()`!),
    //    `TaggedGroup<&Room>`, `&mut Huddle`, and `vault.room()`:
    let players = vec![alice.clone(), bob.clone()];

    let room_task = tokio::spawn({
        let room = room.clone();
        let players = players.clone();
        async move { ready_roll_call(&room, &players).await }
    });
    assert_eq!(
        server.recv_message_to("#town").await,
        "Starting ready check..."
    );
    assert_eq!(server.recv_message_to("#town").await, "Type `!ready`:");
    server.chat("alice", "#town", "!ready");
    server.chat("bob", "#town", "!ready");
    server.settle().await;
    assert_eq!(room_task.await.unwrap().unwrap(), players);

    let tagged_task = tokio::spawn({
        let room = room.clone();
        let players = players.clone();
        async move {
            let tagged = TaggedGroup {
                tag: "R1".into(),
                inner: &room,
            };
            ready_roll_call(tagged, &players).await
        }
    });
    assert_eq!(
        server.recv_message_to("#town").await,
        "[R1] Starting ready check..."
    );
    assert_eq!(server.recv_message_to("#town").await, "[R1] Type `!ready`:");
    server.chat("alice", "#town", "!ready");
    server.chat("bob", "#town", "!ready");
    server.settle().await;
    assert_eq!(tagged_task.await.unwrap().unwrap(), players);

    let mut huddle = bot.huddle("Ready", &players).await.unwrap();
    let huddle_drive = async {
        assert_eq!(
            server.recv_message_to("alice").await,
            "Starting ready check..."
        );
        assert_eq!(
            server.recv_message_to("bob").await,
            "Starting ready check..."
        );
        assert_eq!(server.recv_message_to("alice").await, "Type `!ready`:");
        assert_eq!(server.recv_message_to("bob").await, "Type `!ready`:");
        server.dm("alice", "!ready");
        server.dm("bob", "!ready");
        server.settle().await;
    };
    let (huddle_ready, ()) = tokio::join!(ready_roll_call(&mut huddle, &players), huddle_drive);
    assert_eq!(huddle_ready.unwrap(), players);
    drop(huddle);

    let breakout_setup = tokio::spawn({
        let bot = bot.clone();
        let players = players.clone();
        async move {
            let vault = bot
                .breakout_with(BreakoutOptions::exact("#vault"))
                .await
                .unwrap();
            vault
                .invite_and_wait(&players, Duration::from_secs(5))
                .await
                .unwrap();
            // `vault.room()` borrows `&Room` (`impl GroupChat`) without moving `Breakout`:
            let ready = ready_roll_call(vault.room(), &players).await.unwrap();
            (vault, ready)
        }
    });
    let chan = server
        .accept_breakout_invite("#vault", &["alice", "bob"])
        .await;
    assert_eq!(chan, "#vault");
    assert_eq!(
        server.recv_message_to("#vault").await,
        "Starting ready check..."
    );
    assert_eq!(server.recv_message_to("#vault").await, "Type `!ready`:");
    server.chat("alice", "#vault", "!ready");
    server.chat("bob", "#vault", "!ready");
    server.settle().await;
    let (_vault, vault_ready) = breakout_setup.await.unwrap();
    assert_eq!(vault_ready, players);
}
