# `ircgm` design review, round 2: capability gaps

*Hand-off for the implementing agent. This is a follow-up to round 1 (the decision list, implementation items, deferred list, and rejected list). **Everything in round 1 still stands.** This round asks: are there games or use cases that are impossible, or badly served, because a core API is missing or wrong? It merges two independent gap analyses. Duplicates are collapsed, and suggestions that don't hold up are listed in §5 so they aren't re-litigated.*

---

## 0. Verdict

Nothing here invalidates the core model (pump, `Inbox`, direct-style futures). The gaps are in the surface around it. Three things deserve attention before the public API freezes:

1. A single **multi-source inbox** primitive, which fixes a fairness bug in the Quiz example and unlocks several game types.
2. The **`Error` shape for reconnects**, which is hard to retrofit.
3. The **matcher type** on `Ask`/`wait_map`, which is public and currently too thin.

Moderation APIs (§2) are also missing, but they are additive and cheap.

---

## 1. Decide before freezing the public API

### 1.1 Multi-source inbox (fixes a fairness bug, unlocks four use cases)

**Problem A: unfair races.** The Quiz example gives each player their own `ask` (own inbox) and races them with `select_all`. The pump delivers in wire order, but `select_all` polls futures in `Vec` order. If Bob's correct answer arrives first but both land in the same TCP batch (exactly the case the spec is built around), Alice wins if she is earlier in the vector. The window is small, but "first correct answer wins" is the one place fairness matters. *Verify this with a test first: inject two correct answers in one batch, with Bob first on the wire and Alice first in the vector, and check who wins.*

**Problem B: no way to listen to several rooms or users in one wait.** `Filter` is limited to `All`, `Room`, `UserDm`, and `UserInRoom`. A game spanning `#red-base` and `#blue-base` (capture the flag, cross-channel team games) has to juggle two inboxes in a `select!` and loses the single sequential `wait_map`. A bot-mediated group chat (wolf-team chat on networks where the bot can't create channels or get `+o`, which `Breakout` assumes) has the same problem across a set of users' DMs.

**Required change.** Keep `Inbox` a single concrete type (no scope type parameters), but let its internal `Filter` be a set-based spec, constructed through a public builder, for example:
- [x] `bot.inbox_for(InboxScope::new().rooms([&a, &b]).dms_from([&u1, &u2, &u3]))`
- [x] Convenience: `bot.users_inbox(&[User])` (DM scope) and `bot.rooms_inbox(&[Room])`.

Because one `mpsc` feeds the inbox, **wire order across all sources is preserved for free**, and `ChatMessage` already carries the target (channel or DM) and a stamped `sender()`, so callers can tell which source a message came from. Departure semantics need a decision: for a set of users, one member quitting should surface as an in-band event or non-fatal per-member outcome, not end the whole inbox. Decide this when you specify the filter.

**Related helper.**
- [x] Expose `msg.seq` publicly *(accepted; `bot.ask_many` declined in favor of `exclusive_many` + `users_inbox`, see §6)*, and consider a broadcast-prompt primitive, `bot.ask_many(&users, prompt, matcher)`, that does the following:
1. Acquires DM slots via `exclusive_many` (round 1 §2.2).
2. Subscribes one multi-user inbox.
3. Sends all prompts.
4. Returns the first match together with *which* user won.

Per-user queue-fair timers are the open design question here. Each user's clock starts after their own prompt is enqueued, so the shared deadline needs a defined rule (for example, the latest enqueue time).

### 1.2 Reconnect and long-game semantics (decide the error shape now, build resume later)

**Problem.** Any disconnect fails every `Inbox`, `Ask`, and `ExclusiveDm` with `Disconnected`, and the stack frames that *are* the game die with it. That is fine for a 3-minute quiz. For a 45-minute Mafia game, one ping timeout ends it, and real users will hit this. The spec files it under non-goals, but the cost is understated.

**Required for v1 (cheap, hard to retrofit):**
- [x] Give `Disconnected` enough structure (`Error::Disconnected(DisconnectReason)` + `#[non_exhaustive]`) to distinguish session-ending causes *(no dead unconstructed non-fatal variant in v1; `#[non_exhaustive]` preserves future `SessionInterrupted`, see §6)*. Don't encode "connection gone" as a single opaque fatal error if you may want "interrupted, session resumed" later.
- [x] Add a `session_id` or generation on `Bot` (`bot.session_id() -> u64`) so higher layers can tell which connection a handle belongs to.
- [x] State the scope plainly in the spec: v1 games are bounded by connection lifetime.

**Post-v1 (design for it, don't build it yet):** an internal `ReconnectPolicy::Resume` that re-registers, rejoins tracked channels, keeps existing `Inbox`es alive, and surfaces a `Reconnected { gap }` event, with in-flight `Ask`s failing non-fatally so games can re-prompt.

**Documentation (recommended now):**
- [x] A "checkpoint-friendly" style guide. Keep an explicit serializable state struct, store durable state (scores, roles, phase) in a database, and treat each *phase* function as re-enterable from that state. Direct-style code then gives readability *within* a phase without trapping durable state in stack frames *across* phases. Multi-day play-by-post games (correspondence Diplomacy, async chess) are out of scope for stack-frame state. Say so explicitly. Do **not** attempt to serialize futures.

### 1.3 Matcher type: tri-state results, plus out-of-band commands during exclusive DMs

**Problem 1: no rejection feedback.** `wait_map`/`ask` matchers are sync `FnMut(&ChatMessage) -> Option<T>`. Rejection feedback ("that's not a word", "pick 1 to 3") can only be a side effect inside the matcher. That works by accident, because `say` is sync, but it isn't a designed path.

**Problem 2: free-text prompts swallow commands.** Inboxes fan out, so a global `!cancel` or `!help` is also seen by an active `ExclusiveDm`. With strict matchers (`"Warrior"` / `"Mage"`), `!help` is silently ignored and the user gets no feedback. With free-text reads (`dm.inbox().next_chat()` in the loadout example), `!cancel` is *accepted as an item name*.

**Clarification.** A global listener does not need the DM mutex to *hear or answer* these commands. It can use `bot.dm_inbox()` or `user.inbox()` (unserialized) and reply with `user.say(..)` (sync, no lock). The lock only matters for *asking*. So the "out-of-band commands are impossible" framing is wrong, but the two problems above are real.

**Required changes:**
- [x] Make the `Ask` matcher return a tri-state (`Match<T> = Accept(T) | Reject(Text) | Ignore`) via `MatchOutcome`, keeping `Option<T>`-returning convenience so simple cases stay terse *(kept on `Ask` only so `Inbox` stays a pure read-only listener; see §6)*.
- [x] Add an opt-in `Inbox`/`Ask` modifier that skips configured command-prefixed messages (`.ignore_commands()`, with `BotConfig::command_prefixes` defaulting to `["!"]`), evaluated *before* the matcher so free-text reads don't consume `!cancel` and `Reject` doesn't fire on ignored commands.
- [x] Document the cancel pattern: `exclusive_flow.until(user.inbox().wait_cmd("!cancel"))`. Dropping the future releases the mutex via RAII.
- [x] Optional, lower priority (deferred post-v1): an async-matcher variant (`ask_with`) so a dictionary or DB check can happen inside the wait without losing `on_timeout`, `TimeoutCtx`, and the shared timeout budget. Decide the sync-matcher contract now even if async comes later.

---

## 2. Missing capabilities (additive, do early)

### 2.1 Channel moderation and membership (blocks Mafia and similar)

`Room` has `say`, `invite`, and `part`, and nothing else for managing a channel. Use cases that need more:
- **Mafia day/night:** set `+m`, voice only living players, devoice the dead.
- **Russian roulette:** kick the loser.
- **Topic as scoreboard or status line.**

Also, `occupants()` returns `Vec<String>`, which loses `User` identity even though `WorldState` already tracks `@`/`+` modes.

Add:
- [x] `room.members() -> Vec<Member>`, where `Member` has a `User` plus status modes (`is_op`, `is_voiced`, `prefixes`). Keep `occupants()` as a convenience.
- [x] Synchronous `room.kick(&user, reason)`, `room.set_mode(..)`, `room.voice(&user)`, `room.devoice(&user)`, `room.topic()`, `room.set_topic(..)` *(sync-queued to avoid hanging on silent idempotent IRCd `MODE`s; see §6)*.
- [x] `room.bot_is_op()`, with synced pre-check returning `Err(Error::NotChannelOp { channel })` and async `482 ERR_CHANOPRIVSNEEDED` logged via `tracing::warn!`.

Today this is possible via `send_raw`, but with no op check, no confirmation, and no typed errors, so every game would reimplement it badly. Reuse `Breakout::drop`'s KICK machinery.

### 2.2 Smaller additions

- [x] **Countdown warnings during an ask.** Document `tokio::pin!(ask);` + `&mut ask` in `select!`, and add built-in `Ask::warn_at(remaining, msg)` (scheduled relative to the post-enqueue reply clock).
- [x] **Timing metadata.** Request IRCv3 `server-time` by default and expose `msg.server_time: Option<SystemTime>` + `msg.received_at: Instant` (`msg.seq` remains the sole ordering/fairness authority).
- [x] **Account tracking (`user.account()`).** Request IRCv3 `account-notify` alongside `account-tag` and `extended-join`, tracking `user.account() -> Option<String>` live across login/logout *(full `WHOIS` query layer deferred post-v1; see §6)*.
- [x] **Command routing *(declined as library type; documented as direct-style `match` loop — see §6)*.**
- [x] **IRCv3 `TAGMSG`/reactions/`+reply` extensibility.** Deferred for v1, with `#[non_exhaustive]` added on `RoomEvent`, `Error`, and `DisconnectReason`.

---

## 3. Tests to add

- [x] **Fair-race regression:** two correct answers in one batch, wire order Bob then Alice, vector order Alice then Bob. `bot.users_inbox` picks Bob; document what `select_all` does.
- [x] **Multi-source inbox:** messages from two rooms and several users arrive in wire order; a member quitting yields `RoomEvent::Quit` in-band without ending `wait_map` until all users in a `users_inbox` set have quit.
- [x] **Cancel during exclusive flow:** `!cancel` via a separate unserialized inbox drops the exclusive future, releases `dm_mutex`, and a queued `user.ask` then proceeds.
- [x] **Command skipping:** with `.ignore_commands()`, a free-text read does not consume `!cancel` and a `Match::Reject` matcher does not fire rejection text on `!help`; without `.ignore_commands()`, free-text reads consume `!cancel` (documented behavior).
- [x] **Reject feedback & `MatchOutcome` inference:** a `Match::Reject(Text)` matcher result sends the feedback line and keeps waiting without restarting the timeout; compile-test multi-arm `Option<T>` closures alongside `Match<T>` closures.
- [x] **Moderation:** `room.kick` and `set_mode` on a synced room where the bot lacks `+o` fail synchronously with `Err(Error::NotChannelOp { .. })`; `members()` reflects `@`/`+` across `MODE` and `NICK` changes and keeps `User` identity; `topic()` tracks `332`/`331`/`TOPIC`.
- [x] **Warn-at:** a countdown warning fires mid-ask relative to the post-enqueue reply clock without resending the prompt or aborting the `Ask`.

---

## 4. Suggested order of work

1. Add to the round 1 spec edits: the multi-source `Filter` and public `seq` (§1.1), the `Error`/session shape (§1.2), and the tri-state matcher (§1.3).
2. During core and inbox work: implement the set-based filter, then `users_inbox`/`rooms_inbox`.
3. With `exclusive_many` (round 1): add `ask_many`.
4. Moderation and `members()` (§2.1), once `WorldState` mode tracking is tested.
5. Smaller additions (§2.2) as convenient. `server-time` is the one worth requesting from day one, since the capability list is part of startup config.

---

## 5. Considered and rejected (don't implement)

1. **Serializing or persisting suspended futures (the "serialization wall").** Rejected. It is impossible in Rust and contrary to the design. The legitimate concern is covered by §1.2: state the scope, and recommend a durable-state-plus-re-enterable-phase style. The claim that long games force you to "abandon `async`/`await` entirely" overstates it, because per-turn interactions inside a long game can still use `Ask`, with durable state in a database.
2. **"MUD-style free-roam games are a gap in the core."** Rejected as a library gap. A loop over `bot.inbox()` is the right shape for player-paced systems, and the spec already supports it (see the ping/pong and bridge examples). The residual value is the command-dispatch helper in §2.2, not a change to `Ask`.
3. **"Out-of-band DM commands can't respond while an `ExclusiveDm` holds the lock."** Rejected as stated. A global listener hears and answers via an unserialized inbox plus `say()`, with no lock needed. The real issues (silent drops and free-text swallowing) are in §1.3.
4. **`Ask`-equivalents for `NOTICE` (notice-acknowledges-notice games).** Rejected. RFC 2812 says not to auto-reply to `NOTICE`, so building a request/response layer on it works against the protocol, and the case is niche (bot-to-bot scripting). `wait_event_map` already exposes `RoomEvent::Notice` for anyone who needs it. Sending hidden-information messages *via* `notice()` is already supported.


---

## 6. Design Response & Finalized Resolution (Initial Analysis + Counter-Review Synthesis)

Below is our point-by-point resolution of Sections 1–5, incorporating the follow-up counter-review ("I'd lock it") and tightening all 5 edge cases before implementation.

### 6.1 Accepted Capabilities & Lean API Design

1. **§1.1 — Multi-Source Wire-Ordered `Inbox` (`InboxScope`, `users_inbox`, `rooms_inbox`, `msg.seq`):**
   - **Accepted (`bot.ask_many` declined, retracted in counter-review).**
   - **Why:** In `select_all` over separate per-user `Ask`s, if Bob (`futs[1]`) answers before Alice (`futs[0]`) in the same TCP batch, both inboxes receive their messages before polling and `select_all` polls `futs[0]` first—letting Alice win despite losing on the wire.
   - **Design:** Keep `Inbox` a single concrete struct, expose `pub seq: u64` on `ChatMessage` as the sole wire-ordering authority, and generalize internal `Filter` via `InboxScope::new().rooms(...).dms_from(...)`, `bot.inbox_for(scope)`, `bot.users_inbox(&users)`, and `bot.rooms_inbox(&rooms)`.
   - **Departure & timeout rules (tightened via counter-review):**
     - In `bot.users_inbox(&users)` (or `InboxScope` with multiple `dms_from`), one user quitting emits `RoomEvent::Quit` in-band on `next_event()` while `wait_map` / `next_chat` **keeps running** for the remaining online users; only once **all** users in the set have quit (after draining prior `seq`s) does `wait_map` / `next_chat` return `Err(Error::UserLeft { .. })`.
     - In the Quiz fair-race recipe (§5.3), we wrap `race_in.wait_map(...)` in **`.with_timeout(Duration::from_secs(20)).settled()`** (rather than per-read idle `with_read_timeout(20s)` + `.optional()`), so wrong guesses from busy players **never reset** the fixed 20-second round clock, AND `Turn::Left` cleanly handles the edge case where *all* players disconnect mid-round without aborting `lobby`.
     - We also document explicitly why the Quiz race holds `_locks = bot.exclusive_many(&player_refs).await?` (blocking concurrent `user.ask()` triggers during the race while `users_inbox` reads the unserialized DM stream, with `users_inbox` subscribed *before* `p.say(...)` and unserialized DM listeners still seeing race replies).

2. **§1.2 — `Error::Disconnected(DisconnectReason)`, `#[non_exhaustive]`, `bot.session_id()`, & Checkpoint Guide:**
   - **Accepted (dead unconstructed non-fatal variant declined in v1, agreed in counter-review).**
   - Add `#[non_exhaustive]` on `Error`, `DisconnectReason` (`Quit`, `BotDropped`, `Eof`, `Transport(String)`), and `RoomEvent`, change `Error::Disconnected` → `Error::Disconnected(DisconnectReason)`, and add `bot.session_id() -> u64`.
   - Document v1's connection-lifetime boundary and the "checkpoint-friendly" multi-phase game pattern (serializable state struct + re-enterable phase `async fn`s; multi-day play-by-post games out of scope for stack frames).

3. **§1.3 — `Match<T>` on `Ask` Only, Opt-In `.ignore_commands()`, & `!cancel` Pattern:**
   - **Accepted (`Reject` restricted to `Ask` only, agreed in counter-review).**
   - `Inbox` stays a pure read-only listener (`FnMut(&ChatMessage) -> Option<T>`). `Ask` accepts closures returning either `Option<T>` or `Match<T> = Accept(T) | Reject(Text) | Ignore` via sealed `MatchOutcome` (`Reject(msg)` sends `msg` to `Ask`'s target without resetting the reply timer). We verify multi-arm `Option<T>` closure inference in `cargo test` against all cookbook matchers (with `ask_checked` as fallback only if `rustc` inference ever balks).
   - Add `BotConfig::command_prefixes: Vec<String>` (default `vec!["!".into()]`) and **strictly opt-in** `.ignore_commands()` on `Inbox`, `Ask`, and `AskConfig` (plus `ExclusiveDm::with_ignore_commands()`). Messages matching a configured command prefix are skipped **before** calling the matcher, so free-text reads don't consume `!cancel` and `Match::Reject` never fires on `!help` or `!cancel`.
   - Document cancelling an `ExclusiveDm` flow via `pick_loadout(&alice).until(alice.inbox().wait_cmd("!cancel"))`.

4. **§2.1 — Synchronous Channel Moderation & `Room::members()`:**
   - **Accepted with synchronous queueing (echo-waiting `async fn` trap caught & agreed in counter-review).**
   - Standard IRCds emit **zero** server lines on idempotent `MODE` changes (`MODE #c +m` when already `+m`, `MODE #c +v Alice` when already `+v`), which would hang an echo-waiting `async fn` for 15s.
   - Add `Member { pub user: User, pub nick: String, pub is_op: bool, pub is_voiced: bool, pub prefixes: Vec<char> }`, `room.members() -> Vec<Member>`, `room.bot_is_op() -> bool`, `room.topic() -> Option<String>`, and synchronous `room.kick(&user, reason)`, `room.set_mode(modes, args)`, `room.voice(&user)`, `room.devoice(&user)`, `room.set_topic(topic)`.
   - Synced `WorldState` pre-check returns `Err(Error::NotChannelOp { channel })` immediately without sending traffic if the bot lacks `+o`; if an in-flight de-op race triggers an asynchronous `482 ERR_CHANOPRIVSNEEDED`, the pump logs `tracing::warn!` (and exposes the numeric on `next_raw`) rather than inventing a separate moderation-error stream or polluting chat `Ask`s.

5. **§2.2 — `Ask::warn_at`, `server-time`, `user.account()`, & `#[non_exhaustive]`:**
   - **`Ask::warn_at(remaining, msg)`:** Accepted on `Ask`, scheduled relative to the **post-enqueue reply clock** (consistent with `TimeoutCtx::elapsed`; skipped if initial budget `<= remaining`), plus `tokio::pin!(ask);` docs for custom `select!` tickers.
   - **Timestamps & `seq`:** Request IRCv3 `"server-time"` by default and stamp `pub server_time: Option<SystemTime>` (absent if server omits `time=`) and `pub received_at: tokio::time::Instant` on `ChatMessage`, documenting that `msg.seq` is always the ordering/fairness authority.
   - **Live `user.account() -> Option<String>`:** Request `"account-notify"` alongside `"account-tag"` and `"extended-join"` so `UserTrack` maintains `user.account()` across messages, joins, and runtime login/logout (`CommandRouter` and `WHOIS` deferred per counter-review agreement).
