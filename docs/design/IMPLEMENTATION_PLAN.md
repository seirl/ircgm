# `ircgm` — Implementation Plan

Companion execution checklist for [`DESIGN.md`](./DESIGN.md). Each phase is structured as a vertical, warning-free (`#![forbid(unsafe_code)]`) slice that compiles and passes deterministic unit tests before the next phase builds on top of it.

---

## Global Invariants & Guardrails

Keep these invariants true across every phase:
- **Zero `unsafe` code:** `#![forbid(unsafe_code)]` at crate root (`src/lib.rs`).
- **Acyclic `Bot` liveness (`BotOwner` vs. `BotCore`, `User` holds `Weak<BotCore>`):** Only `Bot` holds `Arc<BotOwner>`, whose `Drop` aborts the pump + outbound scheduler tasks, closes `BotCore` with `DisconnectReason::BotDropped`, **and clears `WorldState` and `InboxRegistry`**. `User` holds **`Weak<BotCore>` + `Arc<UserTrack>`** (never a strong `Arc<BotCore>`), so `WorldState -> Member { User }` and `InboxRegistry -> mpsc -> WireEvent { User }` can never form `Arc` cycles.
- **100% config-free `User`:** `User` carries only `Weak<BotCore>` + `Arc<UserTrack>`, comparing/ordering/hashing solely by `UserTrack::id`.
- **Synchronous indexed `Inbox` subscription & `Drop` removal:** Mutates `std::sync::Mutex<InboxRegistry>` synchronously (never via an async command channel) so `let inbox = room.inbox(); room.say(prompt)?;` can never miss a fast reply.
- **`OutboundSink` priority scheduling & write-receipt `Ask` clocks:** `OutboundSink` paces outbound commands across two priority queues (`Interactive`: `PRIVMSG`/`NOTICE`/`INVITE`/`MODE`/`TOPIC`/`JOIN`/raw vs. `Background`: `KICK`/`PART`, FIFO within each class; flushing same-channel `Background` before `JOIN`, and draining both on `QUIT`) and returns a `WriteReceipt` resolved when a prompt's final wrapped line is actually written. `Ask` starts its `reply_timeout` / `warn_remaining` clock on that **`WriteReceipt`** (not on enqueue).
- **Single rule for `Ask` timeout messages & single timer on `ExclusiveDm`/`Huddle`:** `timeout_message` / `timeout_message_fn` fires **iff** `Ask`'s own post-write reply timer expires; `Ask::poll` bypasses `Inbox::read_limit` on borrowed `ExclusiveDm` / `Huddle` inboxes; outer `GameFutureExt` combinators never emit or suppress messages.
- **`seq` is the sole wire-ordering authority:** Never order or race-settle by `server_time`.

---

## Phase 1 — Crate Scaffold, Pure Protocol Primitives, & Supervision Combinators

**Target files:** `Cargo.toml`, `src/lib.rs`, `src/error.rs`, `src/casemap.rs`, `src/supervise.rs`

- [ ] **1.1 Initialize `Cargo.toml` & crate root (`src/lib.rs`):**
  - [ ] Configure `[package]` (`name = "ircgm"`, `edition = "2021"`, `#![forbid(unsafe_code)]`).
  - [ ] Add dependencies: `irc = "1"`, `irc-colors = { version = "0.1", path = "../irc-colors" }`, `tokio = { version = "1", features = ["sync", "time", "rt", "macros"] }`, `futures-core = "0.3"`, `futures-util = "0.3"`, `thiserror = "2"`, `tracing = "0.1"`, `fastrand = "2"`.
  - [ ] Add `[dev-dependencies]`: `tokio = { version = "1", features = ["test-util", "macros", "rt"] }`.
  - [ ] Configure crate root exports & modules (`pub use irc;`, `pub use irc_colors::{self, Text};`, `pub mod fmt` (`Color, ColorValue, Colorize, Span, Style, Text`), `pub mod future`, `pub mod prelude` (`fmt::Colorize as _`, `GameFutureExt as _`, core types)).
- [ ] **1.2 Implement error model (`src/error.rs`, `DESIGN.md` §4.10):**
  - [ ] Define `#[non_exhaustive]` `DisconnectReason` (`Quit`, `BotDropped`, `Eof`, `Transport(String)`).
  - [ ] Define `#[non_exhaustive]` `Error`: `Irc`, `Disconnected(DisconnectReason)`, `Lagged { scope, skipped, opened_at: &'static Location<'static> }` (with `Display` hint: *"Did you leave an Inbox open across an .await? Use .clear() or .lossy() if intended."*), `TimedOut`, `UserLeft { nick, reason }`, `JoinFailed { channel, code, message }`, `DeliveryFailed { target, code, message }`, `BotLeftRoom { channel, reason }`, `NotChannelOp { channel }`, `HandshakeTimeout { operation }`, `InvalidInput(&'static str)`.
  - [ ] Implement `Error::is_fatal(&self) -> bool` (`false` for `TimedOut | UserLeft { .. } | DeliveryFailed { .. }`) and `pub type Result<T>`.
- [ ] **1.3 Implement IRC casemapping (`src/casemap.rs`, `DESIGN.md` §3.3 & §4.2):**
  - [ ] Define `CaseMap` (`Rfc1459` default, `StrictRfc1459`, `Ascii`, plus `u8` conversions for `AtomicU8` storage) and `FoldedStr(String)` (`Ord + Eq + Hash`).
  - [ ] Implement `CaseMap::fold(self, s: &str) -> FoldedStr` (`[`→`{`, `]`→`}`, `\`→`|`, `~`→`^` in `Rfc1459`; omit `~`→`^` in `StrictRfc1459`; ASCII lowercase only in `Ascii`) and allocation-free `CaseMap::str_eq(self, a: &str, b: &str) -> bool`.
  - [ ] Add parser helper from `005 CASEMAPPING=<value>`.
- [ ] **1.4 Implement supervision & pure future combinators (`src/supervise.rs`, `DESIGN.md` §4.9):**
  - [ ] Define `Outcome<T, S>` (`Completed(T)`, `Stopped(S)`) and `Turn<T>` (`Reply(T)`, `TimedOut`, `Left { nick, reason }`, `Unreachable { target, code, message }` + `into_reply`, `as_reply`, `is_timed_out`, `is_left`, `is_unreachable`).
  - [ ] Implement pin-projected `future::Until<G, S>` (`biased`: polls `S` before `G`) and inherent `Until::or_until(self, next_stop)` (`future::OrUntil<S1, S2>`, `biased` left-to-right so arbitrary `N` heterogeneous stop triggers produce a flat `Outcome<G::Output, S::Output>` with zero heap allocation).
  - [ ] Implement `race_all(futures)` (`future::RaceAll<F>`, `biased` in registration order; stays `Poll::Pending` when empty rather than panicking).
  - [ ] Implement `GameFutureExt` (`until`, `map_output`, `ok_or_pending`, `with_timeout`, `with_deadline`, `optional`, `settled` mapping `TimedOut`, `UserLeft`, and `DeliveryFailed` into `Turn`) and blanket `impl<F: Future> GameFutureExt for F` (with adapter structs `WithTimeout`, `WithDeadline`, `Optional`, `Settled`, `MapOutput`, `OkOrPending` in `pub mod future`).
- [ ] **1.5 Phase 1 unit tests (`cargo test`):**
  - [ ] `CaseMap`: verify `[Admin]` == `{admin}` and `~nick` == `^nick` under `Rfc1459`, `~nick` != `^nick` under `StrictRfc1459`, and `[a]` != `{a}` under `Ascii`.
  - [ ] `supervise` & `Error`: verify `until` + 3-way `.or_until(...)` `biased` tie-breaking, `ok_or_pending`, empty `race_all`, virtual-clock `with_timeout` / `with_deadline`, `optional`, `settled` (`TimedOut`, `UserLeft`, & `DeliveryFailed` caught as `Turn`; `Disconnected` & `Lagged` propagated as `Err`), and `Error::Lagged` `Display` formatting (`opened_at` + hint).

---

## Phase 2 — Protocol State Machines, Amortized `UserRegistry`, & Outbound Priority Scheduler

**Target files:** `src/state.rs`, `src/user.rs` (registry/track core), `src/outbound.rs`

- [ ] **2.1 Implement `WorldState` & `ChannelState` (`src/state.rs`, `DESIGN.md` §3.3):**
  - [ ] Track `casemap: CaseMap`, `chan_types: Vec<char>` (default `['#', '&']`), `status_prefixes: Vec<(char, char)>` (default `[('o', '@'), ('v', '+')]`), `chan_modes` (`A,B,C,D` arity categories), `max_modes: usize` (default `4`), `targmax_kick: usize` (default `1`), `current_nick: String`, and `self_user_host: Option<String>`.
  - [ ] Parse `005 RPL_ISUPPORT` tokens (`CASEMAPPING`, `CHANTYPES`, `PREFIX=(modes)symbols`, `CHANMODES=A,B,C,D`, `MODES=N`, `TARGMAX=...,KICK:N,...`).
  - [ ] Implement `WorldState::rekey_casemap(new_map)` rebuilding `channels` and member `FoldedStr` keys whenever `005 CASEMAPPING` changes dynamically.
  - [ ] Learn `current_nick` (`001`, self `NICK`) and `self_user_host` (self `JOIN`/`NICK` `Prefix::Nickname`, `396 RPL_HOSTHIDDEN`, or trailing `nick!user@host` in `001`).
  - [ ] Implement per-channel `ChannelState`: `topic: Option<String>` (`331`, `332`, `TOPIC`), staged `353 RPL_NAMREPLY` accumulator (stripping multi-prefix `@+` symbols and `userhost-in-names` `!u@h` tails) committed atomically on `366 RPL_ENDOFNAMES` (`names_synced = true`), and `clear()` for `BotOwner::drop`.
  - [ ] Implement parameterized channel `MODE` parser respecting `CHANMODES` + `PREFIX` arities (`+klo key 50 Alice`); maintain ordered `prefixes: Vec<char>` on each occupant (`is_op` for `o`/`a`/`q`, `is_voiced` for `v`).
- [ ] **2.2 Implement `UserRegistry` & `UserTrack` (`src/user.rs`, `DESIGN.md` §3.4):**
  - [ ] Define `UserStatus` (`Online`, `Quit { at_seq: u64, nick: String, reason: Option<String>, netsplit: bool }`) and `is_netsplit_reason(reason: &str) -> bool` (`"a.b c.d"` server-hostname pair heuristic).
  - [ ] Define `UserTrack` (`id: u64`, `current_nick: RwLock<String>`, `account: RwLock<Option<String>>`, `status_tx: watch::Sender<UserStatus>`, `dm_mutex: Arc<tokio::sync::Mutex<()>>`, `lock_holder: std::sync::Mutex<Option<&'static Location<'static>>>`).
  - [ ] Implement `UserRegistry` (`Mutex<RegistryState>` + `casemap: AtomicU8` + `AtomicU64` ID generator):
    - Lookup/insert, `NICK` re-keying (`fold(old) -> fold(new)` preserving `UserTrack`), `rekey_casemap(new_map)`, `account` updates (`"*"` / `"0"` -> `None`), and `QUIT` / `401` offline transition (`Quit { at_seq, netsplit, .. }`).
    - **Amortized dead-`Weak` GC:** sweep dead `Weak<UserTrack>` entries when `tracks.len() > 2 * last_live_count.max(32)` during registry access; **never** lock `UserRegistry` inside `Drop for UserTrack`.
- [ ] **2.3 Implement `OutboundSink` priority scheduler, `WriteReceipt`s, & byte-accurate wrapping (`src/outbound.rs`, `DESIGN.md` §3.5):**
  - [ ] Validate protocol identifiers (`join`, `invite`, `part`, `kick`, `room`, `user`) rejecting `\r`, `\n`, `\0`, or empty strings with `Err(Error::InvalidInput)`.
  - [ ] Compute dynamic 512-byte relayed wire budget: `512 - (1 + current_nick.len() + 1 + user_host_len + cmd_overhead + target.len() + 4)` (using observed `self_user_host.len()` or conservative `76`-byte reserve), clamped by `config.max_payload_bytes` (minus `9` for CTCP `ACTION`).
  - [ ] Strip embedded `\x01` from spans, wrap via `irc_colors::Text::wrap(budget)`, skip empty chunks, and frame `\x01ACTION <chunk>\x01` for `action()`.
  - [ ] Implement `OutboundSink` owning two FIFO priority queues (`Interactive`: `PRIVMSG`/`NOTICE`/`INVITE`/`MODE`/`TOPIC`/`JOIN`/raw; `Background`: `KICK`/`PART`) paced by `outbound_burst` / `outbound_window` (logging `tracing::warn!` when pacing activates):
    - Return a `WriteReceipt` (`oneshot::Receiver<()>`) resolved when the last line of an enqueued batch is written to the underlying sink (`irc::client::Sender` or `TestServer` channel).
    - Flush pending same-channel `Background` `KICK`/`PART` items before emitting an `Interactive` `JOIN <chan>`.
    - Drain both queues in order before emitting `QUIT` and resolving `quit()`.
- [ ] **2.4 Phase 2 unit tests (`cargo test`):**
  - [ ] `WorldState`: `005` parsing (`PREFIX=(qaohv)~&@%+`, `MODES=6`, `TARGMAX=PRIVMSG:4,KICK:1` vs `KICK:3`, `CHANMODES`), dynamic `CASEMAPPING=ascii` re-keying, multi-line `353` + `366` with `@+Alice!u@h`, parameterized `MODE #c +klo secret 50 Alice -v Bob`, `332`/`331`/`TOPIC`, and `396` cloak updates.
  - [ ] `UserRegistry`: `[Alice]` & `{alice}` sharing one `UserTrack::id`, surviving `/nick Alice2`, `QUIT` + rejoin allocating a fresh `UserTrack::id`, `netsplit` detection (`"hub.eu.net leaf.us.net"` vs `"Quit: Leaving"`), `account` transitions (`*` logout), and amortized `len > 2 * last_live_count` GC with zero `Drop` re-entrancy.
  - [ ] `OutboundSink` & wrapping: CRLF splitting (`"a\r\nQUIT :x"` -> 2 `PRIVMSG`s), `\x01` sanitization, 60-byte `396` vhost clamping `<= 512` wire bytes while preserving style spans, `Interactive` overtaking `Background` `KICK`/`PART` bursts while keeping per-class FIFO (and flushing same-channel `PART` before `JOIN`), and virtual-clock `WriteReceipt` resolution under burst pacing.

---

## Phase 3 — Events, Indexed `Inbox`, Connection Pump (`CAP` + `376`/`422`), `Bot`, & `testing`

**Target files:** `src/message.rs`, `src/inbox.rs`, `src/pump.rs`, `src/bot.rs`, `src/testing.rs`

- [ ] **3.1 Implement `Target::Dm`, `MessageKind`, `ChatMessage`, `Event`, & internal `WireEvent` (`src/message.rs`, `DESIGN.md` §4.3):**
  - [ ] Implement `Target` (`Channel(String)`, `Dm`, `is_channel`, `is_dm`) and `MessageKind` (`Privmsg`, `Action`, `Notice`).
  - [ ] Implement `ChatMessage` (`raw`, `seq`, `received_at`, `server_time`, `sender: User`, `nick`, `ident`, `host`, `account`, `target`, `text`, `raw_text`, `kind: MessageKind` + `is_dm`, `is_action`, `is_notice`, `is_in`, `to_styled`, word-boundary `is_cmd`, `cmd_args`).
  - [ ] Implement minimal zero-dep ISO-8601 / RFC 3339 UTC parser (`YYYY-MM-DDThh:mm:ss[.sss]Z`) for IRCv3 `time=` tags into `Option<SystemTime>`.
  - [ ] Define `#[non_exhaustive]` `Event` (`Chat`, `Notice`, `Joined`, `Parted`, `Kicked`, `Quit { user, nick, channels, reason, netsplit }`, `NickChanged`, `DeliveryFailed`) and internal `WireEvent`.
- [ ] **3.2 Implement `InboxScope`, indexed `InboxRegistry`, & `Inbox` (`src/inbox.rs`, `DESIGN.md` §2.3, §2.5, §3.2, §4.4):**
  - [ ] Implement `InboxScope` (`new`, `room`, `rooms`, `dm_from`, `dms_from`) and internal `Filter` (`All`, `AllDms`, `UserInRoom { user_id, channel }`, `Scoped { rooms, dm_users }`) with human-readable `scope_label` for `Error::Lagged`.
  - [ ] Implement synchronous indexed `InboxRegistry`:
    - Maintain `slots`, `all_slots: Vec<SlotId>`, `by_channel: HashMap<FoldedStr, Vec<SlotId>>`, and `by_user: HashMap<u64, Vec<SlotId>>`, deduplicating multi-bucket / multi-channel dispatch via `slot.last_dispatched_seq == seq` (zero per-event allocation).
    - Expose `has_interest_in_channel(&FoldedStr) -> bool` for the pump's early unwatched-channel skip, `rekey_casemap(CaseMap)` for dynamic `005 CASEMAPPING`, and `clear()` for `BotOwner::drop`.
    - Emit a one-time `tracing::warn!` (reset on `Inbox::clear()`) naming `scope` and `opened_at` when an `Inbox`'s queued depth crosses **75% of `inbox_capacity`**.
  - [ ] Implement `Inbox` modifiers: `with_read_timeout`, `with_read_deadline`, `without_read_timeout`, `set_read_timeout`, `set_read_deadline`, `clear_read_timeout`, `ignore_commands`, `set_ignore_commands`, `accept_notices`, `set_accept_notices`, `lossy`, and `clear`.
  - [ ] Implement unified source-exhaustion, `.accept_notices()` filtering, & pre-departure drain logic:
    - Surface `ChatMessage { kind: MessageKind::Notice, .. }` on chat methods **iff** `self.accept_notices` is `true` (and the pump validated a human `nick!user@host` prefix without IRCv3 `bot` tag or services pseudoclient nick).
    - Drain buffered events with `seq < departure_seq` before returning `Err(Error::UserLeft { .. })` or `Err(Error::BotLeftRoom { .. })`, terminating only when **zero live sources** remain across the inbox's scope.
  - [ ] Implement strictly cancel-safe `wait_map`, `wait_for`, `wait_cmd`, `next_chat`, `next_event`, `wait_event_map`, `next_raw`, `wait_raw_map` (plus `poll_raw_ignoring_read_limit` for `Ask::poll`) and `futures::Stream<Item = Result<ChatMessage>>`.
- [ ] **3.3 Implement connection pump & registration barrier (`src/pump.rs`, `DESIGN.md` §3.2):**
  - [ ] Drive either `irc::client::ClientStream` or `TestServer`'s inbound channel, yielding cooperatively via `tokio::task::yield_now()` every 64 consecutive ready frames (and answering `TestServer::settle()` barriers in-band).
  - [ ] Track registration barrier completion: `CAP ACK`/`NAK` -> `CAP END` **and** `376 RPL_ENDOFMOTD` or `422 ERR_NOMOTD`.
  - [ ] Filter self-echoes (`fold(sender_nick) == fold(current_nick)`) out of `Chat`/`Notice` while keeping them on `next_raw`, and classify human-eligible `NOTICE`s (`Prefix::Nickname` with non-empty `user` & `host`, no `bot`/`+bot` tag, non-services nick).
  - [ ] Execute strict 3-step per-line sequence with monotonic `seq: u64`:
    1. Early skip `Text::parse` & `UserTrack` allocation on unwatched channel `PRIVMSG`/`NOTICE` lines; otherwise stamp `sender: User` (`Weak<BotCore>` + `Arc<UserTrack>`) and affected `channels` *before* mutating renames/quits.
    2. Mutate `WorldState` & `UserRegistry` (`001`, `005` incl. `TARGMAX` & `CASEMAPPING` re-keying across `WorldState`+`UserRegistry`+`InboxRegistry`, `331`/`332`/`TOPIC`, `353`/`366`, `376`/`422`, `396`, `JOIN`/`extended-join`, `PART`, `KICK`, `QUIT` with `netsplit`, `NICK`, `ACCOUNT`, `MODE`, self room-loss `BotLeftRoom`, `482` `tracing::warn!`, and send-refusal numerics `401`, `403`, `404`, `442`, `486`, `492`, `531`, `716`).
    3. Dispatch `Arc<WireEvent>` via indexed `InboxRegistry` buckets (`last_dispatched_seq` dedupe, 75%-fill warning, `lagged_skipped` + `lag_notify` on full).
  - [ ] On EOF / transport error / `quit`, mark `BotCore` disconnected with the exact `DisconnectReason` and close/wake all active `Inbox`es and handshakes.
- [ ] **3.4 Implement `BotConfig`, `Bot` (`BotOwner`/`BotCore`), & unconditional `pub mod testing` (`src/bot.rs`, `src/testing.rs`, `DESIGN.md` §4.2):**
  - [ ] Verify in `irc` v1.x how `Config::capabilities` / SASL and `Config::channels` behave on connect: set `irc.max_messages_in_burst = Some(u32::MAX)` so `OutboundSink` owns pacing, use `Config::capabilities` where compatible so SASL never races manual `CAP REQ`, withhold `Config::channels` from pre-barrier auto-join, await the `CAP` + `376`/`422` barrier (bounded by `handshake_timeout`), and join initial channels via `Bot::join`.
  - [ ] Implement `BotConfig` (`new`, `with_ask_config`, `with_command_prefixes`, defaults: `inbox_capacity = 1024`, `request_caps`, `command_prefixes = ["!"]`, `handshake_timeout = 15s`, `ask_config = AskConfig::default()`, `max_payload_bytes = 450`, `outbound_burst = 15`, `outbound_window = 8s`).
  - [ ] Implement `Drop for BotOwner`: abort pump & outbound scheduler tasks, set `DisconnectReason::BotDropped`, and call `world_state.clear()` + `inbox_registry.clear()`.
  - [ ] Implement `pub mod testing` unconditionally (`test_harness` with unthrottled `outbound_burst = usize::MAX` by default, `test_harness_with`, and `TestServer`: `send_line`, `chat`, `dm`, `complete_join`, `quit_user`, `settle`, `disconnect`, `recv_sent`, `try_recv_sent`, and non-destructive `take_messages_to`).
  - [ ] Implement `session_id()`, `nick()`, `casemap()`, `room()`, `user()`, `join()` / `join_with_key()`, `reply()` (**mirroring `MessageKind::Notice` with outbound `NOTICE`**, never `PRIVMSG`!), `send_raw()`, `#[track_caller]` `inbox_all()` / `inbox_dms()` / `inbox_users()` / `inbox_rooms()` / `inbox_scope()`, `request_shutdown()`, `is_shutting_down()`, `shutdown_requested()`, and `quit()`.
- [ ] **3.5 Phase 3 unit tests (`cargo test`):**
  - [ ] Single-batch `PRIVMSG` + `NICK` + nick-steal + `QUIT` ordering & stamped `msg.sender` identity.
  - [ ] `inbox_users` wire-fair `seq` tie-breaking (`Bob` before `Alice` in one batch) and unwatched-channel zero-`UserTrack`-allocation check + multi-channel `Quit` single-delivery dedupe.
  - [ ] Multi-source `InboxScope` + mixed `.room(&r1).dms_from([&alice, &bob])` surviving after both DM users quit.
  - [ ] `NOTICE` isolation by default vs `.accept_notices()` (allowing human `nick!u@h` `NOTICE` + `bot.reply` mirroring `NOTICE`, while still excluding server notices, `NickServ`/`ChanServ`, and `@bot`-tagged notices) + `echo-message` self-filter.
  - [ ] Per-`Inbox` lag isolation, 75%-fill warning, `#[track_caller]` `Error::Lagged { scope, skipped, opened_at }` hint, and `.lossy()` recovery.
  - [ ] Acyclic `Bot` drop & `376`/`422` barrier: verify `Arc<BotCore>` strong count drops to `0` on last `Bot` drop even when `Member { User }` and buffered `WireEvent { User }` existed, and verify `CAP` + multi-line `005` + `376`/`422` registration barrier.

---

## Phase 4 — `AskConfig`, `Ask`, `Room`, `ExclusiveDm`, `Huddle`, & `Breakout`

**Target files:** `src/ask.rs`, `src/room.rs`, `src/user.rs`, `src/huddle.rs`, `src/breakout.rs`

- [ ] **4.1 Implement `Verdict<T>`, `IntoVerdict`, `AskConfig`, `TimeoutCtx`, & `Ask` (`src/ask.rs`, `DESIGN.md` §4.7):**
  - [ ] Implement `Verdict<T>` (`Accept`, `Reject(Text)`, `Ignore`, `Verdict::reject(impl Into<Text>)`) and sealed `IntoVerdict` (`into_verdict`) for `Option<T>` and `Verdict<T>`.
  - [ ] Implement unified 1-to-1 builders on `AskConfig` and `Ask`: `reply_timeout`, `reply_deadline`, `no_reply_timeout`, `warn_remaining`, `clear_warnings`, `ignore_commands`, `accept_notices`, `timeout_message`, `timeout_message_fn`, `no_timeout_message`, and `Ask::config(&AskConfig)`.
  - [ ] Implement lazy `Ask` `Future` state machine across all prompt sources (**starting `reply_timeout` / `warn_remaining` clocks on `OutboundSink`'s `WriteReceipt`, never on enqueue!**):
    - **`RoomOpen` (`Room::ask`):** subscribe `room.inbox()` -> enqueue `prompt` -> await `WriteReceipt` -> start reply clock & warnings -> abort fast on `BotLeftRoom` / `DeliveryFailed`.
    - **`RoomUser` (`Room::ask_user`):** if `room` is `names_synced` and `user` (or bot) is absent, fail immediately (`UserLeft` / `BotLeftRoom`) without sending; subscribe `user.inbox_in(room)` -> enqueue `prompt` -> await `WriteReceipt` -> pass `TimeoutCtx { user: Some(user), .. }`.
    - **`UserDm` (`User::ask`):** lazily acquire `user.exclusive().await?` -> enqueue `prompt` -> await `WriteReceipt` -> start reply clock.
    - **`BorrowedDm` (`ExclusiveDm::ask`):** call `dm.inbox.clear()`, enqueue `prompt`, await `WriteReceipt`, and poll raw events **bypassing `dm.inbox.read_limit`** so `Ask`'s resolved deadline is the sole timer.
    - **`BorrowedHuddle` (`Huddle::ask`):** fail immediately with `Err(Error::UserLeft { .. })` if 0 online users remain; call `huddle.inbox.clear()`, broadcast `prompt` to online users, await last `WriteReceipt`, bypass `huddle.inbox.read_limit`, skip `.ignore_commands()` lines before matcher/relay, send `Verdict::Reject` privately to `msg.sender` only, relay `Verdict::Ignore` lines to fellow online users when `with_relay` is set, and broadcast `warn_remaining` / `timeout_message` to online users.
  - [ ] Instrument `Ask` with `tracing` span (`ircgm::ask`) & debug wait events without holding `Span::enter` across `.await`.
- [ ] **4.2 Implement `Room` & `Member` (`src/room.rs`, `DESIGN.md` §4.5):**
  - [ ] Implement `Member` (`user`, `nick`, `is_op`, `is_voiced`, `prefixes`) and `InviteTarget` (sealed for `User`, `str`, `String`).
  - [ ] Implement `Room` (`name`, `casemap`, `with_ask_config(&cfg)`, `say`, `notice`, `action`, `invite` checking `User::is_online()`, `part`, `members`, `member`, `nicks`, `has_user`, `has_nick`, `bot_is_op`, `topic`, `#[track_caller] inbox`, `ask`, `ask_user`).
  - [ ] Implement synchronous moderation (`kick`, `set_mode`, `voice`, `voice_many`, `devoice`, `devoice_many`, `set_topic`), checking `bot_is_op()` when `names_synced` (`Err(Error::NotChannelOp { .. })`) and chunking `voice_many` / `devoice_many` by `WorldState::max_modes` (`005 MODES`, default `4`).
- [ ] **4.3 Complete `User`, `ExclusiveDm`, `exclusive_many`, & `exclusive_available` (`src/user.rs`, `DESIGN.md` §4.2 & §4.6):**
  - [ ] Implement `User` (`Weak<BotCore>` + `Arc<UserTrack>`: `nick`, `eq_nick`, `account`, `is_online`, `say`, `notice`, `action`, `#[track_caller] exclusive`, `#[track_caller] ask`, `#[track_caller] inbox`, `#[track_caller] inbox_in`).
  - [ ] Track `lock_holder` `Location` on `UserTrack` and emit a `tracing::warn!` naming both waiter and holder `Location`s if `dm_mutex` wait exceeds `15s`.
  - [ ] Implement `ExclusiveDm` (`user`, `with_ask_config`, `set_ask_config`, `with_ignore_commands`, `with_accept_notices`, `say`, `notice`, `action`, `ask`, `inbox`).
  - [ ] Implement `#[track_caller] Bot::exclusive_many<'a, I: IntoIterator<Item = &'a User> + Send>(users)` and `#[track_caller] Bot::exclusive_available<'a, I: IntoIterator<Item = &'a User> + Send>(users)`.
- [ ] **4.4 Implement `Huddle` (`src/huddle.rs`, `DESIGN.md` §4.2 & §4.6):**
  - [ ] Implement `#[track_caller] Bot::huddle` and `#[track_caller] Bot::huddle_available`.
  - [ ] Implement `Huddle` (`users`, `online_users`, `len`, `is_empty`, `with_ask_config`, `set_ask_config`, `with_ignore_commands`, `with_accept_notices`, `with_relay`, `without_relay`, `set_relay`, `say`, `notice`, `action`, `relay_to_others`, `ask`, `inbox`).
- [ ] **4.5 Implement `Breakout`, `BreakoutNaming`, & `BreakoutOptions` (`src/breakout.rs`, `DESIGN.md` §4.8):**
  - [ ] Implement `Bot::breakout(prefix)` / `breakout_with(opts)` (`#<prefix>-<rand6>`, `JOIN` + `366`, verify `require_op`, apply `modes` default `+is`, apply `BreakoutOptions::ask_config`).
  - [ ] Implement `#[must_use]` on `Breakout`, `Deref<Target = Room>`, `to_room()`, inherent `with_ask_config(&cfg)` + `set_ask_config(&cfg)`, `invite_and_wait`, `invite_available`, awaitable `close().await`, and cancellation-safe `Drop` (enqueuing `Background`-priority `KICK` commands batched up to `WorldState::targmax_kick`—default `1`—followed by `PART`).
- [ ] **4.6 Wire full root `pub use` re-exports (`MessageKind`, `fmt`, `prelude`, `future`, `testing`) & static `Send + Sync` assertions (`src/lib.rs`, `DESIGN.md` §4.1–§4.2).**

---

## Phase 5 — Full Acceptance Test Suite (`DESIGN.md` §6), Doctests, & Pre-0.1 Release Check

**Target files:** `src/lib.rs` (doctests) & `tests/` integration tests

- [ ] **5.1 Execute & verify all 9 test groups in [`DESIGN.md` §6](./DESIGN.md#6-testing-strategy):**
  - [ ] **Group 1:** Burst packet batching, `inbox_users` wire-fair tie-breaking, multi-source `InboxScope` & mixed-scope partial quit, single-batch `PRIVMSG`+`NICK`+steal+`QUIT`, `Event` fields (`server_time`, `received_at`, `channels`).
  - [ ] **Group 2:** One-shot `wait_cmd` non-backlog, `inbox_dms` filtering, `NOTICE` isolation by default vs `.accept_notices()` (`MessageKind::Notice`, `bot.reply` `NOTICE` mirroring, exclusion of server/`NickServ`/`@bot` notices).
  - [ ] **Group 3:** `ExclusiveDm` vs queued `User::ask`, >15s lock-wait `Location` warning, `!cancel` release + `.ignore_commands()`, `Verdict::Reject` + `.warn_remaining()` / `.clear_warnings()` + multi-arm closure inference, stale-inbox `clear()` regression, **`WriteReceipt` pacing `Ask` clock test** (`outbound_burst = 2`), `ExclusiveDm::ask` lengthened-deadline single-timer regression, pure combinator table, `exclusive_many` / `exclusive_available`, `Huddle`, and static `Send + Sync` assertions.
  - [ ] **Group 4:** `room.ask_user` `/nick` tracking, live nick in `TimeoutCtx`, `.settled()` isolating `PART`/`KICK`/`QUIT` (`Turn::Left`), `TimedOut` (`Turn::TimedOut`), and `486`/`716` (`Turn::Unreachable`, `is_fatal() == false`) while propagating `Disconnected`.
  - [ ] **Group 5:** `WorldState` `005` (`MODES`, `TARGMAX` `KICK:1` vs `KICK:3`, dynamic `CASEMAPPING` re-keying), batched `voice_many`/`devoice_many`, synchronous `NotChannelOp`, `room.member`/`has_user`, `user.eq_nick`, `QUIT`+rejoin new `UserTrack`, `netsplit: bool`, `user.account()`, and amortized `UserRegistry` GC.
  - [ ] **Group 6:** Acyclic `Bot` drop (`Arc<BotCore>` strong count = `0`), `CAP` + `376`/`422` registration barrier, `Breakout::invite_and_wait` vs `invite_available`, multi-stop `.until().or_until()` + `ok_or_pending()` + `race_all()`, `Interactive` vs `Background` priority scheduling, and `bot.quit` per-class FIFO drain (`Interactive` `PRIVMSG`, `Background` `KICK`s -> `PART`, then `QUIT`).
  - [ ] **Group 7:** Indexed `InboxRegistry` unwatched-channel skip & single-delivery dedupe, per-`Inbox` lag isolation, 75%-fill warning, `#[track_caller]` `Error::Lagged` hint, and `.lossy()` multi-voter recovery.
  - [ ] **Group 8:** `401`/`403`/`404`/`442` and PM-block `486`/`492`/`531`/`716` (`params[1]`) fast-abort -> `Turn::Unreachable` + `Event::DeliveryFailed` for one-way DMs, `BotLeftRoom`, non-occupant `ask_user`, and `echo-message` self-filter.
  - [ ] **Group 9:** CRLF injection splitting, `396` cloak 512-byte dynamic wrapping with `irc-colors` styles, `OutboundSink` burst pacing, and inbound formatting stripping (`msg.text` vs `msg.to_styled()`).
- [ ] **5.2 Compile-checked Cookbook doctests (`DESIGN.md` §5.1–§5.8):**
  - [ ] Embed all 8 cookbook examples (§5.1 Heist, §5.2 Blackjack, §5.3 Quiz, §5.4 Mafia + `Huddle`, §5.5 Live Speed Admin Panel, §5.6 Diplomacy Bridge, §5.7 Quiplash `FuturesUnordered` with DM `timeout_message` + `#room` `Turn::TimedOut` / `Turn::Unreachable`, §5.8 Co-op Bomb Defusal) using `ircgm::prelude::*` as `no_run` Rust doc-tests (and `tests/cookbook_compile.rs`) verified by `cargo test --doc` with zero warnings.
- [ ] **5.3 Final quality gate & pre-0.1 release check:**
  - [ ] `cargo test --all-targets`, `cargo test --doc`, `cargo doc --no-deps`, & `cargo clippy --all-targets -- -D warnings` all green and warning-free.
  - [ ] Before publishing `0.1.0`, verify `crates.io` crate name availability for both `ircgm` and `irc-colors`.
