# `ircgm` — Design & Architecture Specification

**`ircgm`** (*IRC Game Master*) is an asynchronous Rust library built on top of the [`irc`](https://crates.io/crates/irc) crate (`v1.x`, Tokio runtime) and [`irc-colors`](../../../irc-colors) for writing multi-step interactive IRC games, linear narratives, quizzes, Mafia/Werewolf moderators, RPG encounters, and stateful conversational bots.

Instead of forcing bot authors to flatten multi-turn interactions into global event-callback state machines, `ircgm` lets you write games in **direct-style imperative `async`/`await` Rust**—combining `expect`-style message matching, scoped message queues (`Inbox`), presence-aware user handles with serialized DM prompts (`User`), and cancellation-safe temporary rooms (`Breakout`).

---

## Table of Contents

1. [Problem Statement & Core Philosophy](#1-problem-statement--core-philosophy)
2. [Key Architectural Decisions & Justifications](#2-key-architectural-decisions--justifications)
   - [2.1 Thin Connection Pump + Pump-Filtered Per-`Inbox` Channels (Why Not a Global `broadcast`?)](#21-thin-connection-pump--pump-filtered-per-inbox-channels-why-not-a-global-broadcast)
   - [2.2 Separating Acting from Listening: Why `wait_*` Lives *Only* on `Inbox`](#22-separating-acting-from-listening-why-wait_-lives-only-on-inbox)
   - [2.3 Single Concrete `Inbox` & Multi-Source Wire-Ordered Filtering (`InboxScope`)](#23-single-concrete-inbox--multi-source-wire-ordered-filtering-inboxscope)
   - [2.4 Unified `User` Handle, Queued DM Prompts, and Deadlock-Freedom](#24-unified-user-handle-queued-dm-prompts-and-deadlock-freedom)
   - [2.5 Per-`Inbox` Lag Isolation — Strict Scoped Error by Default](#25-per-inbox-lag-isolation--strict-scoped-error-by-default)
   - [2.6 Targeted RAII: `Room` vs. `Breakout` (`PathBuf` vs. `TempDir`)](#26-targeted-raii-room-vs-breakout-pathbuf-vs-tempdir)
   - [2.7 First-Class `Ask` Timeouts, Handle-Carried `AskConfig`, `Room::ask_user`, & `GameFutureExt`](#27-first-class-ask-timeouts-handle-carried-askconfig-roomask_user--gamefutureext)
   - [2.8 Decoupled Supervision, Graceful Shutdown, & Live Control (`GameFutureExt::until` + `bot.shutdown_requested()`)](#28-decoupled-supervision-graceful-shutdown--live-control-gamefutureextuntil--botshutdown_requested)
   - [2.9 Native Rich-Text Integration via `irc-colors`](#29-native-rich-text-integration-via-irc-colors)
3. [Internal Architecture & Protocol Mechanics](#3-internal-architecture--protocol-mechanics)
   - [3.1 Data-Flow Overview](#31-data-flow-overview)
   - [3.2 The Connection Pump Task, `BotOwner` / `BotCore` Split, IRCv3 Caps, & In-Band Stamping](#32-the-connection-pump-task-botowner--botcore-split-ircv3-caps--in-band-stamping)
   - [3.3 `WorldState` & IRC Casemapping (`CaseMap`)](#33-worldstate--irc-casemapping-casemap)
   - [3.4 `UserRegistry`: Stamped Identity, Live `account()`, Shared-Channel Presence, `NICK` Tracking, & `QUIT` Ordering](#34-userregistry-stamped-identity-live-account-shared-channel-presence-nick-tracking--quit-ordering)
   - [3.5 Rich Formatting, Dynamic Self-Hostmask Byte Budgeting, and Injection Safety (`irc-colors`)](#35-rich-formatting-dynamic-self-hostmask-byte-budgeting-and-injection-safety-irc-colors)
   - [3.6 Observability via `tracing`](#36-observability-via-tracing)
4. [Public API Specification](#4-public-api-specification)
   - [4.1 Summary of Core Types & Naming Conventions](#41-summary-of-core-types--naming-conventions)
   - [4.2 Modules (`fmt`, `prelude`, `future`, `testing`), Re-Exports, `BotConfig`, & `Bot`](#42-modules-fmt-prelude-future-testing-re-exports-botconfig--bot)
   - [4.3 Message & Event Views (`ChatMessage`, `MessageKind`, `Event`)](#43-message--event-views-chatmessage-messagekind-event)
   - [4.4 `Inbox` (Unified Listening Primitive, Multi-Source Rules, & `select!` Cancel Safety)](#44-inbox-unified-listening-primitive-multi-source-rules--select-cancel-safety)
   - [4.5 `Room` & `Member` (Channel View, Membership, & Moderation)](#45-room--member-channel-view-membership--moderation)
   - [4.6 `User`, `ExclusiveDm`, & `Huddle`](#46-user-exclusivedm--huddle)
   - [4.7 `Ask`, `Verdict<T>`, `AskConfig`, & `TimeoutCtx`](#47-ask-verdictt-askconfig--timeoutctx)
   - [4.8 `Breakout` & `BreakoutOptions` (RAII Ephemeral Rooms)](#48-breakout--breakoutoptions-raii-ephemeral-rooms)
   - [4.9 Supervision, Arbitrary Stop Chains, `Turn<T>`, & Pure Combinators (`Outcome`, `GameFutureExt`, `race_all`)](#49-supervision-arbitrary-stop-chains-turnt--pure-combinators-outcome-gamefutureext-race_all)
   - [4.10 Error Model (`ircgm::Error` & `DisconnectReason`)](#410-error-model-ircgmerror--disconnectreason)
5. [Complete Cookbook & Idiomatic Patterns](#5-complete-cookbook--idiomatic-patterns)
   - [5.1 The Heist: Lobby Loop, Handle-Scoped Timeouts, Admin Stop, and Breakout Room](#51-the-heist-lobby-loop-handle-scoped-timeouts-admin-stop-and-breakout-room)
   - [5.2 Blackjack: Turn-Based `room.ask_user`, Player Timeout Callouts, & `.settled()`](#52-blackjack-turn-based-roomask_user-player-timeout-callouts--settled)
   - [5.3 The Quiz: Burst-Safe Recruitment, Unique Users, and Wire-Fair Multi-Player DM Race](#53-the-quiz-burst-safe-recruitment-unique-users-and-wire-fair-multi-player-dm-race)
   - [5.4 Mafia Moderation, Secret `Huddle` Deliberation, Cancellable `ExclusiveDm`, & Queued Prompts](#54-mafia-moderation-secret-huddle-deliberation-cancellable-exclusivedm--queued-prompts)
   - [5.5 Live Admin Control Panel: Dynamic Game Speed (`!speed`) + `!stop`](#55-live-admin-control-panel-dynamic-game-speed-speed--stop)
   - [5.6 Multi-Room Wire-Ordered Bridge (`inbox_rooms`) & Stateless Ping/Pong](#56-multi-room-wire-ordered-bridge-inbox_rooms--stateless-pingpong)
   - [5.7 Quiplash / Simultaneous Secret Prompts with Live `"✔ Locked In!"` Progress (`FuturesUnordered`)](#57-quiplash--simultaneous-secret-prompts-with-live--locked-in-progress-futuresunordered)
   - [5.8 Co-op Bomb Defusal: Racing Multi-Step `async fn`s (`tokio::select!` + `tokio::try_join!`)](#58-co-op-bomb-defusal-racing-multi-step-async-fns-tokioselect--tokiotry_join)
6. [Testing Strategy](#6-testing-strategy)
7. [Crate Layout & Dependencies](#7-crate-layout--dependencies)

---

## 1. Problem Statement & Core Philosophy

IRC is a single flat TCP stream of asynchronous protocol lines (`PRIVMSG`, `JOIN`, `PART`, `NICK`, `QUIT`, numeric replies). Traditional IRC libraries expose an event loop where every incoming message invokes a handler callback. Writing a 5-phase game in that style forces the author to manually build an explicit state machine (`enum GamePhase { Recruiting, Planning, Voting, ... }`), correlate timers and per-player DMs by hand, handle mid-game disconnects/renames in every state, and scatter teardown logic across abort paths.

`ircgm` inverts that control flow so the call stack *is* the game state machine, guided by four core principles:

1. **Plain Rust Futures, Zero Framework Lock-In:**
   Every operation is a standard `Send` future or `futures::Stream`. Concurrency uses ordinary `tokio::select!`, `futures::future::{select_all, try_join_all}`, `tokio::sync::watch`, and `tokio::spawn`.
2. **Explicit Listening Lifetimes (`Inbox`):**
   Handles (`Bot`, `Room`, `User`) perform outbound actions and spawn `Inbox` queues. `Inbox` is the *only* type with `.wait_*()` methods, making it impossible to silently drop message bursts in loops while keeping one-shot waits (`room.inbox().wait_cmd("!start").await?`) free of stale backlogs.
3. **RAII Only Where Ownership Is Exclusive (`Breakout` & `ExclusiveDm`):**
   Normal targets (`Room`, `User`) are cheap, `Clone`able, side-effect-free handles (like `PathBuf`). Scoped exclusive resources (`Breakout` channels and `ExclusiveDm` prompt locks) are single-owner RAII guards (like `tempfile::TempDir`) that clean up deterministically on `Drop`—including when a game future is cancelled mid-flight.
4. **Protocol Honesty:**
   `ircgm` handles the sharp edges of real IRC servers rather than assuming idealized chat behavior: RFC 1459 / ASCII case-folding, asynchronous `JOIN`/`NAMES` confirmation, `INVITE` vs. actual `JOIN` arrival, RFC 2812 `NOTICE` isolation, mid-game `NICK` changes, `QUIT` disconnects, serializing prompts on IRC's single shared per-user DM pipe, hostmask/IRCv3 `account-tag` admin verification, and byte-accurate IRC line wrapping.

### Session Lifecycle, Checkpoint-Friendly Long Games, & Scope Boundaries
Representing game phases as live Rust stack frames makes multi-step control flow dramatically simpler to read and write, with three explicit boundaries:
- **Connection-bounded v1 sessions (`Bot` lifecycle, `session_id()`, & cooperative shutdown):** Each `Bot` instance represents **one IRC connection session** (like `tokio_postgres::Client`), tagged with a monotonic `bot.session_id() -> u64`, and is the **sole owner of connection pump liveness**: dropping the last `Bot` clone immediately stops the pump and clears retained state (`User` holds `Weak<BotCore>` rather than a strong `Arc<BotCore>`, so surviving `Room`, `User`, or `Inbox` handles never form `Arc` cycles and cleanly return `Err(Error::Disconnected(DisconnectReason::BotDropped))`). In v1, games are bounded by connection lifetime: if the TCP/TLS stream drops, every active `Inbox`, `ExclusiveDm`, and `Ask` wakes immediately with `Err(Error::Disconnected(reason))`, cleanly unwinding in-flight game futures (`#[non_exhaustive]` on `Error`, `DisconnectReason`, and `Event` preserves space for a post-v1 `ReconnectPolicy::Resume` without breaking callers). Production bots reconnect in a small outer supervisor loop (`loop { let bot = Bot::connect_with(...).await?; ... }` with backoff) and use `bot.request_shutdown()` / `bot.shutdown_requested()` + `bot.quit(reason).await` for cooperative deploys.
- **Checkpoint-friendly multi-phase games (and play-by-post non-goal):** Never attempt to serialize Rust futures. For 3-minute quizzes or heist encounters, keeping all state in local stack variables is ideal. For longer 45-minute games (like Mafia) that want crash/reconnect resilience across phases, keep an explicit serializable `GameState` struct (scores, roles, current phase) persisted at phase boundaries in SQLite/Postgres and write each **phase** (`run_night(&bot, &room, &mut state).await?`, `run_day(&bot, &room, &mut state).await?`) as a re-enterable `async fn` using direct-style `Ask` / `Inbox` *within* the phase. Multi-day play-by-post games (correspondence Diplomacy, async chess across days) should store state in a database and listen via a stateless `bot.inbox_all()` loop rather than holding suspended stack frames across days.
- **Observability via `tracing` & explicit shared state:** Operator diagnostics (`tokio-console` and structured logs) get out-of-the-box visibility into suspended tasks via built-in `tracing` spans/events on `join`, `breakout`, `exclusive`, and `Ask` (§3.6), while application-specific chat queries (like a `!status` command) expose whatever state they want via ordinary Rust primitives (`Arc<RwLock<...>>` or `tokio::sync::watch`).

---

## 2. Key Architectural Decisions & Justifications

Every major design choice in `ircgm` exists to avoid a specific failure mode. This section documents both the chosen design and the alternatives we rejected.

### 2.1 Thin Connection Pump + Pump-Filtered Per-`Inbox` Channels (Why Not a Global `broadcast`?)

The `irc` crate (`v1.x`) already provides:
- `client.sender() -> irc::client::Sender`: a `Clone + Send + Sync` handle backed by an unbounded FIFO channel. Sending commands is synchronous (non-`async`) and thread-safe. Internally, `ircgm` wraps this in a small **`OutboundSink`** scheduler (§3.5) that owns outbound pacing, priority classes (`Interactive` vs. `Background`), and write receipts (setting `irc`'s own burst limit high so `irc`'s throttle never sits blindly in front of our scheduler, and letting socketless unit tests inspect outbound `Command`s directly without instantiating `irc::client::Client`).
- `client.stream()? -> ClientStream`: a `futures::Stream<Item = Result<irc::proto::Message, irc::error::Error>>` yielding parsed `Message` structs (`Prefix`, `Command`, IRCv3 tags).

We do **not** need a heavy central `Dispatcher` actor for inbound/channel ref-counting. Outbound commands enqueue synchronously into `OutboundSink`.

However, we **must** run a dedicated background connection pump task (paired with our `WorldState`, `UserRegistry`, and `InboxRegistry` trackers), for four mechanical reasons:

1. **Socket liveness in `irc`:** `client.stream()?` can only be called once, and *polling `ClientStream` is what drives the underlying socket I/O in the `irc` crate*—including automatic `PING`/`PONG` keepalive replies and flushing outbound messages queued via `Sender`. If a game calls `tokio::time::sleep(Duration::from_secs(60)).await` while no task polls `ClientStream`, outbound writes stall and the IRC server disconnects the bot with `Ping timeout`.
2. **Independent concurrent consumers:** Whenever two tasks listen at the same time (e.g., an admin `!stop` watcher racing against gameplay, `select_all` across 3 players' DMs, or a `!ping` loop alongside a game loop in `#lobby`), sharing `&Mutex<ClientStream>` would cause Consumer A to read and discard messages meant for Consumer B.
3. **Why Pump-filtered per-`Inbox` `mpsc` channels beat a global `tokio::sync::broadcast`:**
   A single global `broadcast::Sender<Arc<WireEvent>>` (say, 4,096 slots) has a subtle cross-channel contamination flaw: because every `broadcast::Receiver` shares one global write cursor, 5,000 messages in a noisy `#lobby` while a game holds `let mut dm = alice.exclusive().await?;` across a 10-minute `sleep` will lag out Alice's DM `Inbox` (`Err(Error::Lagged)`) even when Alice sent **zero** DMs—and every unrelated server line wakes *every* suspended `Inbox` task just to discard non-matching events.
   Instead, calling `.inbox*()` **synchronously** inserts an `InboxSlot { filter, tx, lagged_skipped, lag_notify, opened_at, ... }` into an indexed `std::sync::Mutex<InboxRegistry>` (bucketed by channel, user ID, and global slots—keeping `.inbox*()` 100% synchronous and race-free before `say()`!), and the pump pushes `Arc<WireEvent>` **only** to matching slots via bounded `mpsc::Sender::try_send`:
   - `#lobby` spam can **never** lag out Alice's DM `Inbox` or `#vault`'s `Inbox`.
   - Non-matching `Inbox` tasks are **never** woken up.
   - Dropping an `Inbox` synchronously removes its slot from `InboxRegistry`.
4. **Single-pass parsing, early unwatched-channel skip, & wire-time identity stamping:** When at least one `Inbox` slot cares about an inbound chat line, the pump parses it *once* into a typed `Arc<WireEvent>` (with formatting-stripped `ChatMessage` text and the sender's `Arc<UserTrack>` stamped at the exact point in the wire sequence when the message arrived), avoiding re-parsing `Text::parse` in every active `Inbox`, skipping `Text::parse` and `UserTrack` allocation entirely on unwatched noisy channels, and preventing rename/quit race conditions when multiple lines arrive in a single TCP read (detailed in §3.2).

### 2.2 Separating Acting from Listening: Why `wait_*` Lives *Only* on `Inbox`

This is the most subtle API boundary in `ircgm`. Consider two loops that look syntactically identical if `wait_cmd` existed directly on `Room`:

```rust
// Loop A: Lobby waiting for a game trigger
loop {
    let msg = lobby.wait_cmd("!heist").await?;
    run_heist(&msg).await?;        // Runs for 20 minutes
}

// Loop B: Recruiting 3 players with an async check
while players.len() < 3 {
    let msg = lobby.wait_cmd("!join").await?;
    verify_account(&msg).await?;   // Runs for 50 milliseconds
}
```

Both loops execute `wait_cmd -> other_async_fn -> repeat`, yet they require **opposite** buffering semantics:

- **Why Loop A breaks if `Room` buffers messages:**
  Suppose `lobby` held a persistent subscription. Five minutes into the 20-minute `run_heist`, someone in `#lobby` jokingly types `!heist`. At minute 20, `run_heist` finishes, the loop calls `lobby.wait_cmd("!heist").await?`, and it immediately matches the 15-minute-old buffered message—launching a stale heist nobody asked for! (The same bug occurs across rounds: a late `!join` typed during Round 1's DM phase would immediately enroll an AFK user into Round 2.)
- **Why Loop B breaks if `Room::wait_cmd` is unbuffered (subscribes on call, drops on return):**
  Even *without* `verify_account`, trace what happens when Alice and Bob type `!join` at nearly the same moment:
  1. Iteration 1 registers `rx1` and suspends on `.await`.
  2. One TCP read / TLS record arrives from the IRC server containing *both* lines (`:Alice!... PRIVMSG #quiz :!join\r\n:Bob!... PRIVMSG #quiz :!join\r\n`).
  3. Because both lines are already decoded in the socket buffer, `ClientStream::poll_next` yields both without suspending: the pump pushes Alice into `rx1` (scheduling the game task without preempting the pump) and immediately pushes Bob into `rx1`.
  4. Iteration 1 wakes up, pops `Alice`, returns, and **drops `rx1`—discarding `Bob`**.
  5. Iteration 2 calls `wait_cmd` again, registering a fresh `rx2` *after* both events were already dispatched. Bob sees his `<Bob> !join` right below `<Alice> !join` in IRC and is completely ignored by the bot.
  *(And if the loop body performs an inner `.await` on the same connection—such as `alice.ask("Confirm buy-in?", ...).await`—the server's wire order is literally `Alice !join -> Bob !join -> Alice DM reply`; awaiting Alice's DM reply physically requires reading past Bob's `!join` while Iteration 2's `wait_cmd` hasn't even been called yet.)*

#### Why we rejected putting unbuffered `wait_*` on `Room` alongside `room.inbox()`
If `room.wait_for(...)` exists directly on `Room`, it acts as an **attractive nuisance**: every user writing a 3-player `!join` loop will reach for `room.wait_for(...)` in autocomplete, test it manually by typing slowly, and ship a race condition.

#### Why we also rejected `let mut lobby = lobby.buffered()` (`Deref<Target = Room>`)
Returning a `Buffered<Room>` wrapper that derefs to `Room` encourages **variable shadowing** (`let mut lobby = lobby.buffered();`) at the top of a function so the author can keep calling `lobby.say(...)`. Without explicit `{}` braces, that shadowed `lobby` stays buffered for the *entire rest of a 60-line game function*, reintroducing the stale-backlog bug across later game phases. Moreover, passing `&Buffered<Room>` by shared reference would silently coerce via `Deref` to unbuffered `Room::wait_*(&self)`.

#### The Chosen Design: `Room` / `User` Act; `Inbox` Waits
- **`Bot`, `Room`, and `User` have NO `.wait_*()` methods.** They expose outbound actions (`.say()`, `.reply()`, `.invite()`), single-turn `.ask()`, and **`.inbox()`**.
- **`Inbox` is the *only* type with `.wait_map()`, `.wait_for()`, `.wait_cmd()`, `.next_chat()`.**
- **Consequence for Loop B (multi-message loops):** Users cannot write `room.wait_cmd` in a loop. They call `.inbox()`, naturally binding `let mut inbox = room.inbox();` *before* `room.say("First 3 to !join...")` and looping on `inbox.wait_cmd("!join").await?`. Nobody accidentally writes `room.inbox().wait_cmd(...)` inside a `while` loop without immediately seeing that they are creating a brand-new inbox on every iteration after `say()`.
- **Consequence for Loop A (one-shot triggers):** Chaining `room.inbox().wait_cmd("!heist").await?` is a clean one-liner whose temporary `Inbox` visibly opens for that wait and drops at the semicolon—leaving zero open receivers during the 20-minute `run_heist`.
- **Multiple concurrent loops in the same `Room`:** Because `Room` is `Clone` with no channel-level mutex and every `room.inbox()` receives an independent fan-out of room events, you can freely spawn multiple orthogonal tasks on the same `Room` (e.g., a `!ping -> !pong` loop alongside a `!heist` game loop—neither steals messages from the other). Just note that because fan-out delivers every message to *all* active `Inbox`es on that room, running two concurrent tasks in the same room that listen for **overlapping** inputs (such as two open-floor `room.ask(..., |m| m.text == "yes")` prompts at once) will satisfy both listeners.

### 2.3 Single Concrete `Inbox` & Multi-Source Wire-Ordered Filtering (`InboxScope`)

Rather than parameterizing `Inbox<S>` across five public marker types (`AllScope`, `RoomScope`, `DmScope`, `PlayerDmScope`, `PlayerRoomScope`) and generic traits, **`Inbox` is a single concrete struct** backed by a set-capable internal `Filter` (`All`, `AllDms`, `UserInRoom`, and `Scoped { rooms, dm_users }`).

Beyond eliminating generic bounds on user helper functions, supporting **multi-source filters** (`bot.inbox_users(&users)`, `bot.inbox_rooms(&rooms)`, and `bot.inbox_scope(InboxScope::new().rooms(...).dms_from(...))`) solves a subtle **fairness bug** in multi-player speed races:
- If a Quiz races `N` separate `user.ask(...)` futures with `futures::future::select_all`, and Bob (`futs[1]`) answers *before* Alice (`futs[0]`) inside the same TCP batch, the pump delivers Bob (`seq = 10`) to `futs[1]`'s inbox and Alice (`seq = 11`) to `futs[0]`'s inbox before waking the task—and `select_all` polls `futs[0]` first, awarding the win to Alice despite losing on the wire!
- Subscribing **one** `bot.inbox_users(&players)` feeds all players' DMs into a single `mpsc` queue in exact `seq` wire order (`msg.sender` identifies who answered), guaranteeing strict wire-order fairness while also unlocking multi-room games (`#red-base` + `#blue-base` CTF or 7-power Diplomacy bridges) without juggling `select!` loops.

### 2.4 Unified `User` Handle, Queued DM Prompts, and Deadlock-Freedom

An earlier revision split users into two types—a stateless `Dm` view (which broke on `/nick` and hung forever on `QUIT`) and a `!Clone` server-global `Player` claim guard (`try_claim_player`). That conflated **three independent concerns**:

1. **Identity & Presence (`NICK` + `QUIT` tracking):**
   There is *never* a reason to want a DM handle that silently breaks when Alice runs `/nick Alice_away` or hangs forever when she disconnects.
2. **Serializing overlapping DM questions (an IRC protocol constraint):**
   Unlike channels (where `#game1` and `#game2` have separate target names), IRC gives each user only **one shared 1-on-1 DM buffer** with the bot. Within a *single* game, two concurrent mechanics can easily trigger DM questions to Alice at the same time (e.g., *"Bob played Steal on you! Block? (yes/no)"* and *"Discard which bonus card?"*).
   - Without DM prompt serialization, both questions land in Alice's DM window simultaneously; she types `"yes"`, and *both* listeners see it!
   - With a `!Clone` claim guard, you couldn't share Alice's handle across concurrent tasks—or re-claiming her failed with `"busy"` instead of queuing the second question!
3. **Cross-game enrollment policy ("Can Alice play two games on this server at once?"):**
   Whether a bot permits playing `#wordle` and `#poker` simultaneously is **application policy**, not a transport constraint—especially once `ExclusiveDm` already keeps concurrent DM prompts from colliding.

#### Unification into `User` (`Clone + Eq + Ord + Hash`) + `ExclusiveDm`
We replace both `Dm` and `Player` with a single **`User`** handle (`bot.user(nick)`):
- **Identity across renames:** Backed by a weak-referenced `Arc<UserTrack>` in `UserRegistry`. `User` is `Clone` and implements `PartialEq + Eq + PartialOrd + Ord + Hash` by tracked `UserTrack::id` (not raw nickname string!), so `HashSet<User>` (unique players in a game) and `HashMap<User, Score>` **automatically survive `/nick` changes and RFC 1459 case differences**! (If an application ever wants cross-game enrollment tracking, it can store `User` directly in any standard `HashSet<User>` or `HashMap<User, GameId>`.)
- **Scoped `ExclusiveDm` (`user.exclusive().await?`) & single-turn `user.ask(prompt, matcher)`:**
  Each `UserTrack` owns an `Arc<tokio::sync::Mutex<()>>` serializing interactive DM prompts to that user. Calling `user.exclusive().await?` acquires that mutex (racing against `QUIT`) and **immediately opens a dedicated `Inbox`** owned by the `ExclusiveDm` guard, supporting multi-message replies (e.g., *"name 3 items, one per message"*) and multi-step validation loops without concurrent prompts cutting in mid-exchange. `User::ask` returns an `Ask` future built directly on top of `exclusive()`:
  ```rust
  // Conceptual execution inside `Ask::poll` for `User::ask`:
  let mut dm = user.exclusive().await?; // 1. Wait turn for DM + subscribe fresh Inbox (abort on QUIT)
  let receipt = dm.send_with_receipt(prompt)?; // 2. Enqueue PRIVMSG synchronously AFTER subscribing
  receipt.await;                        // 3. Wait until OutboundSink actually writes the prompt line(s)!
  // 4. Start reply timeout clock NOW (never docked by mutex wait OR outbound pacing!), await `dm.inbox().wait_map(matcher)`;
  //    if timed out, send configured `timeout_message` and return `Err(Error::TimedOut)`.
  //    Dropping `dm` on return/timeout/cancellation releases both `Inbox` and `dm_mutex`!
  ```
  Notice three properties:
  1. **Lazy & race-free:** Nothing is sent if the future is dropped before being polled, yet `user.exclusive()` subscribes the `Inbox` synchronously *before* `prompt` is queued—so even in `select_all` across 3 different users or microsecond `duplex` unit tests, a reply can never beat subscription.
  2. **Clean per-user queuing:** If two game triggers call `alice.ask(...)` (or `alice.exclusive()`) concurrently, the second waits quietly on the mutex *before* subscribing or sending its prompt. As soon as the first exchange completes, the second subscribes a fresh `Inbox` (never seeing answers from the first exchange) and sends its prompt!
  3. **Queue- and pace-fair reply timer:** Because `Ask` starts its reply deadline *after* `user.exclusive().await?` acquires the slot **and** `OutboundSink`'s write receipt confirms the prompt was actually written, a queued or burst-paced question always gets its full reply window on the player's screen.

#### Deadlock & Head-of-Line Blocking Analysis
Introducing a per-user async `Mutex` requires clear guarantees against every deadlock and starvation path:

1. **No recruitment freeze:** `bot.user(nick)` and `HashSet<User>::insert` are 100% synchronous and non-blocking. Recruiting players in `#quiz` never `.await`s a lock held by another game.
2. **Single-turn `user.ask(...)` is deadlock-free by construction:** Inside `user.ask(prompt, matcher)`, `matcher` is a synchronous `FnMut(&ChatMessage) -> R` (`R: IntoVerdict`, cannot `.await`), so `user.ask` alone can never re-enter itself or lock a second user while holding the first.
3. **One Golden Locking Rule (`ExclusiveDm` / `Huddle`), multi-user locking (`exclusive_many` / `exclusive_available`), & lock-wait diagnostics:**
   - **The One Rule:** *A task holding any `ExclusiveDm` (or `Huddle`) must **never** `.await` `exclusive*()`, `ask()`, or `huddle*()` on the same or another `User`—always acquire multi-user locks upfront in a single `bot.exclusive_many` / `exclusive_available` / `huddle` / `huddle_available` call.* Interact with held users through `&mut dm` / `&mut huddle` (and note that one-way `user.say(...)` bypasses `dm_mutex` so cross-player notifications never deadlock). To make accidental violations immediately obvious in development, every `exclusive*` / `huddle*` call is `#[track_caller]`: if waiting for `dm_mutex` exceeds `15s`, `ircgm` logs a `tracing::warn!` naming both the waiting call site and the `Location` currently holding that user's lock!
   - When a trade or negotiation must lock **all** participants simultaneously, **`bot.exclusive_many(&players).await?`** rejects duplicate users upfront with `Err(Error::InvalidInput)` (preventing self-deadlock while preserving `guards.len() == count`), acquires locks in canonical `UserTrack::id` order racing `QUIT`, and returns guards in the caller's iteration order. When locking a team/pack where disconnected members should simply be skipped (matching `Breakout::invite_available`), call **`bot.exclusive_available(&wolves).await?`**.
   - For secret team deliberations on standard IRC clients where human players don't auto-join `INVITE` channels (`#wolves`), **`bot.huddle(&users).await?`** and **`bot.huddle_available(&wolves).await?`** wrap those ordered `ExclusiveDm` guards + a wire-ordered `inbox_users` into a channel-free **`Huddle`** with group `.say(...)`, `.ask(...)`, and optional peer whisper relay (`.with_relay("[Wolves] ")`).
4. **No zombie queues on `QUIT`:** `exclusive()` and `exclusive_many()` race `dm_mutex.lock_owned()` against `status_rx` (`UserStatus::Quit { .. }`), so if Alice disconnects while 3 exchanges are queued for her, *all* 3 immediately fail with `Err(Error::UserLeft { .. })` (releasing any already-acquired guards).
5. **Bounded head-of-line waiting by default (`300s` default `reply_timeout` on IRC reads + outer `.with_timeout` for non-read work):**
   - `exclusive()` lock acquisition itself has no built-in timeout (so legitimately queued prompts progress sequentially), while every `ask` *and* every raw `dm.inbox()` read inside `ExclusiveDm` defaults to a 300-second idle deadline (`BotConfig::ask_config` default `reply_timeout = 300s`), overridable via `.with_ask_config(&cfg)` / `.set_ask_config(&cfg)` on `ExclusiveDm`, `.set_read_timeout(...)` on `dm.inbox()`, or per call on `Ask`. Even if an IRC user walks away from a 24/7 bouncer (`ZNC` / `soju`) without sending `QUIT`, neither `user.ask(...)` nor a multi-message `let mut dm = user.exclusive().await?; dm.inbox().next_chat().await?;` block can hold `dm_mutex` forever unless `no_reply_timeout()` / `clear_read_timeout()` was explicitly requested.
   - **Important scope note:** This 300s AFK protection covers **IRC read waits only** (`dm.ask` / `dm.inbox()` reads). If user code holds an `ExclusiveDm` guard across a `tokio::time::sleep`, a hung database query, or an unrelated `.await` without polling `dm`, `read_limit` is not running—wrap the exchange in an outer `.with_timeout(dur)` (or drop `dm` before non-IRC awaits) to bound total hold time!
   - If an active `ask` / `exclusive` block times out or gets cancelled via `.until(...)`, dropping its future immediately drops `ExclusiveDm` and advances the next queued caller.
   - When a caller wants a hard wall-clock bound on *mutex queue wait + exchange* (for instance, in case an earlier holder used `no_reply_timeout()`), `GameFutureExt::with_timeout(dur)` (or `.with_deadline(instant)`) wraps the outer future (`tokio::time::timeout` semantics), cancelling cleanly without sending a stale prompt if the lock was never acquired.

### 2.5 Per-`Inbox` Lag Isolation — Strict Scoped Error by Default

Each `Inbox` owns an isolated bounded `tokio::sync::mpsc::Receiver<Arc<WireEvent>>` fed by the pump **only when an event matches that `Inbox`'s `Filter`** (`BotConfig::inbox_capacity = 1024` by default; Tokio allocates `mpsc` capacity lazily in small 32-slot blocks, so an idle `Inbox` uses ~256 bytes), paired with an `Arc<AtomicU64>` skipped-event counter (`lagged_skipped`) and `Arc<tokio::sync::Notify>` (`lag_notify`).

- **Zero cross-channel lag contamination:** Traffic in `#lobby` or a server-wide netsplit `QUIT` storm in unrelated rooms never enters Alice's DM `Inbox` or `#vault`'s `Inbox`. An `Inbox` can only fill up if *its own matching source* emits more than 1,024 unread events while that `Inbox` sits unpolled (e.g., across a slow DB call or `sleep`).
- **Non-blocking pump, 75%-fill early warning, + cooperative burst yielding:** In the pump, `tx.try_send(event)` never blocks on a slow consumer; if an `Inbox` buffer crosses **75% capacity**, the pump emits a one-time `tracing::warn!` naming the `Inbox`'s `scope` and `#[track_caller]` creation site (`opened_at`), and if full, increments `lagged_skipped` and notifies `lag_notify` (yielding cooperatively every 64 consecutive ready socket lines so active consumers drain promptly during large bursts).
- **Strict `Err(Error::Lagged { scope, skipped, opened_at })` by default:** Because `Bot`, `Room`, and `User` hold **no** `Inbox` at rest, overflowing an `Inbox`'s isolated queue means the consumer left an open `Inbox` unpolled across over a thousand matching events while expecting lossless sequential delivery. Every `.inbox*()` constructor carries `#[track_caller]`, and returning **`Err(Error::Lagged { scope, skipped, opened_at })` by default** (naming the exact lagging scope—`"bot"`, `"dm"`, `"room #lobby"`, `"user Alice"`, or `"user Alice in #lobby"`—its `file:line:col` creation site, and the actionable hint *"Did you leave an Inbox open across an .await? Use .clear() or .lossy() if intended."*) makes any flood or forgotten `Inbox` immediately diagnosable, while `inbox.lossy()` opts into warn-and-skip (`tracing::warn!` + continuing from buffered events—recommended for public-prompt multi-voter DM fan-out like `collect_secret_votes`) and `inbox.clear()` drains queued messages and resets `lagged_skipped` (and re-arms the 75% warning).

### 2.6 Targeted RAII: `Room` vs. `Breakout` (`PathBuf` vs. `TempDir`)

Making *every* channel context send `PART` on `Drop` fails on IRC (stateless `!ping` handlers spam `JOIN`/`PART`, DM replies send invalid `JOIN <nick>`, and persistent `#lobby` channels get parted unexpectedly). Conversely, **temporary secret rooms (`Breakout`) *require* RAII cleanup**: if `heist_mission` creates `#vault-7k2m` and gets cancelled mid-flight by `!stop` (`.until(stop)`), a timeout, or `?`, an explicit `vault.close().await` at the end of the function would be skipped, stranding players inside `#vault-7k2m`.

`ircgm` models this after **`PathBuf` vs. `tempfile::TempDir`**:
- **`Room` (`Clone`, no `Drop` side effects):** Lightweight non-owning channel handle.
- **`Breakout` (single-owner RAII guard, `Deref<Target = Room>`):**
  - Creates a fresh unique channel (`#vault-7k2m` by default so the room is guaranteed empty and standard IRCds grant `+o` chanop), awaits `JOIN` + `366 RPL_ENDOFNAMES`, and sets `+is` (invite-only + secret).
  - Because `OutboundSink` exposes a synchronous queue drained by our background scheduler and `WorldState` tracks channel occupants + `005 TARGMAX` in memory, **`Breakout::drop` synchronously enqueues `Background`-priority `KICK` commands (batched only when `005 TARGMAX` allows `KICK > 1`) followed by `PART`—zero `.await` required, 100% cancellation-safe, and never starving subsequent `Interactive` prompts!**
  - Also provides `vault.close().await?` as the awaitable eviction path to explicitly await server confirmation of `PART` and disarm `Drop`.

### 2.7 First-Class `Ask` Timeouts, Handle-Carried `AskConfig`, `Room::ask_user`, & `GameFutureExt`

A tempting but flawed "simplification" would be to keep `ask(...)` as a bare `async fn` with no built-in timeout, no timeout message hook, and no player-directed room variant—telling callers to wrap futures externally with `tokio::time::timeout` and match nicknames manually with `m.nick == user.nick()`. **Do not simplify `Ask` down to an external timeout wrapper.** Real multi-step IRC games break in four concrete ways under that naive model:

1. **Why `AskConfig::default()` (`BotConfig::ask_config`) MUST have a non-`None` default `reply_timeout` (`300s`), with `timeout_message = None`:**
   - **IRC bouncers never `QUIT` when AFK:** Unlike HTTP or CLI tools, IRC users routinely connect via 24/7 bouncers (`ZNC`, `soju`, `TheLounge`). When a human closes their laptop mid-game, no `QUIT` line is sent. If `ask` had no default timeout, any `user.ask(...)` where the author forgot `.reply_timeout(...)` would hold `UserTrack::dm_mutex` forever, permanently wedging all future DM prompts to that user. Defaulting `AskConfig` to a 300s `reply_timeout` guarantees bounded recovery out of the box (while `.no_reply_timeout()` explicitly opts out for long-form correspondence games).
   - **Zero hardcoded natural-language messages:** Conversely, the default timeout *message* MUST be `None` (`no_timeout_message`). A library must never emit hardcoded English strings like `"Timed out!"` into a French RPG channel or a themed game unless the bot author configured one.

2. **Why `Ask` MUST own its timer & message (started on `OutboundSink` write receipt!) instead of relying on an outer `GameFutureExt::with_timeout` wrapper:**
   - **Queue- and pace-fair deadlines on `Ask`:** Suppose two game triggers call `alice.ask(...)` concurrently with a 30s timeout, or a 16-player round sends 20 prompts at once while `OutboundSink` paces outbound lines to avoid server flood kicks. An *outer* `tokio::time::timeout(30s, alice.ask(...))` starts ticking **before the prompt is even written to the wire**—docking the player's reading time by mutex queue wait *and* outbound pacing delay! Because `Ask` owns its deadline and awaits `OutboundSink`'s **write receipt** (which resolves when the prompt's final wrapped line is actually written to the socket channel), every player gets their full reply window on screen.
   - **Lengthening or disabling an inherited timeout:** If a game configures a 30s default timeout on `room`, an *outer* `tokio::time::timeout(120s, room.ask(...))` **cannot lengthen** it for a harder riddle—the inner 30s timer would still fire first! Returning an `Ask` future builder (`room.ask(...).reply_timeout(Duration::from_secs(120))`, `.reply_deadline(instant)`, or `.no_reply_timeout()`) overrides the deadline *before* the timer is constructed.
   - **Eliminating per-step `let Some(x) = ... else` boilerplate:** In a 10-step linear quest, writing `let Some(x) = ... else { room.say("Too slow!")?; return Ok(()); };` ten times obscures the game logic. Instead, when `Ask` expires, it automatically sends its configured `timeout_message` (if any), releases its `Inbox` / `ExclusiveDm`, and returns **`Err(Error::TimedOut)`**—so linear steps simply use `?` (triggering `Breakout` / `ExclusiveDm` RAII cleanup identically to `Error::UserLeft { .. }`).

3. **Why `Bot` & `User` stay config-free, with `AskConfig` as the single prompt-config vocabulary across handles:**
   - Wrapping game bodies in `ask_scope().timeout(30s).run(async { ... }).await` adds two levels of rightward indentation across entire functions, silently loses context across `tokio::spawn` boundaries, and creates an unresolvable precedence paradox if combined with handle settings.
   - Conversely, **`Bot` and `User` are long-lived identity handles** (`User: Eq + Ord + Hash` is stored in `HashSet<User>` and reused across rounds). Attaching `AskConfig` to `User` would violate behavioral equality (`u1 == u2` behaving differently depending on which clone entered a `HashSet` first), leak room-specific `timeout_message` text into later private DMs, and carry expired `Instant` deadlines into future rounds! Keeping `User` 100% config-free also lets the pump stamp `pub sender: User` directly onto every `ChatMessage` and `Event` with zero context arguments.
   - Rather than duplicating 24 differently-named timeout methods across `BotConfig`, `Room`, `Breakout`, and `ExclusiveDm`, **`AskConfig` and `Ask` share a single 1-to-1 builder vocabulary** (`reply_timeout`, `reply_deadline`, `no_reply_timeout`, `warn_remaining`, `clear_warnings`, `ignore_commands`, `accept_notices`, `timeout_message`, `timeout_message_fn`, `no_timeout_message`) applied via a clean 3-level hierarchy:
     1. **Startup defaults on `BotConfig`** (`BotConfig::with_ask_config(cfg)`), stored once in `BotCore::default_ask_config`.
     2. **Phase/place overrides via `AskConfig`:**
        - `Room::with_ask_config(&self, cfg: &AskConfig) -> Room` (borrows `&self` to derive a `Room` view).
        - `Breakout::with_ask_config(mut self, cfg: &AskConfig) -> Self` + `set_ask_config(&mut self, cfg)` (plus `BreakoutOptions::ask_config`), `ExclusiveDm::with_ask_config` / `set_ask_config`, and `Huddle::with_ask_config` / `set_ask_config`.
        - Shared DM rounds pass `&cfg` directly via `ask.config(&cfg)`.
     3. **Per-call `Ask` overrides** (`.config(&cfg)`, `.reply_timeout()`, `.reply_deadline()`, `.no_reply_timeout()`, `.warn_remaining()`, `.ignore_commands()`, `.accept_notices()`, `.timeout_message()`, `.timeout_message_fn()`, `.no_timeout_message()`).

4. **Why `Room::ask_user(&user, prompt, matcher)` + `TimeoutCtx` MUST exist alongside open-floor `Room::ask`:**
   In turn-based channel games (Blackjack, Poker, Trivia turns, RPG combat), prompts happen publicly in `#room`, but it is a **specific player's turn**. Emulating this via `room.ask(prompt, |m| (m.nick == player.nick()).then(...))` has three bugs:
   - **Breaks on `/nick` & RFC 1459 case changes:** Comparing a snapshot `String` misses replies if `[Alice]` renames to `Alice_away` or `{alice}` mid-turn.
   - **Hangs when the player leaves:** If Alice `PART`s `#room`, gets `KICK`ed, or `QUIT`s IRC 2 seconds into a 45-second turn, `room.ask` sits frozen for the remaining 43 seconds instead of failing immediately with `Err(Error::UserLeft { .. })`.
   - **Cannot call out the timed-out player in `#room`:** An open-floor `room.ask` does not know *who* was being asked, so a room-level timeout message cannot say `"Alice: ⏳ too slow!"`.
   `room.ask_user(&player, prompt, matcher)` (also available on `Breakout` via `Deref`) subscribes `player.inbox_in(&room)` before sending `prompt`, does **not** lock `player`'s `dm_mutex` *(note: unlike `User::ask`, two concurrent `room.ask_user` calls to the same player in the same channel are not serialized and will both see one matching reply)*, uses **`room`'s** `AskConfig`, and passes `TimeoutCtx { target, user: Some(player), configured_timeout, elapsed }` to `.timeout_message_fn(...)` so the room's shared handler can call out `player.nick()` live!

5. **One Rule for Timeout Messages, Distinct `.reply_timeout` (`Ask`) vs `.with_timeout` (`GameFutureExt`), `.optional()`, and `.settled()` (`Turn<T>`):**
   - **Single rule for timeout messages:** `timeout_message` / `timeout_message_fn` fires **if and only if** an `Ask`'s own reply timer expires, controlled solely by `timeout_message` / `timeout_message_fn` / `no_timeout_message()` (inherited or explicit). Combinators (`optional`, `settled`, `with_timeout`, `with_deadline`) live **exclusively on `GameFutureExt`** as pure result/future wrappers and **never** emit or suppress messages. (To make an optional prompt quiet on a handle that configured `timeout_message`, chain `.no_timeout_message().optional()` explicitly.)
   - **`Ask::reply_timeout(dur)` / `reply_deadline(instant)` vs `GameFutureExt::with_timeout(dur)` / `with_deadline(instant)`:** Naming `Ask`'s post-send timers `reply_timeout` / `reply_deadline` keeps them visually distinct from outer `with_timeout` / `with_deadline` in IDE autocomplete (never looking like `foo` vs `with_foo` synonyms). Both return `Err(Error::TimedOut)` on expiry so callers matching `Err(Error::TimedOut)` never miss outer timeouts; outer `with_timeout` bounds total wall-clock time (mutex queue + reply) and cancels the inner future on expiry without sending `timeout_message`.
   - **`.optional()`**: pure mapper converting **`Err(Error::TimedOut)` -> `Ok(None)`** while keeping `UserLeft { .. }` and fatal errors as `Err`.
   - **`.settled()` -> `Result<Turn<T>>` (solving the multi-player table & PM-block problem):** Pure mapper catching `Error::TimedOut`, `Error::UserLeft { nick, reason }`, *and* `Error::DeliveryFailed { target, code, message }` (e.g., when a single player has `+g` `callerid` or `+R` registered-only DMs enabled!) into `Ok(Turn::Reply(v) | Turn::TimedOut | Turn::Left { nick, reason } | Turn::Unreachable { target, code, message })`, while propagating fatal connection/protocol errors (`Error::is_fatal() == true`, including `Disconnected`, `Lagged`, `Irc`) via `?`.

### 2.8 Decoupled Supervision, Arbitrary Stop Chains, Graceful Shutdown, & Live Control (`until` / `or_until` + `bot.shutdown_requested()`)

`game_fut.until(s1).or_until(s2).or_until(s3)...` races a game future against **any arbitrary number of stop triggers** (`biased` in registration order so stop signals always win ties, returning a single flat `Outcome<T, S>` with zero `Either` nesting or heap allocation)—plus `ircgm::race_all(iter)` when stop triggers come from a dynamic `0..=N` runtime collection, and `.ok_or_pending()` so `Inbox` stop listeners (`wait_cmd("!stop")`, `wait_cmd("!cancel")`) never cause false stops when a user disconnects. For non-terminating admin commands (e.g., `/msg bot !speed 2.0` alongside `/msg bot !stop`), `Bot` and `Inbox` compose directly with `tokio::select!` and `tokio::sync::watch`.

### 2.9 Native Rich-Text Integration via `irc-colors`

`ircgm` integrates directly with **`irc-colors`** (`../irc-colors`) at both the outbound and inbound boundaries:
1. **`impl Into<ircgm::Text>` everywhere outbound text is accepted** (`say`, `notice`, `action`, `reply`, `ask`, `BreakoutOptions::kick_reason`): plain `&str`/`String` work unchanged, while styled expressions (`"ALERT".bold().white().on_red() + " " + user.nick().bold().yellow()`) pass straight in without `.to_string()`.
2. **Automatic byte-accurate wrapping (`Text::wrap`):** Splits multi-line/overlong messages cleanly within IRC's 512-byte wire limit while preserving active styles across continuation lines.
3. **Formatting-resilient inbound matching + lossless relay (`Text::parse`):** `ChatMessage::text` exposes formatting-stripped plain text (`Text::parse(&raw_text).plain()`) so colored client input like `"\x02!join\x02"` still matches `msg.is_cmd("!join")`, while `ChatMessage::to_styled()` re-parses `raw_text` into `Text` for relays/bridges.
4. **Clean namespacing (`Text` at root + `ircgm::fmt` + `ircgm::prelude`):** `Text` is re-exported at the crate root (since it appears directly in `ircgm` signatures), while `Color, ColorValue, Colorize, Span, Style, Text` live in `ircgm::fmt` (and `Colorize as _` in `ircgm::prelude`) so `ircgm` never pollutes root imports or collides with `colored::Colorize`.

---

## 3. Internal Architecture & Protocol Mechanics

### 3.1 Data-Flow Overview

```mermaid
flowchart LR
    IRC["IRC Server (TCP / TLS)"]
    CS["irc::ClientStream"]
    SND["OutboundSink (Paced Interactive/Background Queue + WriteReceipt)"]
    PUMP["Connection Pump Task"]
    WS["Arc&lt;RwLock&lt;WorldState&gt;&gt;"]
    UR["Arc&lt;UserRegistry&gt; (Weak&lt;UserTrack&gt;)"]
    REG["Arc&lt;Mutex&lt;InboxRegistry&gt;&gt; (Indexed by Chan / UserId / All)"]

    HANDLES["Bot / Room / Breakout / User"]
    IB["Inbox (isolated mpsc::Receiver + lag counter)"]

    IRC -->|inbound lines| CS
    CS --> PUMP
    PUMP -->|1. update channels, self hostmask, &amp; 005| WS
    PUMP -->|2. stamp sender Arc&lt;UserTrack&gt; + seq| UR
    PUMP -->|3. filter &amp; try_send Arc&lt;WireEvent&gt; in wire order| REG

    REG --> IB
    UR -.->|Quit { at_seq } watch| IB

    HANDLES -.->|sync register / Drop remove| REG
    HANDLES -->|Text::wrap (exact 512B budget) + priority enqueue| SND
    WS -.->|nicks + TARGMAX on Breakout::drop| HANDLES
    SND -->|paced writes + receipts| IRC
```

### 3.2 The Connection Pump Task, Registration Barrier, Acyclic `BotOwner` / `BotCore` Split, & In-Band Stamping

When `Bot` is initialized from an `irc::client::Client` (or `ircgm::testing::test_harness`), it takes `client.stream()?` once and spawns the background pump + outbound pacing tasks. To eliminate **`Arc<BotCore>` reference cycles** (which would otherwise form via `BotCore -> WorldState -> Member { User } -> BotCore` and `BotCore -> InboxRegistry -> mpsc -> WireEvent { User } -> BotCore`):
- **`Bot` wraps `Arc<BotOwner>`**, which holds `core: Arc<BotCore>` and, inside `Drop for BotOwner` when the last `Bot` clone drops, aborts the pump/scheduler tasks, marks `core` closed with `DisconnectReason::BotDropped`, **and explicitly clears `WorldState` and `InboxRegistry`**.
- **`User` holds `Weak<BotCore>` + `Arc<UserTrack>`** (rather than a strong `Arc<BotCore>`, upgrading `Weak<BotCore>` on outbound/inbox operations and returning `Err(Error::Disconnected(DisconnectReason::BotDropped))` once `BotCore` drops; `casemap` is stored atomically on `UserRegistry` so `User::eq_nick` works synchronously with zero `Arc` upgrade). `Room`, `Inbox`, and the pump task hold `Arc<BotCore>` (never `Arc<BotOwner>`), so dropping the last `Bot` stops the pump immediately and releases `BotCore` cleanly with zero retained cycles.

#### Registration Barrier (`CAP` + `376`/`422`), `005 CASEMAPPING` Re-keying, & Self-Message Filtering
- **Registration barrier (`CAP` negotiation + `376`/`422`):** On `Bot::connect` / `Bot::connect_with`, `ircgm` sets `irc`'s `max_messages_in_burst` high (`u32::MAX`) so `OutboundSink` is the sole outbound scheduler, negotiates `BotConfig::request_caps` (default: `["account-tag", "account-notify", "extended-join", "multi-prefix", "server-time", "userhost-in-names"]`—preferring `irc::client::data::Config::capabilities` when compatible so `irc`'s built-in SASL / `CAP` state machine never races manual `CAP REQ`, and ignoring `NAK`s on older IRCds), and completes `connect*` **only after `CAP` negotiation finishes (`CAP END`) AND `376 RPL_ENDOFMOTD` or `422 ERR_NOMOTD` arrives** (falling back to `handshake_timeout` if a non-standard server omits both). Waiting for `"first `005`"` or `001` alone is insufficient because `005 RPL_ISUPPORT` routinely spans 2–4 lines between `001` and `376`/`422`. *(If the caller passed initial `Config::channels`, `connect*` withholds them from `irc`'s pre-barrier auto-join and joins them via `Bot::join` after the `376`/`422` barrier so `CASEMAPPING`, `PREFIX`, and `CHANMODES` are already active.)*
- **Dynamic `005 CASEMAPPING` re-keying:** If an `005 CASEMAPPING=...` token changes the active `CaseMap` after any `UserTrack`, `ChannelState`, or `InboxSlot` already exists (for instance via `Bot::from_client` or `TestServer`), `ircgm` atomically updates `casemap` and rebuilds the `FoldedStr` map keys in `WorldState`, `UserRegistry`, and `InboxRegistry` so pre-existing handles stay consistent.
- **Unconditional `echo-message` protection:** Whenever an inbound `PRIVMSG` or `NOTICE` has a sender nickname matching `fold(bot.current_nick)` under the active `CaseMap`, the pump **never** emits it as `WireEvent::Chat` or `WireEvent::Notice` (exposing it only on `next_raw`). Even if a caller wraps a client that negotiated `echo-message`, the bot's own prompts can never self-match an `ask` or `wait_cmd`.

#### Why Naive Out-of-Band State Lookups Break Under TCP Batching
Suppose a single TCP read delivers two lines back-to-back:
1. `:Alice!u@h PRIVMSG #blackjack :hit`
2. `:Alice!u@h NICK :Alice_away` (or `:Alice!u@h QUIT :bye`, followed by `:Mallory!m@h NICK :Alice`)

Because the pump drains ready lines from `ClientStream` before consumer tasks are scheduled:
- If `Inbox` filtered messages by comparing `msg.nick` against `user.nick()` (latest `WorldState`), `"hit"` would be dropped because `user.nick()` is already `"Alice_away"`.
- If `Inbox` aborted immediately on an out-of-band `watch::Receiver<UserStatus>`, `"hit"` would be discarded in favor of `Err(Error::UserLeft { .. })`.
- If `msg.sender` resolved `msg.nick` (`"Alice"`) lazily at call time, it would resolve to `Mallory` instead of `Alice`!

#### Single-Pass Stamping, Outbound Failure Correlation, & Monotonic `seq` Ordering
For each inbound `irc::proto::Message` (yielding cooperatively via `tokio::task::yield_now()` every 64 consecutive ready lines during bursts), the pump assigns a strictly increasing **`seq: u64`** (`1, 2, 3, ...`) and executes in exact order:
1. **Early interest check & sender identity resolution *before* mutating renames/quits:**
   - **Optimization on unwatched channels:** For channel `PRIVMSG` / `NOTICE` lines (excluding self-echoes), if no `account-tag` update is needed for an already-tracked user and `InboxRegistry` has **zero** slots watching `all_slots` or `by_channel[fold(chan)]`, skip `Text::parse` and transient `UserTrack` allocation completely.
   - Otherwise, for `PRIVMSG` (and human-shaped `NOTICE`s), look up or allocate the sender's `Arc<UserTrack>` in `UserRegistry` and stamp `pub sender: User` (`Weak<BotCore>` + `Arc<UserTrack>`) directly onto `ChatMessage` (parsing `Text::parse` once here so no `Inbox` re-parses IRC formatting; tagging `MessageKind::{Privmsg, Action, Notice}`, plus filtering `NOTICE` eligibility for `.accept_notices()`).
   - For `JOIN`, `PART`, `KICK`, `NICK`, and `QUIT` (and server/service `NOTICE`s), look up or allocate the affected `User` (`Arc<UserTrack>`) and affected channels (`WorldState` channel list prior to removal) and stamp them directly onto the `WireEvent` / `Event`.
2. **Update `WorldState` & `UserRegistry` (including delivery refusals, account updates, & bot room loss):**
   - Process `001 RPL_WELCOME`, `005 RPL_ISUPPORT` (including `TARGMAX` and dynamic `CASEMAPPING` re-keying), `331 RPL_NOTOPIC`, `332 RPL_TOPIC`, `376`/`422` registration barrier, `396 RPL_HOSTHIDDEN`, `JOIN` (incl. `extended-join` account), `PART`, `KICK`, `QUIT`, `NICK`, `TOPIC`, `ACCOUNT` (`account-notify`), `353 RPL_NAMREPLY`, `366 RPL_ENDOFNAMES`, and channel `MODE` deltas (updating `UserTrack::account` whenever `account-tag`, `extended-join`, or `ACCOUNT` is observed).
   - **Self departure (`PART` / `KICK` of `bot.current_nick`):** Remove the channel from `WorldState` and emit `WireEvent::BotLeftRoom { channel, reason }` so any active room `Inbox` or `Ask` on that channel aborts immediately with `Err(Error::BotLeftRoom { .. })` instead of waiting 300s.
   - **Asynchronous send refusals (`401`, `403`, `404`, `442`, `486`, `492`, `531`, `716`) & stale-op `482`:** Emit `WireEvent::SendError { target, code, message, raw }` (also surfaced on `Inbox::next_event()` as `Event::DeliveryFailed`) for channel and PM-block refusals—including `486 ERR_NONONREG` (`+R`), `492 ERR_NOCTCP`, `531 ERR_CANTSENDTOUSER`, and `716 RPL_TARGUMODEG` (`+g` `callerid`, extracting the target nick at `params[1]`; on `401`, also marking any matching `UserTrack` offline with `UserStatus::Quit { at_seq: seq, netsplit: false, .. }`)—so an undeliverable `ask` fails in milliseconds with non-fatal `Err(Error::DeliveryFailed { .. })` (mapped by `.settled()` to `Turn::Unreachable`) or `Err(Error::UserLeft { .. })` instead of hanging for 300s, and games dealing one-way secret DMs can observe bounces. If `482 ERR_CHANOPRIVSNEEDED` arrives asynchronously after an in-flight moderation command raced with a server-side de-op, log `tracing::warn!` (and expose the numeric on `next_raw`) without polluting unrelated chat `Ask`s.
   - On `NICK`: re-key `fold(old_nick) -> fold(new_nick)` on the same `UserTrack` and update its live nickname.
   - On `QUIT`: detect `netsplit: bool` (`"a.b c.d"` server-pair heuristic), remove `fold(nick)` from `UserRegistry`, and publish `UserStatus::Quit { at_seq: seq, nick, reason, netsplit }` on that track's `watch` channel.
3. **Indexed Per-`Inbox` Dispatch (Synchronous `InboxRegistry`):**
   - `InboxRegistry` indexes active `InboxSlot`s into `all_slots: Vec<SlotId>`, `by_channel: HashMap<FoldedStr, Vec<SlotId>>`, and `by_user: HashMap<u64, Vec<SlotId>>`. On each event, lock `InboxRegistry` briefly and visit only the candidate buckets (`all_slots` + relevant `by_channel` / `by_user` buckets, deduplicating multi-bucket or multi-channel `Quit`/`NickChanged` hits via `slot.last_dispatched_seq == seq` with zero per-event allocation), calling `slot.tx.try_send(event.clone())` for matching slots (emitting a one-time `tracing::warn!` at 75% capacity, and incrementing `slot.lagged_skipped` + notifying `slot.lag_notify` if full).
   - **Anti-simplification guardrail:** `Inbox` registration (`room.inbox()`, `user.inbox()`, `bot.inbox_*()`) and `Drop` deregistration **must** mutate `Mutex<InboxRegistry>` synchronously—never via an async command channel to the pump—so `let inbox = room.inbox(); room.say(prompt)?;` is guaranteed subscribed before `prompt` is sent!

**Resulting ordering invariants:**
- **Wire-order `Inbox` delivery:** Open `Inbox`es match by stamped `UserId` (not string nick!) and process `Parted` / `Kicked` / `Quit` / `BotLeftRoom` / `SendError` **in-band**. Even when `status_rx` reports `Quit { at_seq, .. }`, an `Inbox` drains all buffered events with `event.seq < at_seq` first—so `"hit"` at `seq = 50` before `QUIT` at `seq = 51` is **always** delivered, while an `Inbox` subscribed *after* `seq = 51` sees an empty buffer (`>= at_seq`) and fails immediately with `Err(Error::UserLeft { .. })`!
- **Snapshot vs. live state:** `Event` payloads (`msg.nick`, `msg.sender`) reflect exact wire-time sender identity, whereas direct handle accessors (`bot.nick()`, `msg.sender.nick()`, `room.nicks()`) reflect the latest processed `WorldState`.

### 3.3 `WorldState` & IRC Casemapping (`CaseMap`)

While the async pump loop itself is small, **`WorldState` is a dedicated, unit-tested protocol state machine**. Under IRC's default `rfc1459` casemapping, `[`, `]`, `\`, `~` are the uppercase equivalents of `{`, `}`, `|`, `^` (`[Admin]` and `{admin}` are the **same nickname**!). `WorldState` tracks:
- **`casemap: CaseMap`:** Defaults to `CaseMap::Rfc1459`; updated from `005 CASEMAPPING=ascii | rfc1459 | strict-rfc1459` (re-keying `WorldState`, `UserRegistry`, and `InboxRegistry` if changed dynamically).
- **`chan_types`, `status_prefixes`, `max_modes`, & `targmax_kick`:** Defaults to `['#', '&']`, `[('o', '@'), ('v', '+')]`, `max_modes = 4`, and `targmax_kick = 1`; updated from `005 CHANTYPES`, `005 PREFIX=(ov)@+`, `005 MODES=N` (used to batch `Room::voice_many` / `devoice_many`), `005 TARGMAX=...,KICK:N,...` (used to batch `Breakout::drop` `KICK #c a,b,c` **only** when the server explicitly advertises `KICK:N` with `N > 1`—never assumed, since many IRCds enforce `KICK:1`), and `CHANMODES` arity rules so parameterized modes like `+k key` or `+l 10` don't misparse subsequent `+o nick` arguments.
- **`current_nick: String` & `self_user_host: Option<String>`:** `current_nick` updates on `001 RPL_WELCOME` and self `NICK`; `self_user_host` (`"user@host"`) is learned dynamically from self `JOIN`/`NICK` prefixes, `396 RPL_HOSTHIDDEN` cloak changes, or trailing `nick!user@host` tokens in `001`.
- **`channels: HashMap<FoldedStr, ChannelState>`:** Tracks `topic: Option<String>` (`332 RPL_TOPIC`, `331 RPL_NOTOPIC`, `TOPIC`), accumulates multi-line `353 RPL_NAMREPLY` batches into a staging set until `366 RPL_ENDOFNAMES` atomically commits the occupant list (`names_synced = true`), and updates occupant `User` handles, nicknames, and status prefixes (`+o`, `+v`, including IRCv3 `multi-prefix` / `userhost-in-names` and `extended-join`) across `JOIN`, `PART`, `KICK`, `QUIT`, `NICK`, and `MODE`.

### 3.4 `UserRegistry`: Stamped Identity, Amortized GC, `netsplit` Detection, `account()`, & Channel-Recruitment Invariant

```rust
struct UserRegistry {
    // Weak references ensure random channel chatter never leaks unbounded memory!
    // Amortized sweep runs when `tracks.len() > 2 * last_live_count.max(32)` during registry access
    // (NEVER inside `Drop for UserTrack`, which could re-enter this Mutex!).
    state: std::sync::Mutex<RegistryState>, // { tracks: HashMap<FoldedStr, Weak<UserTrack>>, last_live_count: usize }
    casemap: AtomicU8,
    next_id: AtomicU64,
}

struct UserTrack {
    id: u64,                                        // Stable key for `Eq`, `Ord`, & `Hash` across `/nick`!
    current_nick: std::sync::RwLock<String>,
    account: std::sync::RwLock<Option<String>>,     // Updated via `account-tag`, `extended-join`, & `account-notify`.
    status_tx: tokio::sync::watch::Sender<UserStatus>,
    dm_mutex: Arc<tokio::sync::Mutex<()>>,          // Serializes `User::exclusive()` / `User::ask`.
    lock_holder: std::sync::Mutex<Option<&'static std::panic::Location<'static>>>, // For >15s lock-wait warnings.
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum UserStatus {
    Online,
    Quit {
        at_seq: u64,
        nick: String,
        reason: Option<String>,
        netsplit: bool, // `true` when `reason` matches IRC's `"server1.tld server2.tld"` netsplit signature.
    },
}
```

- **Zero retained memory & deadlock-free amortized GC:** Each `ChatMessage` and `Event` holds a `User` (`Weak<BotCore>` + `Arc<UserTrack>`) stamped when the line arrived. `QUIT` removes map keys immediately, while dead `Weak` entries left behind by `PART`/`KICK` of unheld users are swept via an amortized scan whenever `tracks.len() > 2 * last_live_count.max(32)` during normal registry access—**never** inside `Drop for UserTrack`, avoiding mutex re-entrancy deadlocks if the last `Arc<UserTrack>` drops while `RegistryState` is locked.
- **Stable identity across `/nick`, new `UserTrack` after `QUIT`+rejoin, & keying long games on `account()`:** Calling `bot.user("Alice")` twice while a `User` clone (or buffered event) is alive upgrades the same `Weak<UserTrack>` (same `id` and `dm_mutex`). If Alice renames to `"Alice_AFK"`, `UserRegistry` re-keys `"alice" -> "alice_afk"` on the same `UserTrack` (`bot.user("Alice_AFK")? == existing_alice` is `true`!). Conversely, **a `QUIT` followed by a later rejoin intentionally allocates a fresh `UserTrack`** (preventing an unauthenticated stranger from inheriting a disconnected player's seat by taking their nick); long-running games that allow players to reconnect across disconnects or netsplits (`Event::Quit { netsplit: true, .. }`) should key persistent player seats by **`user.account()`**.
- **Shared-channel presence invariant (recruit via a channel):** Per the IRC protocol, servers only broadcast proactive `NICK` and `QUIT` events for users who share **at least one channel** with the bot. **DM-only users sharing zero channels with the bot have best-effort `NICK`/`QUIT` visibility** (`ircgm` marks them offline when an outbound send triggers `401 ERR_NOSUCHNICK`, avoiding fragile server-capped `MONITOR` lists). Games should therefore always recruit players through a channel the bot remains joined to.

### 3.5 Outbound Priority Scheduler, Write Receipts, Dynamic Byte Budgeting, and Injection Safety (`OutboundSink` + `irc-colors`)

Routing all outbound commands through `OutboundSink` + `irc_colors::Text` solves flood pacing, `Ask` timer accuracy, `Breakout::drop` starvation, CRLF/CTCP injection, and 512-byte truncation in one unified pipeline:
1. **`OutboundSink` priority queues, pacing, & `WriteReceipt`s (replacing `irc`'s blind burst throttle):**
   - `Bot::connect*` sets `irc::client::data::Config::max_messages_in_burst` high (`u32::MAX`) so `irc`'s internal throttle never sits invisibly ahead of `ircgm`'s scheduler, and `OutboundSink` paces writes according to `BotConfig::{outbound_burst, outbound_window}` (default: `15` lines per `8s` rolling window, logging a `tracing::warn!` when pacing kicks in; `ircgm::testing::test_harness` defaults to unthrottled `usize::MAX` unless configured explicitly for pacing tests).
   - **Two priority classes (FIFO within each class):**
     - **`Interactive` (high priority):** `PRIVMSG`, `NOTICE`, `INVITE`, `MODE`, `TOPIC`, `JOIN`, and `send_raw`. Because all chat and moderation commands share one FIFO queue, per-target line ordering (including multi-line wrapped `say` chunks and `say` before `MODE`) is strictly preserved!
     - **`Background` (low priority):** `KICK` and `PART` (such as `Breakout::drop` evicting 12 players at once). Background teardown drains whenever `Interactive` is empty, so dropping a `Breakout` never delays the next phase's `Ask` prompts in `#lobby`! *(Channel-reuse safety: before emitting an `Interactive` `JOIN <chan>`, the scheduler flushes any pending `Background` `KICK`/`PART` items for `<chan>` first so `JOIN #foo` can never jump ahead of an earlier `PART #foo`; `QUIT` drains both queues in order before closing.)*
     - **Awaitable eviction vs. background `Drop`:** Callers that want deterministic wait-until-gone teardown call `vault.close().await?`, whereas synchronous `Breakout::drop` relies on `Background` priority + `005 TARGMAX` `KICK` batching (`KICK #c a,b,c` up to `WorldState::targmax_kick`, default `1`).
   - **Write receipts (`WriteReceipt`) for accurate `Ask` clocks:** Public `.say()`, `.notice()`, `.action()`, and `.reply()` remain synchronous (`Result<()>`), while internal prompt senders receive a lightweight `WriteReceipt` (`oneshot::Receiver<()>`) resolved by `OutboundSink` the instant the final wrapped line of the prompt is actually written. **`Ask` starts its `reply_timeout` and `warn_remaining` clocks on that `WriteReceipt`, never on enqueue!**
2. **Control-code, NUL, & CTCP `\x01` sanitization:** Converting `&str` / `String` into `Text` automatically strips raw IRC formatting bytes and `\0` via `irc-colors`. `say`, `notice`, and `reply` strip embedded `\x01` bytes from spans before wrapping; `action()` strips embedded `\x01` and wraps each emitted chunk in `\x01ACTION <chunk>\x01`.
3. **Exact 512-byte relayed wire budgeting via dynamic self-hostmask + `Text::wrap(budget)`:**
   - IRC's 512-byte line limit applies to the *server-relayed* frame `:<nick>!<user>@<host> PRIVMSG <target> :<payload>\r\n`.
   - `WorldState` computes `wire_max = 512 - (1 + current_nick.len() + 1 + user_host_len + cmd_overhead + target.len() + 4)`, using the exact byte length of `self_user_host` once observed (from self `JOIN`, `396` cloak, or `001`), or a conservative `76`-byte `user@host` reserve beforehand—recalculating automatically whenever our nick or cloak changes!
   - The payload budget passed to `text.wrap(budget)` is `min(config.max_payload_bytes, wire_max)` (minus `9` bytes in `action()`), splitting on `\r\n`/`\n`/`\r`, wrapping on word/grapheme boundaries, preserving active styles across continuation lines, and omitting empty chunks (avoiding `412 ERR_NOTEXTTOSEND`).
4. **Identifier validation:** Protocol targets (`join`, `invite`, `part`, `kick` target) reject `\r`, `\n`, and `\0` with `Err(Error::InvalidInput)`.

### 3.6 Observability via `tracing`

Because a direct-style game's state lives on its async call stack, `ircgm` instruments all lifecycle and wait boundaries with `tracing` (using async-safe `tracing::Instrument` / poll-scoped enters, never holding `Span::enter` guards across `.await` points):
- **`Bot::join`, `Bot::breakout_with`, and `User::exclusive`:** Instrumented with structured spans recording `channel`, `user_id`, and `nick` (plus a `tracing::warn!` naming the current holder's `#[track_caller]` `Location` if `dm_mutex` wait exceeds 15s, and a one-time `tracing::warn!` if an `Inbox` buffer crosses 75% capacity).
- **`Ask` wait diagnostics:** Emits `tracing::debug!` events (within an `ircgm::ask` span carrying `target`, `user`, and `timeout_ms`) when a prompt's `WriteReceipt` resolves and the task suspends waiting for a reply, as well as on match, timeout expiry, `UserLeft`, `DeliveryFailed`, or cancellation—making "who is this stuck game task waiting on?" immediately visible in both standard logs and `tokio-console`.

---

## 4. Public API Specification

### 4.1 Summary of Core Types & Naming Conventions

| Type | Clone? | `Drop` Behavior | Role |
|---|---|---|---|
| `Bot` | `Clone` (`Arc<BotOwner>`) | Last `Bot` drops -> stops pump & clears state | Connection session owner (`session_id()`); joins channels, creates `Room`/`User`/`Breakout`/`Huddle`, `exclusive_many`/`exclusive_available`, `shutdown_requested`, `inbox_all()`, `inbox_dms()`, `inbox_users()`, `inbox_rooms()`, `inbox_scope()` |
| `Room` | `Clone` | None | Lightweight channel view + `with_ask_config(&cfg)` + membership/moderation (`say`, `ask`, `ask_user`, `members`, `member`, `nicks`, `has_user`, `bot_is_op`, `kick`, `set_mode`, `voice`, `voice_many`, `devoice`, `devoice_many`, `topic`, `set_topic`, `inbox`) |
| `User` | `Clone` (`Eq + Ord + Hash`) | Last clone drops `UserTrack` | **Config-free & cycle-free** (`Weak<BotCore>`) `NICK`/`QUIT`-aware session identity handle (`nick()`, `eq_nick()`, `account()`, `ask()`, `exclusive()`, `say()`, `inbox()`, `inbox_in(&room)`) |
| `ExclusiveDm` | `!Clone` | Releases per-user DM mutex & `Inbox` | Multi-message / multi-turn exclusive DM guard (`say`, `ask`, `inbox(&mut self)`, `with_ask_config()`, `set_ask_config()`, `with_ignore_commands()`, `with_accept_notices()`) |
| `Huddle` | `!Clone` | Releases all members' DM mutexes & `Inbox` | Channel-free group DM deliberation guard (`users()`, `online_users()`, `say`, `ask`, `with_relay()`, `relay_to_others()`, `inbox(&mut self)`, `with_ask_config()`, `set_ask_config()`, `with_accept_notices()`) |
| `Ask` | `!Clone` (`Future`) | Drops active `Inbox` / `ExclusiveDm` | Lazy prompt + reply future builder (`Option<T>` or `Verdict<T>`, shares 1-to-1 vocabulary with `AskConfig`: `.reply_timeout()`, `.warn_remaining()`, `.clear_warnings()`, `.ignore_commands()`, `.accept_notices()`, `.timeout_message()`, `.no_timeout_message()`, `.config()`) |
| `Breakout` | `!Clone` (`Deref<Target = Room>`) | Enqueues `Background` `KICK`s + `PART` | `#[must_use]` exclusive temporary room (`to_room()`, `with_ask_config()`, `set_ask_config()`, `invite_and_wait`, `invite_available`, `close().await`) |
| `Inbox` | `!Clone` (`Stream`) | Deregisters slot & drops `mpsc::Receiver` | **Sole owner of `wait_*` methods** (`wait_map`, `wait_for`, `wait_cmd`, `next_chat`, `next_event`, `with_read_timeout` / `set_read_timeout`, `ignore_commands`, `accept_notices`) |

#### Naming Conventions Across `ircgm`
- **Builders (`AskConfig`, `Ask`, `BreakoutOptions`, `InboxScope`):** Bare consuming method names (`reply_timeout`, `warn_remaining`, `timeout_message`, `no_timeout_message`, `config(&cfg)`, `ignore_commands`, `accept_notices`).
- **Long-lived handles & RAII guards (`Room`, `Breakout`, `ExclusiveDm`, `Huddle`, `BotConfig`):** `with_*` for consuming/deriving updates (`with_ask_config`, `with_relay`), `set_*` for in-place `&mut self` mutation (`set_ask_config`, `set_relay`, `set_read_timeout`), and `without_*` for consuming off-switches (`without_relay`, `without_read_timeout`).
- **`to_*` for clones or re-parsing conversions:** `Breakout::to_room(&self) -> Room` and `ChatMessage::to_styled(&self) -> Text`.
- **`*_with` for config/options constructors:** `Bot::connect_with`, `Bot::from_client_with`, `Bot::breakout_with`, `testing::test_harness_with`.
- **`cmd` means chat command ONLY:** `is_cmd`, `cmd_args`, `wait_cmd`, and `command_prefixes` always refer to user `!commands` in chat—never raw IRC protocol commands (`TestServer::recv_sent` / `try_recv_sent`, `Bot::send_raw`).
- **Import hygiene:** Prefer `use ircgm::prelude::*;` in bot code. Never glob-import `ircgm::*` and `irc::*` together (`User` collides), and never bring `colored::Colorize` and `ircgm::fmt::Colorize` into the same lexical scope.

> **Thread-safety guarantee (`Send` / `Sync`):** All core handle and guard types (`Bot`, `Room`, `User`, `Member`, `AskConfig`, `InboxScope`, `MessageKind`, `ChatMessage`, `Event`, `Breakout`, `ExclusiveDm`, `Huddle`, `Inbox`) are `Send + Sync` (verified via static compile-time assertions), and every future returned by `ircgm` (including `Ask`) is `Send` when its closure/output is `Send`.

### 4.2 Modules (`fmt`, `prelude`, `future`, `testing`), Re-Exports, `BotConfig`, & `Bot`

```rust
// Root `ircgm` public re-exports & submodules:
pub use irc;
pub use irc_colors::{self, Text};

pub mod fmt {
    pub use irc_colors::{Color, ColorValue, Colorize, Span, Style, Text};
}

pub mod future {
    pub use crate::supervise::{
        MapOutput, OkOrPending, Optional, OrUntil, RaceAll, Settled, Until, WithDeadline, WithTimeout,
    };
}

pub mod prelude {
    pub use crate::{
        fmt::Colorize as _, AskConfig, Bot, BotConfig, Breakout, ChatMessage, Error, Event, ExclusiveDm,
        GameFutureExt as _, Huddle, Inbox, InboxScope, MessageKind, Outcome, Result, Room, Target, Text,
        Turn, User, Verdict,
    };
}

pub use crate::{
    ask::{Ask, AskConfig, IntoVerdict, TimeoutCtx, Verdict},
    bot::{Bot, BotConfig, ShutdownRequested},
    breakout::{Breakout, BreakoutNaming, BreakoutOptions, InviteTarget},
    casemap::{CaseMap, FoldedStr},
    error::{DisconnectReason, Error, Result},
    huddle::Huddle,
    inbox::{Inbox, InboxScope},
    message::{ChatMessage, Event, MessageKind, Target},
    room::{Member, Room},
    supervise::{race_all, GameFutureExt, Outcome, Turn},
    user::{ExclusiveDm, User},
};

/// Unconditional in-memory test harness module (zero extra Cargo dependencies—no feature flag required!).
pub mod testing {
    use crate::{Bot, BotConfig};

    /// Creates an in-memory `(Bot, TestServer)` pair with zero sockets (`test_harness` uses unthrottled outbound
    /// writes for instant unit tests; `test_harness_with` honors custom `BotConfig` including `outbound_burst` pacing tests).
    pub fn test_harness(initial_nick: impl Into<String>) -> (Bot, TestServer);
    pub fn test_harness_with(initial_nick: impl Into<String>, config: BotConfig) -> (Bot, TestServer);

    /// Deterministic in-memory test harness peer for driving `Bot` without sockets or `irc::client::Client`.
    pub struct TestServer { /* private inbound_tx, outbound_rx, pending_cmds, & Arc<BotCore> */ }

    impl TestServer {
        /// Parses a raw IRC wire line (e.g. `":Alice!u@h PRIVMSG #q :!join\r\n"`) and injects it into the pump.
        pub fn send_line(&self, raw_line: &str);
        /// Convenience helper: injects `:<nick>!~<nick>@test.host PRIVMSG <channel> :<text>\r\n`.
        pub fn chat(&self, nick: &str, channel: &str, text: &str);
        /// Convenience helper: injects `:<nick>!~<nick>@test.host PRIVMSG <bot_nick> :<text>\r\n` (using live `bot.nick()`).
        pub fn dm(&self, nick: &str, text: &str);
        /// Convenience helper: injects self `JOIN`, `353 RPL_NAMREPLY` (`@<bot_nick>` + `names`), and `366 RPL_ENDOFNAMES`.
        pub fn complete_join(&self, channel: &str, names: &[&str]);
        /// Convenience helper: injects `:<nick>!~<nick>@test.host QUIT :<reason>\r\n`.
        pub fn quit_user(&self, nick: &str, reason: Option<&str>);
        /// Awaits an in-band barrier confirming the pump has processed every line injected prior to this call.
        pub async fn settle(&self);
        /// Simulates socket EOF / server disconnect (`Error::Disconnected(DisconnectReason::Eof)` on all active waits).
        pub fn disconnect(self);
        pub async fn recv_sent(&mut self) -> Option<irc::proto::Command>;
        pub fn try_recv_sent(&mut self) -> Option<irc::proto::Command>;
        /// Drains ready outbound `PRIVMSG`/`NOTICE` payloads (plain-stripped) addressed to `target` (case-folded),
        /// while preserving commands addressed to other targets or non-message commands (`MODE`, `KICK`, `PART`) in `recv_sent` order!
        pub fn take_messages_to(&mut self, target: &str) -> Vec<String>;
    }
}

/// Server-negotiated IRC case-folding rule (`005 CASEMAPPING=rfc1459 | strict-rfc1459 | ascii`; default `Rfc1459`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CaseMap {
    #[default]
    Rfc1459,
    StrictRfc1459,
    Ascii,
}

impl CaseMap {
    pub fn fold(self, s: &str) -> FoldedStr;
    /// Allocation-free case-folded string equality (`str_eq` avoids shadowing `PartialEq::eq` on `CaseMap`).
    pub fn str_eq(self, a: &str, b: &str) -> bool;
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FoldedStr(String);

impl FoldedStr {
    pub fn as_str(&self) -> &str;
}

/// Sealed target trait accepted by `Room::invite` and `Breakout::invite_and_wait`
/// (implemented for `User`, `str`, and `String`).
pub trait InviteTarget: private::Sealed { /* resolves live nick + optional UserTrack */ }

pub struct BotConfig {
    pub irc: irc::client::data::Config,
    /// Per-inbox bounded event queue size (default: 1024; Tokio allocates `mpsc` blocks lazily on demand).
    pub inbox_capacity: usize,
    /// IRCv3 capabilities requested via `CAP REQ` on connect (default:
    /// `["account-tag", "account-notify", "extended-join", "multi-prefix", "server-time", "userhost-in-names"]`; `NAK`s are ignored).
    pub request_caps: Vec<String>,
    /// Command prefixes skipped when `.ignore_commands()` is explicitly opted into on an `Inbox`, `Ask`, `AskConfig`, or `ExclusiveDm`
    /// (default: `vec!["!".into()]`; never active globally unless `.ignore_commands()` is configured).
    pub command_prefixes: Vec<String>,
    /// Default timeout for protocol handshakes like JOIN / NAMES / Breakout setup / quit drain (default: 15s).
    pub handshake_timeout: Duration,
    /// Startup bot-wide `AskConfig` defaults inherited by `Room`, `Breakout`, `User::ask`, and `ExclusiveDm`
    /// (default: `AskConfig::default()` — 300s `reply_timeout`, `no_timeout_message`, `ignore_commands = false`).
    /// Prevents AFK bouncer users from holding a `User`'s `dm_mutex` forever when `.reply_timeout()` is omitted.
    pub ask_config: AskConfig,
    /// Upper ceiling on payload bytes per outbound PRIVMSG/NOTICE passed to `Text::wrap` (default: 450;
    /// dynamically clamped further by exact `512 - self_hostmask_and_target_overhead`).
    pub max_payload_bytes: usize,
    /// Outbound scheduler burst allowance and rolling window (default: `15` lines per `8s` on live connections;
    /// `testing::test_harness` defaults `outbound_burst` to `usize::MAX` unless overridden via `test_harness_with`).
    /// `Bot::connect*` sets `irc.max_messages_in_burst = Some(u32::MAX)` so `OutboundSink` is the sole outbound pacer.
    pub outbound_burst: usize,
    pub outbound_window: Duration,
}

impl BotConfig {
    pub fn new(irc: irc::client::data::Config) -> Self;
    pub fn with_ask_config(mut self, cfg: AskConfig) -> Self;
    pub fn with_command_prefixes<I, S>(mut self, prefixes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>;
}

/// Multi-source filter builder for `Bot::inbox_scope` (delivers matching channels and/or DMs into one wire-ordered `Inbox`).
#[derive(Clone, Debug, Default)]
pub struct InboxScope { /* room names + DM User tracks */ }

impl InboxScope {
    pub fn new() -> Self;
    pub fn room(self, room: &Room) -> Self;
    pub fn rooms<'a, I: IntoIterator<Item = &'a Room>>(self, rooms: I) -> Self;
    pub fn dm_from(self, user: &User) -> Self;
    pub fn dms_from<'a, I: IntoIterator<Item = &'a User>>(self, users: I) -> Self;
}

impl Bot {
    /// Connects with default `BotConfig` settings, disables `irc`'s redundant burst throttle in favor of `OutboundSink`,
    /// negotiates IRCv3 `request_caps` (`CAP END`), awaits `376 RPL_ENDOFMOTD` or `422 ERR_NOMOTD` (so multi-line `005`
    /// `ISUPPORT` tokens are fully active before returning; `handshake_timeout` fallback), and joins any initial channels.
    pub async fn connect(irc_config: irc::client::data::Config) -> Result<Self>;
    pub async fn connect_with(config: BotConfig) -> Result<Self>;

    /// Wraps an already-constructed `irc::client::Client` (takes `client.stream()?` once; tip: set
    /// `irc_config.max_messages_in_burst = Some(u32::MAX)` beforehand so `OutboundSink` owns pacing).
    pub fn from_client(client: &mut irc::client::Client) -> Result<Self>;
    pub fn from_client_with(client: &mut irc::client::Client, config: BotConfig) -> Result<Self>;

    /// Monotonic process-unique connection session ID (`1, 2, 3, ...`), enabling higher layers to distinguish handles across outer reconnects.
    pub fn session_id(&self) -> u64;
    pub fn nick(&self) -> String;
    pub fn casemap(&self) -> CaseMap;

    /// Lightweight views (`Room` inherits `BotConfig::ask_config`; `User` is config-free session identity).
    /// Note: `Bot` owns pump liveness—if all `Bot` clones drop, surviving `Room`/`User`/`Inbox` handles return `Err(Error::Disconnected(DisconnectReason::BotDropped))`.
    pub fn room(&self, channel: &str) -> Result<Room>;
    pub fn user(&self, nick: &str) -> Result<User>;

    /// Strict all-or-nothing multi-user DM lock: acquires `ExclusiveDm` guards in canonical `UserTrack::id` order
    /// (preventing A↔B deadlocks) and returns them in the caller's iteration order (`guards.len() == count`).
    /// Accepts `&players` (`&[User]` / `&Vec<User>`), `[&alice, &bob]`, or `&HashSet<User>`.
    /// Fails upfront with `Err(Error::InvalidInput)` if `users` contains duplicates or a `User` from another `Bot`,
    /// and aborts with `Err(Error::UserLeft { .. })` (releasing already-acquired guards) if any user is offline or quits.
    /// (One locking rule: never call while this task already holds an `ExclusiveDm` or `Huddle`!)
    #[track_caller]
    pub async fn exclusive_many<'a, I>(&self, users: I) -> Result<Vec<ExclusiveDm>>
    where
        I: IntoIterator<Item = &'a User> + Send;

    /// Partial/team-resilient counterpart to `exclusive_many` (matching `Breakout::invite_available`):
    /// skips any `User` who is already offline or quits during lock acquisition, locking all remaining online users
    /// in canonical `UserTrack::id` order and returning their guards in caller relative order (`Ok(Vec::new())` if none remain online).
    /// Still fails upfront with `Err(Error::InvalidInput)` on duplicate users or cross-`Bot` handles.
    #[track_caller]
    pub async fn exclusive_available<'a, I>(&self, users: I) -> Result<Vec<ExclusiveDm>>
    where
        I: IntoIterator<Item = &'a User> + Send;

    /// Channel-free secret group DM deliberation (zero `/join` friction on standard IRC clients):
    /// `huddle` locks all `users` strictly via `exclusive_many`, whereas `huddle_available` skips offline/quitting
    /// users via `exclusive_available` so a single disconnected teammate never aborts a night phase.
    #[track_caller]
    pub async fn huddle<'a, I>(&self, users: I) -> Result<Huddle>
    where
        I: IntoIterator<Item = &'a User> + Send;
    #[track_caller]
    pub async fn huddle_available<'a, I>(&self, users: I) -> Result<Huddle>
    where
        I: IntoIterator<Item = &'a User> + Send;

    /// Sends `JOIN <channel>` (if not already joined) and awaits self `JOIN` + `366 RPL_ENDOFNAMES`
    /// or surfaces server rejection (`403`, `405`, `471`, `473`, `474`, `475`).
    pub async fn join(&self, channel: &str) -> Result<Room>;
    pub async fn join_with_key(&self, channel: &str, key: &str) -> Result<Room>;

    /// Creates an RAII temporary breakout channel (default `#<prefix>-<random>`, `+is`, `kick_on_drop`).
    pub async fn breakout(&self, prefix: &str) -> Result<Breakout>;
    pub async fn breakout_with(&self, opts: BreakoutOptions) -> Result<Breakout>;

    /// Replies to the channel if `msg` was in a channel, or directly to `msg.nick` if `msg` was a DM,
    /// mirroring `msg.kind`: sends `NOTICE` when `msg.kind == MessageKind::Notice` (never answering a `NOTICE`
    /// with a `PRIVMSG`, per RFC 2812 §3.3.2 to prevent bot loops!) and `PRIVMSG` when `Privmsg | Action`.
    pub fn reply(&self, msg: &ChatMessage, text: impl Into<Text>) -> Result<()>;
    pub fn send_raw(&self, cmd: impl Into<irc::proto::Command>) -> Result<()>;

    /// Grouped `inbox_*` constructors on `Bot` (all `#[track_caller]`):
    /// - `inbox_all()`: firehose across the entire connection (`PRIVMSG` + channel/DM events).
    /// - `inbox_dms()`: DMs across all users.
    /// - `inbox_users(..)` / `inbox_rooms(..)` / `inbox_scope(..)`: wire-ordered multi-source `Inbox`.
    #[track_caller]
    pub fn inbox_all(&self) -> Inbox;
    #[track_caller]
    pub fn inbox_dms(&self) -> Inbox;
    #[track_caller]
    pub fn inbox_users<'a, I: IntoIterator<Item = &'a User>>(&self, users: I) -> Inbox;
    #[track_caller]
    pub fn inbox_rooms<'a, I: IntoIterator<Item = &'a Room>>(&self, rooms: I) -> Inbox;
    #[track_caller]
    pub fn inbox_scope(&self, scope: InboxScope) -> Inbox;

    /// Cooperative graceful shutdown signal (backed by `tokio::sync::watch`, firing *before* `QUIT` is queued):
    /// `game.until(bot.shutdown_requested())` resolves to `Outcome::Stopped(())` so games can say goodbye and
    /// drop `Breakout`s while the socket is still live!
    pub fn request_shutdown(&self);
    pub fn is_shutting_down(&self) -> bool;
    pub fn shutdown_requested(&self) -> ShutdownRequested;

    /// Signals `request_shutdown()`, yields once so ready shutdown tasks enqueue cleanup (`say` / `Breakout::drop`),
    /// drains both `Interactive` and `Background` queues in `OutboundSink` followed by `QUIT`, and awaits stream
    /// closure (bounded by `handshake_timeout`; raise `handshake_timeout` or set `kick_on_drop = false` for huge rooms).
    pub async fn quit(&self, reason: Option<&str>) -> Result<()>;
}
```

### 4.3 Message & Event Views (`ChatMessage`, `MessageKind`, `Event`)

Per RFC 2812 §3.3.2, **by default `ChatMessage` represents `PRIVMSG` (`MessageKind::Privmsg` and CTCP `\x01ACTION ...\x01` as `MessageKind::Action`) ONLY**—never `NOTICE`. Service notices (`NickServ`, `ChanServ`), server notices, ZNC bouncer notices, and notices from other bots never trigger `wait_map`, `wait_cmd`, `next_chat`, `ask`, or `ask_user` by default.

For games where human IRC players prefer `/notice Bot !vote Alice` (common in Mafia/Werewolf communities so accidental missing-target typos don't leak into channel windows), callers can opt in **per `Inbox` / `Ask` / `AskConfig` via `.accept_notices()`** (never a global `BotConfig` flag):
- Even when `.accept_notices()` is enabled, `ircgm` surfaces a `NOTICE` as `ChatMessage { kind: MessageKind::Notice, .. }` **only** when its prefix is a full human `nick!user@host` (`Prefix::Nickname` with non-empty `user` and `host`), it does **not** carry the IRCv3 `bot` / `+bot` tag, and its sender is not an IRC services pseudoclient (`NickServ`, `ChanServ`, `MemoServ`, `OperServ`, `HostServ`, `BotServ`, `Global`, `SaslServ`). Server and services notices remain strictly excluded!
- **`bot.reply(&msg, text)` always mirrors `msg.kind`:** if `msg.kind == MessageKind::Notice`, `bot.reply` sends an outbound `NOTICE` (never answering a `NOTICE` with a `PRIVMSG`, per RFC 2812 §3.3.2).
- `Event::Notice` on `inbox.next_event()` / `wait_event_map()` continues to surface all notices unconditionally.

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Channel(String),
    Dm,
}

impl Target {
    pub fn is_channel(&self) -> bool;
    pub fn is_dm(&self) -> bool;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MessageKind {
    Privmsg,
    Action,
    Notice,
}

#[derive(Clone, Debug)]
pub struct ChatMessage {
    pub raw: Arc<irc::proto::Message>,
    /// Strictly monotonic wire-arrival sequence number (`1, 2, 3, ...`) assigned by the pump.
    /// **`seq` (never `server_time`) is the sole ordering and race-fairness authority.**
    pub seq: u64,
    /// Monotonic local timestamp captured when the pump processed this frame.
    pub received_at: tokio::time::Instant,
    /// IRCv3 `server-time` (`time=` tag), when negotiated and supplied by the IRCd (`None` if absent).
    pub server_time: Option<std::time::SystemTime>,
    /// Presence-tracked `User` handle stamped at wire-arrival time (immune to subsequent `/nick` renames or nick-stealing!).
    pub sender: User,
    /// Nickname snapshot of the sender at the exact moment this message arrived on the wire.
    pub nick: String,
    /// IRC username / ident string (`~ident` in `nick!~ident@host`).
    pub ident: Option<String>,
    pub host: Option<String>,
    /// Populated automatically when IRCv3 `account-tag` is present on `raw.tags`.
    pub account: Option<String>,
    pub target: Target,
    /// Formatting-stripped plain text (`Text::parse(&raw_text).plain()`), parsed once in the pump.
    pub text: String,
    /// Unmodified wire payload (with CTCP `\x01ACTION ...\x01` framing unwrapped if `kind == MessageKind::Action`).
    pub raw_text: String,
    pub kind: MessageKind,
}

impl ChatMessage {
    pub fn is_dm(&self) -> bool;
    pub fn is_action(&self) -> bool;
    pub fn is_notice(&self) -> bool;
    /// Case-folded channel match using `room`'s active `CaseMap`.
    pub fn is_in(&self, room: &Room) -> bool;
    /// Re-parses `raw_text` into an `irc_colors::Text` preserving sender colors/styles.
    pub fn to_styled(&self) -> Text;
    /// Word-boundary command check against plain `self.text` (`"!join"` matches `"!join"`, not `"!joined"`).
    pub fn is_cmd(&self, cmd: &str) -> bool;
    /// Returns trimmed arguments after `cmd` if matched at a word boundary.
    pub fn cmd_args(&self, cmd: &str) -> Option<&str>;
}

#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum Event {
    Chat(ChatMessage),
    Notice {
        sender: Option<User>, // `None` for server notices (`:irc.example.org NOTICE ...`)
        nick: Option<String>,
        target: Target,
        text: String,
        raw: Arc<irc::proto::Message>,
    },
    Joined {
        channel: String,
        user: User,
        nick: String,
        ident: Option<String>,
        host: Option<String>,
    },
    Parted {
        channel: String,
        user: User,
        nick: String,
        reason: Option<String>,
    },
    Kicked {
        channel: String,
        user: User,
        nick: String,
        by: String,
        reason: Option<String>,
    },
    Quit {
        user: User,
        nick: String,
        /// Joined channels (tracked by `WorldState`) that this user occupied at the moment of `QUIT`.
        channels: Vec<String>,
        reason: Option<String>,
        /// Heuristic `true` when `reason` looks like an IRC server-to-server netsplit (`"a.b c.d"`).
        netsplit: bool,
    },
    NickChanged {
        user: User,
        old_nick: String,
        new_nick: String,
        /// Joined channels (tracked by `WorldState`) that this user occupied at the moment of `NICK`.
        channels: Vec<String>,
    },
    /// Server rejected or bounced an outbound delivery (`401`, `403`, `404`, `442`, `486` `+R`, `492`, `531`, `716` `+g` `callerid`),
    /// allowing games that send one-way secret DMs (`u.say(...)`) to observe bounces without opening an `Ask`.
    DeliveryFailed {
        target: String,
        code: irc::proto::Response,
        message: String,
        raw: Arc<irc::proto::Message>,
    },
}
```

### 4.4 `Inbox` (Unified Listening Primitive, Multi-Source Rules, & `select!` Cancel Safety)

```rust
pub struct Inbox { /* Arc<BotCore>, slot_id, opened_at, mpsc::Receiver<Arc<WireEvent>>, Arc<AtomicU64>, LagPolicy, read_limit: TimeLimit, ignore_commands: bool, accept_notices: bool */ }

impl Inbox {
    /// Core passive listener primitive (read-only—never emits outbound messages): polls incoming `ChatMessage`
    /// events (`PRIVMSG`/`ACTION`, plus human `NOTICE`s only if `.accept_notices()` is enabled) matching this inbox's filter in exact `seq` wire order until `f` returns `Some(val)`.
    ///
    /// **Unified departure / source-exhaustion rule:**
    /// - A scoped `Inbox` (`user.inbox()`, `user.inbox_in(&room)`, `room.inbox()`, `bot.inbox_users(&users)`,
    ///   `bot.inbox_rooms(&rooms)`, or mixed `bot.inbox_scope(scope)`) terminates with `Err(Error::UserLeft { .. })`
    ///   or `Err(Error::BotLeftRoom { .. })` **only when zero live sources remain in its scope** (after draining
    ///   any buffered events with `seq < departure_seq`):
    ///   - Single `User` (`user.inbox()` / `user.inbox_in(&room)`): drains prior `seq`s, then returns `Err(Error::UserLeft { .. })`
    ///     (or `Err(Error::BotLeftRoom { .. })` if the bot left `room`).
    ///   - Multi-user (`bot.inbox_users(&users)`): one member quitting emits `Event::Quit` in-band on `next_event()`
    ///     while `wait_map` / `next_chat` **keeps running** for remaining online members; only once **all** users in the
    ///     set are offline does `wait_map` return `Err(Error::UserLeft { .. })` (which `.settled()` maps to `Turn::Left`!).
    ///   - Single/multi-room (`room.inbox()` / `bot.inbox_rooms(&rooms)`): yields `Event::{Chat, Notice, Joined, Parted, Kicked}`
    ///     for scoped rooms plus `Event::{Quit, NickChanged}` whenever `channels` intersects a scoped room (and
    ///     `DeliveryFailed` targeting a scoped room/user), failing with `Err(Error::BotLeftRoom { .. })` only once all scoped rooms are lost.
    ///   - Mixed scope (`InboxScope::new().room(&r).dms_from(&users)`): if all `users` quit while `r` is still joined,
    ///     the inbox **keeps running** for `r` (and vice versa), failing only when *both* zero live rooms and zero live DM users remain!
    pub async fn wait_map<F, T>(&mut self, f: F) -> Result<T>
    where
        F: FnMut(&ChatMessage) -> Option<T>;

    pub async fn wait_for<P>(&mut self, pred: P) -> Result<ChatMessage>
    where
        P: FnMut(&ChatMessage) -> bool;

    pub async fn wait_cmd(&mut self, cmd: &str) -> Result<ChatMessage>;
    pub async fn next_chat(&mut self) -> Result<ChatMessage>;

    /// Yields room membership, notice, delivery-bounce, & chat events
    /// (`Joined`, `Parted`, `Kicked`, `Quit`, `NickChanged`, `Notice`, `DeliveryFailed`, `Chat`).
    pub async fn next_event(&mut self) -> Result<Event>;
    pub async fn wait_event_map<F, T>(&mut self, f: F) -> Result<T>
    where
        F: FnMut(&Event) -> Option<T>;

    /// Escape hatch for arbitrary raw `irc::proto::Message` numerics / commands.
    pub async fn next_raw(&mut self) -> Result<Arc<irc::proto::Message>>;
    pub async fn wait_raw_map<F, T>(&mut self, f: F) -> Result<T>
    where
        F: FnMut(&irc::proto::Message) -> Option<T>;

    /// Configures an **idle per-read timeout** (`Err(Error::TimedOut)` if no matching/incoming event arrives within
    /// `duration`; resets as events arrive—used on `ExclusiveDm::inbox()` direct reads, default 300s, so AFK bouncers cannot hold `dm_mutex`).
    /// For a **fixed phase deadline** that non-matching chatter or wrong guesses must NOT reset, use
    /// `with_read_deadline(Instant)` / `set_read_deadline(Instant)` or outer `GameFutureExt::with_timeout(dur)` / `with_deadline(instant)`!
    /// (`&mut self` `set_*` counterparts mutate borrowed inboxes such as `dm.inbox()` in place.)
    pub fn with_read_timeout(self, duration: Duration) -> Self;
    pub fn with_read_deadline(self, deadline: tokio::time::Instant) -> Self;
    pub fn without_read_timeout(self) -> Self;
    pub fn set_read_timeout(&mut self, duration: Duration);
    pub fn set_read_deadline(&mut self, deadline: tokio::time::Instant);
    pub fn clear_read_timeout(&mut self);

    /// Opt-in command skipping: silently skips `ChatMessage`s whose trimmed plain text starts with any prefix in
    /// `BotConfig::command_prefixes` (default `["!"]`) so free-text reads (`next_chat` / `wait_map`) don't swallow
    /// out-of-band commands like `!cancel` or `!help`. Off by default unless explicitly enabled!
    pub fn ignore_commands(self) -> Self;
    pub fn set_ignore_commands(&mut self, enabled: bool);

    /// Opt-in human `NOTICE` acceptance: surfaces `NOTICE`s whose prefix is a full `nick!user@host` (excluding
    /// server notices, IRC services pseudoclients, and messages tagged with IRCv3 `bot`) as
    /// `ChatMessage { kind: MessageKind::Notice, .. }`. Off by default!
    pub fn accept_notices(self) -> Self;
    pub fn set_accept_notices(&mut self, enabled: bool);

    /// Opts into warn-and-skip on buffer lag instead of returning `Err(Error::Lagged { .. })`.
    pub fn lossy(self) -> Self;

    /// Discards all messages currently buffered in this inbox and clears any pending lag state.
    pub fn clear(&mut self);
}

// `Inbox` also implements `futures::Stream<Item = Result<ChatMessage>>`.
```

**`tokio::select!` Cancellation Safety:** Every method on `Inbox` (`wait_map`, `wait_for`, `wait_cmd`, `next_chat`, `next_event`, `wait_event_map`, `next_raw`, `wait_raw_map`) is **strictly cancel-safe**. No item is popped from the underlying `mpsc::Receiver` until `recv()` resolves, and synchronous predicates (`f` / `pred`) run to completion without `.await` points—so dropping a wait mid-flight when another `tokio::select!` branch wins never loses a buffered event. Dropping `Inbox` itself synchronously removes its slot from `InboxRegistry`.

### 4.5 `Room` & `Member` (Channel View, Membership, & Moderation)

```rust
/// Snapshot of a channel occupant in `WorldState`, preserving `User` identity and status prefixes (`@`, `+`, ...).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub user: User,
    pub nick: String,
    /// `true` when the member holds `@` (`+o`) or higher channel privilege (`&`, `~`).
    pub is_op: bool,
    /// `true` when the member holds `+` (`+v`).
    pub is_voiced: bool,
    /// Raw status prefix symbols ordered from highest to lowest per `005 PREFIX` (e.g. `['@', '+']`).
    pub prefixes: Vec<char>,
}

#[derive(Clone, Debug)]
pub struct Room { /* Arc<BotCore>, channel_name: String, Arc<AskConfig> */ }

impl Room {
    pub fn name(&self) -> &str;
    pub fn casemap(&self) -> CaseMap;

    /// Derives a lightweight `Room` view carrying `cfg` as its local `AskConfig` (`&self -> Room`).
    /// (`Breakout` defines an inherent `with_ask_config(mut self, cfg) -> Breakout` so chaining on a fresh `Breakout`
    /// never drops it; call `vault.to_room().with_ask_config(&cfg)` to derive a `Room` view from a `Breakout`.)
    pub fn with_ask_config(&self, cfg: &AskConfig) -> Self;

    pub fn say(&self, text: impl Into<Text>) -> Result<()>;
    pub fn notice(&self, text: impl Into<Text>) -> Result<()>;
    pub fn action(&self, text: impl Into<Text>) -> Result<()>;
    /// Sends `INVITE <target> <channel>` (accepts `&User`, `&str`, or `&String`; fails with `Err(Error::UserLeft { .. })`
    /// without sending traffic if a `User` target is already known offline).
    pub fn invite<T: InviteTarget + ?Sized>(&self, target: &T) -> Result<()>;
    pub fn part(&self, reason: Option<&str>) -> Result<()>;

    /// Channel membership & topic queries from `WorldState`:
    pub fn members(&self) -> Vec<Member>;
    pub fn member(&self, user: &User) -> Option<Member>;
    pub fn nicks(&self) -> Vec<String>;
    pub fn has_user(&self, user: &User) -> bool;
    pub fn has_nick(&self, nick: &str) -> bool;
    pub fn bot_is_op(&self) -> bool;
    pub fn topic(&self) -> Option<String>;

    /// Synchronous channel moderation commands (matching `invite` / `part` rather than echo-waiting `async fn`,
    /// because standard IRCds emit **zero** server lines on idempotent `MODE #c +m` or `MODE #c +v Alice`!).
    /// If `WorldState` has synced `self` (`366 RPL_ENDOFNAMES`) and `!self.bot_is_op()`, fails immediately
    /// without sending traffic via `Err(Error::NotChannelOp { channel })` (or `Err(Error::BotLeftRoom { .. })` if absent).
    /// `voice_many` / `devoice_many` automatically chunk nicks into `MODE #c +vvvv ...` batches bounded by
    /// `WorldState`'s `005 MODES=N` (default `4`), sparing outbound burst budget on 12-player phase transitions.
    /// If an in-flight de-op race causes an asynchronous `482 ERR_CHANOPRIVSNEEDED`, it is logged via `tracing::warn!`.
    pub fn kick(&self, user: &User, reason: Option<&str>) -> Result<()>;
    pub fn set_mode(&self, modes: &str, args: &[&str]) -> Result<()>;
    pub fn voice(&self, user: &User) -> Result<()>;
    pub fn voice_many<'a, I: IntoIterator<Item = &'a User>>(&self, users: I) -> Result<()>;
    pub fn devoice(&self, user: &User) -> Result<()>;
    pub fn devoice_many<'a, I: IntoIterator<Item = &'a User>>(&self, users: I) -> Result<()>;
    pub fn set_topic(&self, topic: impl Into<Text>) -> Result<()>;

    /// Open-floor channel prompt: subscribes `self.inbox()` before sending `prompt` in the channel,
    /// then awaits one matching reply (`Option<T>` or `Verdict<T>`) from *anyone* in the room (`TimeoutCtx::user` is `None`).
    pub fn ask<'a, F, R>(&'a self, prompt: impl Into<Text>, matcher: F) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoVerdict + 'a,
        R::Output: Send + 'a;

    /// Player-directed channel prompt: subscribes `user.inbox_in(self)` before sending `prompt`
    /// in the channel, ignoring other chatters, tracking `/nick` changes, failing immediately without sending
    /// if `WorldState` has synced `self` and `user` (or the bot) is already absent (`UserLeft` / `BotLeftRoom`),
    /// failing immediately mid-wait on `PART`/`KICK`/`QUIT` or server `404`/`442` (`DeliveryFailed`),
    /// NOT locking `user`'s `dm_mutex` (note: two concurrent `ask_user` calls to the same player in the same channel
    /// will both match one reply), and passing `TimeoutCtx { user: Some(user), .. }` on timeout!
    pub fn ask_user<'a, F, R>(&'a self, user: &'a User, prompt: impl Into<Text>, matcher: F) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoVerdict + 'a,
        R::Output: Send + 'a;

    #[track_caller]
    pub fn inbox(&self) -> Inbox;
}
```

### 4.6 `User`, `ExclusiveDm`, & `Huddle`

```rust
#[derive(Clone, Debug)]
pub struct User { /* Weak<BotCore>, Arc<UserTrack> — 100% config-free & cycle-free! */ }

// Equality, ordering, and hashing use `UserTrack::id` ONLY, surviving `/nick` changes and RFC 1459 case-folding
// with 100% behavioral equivalence across equal `User` handles!
// (Note: a `QUIT` followed by a rejoin allocates a new `UserTrack`; key reconnectable long games by `user.account()`.)
impl PartialEq for User { /* self.track.id == other.track.id */ }
impl Eq for User {}
impl PartialOrd for User { /* Some(self.cmp(other)) */ }
impl Ord for User { /* self.track.id.cmp(&other.track.id) */ }
impl Hash for User { /* self.track.id.hash(state) */ }

impl User {
    /// Current nickname (live, automatically updated on `NICK`).
    pub fn nick(&self) -> String;
    /// Compares `nick` against this user's live nickname under the bot's active `CaseMap` (`Rfc1459` / `StrictRfc1459` / `Ascii`).
    pub fn eq_nick(&self, nick: &str) -> bool;
    /// Authenticated IRC Services account name (live, updated from `account-tag`, `extended-join`, and `account-notify`; `None` if unauthenticated).
    pub fn account(&self) -> Option<String>;
    pub fn is_online(&self) -> bool;

    /// Non-blocking one-way outbound DM (does NOT acquire `dm_mutex`, so cross-player status updates never deadlock).
    pub fn say(&self, text: impl Into<Text>) -> Result<()>;
    pub fn notice(&self, text: impl Into<Text>) -> Result<()>;
    pub fn action(&self, text: impl Into<Text>) -> Result<()>;

    /// Acquires this user's per-user DM mutex (aborting with `Err(Error::UserLeft { .. })` on `QUIT`; logs a `tracing::warn!`
    /// naming the holder's `Location` if waiting exceeds 15s) and immediately opens a dedicated `Inbox` for a multi-message
    /// or multi-turn DM exchange (initialized with `BotConfig::ask_config`).
    /// - **One locking rule:** While holding any `ExclusiveDm` (or `Huddle`), never `.await` `exclusive*()`, `ask()`, or `huddle*()`
    ///   on the same or another `User`—use `bot.exclusive_many` / `exclusive_available` / `huddle*` upfront instead!
    /// - **Read-only AFK bound:** `BotConfig::ask_config`'s 300s default covers IRC reads (`dm.ask` / `dm.inbox()`) only;
    ///   wrap in outer `.with_timeout(dur)` if holding `dm` across `sleep` / DB / non-IRC `.await`s or to cap queue + exchange time.
    /// - Out-of-band cancellation: race the flow against an unserialized inbox (`flow.until(user.inbox().wait_cmd("!cancel").ok_or_pending())`);
    ///   dropping the cancelled future immediately drops `ExclusiveDm` and releases `dm_mutex` for queued callers!
    #[track_caller]
    pub async fn exclusive(&self) -> Result<ExclusiveDm>;

    /// Acquires `exclusive()`, sends `prompt` in DM, starts the reply deadline *after* `OutboundSink`'s write receipt resolves
    /// (defaulting to `BotConfig::ask_config`, overridable via `.config(&cfg)` / `.reply_timeout()`),
    /// and awaits one matching reply (`Option<T>` or `Verdict<T>`). Concurrent calls to the same `User` queue cleanly one by one!
    #[track_caller]
    pub fn ask<'a, F, R>(&'a self, prompt: impl Into<Text>, matcher: F) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoVerdict + 'a,
        R::Output: Send + 'a;

    /// Opens an unserialized DM `Inbox` (for passive/public-prompt DM collection like simultaneous night votes
    /// or out-of-band `!cancel` / `!help` listeners) tracking `NICK` changes and failing with `Err(Error::UserLeft { .. })` on `QUIT` or `401`.
    /// (Invariant: IRC servers only broadcast proactive `NICK`/`QUIT` for users sharing at least one channel with the bot—recruit via a channel!)
    #[track_caller]
    pub fn inbox(&self) -> Inbox;

    /// Opens a channel `Inbox` matching only this user's messages inside `room`, tracking `NICK` changes
    /// and failing immediately with `Err(Error::UserLeft { .. })` if `room` is synced and `user` is absent
    /// (or later `QUIT`s / leaves `room`), or `Err(Error::BotLeftRoom { .. })` if the bot leaves `room`.
    #[track_caller]
    pub fn inbox_in(&self, room: &Room) -> Inbox;
}

/// RAII guard holding exclusive access to a `User`'s interactive DM prompt slot plus a pre-subscribed `Inbox` and local `AskConfig`.
pub struct ExclusiveDm {
    user: User,
    inbox: Inbox,
    ask_config: Arc<AskConfig>,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl ExclusiveDm {
    pub fn user(&self) -> &User;

    /// Updates `self.ask_config` and synchronizes `self.inbox`'s direct-read limit, `ignore_commands`, & `accept_notices` without dropping `_guard`!
    /// (`set_ask_config` mutates in place when holding `dm: &mut ExclusiveDm` inside a helper function.)
    pub fn with_ask_config(mut self, cfg: &AskConfig) -> Self;
    pub fn set_ask_config(&mut self, cfg: &AskConfig);
    pub fn with_ignore_commands(mut self) -> Self;
    pub fn with_accept_notices(mut self) -> Self;

    pub fn say(&self, text: impl Into<Text>) -> Result<()>;
    pub fn notice(&self, text: impl Into<Text>) -> Result<()>;
    pub fn action(&self, text: impl Into<Text>) -> Result<()>;

    /// Calls `self.inbox.clear()` (intentionally discarding any unsolicited pre-prompt lines or late reply from
    /// an earlier timed-out step), sends `prompt`, and awaits one matching reply (`Option<T>` or `Verdict<T>`) using `self.ask_config`.
    /// **Single-timer invariant:** `Ask::poll` reads raw events from `self.inbox` bypassing `self.inbox`'s idle `read_limit`
    /// so `Ask`'s resolved deadline (including lengthening via `.reply_timeout(120s)` or `.no_reply_timeout()`) is the sole active timer!
    pub fn ask<'a, F, R>(&'a mut self, prompt: impl Into<Text>, matcher: F) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoVerdict + 'a,
        R::Output: Send + 'a;

    /// Mutable access to the pre-subscribed DM `Inbox` for reading multiple burst messages in sequence without clearing.
    /// **Important:** Unlike `room.inbox()`, direct `dm.inbox().next_chat()` / `wait_*` reads start with an active idle
    /// per-read timeout from `BotConfig::ask_config` (default 300s, overridable via `dm.with_ask_config(&cfg)` / `dm.set_ask_config(&cfg)`
    /// or `dm.inbox().set_read_timeout(...)` / `clear_read_timeout()`) so AFK bouncers cannot hold `dm_mutex` forever!
    pub fn inbox(&mut self) -> &mut Inbox;
}

/// RAII channel-free group DM guard (`bot.huddle(&users)` / `bot.huddle_available(&users)`):
/// holds ordered `ExclusiveDm` locks for all members + a wire-ordered `inbox_users`, releasing all locks on `Drop`.
/// Ideal for 2–5 player secret teams (Werewolves, Vampires, Spies) on standard IRC clients without auto-join on `INVITE`
/// (note: peer chat relay sends `N - 1` outbound DMs per chat line, so prefer `Breakout` for >5-player chatty rooms).
pub struct Huddle {
    guards: Vec<ExclusiveDm>,
    inbox: Inbox,
    ask_config: Arc<AskConfig>,
    relay_prefix: Option<Text>,
}

impl Huddle {
    pub fn users(&self) -> Vec<User>;
    pub fn online_users(&self) -> Vec<User>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;

    /// Configures `AskConfig`, command-skipping, human `NOTICE` acceptance, and optional peer whisper relay (`<prefix><nick> <styled_text>`).
    pub fn with_ask_config(mut self, cfg: &AskConfig) -> Self;
    pub fn set_ask_config(&mut self, cfg: &AskConfig);
    pub fn with_ignore_commands(mut self) -> Self;
    pub fn with_accept_notices(mut self) -> Self;
    pub fn with_relay(mut self, prefix: impl Into<Text>) -> Self;
    pub fn without_relay(mut self) -> Self;
    pub fn set_relay(&mut self, prefix: Option<Text>);

    /// Broadcasts to all currently-online huddle users (skipping any user who disconnected mid-huddle).
    pub fn say(&self, text: impl Into<Text>) -> Result<()>;
    pub fn notice(&self, text: impl Into<Text>) -> Result<()>;
    pub fn action(&self, text: impl Into<Text>) -> Result<()>;

    /// Relays `msg` to all other online huddle users (`<prefix><<msg.nick>> <msg.to_styled()>`, using `relay_prefix`
    /// or empty prefix if none); handy when driving `huddle.inbox()` manually for unanimous consensus loops.
    pub fn relay_to_others(&self, msg: &ChatMessage) -> Result<()>;

    /// Group DM prompt in wire `msg.seq` order (`TimeoutCtx::user` is `None`):
    /// - Fails immediately with `Err(Error::UserLeft { .. })` (mapped to `Turn::Left` by `.settled()`) if zero online users remain.
    /// - Clears stale buffered lines, broadcasts `prompt` to online users, and starts `Ask`'s single reply clock
    ///   once the last user prompt's `WriteReceipt` resolves (bypassing `self.inbox`'s idle `read_limit`; `warn_remaining` and `timeout_message` broadcast to online users).
    /// - When `.ignore_commands()` is enabled, unrelated `!commands` are skipped *before* `matcher` and never relayed.
    /// - `Verdict::Reject(err)` sends `err` **privately to `msg.sender` only** (sparing burst quota and teammates' screens) without relaying.
    /// - `Verdict::Ignore` (`None`) relays `msg` to fellow online huddle users when `with_relay(...)` is configured!
    pub fn ask<'a, F, R>(&'a mut self, prompt: impl Into<Text>, matcher: F) -> Ask<'a, F, R>
    where
        F: FnMut(&ChatMessage) -> R + Send + 'a,
        R: IntoVerdict + 'a,
        R::Output: Send + 'a;

    /// Direct mutable access to the wire-ordered multi-user DM `Inbox` (starts with `BotConfig::ask_config`'s 300s idle `read_limit`).
    pub fn inbox(&mut self) -> &mut Inbox;
}
```

### 4.7 `Ask`, `Verdict<T>`, `AskConfig`, & `TimeoutCtx`

```rust
/// Tri-state result for `Ask` matchers that want to send validation feedback ("Pick 1, 2, or 3!")
/// without restarting the reply timer or leaving the `Ask` loop.
/// (`Inbox::wait_map` intentionally stays `FnMut(&ChatMessage) -> Option<T>` so `Inbox` remains a pure read-only listener.)
/// **Open-floor caution:** On open-floor `room.ask(...)`, return `Verdict::Reject` only when a message is clearly an attempted
/// answer (e.g. `m.cmd_args("!pick").is_some()`), and return `Verdict::Ignore` on unrelated spectator chatter!
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict<T> {
    /// Accepts `T` and resolves the `Ask` with `Ok(T)`.
    Accept(T),
    /// Sends validation feedback `Text` to the `Ask`'s target and keeps waiting **without** resetting the reply timer.
    Reject(Text),
    /// Silently ignores this message and keeps waiting.
    Ignore,
}

impl<T> Verdict<T> {
    pub fn reject(msg: impl Into<Text>) -> Self {
        Self::Reject(msg.into())
    }
}

/// Sealed trait implemented for both `Option<T>` (`Some(v) -> Accept(v)`, `None -> Ignore`) and `Verdict<T>`
/// so simple `Option<T>` closures stay terse while `Verdict<T>` unlocks inline rejection feedback.
/// (Verified via compile-tests on multi-arm `None` / `Some(x)` closures; if `rustc` inference ever balked on a
/// pattern, a separate `ask_checked` method is the designated fallback rather than breaking `Option<T>` ergonomics.)
pub trait IntoVerdict: private::Sealed {
    type Output;
    fn into_verdict(self) -> Verdict<Self::Output>;
}

pub struct TimeoutCtx<'a> {
    /// Where the prompt was sent (and where any timeout message will be sent):
    /// `Target::Channel("#blackjack")` or `Target::Dm`.
    pub target: &'a Target,
    /// The specific player being waited on:
    /// - `Some(user)` on `room.ask_user(user, ...)`, `user.ask(...)`, and `dm.ask(...)`
    ///   (with live `user.nick()` surviving mid-turn `/nick` renames!).
    /// - `None` on open-floor `room.ask(...)` where anyone in the channel could answer.
    pub user: Option<&'a User>,
    /// `Some(dur)` when configured with a relative timeout (`reply_timeout`),
    /// or `None` when configured with an absolute `Instant` (`reply_deadline`).
    pub configured_timeout: Option<Duration>,
    /// Actual wall-clock time spent waiting on the reply after `OutboundSink`'s write receipt resolved.
    pub elapsed: Duration,
}

/// Plain-data reusable `Ask` configuration sharing an exact 1-to-1 builder vocabulary with `Ask`
/// (passed to `BotConfig::with_ask_config`, `Room::with_ask_config`, `Breakout::with_ask_config` / `set_ask_config`,
/// `BreakoutOptions::ask_config`, `ExclusiveDm::with_ask_config` / `set_ask_config`, `Huddle::with_ask_config` / `set_ask_config`, and `Ask::config`).
#[derive(Clone, Debug)]
pub struct AskConfig { /* TimeLimit + Vec<(Duration, Text)> warnings + Option<TimeoutHandler> + ignore_commands + accept_notices */ }

impl Default for AskConfig { /* 300s reply_timeout, no warnings, no_timeout_message, ignore_commands = false, accept_notices = false */ }

impl AskConfig {
    pub fn new() -> Self;
    pub fn reply_timeout(self, duration: Duration) -> Self;
    pub fn reply_deadline(self, instant: tokio::time::Instant) -> Self;
    pub fn no_reply_timeout(self) -> Self;
    pub fn warn_remaining(self, remaining: Duration, msg: impl Into<Text>) -> Self;
    pub fn clear_warnings(self) -> Self;
    pub fn ignore_commands(self) -> Self;
    pub fn accept_notices(self) -> Self;
    pub fn timeout_message(self, msg: impl Into<Text>) -> Self;
    pub fn timeout_message_fn<F, M>(self, f: F) -> Self
    where
        F: Fn(&TimeoutCtx<'_>) -> M + Send + Sync + 'static,
        M: Into<Text>;
    pub fn no_timeout_message(self) -> Self;
}

/// Lazy prompt-and-reply future returned by `Room::ask`, `Room::ask_user`, `User::ask`, `ExclusiveDm::ask`, and `Huddle::ask`.
/// Implements `Future<Output = Result<R::Output>>` directly so `.await`, `tokio::select!`, `tokio::try_join!`,
/// `futures::future::select_all`, and `GameFutureExt` (`.optional()`, `.settled()`, `.with_timeout()`) all work seamlessly.
/// (Tip: if driving an `Ask` manually across iterations of a `tokio::select!` loop, pin it first via
/// `tokio::pin!(ask);` and poll `&mut ask` so losing branches never drop/re-send the prompt!)
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Ask<'a, F, R> { /* lazy state machine */ }

impl<'a, F, R> Ask<'a, F, R> {
    /// Applies a reusable `AskConfig` value (time limit + warnings + timeout message policy + `ignore_commands` + `accept_notices`) to this prompt.
    pub fn config(self, cfg: &AskConfig) -> Self;

    /// Overrides the post-write reply deadline for this prompt (starts ticking *after* DM mutex acquisition AND
    /// `OutboundSink`'s write receipt resolves; can shorten OR lengthen an inherited `reply_timeout`).
    pub fn reply_timeout(self, duration: Duration) -> Self;
    /// Overrides the post-write deadline for this prompt using an absolute `tokio::time::Instant`.
    pub fn reply_deadline(self, instant: tokio::time::Instant) -> Self;
    /// Disables the reply deadline for this prompt (even if `BotConfig` or the handle has a 300s default).
    pub fn no_reply_timeout(self) -> Self;

    /// Schedules a countdown warning `msg` when `remaining` time is left on this `Ask`'s **post-write reply clock**
    /// (`deadline - remaining`, consistent with `TimeoutCtx::elapsed`), without resending the prompt or resetting the deadline.
    /// Can be chained multiple times (e.g. `.warn_remaining(30s, ...).warn_remaining(10s, ...)`). Skipped if `no_reply_timeout()` or if
    /// the initial post-write reply budget is `<= remaining`.
    pub fn warn_remaining(self, remaining: Duration, msg: impl Into<Text>) -> Self;
    /// Clears any countdown warnings inherited from the handle's or `config`'s `AskConfig`.
    pub fn clear_warnings(self) -> Self;

    /// Opt-in command skipping for this prompt: skips messages starting with `BotConfig::command_prefixes` (default `["!"]`)
    /// **before** invoking `matcher`, so free-text prompts don't swallow `!cancel` and `Verdict::Reject` never fires on `!help`!
    pub fn ignore_commands(self) -> Self;

    /// Opt-in human `NOTICE` acceptance for this prompt: allows human `nick!user@host` `NOTICE` replies (excluding
    /// server notices, IRC services, and IRCv3 `bot`-tagged messages) to satisfy `matcher` (`MessageKind::Notice`).
    pub fn accept_notices(self) -> Self;

    /// Overrides the message sent to the target if *this `Ask`'s own reply timer* expires (`Err(Error::TimedOut)`).
    pub fn timeout_message(self, msg: impl Into<Text>) -> Self;
    pub fn timeout_message_fn<G, M>(self, f: G) -> Self
    where
        G: Fn(&TimeoutCtx<'_>) -> M + Send + Sync + 'static,
        M: Into<Text>;
    /// Suppresses any inherited timeout message on this prompt (e.g. `room.ask(...).no_timeout_message().optional()`).
    pub fn no_timeout_message(self) -> Self;
}

impl<'a, F, R> Future for Ask<'a, F, R>
where
    F: FnMut(&ChatMessage) -> R + Send + 'a,
    R: IntoVerdict + 'a,
    R::Output: Send + 'a,
{
    type Output = Result<R::Output>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>;
}
```

**Timeout Messaging & Cancellation Rules of `Ask`:**
- **Single rule for `timeout_message`:** `timeout_message` / `timeout_message_fn` fires **if and only if** `Ask`'s own reply timer expires (`reply_timeout` / `reply_deadline` — explicit or inherited via `AskConfig`). Result-mapping and outer combinators (`GameFutureExt::optional`, `settled`, `with_timeout`, `with_deadline`) never emit or suppress messages.
- Dropping an `Ask` *before* its first `poll` sends nothing on IRC and acquires no locks.
- Dropping/cancelling an in-flight `Ask` (e.g., via `.until(stop)`, `.with_timeout(dur)` expiring externally, or another branch of `select_all` winning) immediately drops its internal `Inbox` and `ExclusiveDm` guard (releasing `dm_mutex`) and **never sends any `timeout_message`**.

### 4.8 `Breakout` & `BreakoutOptions` (RAII Ephemeral Rooms)

```rust
#[derive(Clone, Debug)]
pub enum BreakoutNaming {
    /// Generates `#<prefix>-<6 lowercase alphanumeric>` (default).
    RandomSuffix { prefix: String },
    Exact(String),
}

#[derive(Clone, Debug)]
pub struct BreakoutOptions {
    pub naming: BreakoutNaming,
    /// Modes applied upon creation (default: `Some("+is".into())`).
    pub modes: Option<String>,
    /// Verifies `@` chanop after `JOIN`/`NAMES` (default: `true` if `modes.is_some()` or `kick_on_drop`).
    pub require_op: bool,
    pub kick_on_drop: bool,
    pub kick_reason: Text,
    pub setup_timeout: Duration,
    /// Optional local `AskConfig` applied to the created `Breakout` (defaults to `BotConfig::ask_config`).
    pub ask_config: Option<AskConfig>,
}

impl BreakoutOptions {
    pub fn new(prefix: impl Into<String>) -> Self;
    pub fn exact(channel: impl Into<String>) -> Self;
    pub fn modes(self, modes: Option<impl Into<String>>) -> Self;
    pub fn require_op(self, require: bool) -> Self;
    pub fn kick_on_drop(self, kick: bool) -> Self;
    pub fn kick_reason(self, reason: impl Into<Text>) -> Self;
    pub fn setup_timeout(self, timeout: Duration) -> Self;
    pub fn ask_config(self, cfg: &AskConfig) -> Self;
}

/// RAII ephemeral channel handle (`bot.breakout(prefix)` / `bot.breakout_with(opts)`).
/// **Temporary-drop note:** Always bind a `Breakout` to a variable (`let vault = bot.breakout("v").await?;`)
/// before calling `&Room` methods via `Deref`—calling `bot.breakout("v").await?.say(...)` directly on an unbound
/// temporary would drop `Breakout` and `PART` the channel at the end of that statement!
#[must_use = "dropping a Breakout immediately parts the channel; bind it to a variable"]
pub struct Breakout {
    room: Room,
    opts: BreakoutOptions,
    armed: bool,
}

impl Deref for Breakout {
    type Target = Room;
    fn deref(&self) -> &Room { &self.room }
}

impl Breakout {
    /// Clones the underlying `Room` view without moving or disarming `Breakout`.
    pub fn to_room(&self) -> Room { self.room.clone() }

    /// In-place `AskConfig` override (`mut self -> Self`). Kept as an inherent by-value method shadowing
    /// `Room::with_ask_config(&self) -> Room` so `let vault = bot.breakout("v").await?.with_ask_config(&cfg);`
    /// returns `Breakout` rather than auto-deref-ing to `&Room` and silently dropping the `Breakout`!
    /// (Use `set_ask_config(&mut self, cfg)` to mutate in place, or `vault.to_room().with_ask_config(&cfg)` for a `Room` view.)
    pub fn with_ask_config(mut self, cfg: &AskConfig) -> Self;
    pub fn set_ask_config(&mut self, cfg: &AskConfig);

    /// Strict all-or-nothing invite handshake (matching `Bot::exclusive_many`): subscribes `self.inbox()` before sending
    /// `INVITE`s, checks `User::is_online()` upfront (never sending `INVITE` to an already-offline `User`), and waits until
    /// all `targets` join—failing early with `Err(Error::UserLeft { .. })` if any `User` target is offline or quits,
    /// or `Err(Error::HandshakeTimeout { .. })` if `timeout` elapses first.
    pub async fn invite_and_wait<'a, I, P>(&self, targets: I, timeout: Duration) -> Result<()>
    where
        I: IntoIterator<Item = &'a P>,
        P: InviteTarget + ?Sized + 'a; // Implemented for `User`, `str`, `String`

    /// Partial/pack-resilient invite handshake (matching `Bot::exclusive_available`): skips any already-offline `User`
    /// without sending `INVITE`, invites all online users, finishes immediately once every invited user has either joined
    /// or quit, and on `timeout` expiry returns whichever users joined so far (`Ok(Vec::new())` if none joined;
    /// propagates fatal connection errors like `Disconnected`).
    pub async fn invite_available<'a, I>(&self, users: I, timeout: Duration) -> Result<Vec<User>>
    where
        I: IntoIterator<Item = &'a User>;

    /// Awaitable eviction & teardown: kicks remaining non-bot occupants (if `kick_on_drop`), sends `PART`, awaits
    /// server confirmation of self `PART`, and disarms `Drop`.
    pub async fn close(mut self) -> Result<()>;
}

impl Drop for Breakout {
    fn drop(&mut self) {
        // If `self.armed`: synchronously enqueues `Background`-priority `KICK` commands (batched only up to
        // `WorldState::targmax_kick`, default `1`) + `PART` via `OutboundSink`, never starving `Interactive` prompts!
    }
}
```

### 4.9 Supervision, Arbitrary Stop Chains, `Turn<T>`, & Pure Combinators (`Outcome`, `GameFutureExt`, `race_all`)

```rust
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome<T, S> {
    Completed(T),
    Stopped(S),
}

/// Per-player outcome for multi-player turns, races, and votes where a single player timing out,
/// leaving, or blocking DMs (`+g` / `+R`) should be handled locally without aborting the whole game via `?`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Turn<T> {
    Reply(T),
    TimedOut,
    Left {
        nick: String,
        reason: Option<String>,
    },
    Unreachable {
        target: String,
        code: irc::proto::Response,
        message: String,
    },
}

impl<T> Turn<T> {
    /// Returns `Some(val)` on `Reply(val)`, or `None` on `TimedOut | Left { .. } | Unreachable { .. }`.
    pub fn into_reply(self) -> Option<T>;
    pub fn as_reply(&self) -> Option<&T>;
    pub fn is_timed_out(&self) -> bool;
    pub fn is_left(&self) -> bool;
    pub fn is_unreachable(&self) -> bool;
}

/// Races a runtime-sized `0..=N` collection of futures yielding `F::Output` in registration order.
/// Unlike `futures::future::select_all`, an empty iterator **never panics**—it stays `Poll::Pending` forever,
/// making it safe to pass a dynamic `Vec` of `0..=N` stop triggers into `.until(race_all(triggers))`!
pub fn race_all<I, F>(futures: I) -> future::RaceAll<F>
where
    I: IntoIterator<Item = F>,
    F: Future;

// Adapter structs live in `ircgm::future` so they never collide with `tokio::time::Timeout` or `std`:
impl<G: Future, S: Future> future::Until<G, S> {
    /// Chains another stop condition producing the same stop output `S::Output` (`biased`: earlier stop conditions
    /// are polled before later ones, and all stop conditions take priority over `G`).
    /// Scales cleanly to any arbitrary number of heterogeneous stop futures with a single flat `Outcome<G::Output, S::Output>`!
    pub fn or_until<S2>(self, next_stop: S2) -> future::Until<G, future::OrUntil<S, S2>>
    where
        S2: Future<Output = S::Output>;
}

/// Named `GameFutureExt` so importing it never collides with `futures::FutureExt` (imported anonymously in `ircgm::prelude::*`).
/// All timeout and result-mapping combinators (`with_timeout`, `with_deadline`, `optional`, `settled`)
/// are **pure wrappers** that never alter `Ask` messaging.
pub trait GameFutureExt: Future + Sized {
    /// Races `self` against `stop` using `biased` selection (`stop` wins ties); chain `.or_until(...)`
    /// on the returned `future::Until` to attach arbitrarily many stop triggers!
    fn until<S: Future>(self, stop: S) -> future::Until<Self, S>;

    /// Lightweight output mapper (`Future<Output = A> -> Future<Output = U>`), handy for tagging heterogeneous
    /// stop futures into a flat stop enum (e.g. `bot.shutdown_requested().map_output(|()| Stop::Shutdown)`).
    fn map_output<U, F: FnOnce(Self::Output) -> U>(self, f: F) -> future::MapOutput<Self, F>;

    /// Converts a `Future<Output = Result<T>>` into a stop-trigger future yielding `T` on `Ok(val)` and parking
    /// `Poll::Pending` on `Err(_)`—so an `Inbox` stop listener (`wait_cmd("!stop")`, `wait_cmd("!cancel")`)
    /// never falsely triggers `Outcome::Stopped` when a watched user quits or the connection closes!
    fn ok_or_pending<T>(self) -> future::OkOrPending<Self>
    where
        Self: Future<Output = Result<T>>;

    /// Outer wall-clock timeout: races any `Future<Output = Result<T>>` against `duration`, cancelling the inner
    /// future (sending no `timeout_message`) and returning `Err(Error::TimedOut)` on expiry.
    /// `alice.ask(...).with_timeout(60s)` bounds DM mutex queue wait + reply together!
    fn with_timeout<T>(self, duration: Duration) -> future::WithTimeout<Self>
    where
        Self: Future<Output = Result<T>>;

    /// Outer absolute-deadline timeout (`tokio::time::timeout_at`): enforces a fixed phase end-time across loops.
    fn with_deadline<T>(self, deadline: tokio::time::Instant) -> future::WithDeadline<Self>
    where
        Self: Future<Output = Result<T>>;

    /// Pure result mapper converting `Err(Error::TimedOut)` to `Ok(None)` while preserving all other `Err(e)` variants for `?`
    /// (chain `.no_timeout_message().optional()` on an `Ask` if suppressing an inherited handle `timeout_message`).
    fn optional<T>(self) -> future::Optional<Self>
    where
        Self: Future<Output = Result<T>>;

    /// Pure result mapper converting `Err(Error::TimedOut)`, `Err(Error::UserLeft { .. })`, and
    /// `Err(Error::DeliveryFailed { .. })` into `Ok(Turn<T>)` (`TimedOut`, `Left`, `Unreachable`)
    /// while preserving fatal errors (`Disconnected`, `Lagged`, `Irc`) as `Err` for `?`:
    /// `let vote = inbox.wait_map(parse_vote).with_deadline(deadline).settled().await?.into_reply();`
    fn settled<T>(self) -> future::Settled<Self>
    where
        Self: Future<Output = Result<T>>;
}

impl<F: Future> GameFutureExt for F {}
```

### 4.10 Error Model (`ircgm::Error` & `DisconnectReason`)

```rust
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisconnectReason {
    /// Clean shutdown requested via `bot.quit(...)`.
    Quit,
    /// All `Bot` handles were dropped while a `Room` / `User` / `Inbox` survived.
    BotDropped,
    /// Server closed the stream cleanly (`EOF`).
    Eof,
    /// Underlying socket / TLS / protocol transport error.
    Transport(String),
}

#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("IRC client error: {0}")]
    Irc(#[from] irc::error::Error),

    #[error("IRC connection closed ({0:?})")]
    Disconnected(DisconnectReason),

    #[error(
        "inbox ({scope}, opened at {opened_at}) lagged behind stream by {skipped} messages. \
         Did you leave an Inbox open across an .await? Use .clear() or .lossy() if intended."
    )]
    Lagged {
        scope: String,
        skipped: u64,
        opened_at: &'static std::panic::Location<'static>,
    },

    #[error("timed out waiting for reply")]
    TimedOut,

    #[error("user {nick} left{}", reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default())]
    UserLeft {
        nick: String,
        reason: Option<String>,
    },

    #[error("failed to join {channel} ({code:?}): {message}")]
    JoinFailed {
        channel: String,
        code: irc::proto::Response,
        message: String,
    },

    #[error("message delivery to {target} rejected ({code:?}): {message}")]
    DeliveryFailed {
        target: String,
        code: irc::proto::Response,
        message: String,
    },

    #[error("bot is no longer in channel {channel}{}", reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default())]
    BotLeftRoom {
        channel: String,
        reason: Option<String>,
    },

    #[error("bot does not have channel operator (+o) privileges in {channel}")]
    NotChannelOp {
        channel: String,
    },

    #[error("timed out waiting for {operation}")]
    HandshakeTimeout {
        operation: &'static str,
    },

    #[error("invalid IRC parameter: {0}")]
    InvalidInput(&'static str),
}

impl Error {
    /// Returns `false` for expected per-player/per-target interruptions (`TimedOut`, `UserLeft { .. }`, `DeliveryFailed { .. }`),
    /// and `true` for connection, lag, or protocol-level failures.
    pub fn is_fatal(&self) -> bool {
        !matches!(
            self,
            Error::TimedOut | Error::UserLeft { .. } | Error::DeliveryFailed { .. }
        )
    }
}

pub type Result<T> = std::result::Result<T, Error>;
```

---

## 5. Complete Cookbook & Idiomatic Patterns

### 5.1 The Heist: Lobby Loop, Handle-Scoped Timeouts, Admin Stop, and Breakout Room

```rust
use std::time::Duration;
use ircgm::prelude::*;

enum Stop {
    Shutdown,
    Admin(ChatMessage),
    LeaderAbort(ChatMessage),
}

// Prefer IRCv3 `account-tag`; fall back to network-cloaked hostmasks only on networks that enforce cloaks.
fn is_admin(msg: &ChatMessage) -> bool {
    msg.account.as_deref() == Some("seirl") || msg.host.as_deref() == Some("staff/seirl")
}

async fn lobby(bot: Bot, room: Room) -> Result<()> {
    loop {
        let trigger = match room.inbox().wait_cmd("!heist").until(bot.shutdown_requested()).await {
            Outcome::Completed(res) => res?,
            Outcome::Stopped(()) => break,
        };
        let leader = &trigger.sender;

        // `ok_or_pending()` ensures a user disconnecting (`Err(UserLeft)`) never falsely triggers `Outcome::Stopped`!
        let admin_stop = bot
            .inbox_all()
            .wait_for(|m| is_admin(m) && m.is_cmd("!stop") && (m.is_dm() || m.is_in(&room)))
            .ok_or_pending()
            .map_output(Stop::Admin);
        // Note: `leader.inbox()` listens to DMs from `leader` (`!abort` in private message; to also hear `!abort`
        // in `#room`, use `bot.inbox_scope(InboxScope::new().room(&room).dm_from(leader)).wait_for(|m| &m.sender == leader && m.is_cmd("!abort"))`).
        let leader_abort = leader
            .inbox()
            .wait_cmd("!abort")
            .ok_or_pending()
            .map_output(Stop::LeaderAbort);

        match heist_mission(bot.clone(), room.clone(), leader)
            .until(bot.shutdown_requested().map_output(|()| Stop::Shutdown))
            .or_until(admin_stop)
            .or_until(leader_abort)
            .await
        {
            Outcome::Completed(Ok(())) => {}
            Outcome::Completed(Err(Error::TimedOut)) => {
                // `vault`'s `timeout_message` was already sent before `Breakout::drop` kicked & parted!
                room.say(leader.nick().bold() + " hesitated too long and got caught!")?;
            }
            Outcome::Completed(Err(Error::HandshakeTimeout { .. })) => {
                room.say(leader.nick().bold() + " never showed up at the vault!")?;
            }
            Outcome::Completed(Err(e)) => room.say("Heist ended early: ".red() + e.to_string())?,
            Outcome::Stopped(Stop::Shutdown) => {
                room.say("Server restarting—heist called off!".yellow())?;
                break;
            }
            Outcome::Stopped(Stop::Admin(admin_msg)) => {
                // `heist_mission` dropped -> `vault` synchronously queued KICK + PART!
                bot.reply(&admin_msg, "Heist aborted.".yellow())?;
                if admin_msg.is_dm() {
                    room.say("ABORTED".bold().white().on_red() + " Heist stopped by an admin.")?;
                }
            }
            Outcome::Stopped(Stop::LeaderAbort(_)) => {
                room.say(leader.nick().bold() + " called off the heist!".yellow())?;
            }
        }
    }
    Ok(())
}

async fn heist_mission(bot: Bot, lobby: Room, leader: &User) -> Result<()> {
    // Zero-indentation `AskConfig` directly on `vault`:
    let vault = bot
        .breakout("vault")
        .await?
        .with_ask_config(
            &AskConfig::new()
                .reply_timeout(Duration::from_secs(45))
                .timeout_message("ALERT".bold().white().on_red() + " Too slow—security spotted you!"),
        );

    lobby.say(leader.nick().bold().yellow() + ": check your invite to " + vault.name().cyan() + "!")?;
    vault.invite_and_wait([leader], Duration::from_secs(30)).await?;

    let plan = vault
        .ask_user(
            leader,
            "Choose approach: " + "!plan stealth".bold().light_cyan() + " | " + "!plan loud".bold().orange(),
            |m| match m.cmd_args("!plan") {
                Some(choice @ ("stealth" | "loud")) => Some(choice.to_owned()),
                _ => None,
            },
        )
        .await?;

    // Per-call override for a fast final step (inherits/overrides cleanly without indenting):
    let wire = vault
        .ask_user(leader, "Cut " + "red".red().bold() + " or " + "blue".light_blue().bold() + " wire?! (10s)", |m| {
            match m.text.trim() {
                w @ ("red" | "blue") => Some(w.to_owned()),
                _ => None,
            }
        })
        .reply_timeout(Duration::from_secs(10))
        .await?;

    lobby.say(leader.nick().bold().green() + " cut the " + wire + " wire and pulled off the " + plan.bold() + " heist!")?;
    Ok(())
}
```

### 5.2 Blackjack: Turn-Based `room.ask_user`, Player Timeout Callouts, & `.settled()`

Demonstrates configuring a `Room` handle once with zero indentation via `room.with_ask_config(...)` so open-floor prompts and per-player turns share a 30-second deadline, calling out whichever player timed out by their live nickname, and using `.settled()` so one player timing out *or* leaving the channel folds only their hand without aborting the table:

```rust
use std::time::Duration;
use ircgm::prelude::*;

#[derive(PartialEq, Eq)]
enum Move {
    Hit,
    Stand,
}

async fn blackjack_round(room: &Room, players: &[User]) -> Result<()> {
    let room = room.with_ask_config(
        &AskConfig::new()
            .reply_timeout(Duration::from_secs(30))
            .timeout_message_fn(|ctx| match ctx.user {
                Some(u) => u.nick().bold().yellow() + ": ⏳ too slow, auto-standing!",
                None => Text::from("⏳ Time's up!".yellow()),
            }),
    );

    // Open-floor prompt (`ctx.user` is `None` -> says "⏳ Time's up!" and aborts via `?` if nobody deals):
    room.ask("Anyone type `!deal` to cut the deck!", |m| m.is_cmd("!deal").then_some(())).await?;

    // Per-player turns (`ctx.user` is `Some(player)` -> calls out `<live_nick>: ⏳ too slow, auto-standing!`):
    for player in players {
        loop {
            match room
                .ask_user(player, player.nick().bold() + ": `hit` or `stand`?", |m| match m.text.trim() {
                    "hit" => Some(Move::Hit),
                    "stand" => Some(Move::Stand),
                    _ => None,
                })
                .settled() // Catches both `TimedOut` and `UserLeft` locally; propagates fatal errors via `?`!
                .await?
            {
                Turn::Reply(Move::Hit) => room.say(player.nick().bold() + " draws a card!")?,
                Turn::Reply(Move::Stand) | Turn::TimedOut => break, // Callout already sent on TimedOut!
                Turn::Left { nick, .. } => {
                    room.say(nick.bold() + " left the table—hand folded.")?;
                    break;
                }
                Turn::Unreachable { .. } => break,
            }
        }
    }
    Ok(())
}
```

### 5.3 The Quiz: Burst-Safe Recruitment, Unique Users, and Wire-Fair Multi-Player DM Race

Notice three critical details in this pattern:
1. **`HashSet<User>`** deduplicates players across `/nick` and RFC 1459 case changes out of the box.
2. **Wire-order fairness (`exclusive_many` + `inbox_users` instead of `select_all`):** Holding `_locks = bot.exclusive_many(&players).await?` blocks any concurrent `user.ask()` triggers for the duration of the round, while subscribing a **single** `bot.inbox_users(&players)` *after* locking and *before* `p.say(...)` delivers every player's DM replies in exact `msg.seq` wire arrival order (whereas `select_all` over separate `Ask`s biases same-batch ties toward index `0`; note that unserialized DM listeners like `!help` still see race replies).
3. **Fixed round deadline + `.settled()`:** Wrapping `race_in.wait_map(...)` in `.with_timeout(Duration::from_secs(20)).settled()` enforces a fixed 20-second round clock (unlike per-read idle `with_read_timeout`, wrong guesses from busy players **never** reset it!) and handles *all* players disconnecting mid-round via `Turn::Left` without aborting `lobby`:

```rust
use std::collections::HashSet;
use std::time::Duration;
use ircgm::prelude::*;

async fn quiz_game(bot: Bot, room: Room) -> Result<()> {
    let players: Vec<User> = {
        let mut inbox = room.inbox();
        room.say("First 3 to type " + "!join".bold().green() + " get to play!")?;

        let mut set = HashSet::with_capacity(3);
        while set.len() < 3 {
            let msg = inbox.wait_cmd("!join").await?;
            if set.insert(msg.sender.clone()) {
                room.say(msg.sender.nick().bold() + format!(" joined! ({}/3)", set.len()))?;
            }
        }
        set.into_iter().collect()
    }; // `inbox` drops here!

    let _locks = bot.exclusive_many(&players).await?;
    let mut race_in = bot.inbox_users(&players);
    for p in &players {
        p.say("Quick! What is 17 * 6? (20s)")?;
    }

    match race_in
        .wait_map(|m| (m.text.trim() == "102").then(|| (m.sender.clone(), m.text.clone())))
        .with_timeout(Duration::from_secs(20))
        .settled()
        .await?
    {
        Turn::Reply((winner, answer)) => {
            room.say(winner.nick().bold().green() + " won with " + answer.bold() + "!")?;
        }
        Turn::TimedOut => {
            for p in &players {
                let _ = p.say("⏳ Time's up!".yellow());
            }
            room.say("Nobody answered in time!".yellow())?;
        }
        Turn::Left { .. } | Turn::Unreachable { .. } => {
            room.say("All players disconnected—quiz cancelled.".yellow())?;
        }
    }
    Ok(())
}
```

### 5.4 Mafia Moderation, Secret `Huddle` Deliberation, Cancellable `ExclusiveDm`, & Queued Prompts

Demonstrates channel moderation + all four DM interaction patterns:
1. **Batched synchronous Mafia phase moderation (`set_mode`, `voice_many`, `devoice_many`, `set_topic`):** Updating channel voice in `005 MODES`-sized chunks and setting `+m` without hanging on idempotent `MODE`s or burning burst throttle lines.
2. **Channel-free pack deliberation (`bot.huddle_available` + `.with_relay(...)`):** Werewolves whisper to coordinate and pick a victim right in their bot DM window (zero `/join` friction on standard IRC clients, private `Verdict::reject` on invalid picks, resilient if a wolf disconnected).
3. **Cancellable multi-message `ExclusiveDm` (`with_ignore_commands()` + `.until(...)` + `ok_or_pending()`):** Reading 3 free-text item names without swallowing `!cancel` (dropping the flow immediately releases `dm_mutex` for queued callers!).
4. **Queued reactive DM questions + inline `Verdict::Reject` validation (`user.ask()`) & secret public-prompt DM votes:**

```rust
use std::time::Duration;
use futures::future::try_join_all;
use ircgm::prelude::*;

fn start_mafia_night(room: &Room, alive: &[User], dead: &[User], night: u32) -> Result<()> {
    room.set_topic(format!("🌙 Mafia — Night {night} ({} alive)", alive.len()))?;
    room.set_mode("+m", &[])?;
    room.devoice_many(dead)?;
    room.voice_many(alive)?;
    Ok(())
}

async fn werewolf_pack_kill(bot: &Bot, wolves: &[User], townsfolk: &[User]) -> Result<Option<User>> {
    let mut pack = bot
        .huddle_available(wolves)
        .await?
        .with_relay("[Wolves] ".red().bold())
        .with_ask_config(
            &AskConfig::new()
                .reply_timeout(Duration::from_secs(30))
                .timeout_message("⏳ Dawn breaks—the pack missed its chance!".yellow())
                .ignore_commands(),
        );

    let pick = pack
        .ask("🐺 Whisper here to deliberate, or `!kill <nick>` to strike (30s):", |m| {
            let Some(target) = m.cmd_args("!kill") else { return Verdict::Ignore }; // Relayed as pack chat!
            match townsfolk.iter().find(|u| u.eq_nick(target)) {
                Some(victim) => Verdict::Accept((m.sender.clone(), victim.clone())),
                None => Verdict::reject("Pick a living non-Werewolf villager!".yellow()),
            }
        })
        .settled()
        .await?;

    match pick {
        Turn::Reply((killer, victim)) => {
            pack.say("🐺 ".bold() + killer.nick().bold() + " locked in " + victim.nick().bold().red() + "!")?;
            Ok(Some(victim))
        }
        Turn::TimedOut | Turn::Left { .. } | Turn::Unreachable { .. } => Ok(None),
    }
}

async fn pick_loadout_or_cancel(alice: &User) -> Result<Option<[String; 3]>> {
    let flow = async {
        // `with_ignore_commands()` ensures `!cancel` / `!help` are NOT consumed as item names!
        let mut dm = alice.exclusive().await?.with_ignore_commands();
        dm.say("Name 3 items to bring into the vault (send 3 messages, or `!cancel`):")?;
        Ok::<_, ircgm::Error>([
            dm.inbox().next_chat().await?.text,
            dm.inbox().next_chat().await?.text,
            dm.inbox().next_chat().await?.text,
        ])
    };

    // Unserialized `alice.inbox()` hears `!cancel` while `dm` holds `dm_mutex`;
    // `ok_or_pending()` ensures Alice disconnecting returns `Err(UserLeft)` from `flow` rather than a false cancel!
    match flow.until(alice.inbox().wait_cmd("!cancel").ok_or_pending()).await {
        Outcome::Completed(res) => res.map(Some),
        Outcome::Stopped(_) => {
            alice.say("Loadout selection cancelled.".yellow())?;
            Ok(None)
        }
    }
}

async fn resolve_simultaneous_triggers(alice: &User) -> Result<(bool, Option<usize>)> {
    // Even though `tokio::try_join!` polls both concurrently on the SAME user,
    // `alice.ask(...)` serializes them cleanly: Alice gets Question 1 (with 20s), answers it,
    // and THEN gets Question 2 with its own full 20s reply clock!
    // `.ignore_commands()` skips `!help` BEFORE the matcher so `Verdict::reject` never fires on commands.
    let prompt_cfg = AskConfig::new()
        .reply_timeout(Duration::from_secs(20))
        .ignore_commands()
        .no_timeout_message();
    let (opt_block, opt_card) = tokio::try_join!(
        alice
            .ask("Bob played Steal on you! Play Block? (yes/no, 20s)", |m| match m.text.trim() {
                "yes" => Some(true),
                "no" => Some(false),
                _ => None,
            })
            .config(&prompt_cfg)
            .warn_remaining(Duration::from_secs(5), "⏳ 5 seconds left!".yellow())
            .optional(),
        alice
            .ask("Discard a card (1, 2, or 3):", |m| match m.text.trim().parse::<usize>() {
                Ok(n @ 1..=3) => Verdict::Accept(n),
                _ => Verdict::reject("Please reply with 1, 2, or 3.".yellow()),
            })
            .config(&prompt_cfg)
            .optional(),
    )?;
    Ok((opt_block.unwrap_or(false), opt_card))
}

async fn collect_secret_votes(room: &Room, alive: &[User]) -> Result<Vec<(User, Option<String>)>> {
    let mut dms: Vec<_> = alive.iter().map(|u| (u.clone(), u.inbox().lossy())).collect();
    room.say("Night falls! Everyone DM me `!vote <nick>` within 45 seconds.")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);

    try_join_all(dms.iter_mut().map(|(voter, inbox)| async move {
        let pick = inbox
            .wait_map(|m| m.cmd_args("!vote").filter(|s| !s.is_empty()).map(str::to_owned))
            .with_deadline(deadline)
            .settled() // Voter abstaining (`TimedOut`) OR disconnecting (`Left`) yields `None` without aborting others!
            .await?
            .into_reply();
        Ok::<_, ircgm::Error>((voter.clone(), pick))
    }))
    .await
}
```

### 5.5 Live Admin Control Panel: Dynamic Game Speed (`!speed`) + `!stop`

Demonstrates composing `Bot` and `Inbox` with `tokio::select!` and `tokio::sync::watch`—including a 15-line `scaled_sleep` recipe in user code that adjusts an already-ticking countdown mid-flight:

```rust
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;
use ircgm::{Bot, ChatMessage, Result, Room};

enum AdminCmd {
    Speed(f64),
    Stop,
}

fn is_admin(m: &ChatMessage) -> bool {
    m.account.as_deref() == Some("seirl") || m.host.as_deref() == Some("staff/seirl")
}

fn parse_admin_dm(m: &ChatMessage) -> Option<(ChatMessage, AdminCmd)> {
    if !is_admin(m) {
        return None;
    }
    if m.is_cmd("!stop") {
        return Some((m.clone(), AdminCmd::Stop));
    }
    let factor = m.cmd_args("!speed")?.parse::<f64>().ok().filter(|&x| x > 0.0)?;
    Some((m.clone(), AdminCmd::Speed(factor)))
}

async fn scaled_sleep(virtual_dur: Duration, speed_rx: &mut watch::Receiver<f64>) {
    let mut remaining = virtual_dur.as_secs_f64();
    while remaining > 0.0 {
        let rate = (*speed_rx.borrow_and_update()).max(0.01);
        let start = Instant::now();
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs_f64(remaining / rate)) => break,
            Ok(()) = speed_rx.changed() => {
                remaining = (remaining - start.elapsed().as_secs_f64() * rate).max(0.0);
            }
        }
    }
}

async fn run_supervised_game(bot: Bot, room: Room) -> Result<()> {
    let (speed_tx, mut speed_rx) = watch::channel(1.0_f64);

    let admin_panel = async {
        let mut inbox = bot.inbox_dms();
        loop {
            let (msg, cmd) = inbox.wait_map(parse_admin_dm).await?;
            match cmd {
                AdminCmd::Speed(factor) => {
                    let _ = speed_tx.send(factor);
                    bot.reply(&msg, format!("Speed set to {factor}x."))?;
                }
                AdminCmd::Stop => {
                    bot.reply(&msg, "Stopping game.")?;
                    return Ok::<_, ircgm::Error>(());
                }
            }
        }
    };

    let game = async {
        room.say("You have 60 seconds to deliberate!")?;
        scaled_sleep(Duration::from_secs(60), &mut speed_rx).await;
        room.say("Time's up!")?;
        Ok(())
    };

    tokio::select! {
        res = game => res,
        res = admin_panel => res,
    }
}
```

### 5.6 Multi-Room Wire-Ordered Bridge (`inbox_rooms`) & Stateless Ping/Pong

```rust
use ircgm::prelude::*;

async fn diplomat_bridge(bot: Bot) -> Result<()> {
    let usa = bot.join("#usa").await?;
    let ussr = bot.join("#ussr").await?;
    // Single wire-ordered multi-room inbox—no `tokio::select!` needed even across 7 Diplomacy powers!
    let mut bridge_in = bot.inbox_rooms([&usa, &ussr]);

    loop {
        let msg = bridge_in.next_chat().await?;
        if msg.is_in(&usa) {
            ussr.say("[USA] ".light_blue().bold() + "<" + (&msg.nick).bold() + "> " + msg.to_styled())?;
        } else {
            usa.say("[USSR] ".red().bold() + "<" + (&msg.nick).bold() + "> " + msg.to_styled())?;
        }
    }
}

async fn ping_pong(bot: Bot) -> Result<()> {
    let mut inbox = bot.inbox_all();
    loop {
        let msg = inbox.wait_cmd("!ping").await?;
        bot.reply(&msg, "pong".green().bold())?;
    }
}
```

### 5.7 Quiplash / Simultaneous Secret Prompts with Live `"✔ Locked In!"` Progress (`FuturesUnordered`)

> **Concurrency Rule of Thumb (`inbox_users` vs. Future Combinators):**
> - For **single-message speed races** (*"first player to answer `102` wins"*, §5.3), always use **`bot.inbox_users`** / **`bot.inbox_rooms`**—never `select_all` over separate `Ask`s—so same-batch ties are decided strictly by wire `msg.seq` rather than `Vec` poll order.
> - Use **`FuturesUnordered`**, **`tokio::select!`**, and **`tokio::try_join!`** when every player has a **personalized prompt / `Verdict::Reject` validator** whose answers are all collected (below), or when branches are **multi-step `async fn` sub-flows** that a flat `Inbox` cannot express (§5.8).

Here each player receives a different secret prompt in DM (with inline length validation, `.ignore_commands()`, and a 10s countdown warning), and `#room` announces `"✔ Alice locked in! (1/4)"` in real time as each player finishes. Notice also two key details in how `.settled()` handles DM outcomes:
- **`timeout_message` is local to the `Ask`'s target:** `cfg.timeout_message("⏳ Pencils down...")` sends that message **privately in the player's DM**, while matching `Turn::TimedOut => room.say(...)` below announces `"Alice ran out of time!"` publicly in `#room`!
- **`Turn::Unreachable` isolates PM-blockers (`+g` / `+R`):** If one player has server-side `+g` (`callerid`) or `+R` enabled so the bot's DM bounces with `716` / `486`, `.settled()` yields `Turn::Unreachable` for *that player only* instead of aborting the other 3 players' round:

```rust
use std::time::Duration;
use futures::stream::{FuturesUnordered, StreamExt};
use ircgm::prelude::*;

async fn quiplash_round(room: &Room, assignments: &[(User, &str)]) -> Result<Vec<(User, String)>> {
    let total = assignments.len();
    let cfg = AskConfig::new()
        .reply_timeout(Duration::from_secs(45))
        // Sent locally to the prompt's target (each player's private DM) if their 45s clock expires:
        .timeout_message("⏳ Pencils down—time's up!".yellow())
        .ignore_commands();

    let mut tasks: FuturesUnordered<_> = assignments
        .iter()
        .map(|(player, secret_prompt)| {
            let cfg = &cfg;
            async move {
                let turn = player
                    .ask(
                        "Prompt: ".bold().cyan() + *secret_prompt + " (45s, max 80 chars)",
                        |m| {
                            let quip = m.text.trim();
                            if quip.is_empty() || quip.len() > 80 {
                                Verdict::reject("Keep your answer between 1 and 80 characters!".yellow())
                            } else {
                                Verdict::Accept(quip.to_owned())
                            }
                        },
                    )
                    .config(cfg)
                    .warn_remaining(Duration::from_secs(10), "⏳ 10 seconds left to submit!".yellow())
                    .settled()
                    .await?;
                Ok::<_, ircgm::Error>((player.clone(), turn))
            }
        })
        .collect();

    let mut submissions = Vec::with_capacity(total);
    while let Some(res) = tasks.next().await {
        let (player, turn) = res?;
        match turn {
            Turn::Reply(quip) => {
                submissions.push((player.clone(), quip));
                room.say(
                    "✔ ".green().bold()
                        + player.nick().bold()
                        + format!(" locked in! ({}/{total})", submissions.len()),
                )?;
            }
            Turn::TimedOut => {
                // `cfg.timeout_message` already notified `player` in DM; announce it publicly in `#room` too:
                room.say(player.nick().bold().yellow() + " ran out of time!")?;
            }
            Turn::Left { nick, .. } => {
                room.say(nick.bold() + " disconnected.")?;
            }
            Turn::Unreachable { code, .. } => {
                room.say(
                    player.nick().bold().red()
                        + format!(" couldn't receive DMs ({code:?} — check `/umode -g` / NickServ)!"),
                )?;
            }
        }
    }
    Ok(submissions)
}
```

### 5.8 Co-op Bomb Defusal: Racing Multi-Step `async fn`s (`tokio::select!` + `tokio::try_join!`)

Demonstrates where direct-style futures truly shine—composing **whole multi-turn `async fn` state machines** with standard Tokio primitives:
1. **Stage 1 (`tokio::select!` + `.settled()` on multi-step `async fn`s):** Two specialists attempt asymmetric multi-step bypasses simultaneously (`hack_terminal` in a 2-step `ExclusiveDm` vs. `pick_pins` in `#vault`). Because `GameFutureExt::settled()` works on *any* `Future<Output = Result<T>>`, one specialist timing out or disconnecting disables only their branch while the other keeps going; as soon as *either* finishes, returning drops the other pinned future—automatically releasing `hacker`'s `ExclusiveDm` mutex without firing `timeout_message`!
2. **Stage 2 (`tokio::try_join!` + outer `.with_timeout(15s)`):** Both players must turn their keys before a shared 15-second bomb clock expires:

```rust
use std::time::Duration;
use ircgm::prelude::*;

async fn hack_terminal(hacker: &User) -> Result<()> {
    let mut dm = hacker
        .exclusive()
        .await?
        .with_ask_config(
            &AskConfig::new()
                .reply_timeout(Duration::from_secs(30))
                .ignore_commands(),
        );

    dm.ask("Step 1/2 — Convert `0x2A` to decimal:", |m| match m.text.trim() {
        "42" => Verdict::Accept(()),
        _ => Verdict::reject("Wrong checksum—try again!".red()),
    })
    .await?;

    dm.ask("Step 2/2 — Type `override` to cut blast-door power:", |m| {
        (m.text.trim() == "override").then_some(())
    })
    .await
}

async fn pick_pins(room: &Room, lockpick: &User) -> Result<()> {
    let room = room.with_ask_config(&AskConfig::new().reply_timeout(Duration::from_secs(20)));
    for (pin, expected) in [(1, "!tension"), (2, "!rake"), (3, "!set")] {
        room.ask_user(
            lockpick,
            lockpick.nick().bold() + format!(": Pin {pin}/3 (`!tension`, `!rake`, or `!set`)?"),
            |m| m.is_cmd(expected).then_some(()),
        )
        .await?;
    }
    Ok(())
}

async fn coop_bomb_defusal(room: &Room, hacker: &User, lockpick: &User) -> Result<()> {
    room.say("🚨 Blast door sealed! Hacker (check DM) and Lockpick (here)—first bypass opens the core!")?;

    let method = {
        let hack_fut = hack_terminal(hacker).settled();
        let pick_fut = pick_pins(room, lockpick).settled();
        tokio::pin!(hack_fut, pick_fut);
        let (mut hack_active, mut pick_active) = (true, true);

        loop {
            if !hack_active && !pick_active {
                room.say("💥 Neither specialist breached the door in time!".red().bold())?;
                return Ok(());
            }
            tokio::select! {
                res = &mut hack_fut, if hack_active => match res? {
                    Turn::Reply(()) => break "hacked the terminal", // Drops `pick_fut` cleanly!
                    Turn::TimedOut | Turn::Left { .. } | Turn::Unreachable { .. } => hack_active = false,
                },
                res = &mut pick_fut, if pick_active => match res? {
                    Turn::Reply(()) => break "picked the vault lock", // Drops `hack_fut` + `ExclusiveDm`!
                    Turn::TimedOut | Turn::Left { .. } | Turn::Unreachable { .. } => pick_active = false,
                },
            }
        }
    };

    room.say("🔓 Door open (" + method.green().bold() + ")! BOTH turn keys within 15s (`!turn alpha` / `!turn omega`)!")?;

    let defused = tokio::try_join!(
        room.ask_user(hacker, hacker.nick().bold() + ": `!turn alpha`!", |m| {
            (m.cmd_args("!turn") == Some("alpha")).then_some(())
        })
        .no_reply_timeout(),
        room.ask_user(lockpick, lockpick.nick().bold() + ": `!turn omega`!", |m| {
            (m.cmd_args("!turn") == Some("omega")).then_some(())
        })
        .no_reply_timeout(),
    )
    .with_timeout(Duration::from_secs(15))
    .optional()
    .await?;

    if defused.is_some() {
        room.say("🎉 BOMB DEFUSED with seconds to spare!".green().bold())?;
    } else {
        room.say("💥 BOOM! Keys weren't turned in sync!".bold().white().on_red())?;
    }
    Ok(())
}
```

---

## 6. Testing Strategy

`ircgm` is **100% deterministically unit-testable in memory** via `ircgm::testing::test_harness` (`TestServer` + `TestServer::settle().await` on Tokio's `current_thread` test runtime) without sockets, `irc::client::Client`, or an external IRCd:

1. **Packet-batching burst, wire-order fair race (`inbox_users`), & event identity test (`current_thread` + `settle()`):**
   - Inject `:Alice!u@h PRIVMSG #q :!join` and `:Bob!u@h PRIVMSG #q :!join` back-to-back before polling (and verify with `server.settle().await`); assert `room.inbox()` yields both in order with zero drops.
   - **Fair-race regression:** With `[alice, bob]` in vector order, inject `:Bob!u@h PRIVMSG Bot :102` followed by `:Alice!u@h PRIVMSG Bot :102` in a **single batch**; assert `bot.inbox_users(&[alice, bob])` picks **Bob** (`msg.seq` wire order).
   - **Multi-source `InboxScope` & unified source-exhaustion rule:** Verify `bot.inbox_scope(InboxScope::new().rooms([&r1, &r2]).dms_from([&alice, &bob]))` yields interleaved room + DM messages in exact `seq` order; verify `alice` quitting emits `Event::Quit` in-band (including on `r1.inbox().next_event()`) while `bot.inbox_users(&[alice, bob]).wait_map(...)` keeps running for `bob` and returns `Err(Error::UserLeft { .. })` only once `bob` also quits; and verify a mixed `InboxScope` (`.room(&r1).dms_from([&alice, &bob])`) **keeps running** for `r1` even after *both* `alice` and `bob` quit!
   - Inject `:Alice!u@h PRIVMSG #r :hit`, `:Alice!u@h NICK :Alice2`, `:Mallory!m@h NICK :Alice`, and `:Alice2!u@h QUIT :bye` in a **single batch**; verify `room.ask_user(&alice, ...)` still receives `"hit"` (not `Err(Error::UserLeft { .. })`), and `msg.sender` on an earlier message resolves to `Alice2` (`!= bot.user("Alice")?`).
   - Verify `Event::{Joined, Parted, Kicked}` from `bot.inbox_all()` carry `channel` and a stable `User` (plus `channels` on `Quit`/`NickChanged`, `ident`, `server_time` parsing, and `received_at`).
2. **One-shot non-backlog, `bot.inbox_dms()`, `MessageKind`, & `NOTICE` isolation / `.accept_notices()` test:**
   - Verify `room.inbox().wait_cmd("!heist").await` drops its subscription upon matching so messages sent during a sub-task are never replayed, and `bot.inbox_dms()` receives DMs across users while ignoring channel chatter.
   - Inject `:NickServ!s@services NOTICE Bot :!heist`, `:irc.example.org NOTICE Bot :!heist`, `@bot :OtherBot!b@h NOTICE #lobby :!heist`, and `:Alice!u@h NOTICE Bot :!vote Bob`:
     - Verify by default **none** of them satisfies `wait_cmd`, `next_chat`, or `ask`, while `inbox.next_event()` surfaces `Event::Notice`.
     - With `.accept_notices()` enabled on `Inbox` / `Ask` / `AskConfig`, verify `:Alice!u@h NOTICE Bot :!vote Bob` **does** yield `ChatMessage { kind: MessageKind::Notice, .. }` and `bot.reply(&msg, "Recorded")` emits an outbound `NOTICE` (never `PRIVMSG`!), while server notices, `NickServ`/`ChanServ` pseudoclients, and IRCv3 `bot`-tagged notices stay excluded!
3. **Concurrent `ask` / `exclusive`, write-receipt `Ask` timers, `!cancel` release, `.ignore_commands()`, `Verdict::Reject`, `.warn_remaining()`, `exclusive_many` / `exclusive_available`, & combinator table test:**
   - Launch a 3-message `alice.exclusive()` block alongside a concurrent `alice.ask("Interrupt?", ...)`; verify all 3 replies are delivered exclusively to the `ExclusiveDm` block before `"Interrupt?"` is even sent on the wire, and verify holding `dm_mutex` past 15s while another caller waits logs a `tracing::warn!` naming the holder's `#[track_caller]` `Location`.
   - **Cancel during `ExclusiveDm` & command skipping:** Race an `ExclusiveDm` (`with_ignore_commands()`) against `alice.inbox().wait_cmd("!cancel")` alongside a queued `alice.ask(...)`; verify `!cancel` is skipped by the free-text read (whereas without `ignore_commands()` it is consumed), cancels the exclusive flow, releases `dm_mutex`, and lets the queued `alice.ask(...)` proceed immediately!
   - **`Verdict::Reject`, `.warn_remaining()` / `.clear_warnings()`, & `IntoVerdict` closure inference:** Verify `Verdict::Reject(Text)` sends validation feedback without restarting the reply timer (and never fires on ignored `!help` commands when `.ignore_commands()` is set), verify `.warn_remaining(5s, ...)` (on both `AskConfig` and `Ask`) fires relative to the post-write-receipt reply clock without resending the prompt or aborting the `Ask` (and `.clear_warnings()` suppresses inherited warnings), and compile-test multi-arm `None`/`Some(x)` closures alongside `Verdict<T>` closures.
   - **Stale-inbox regression:** Inside one `ExclusiveDm`, let Ask #1 time out, inject Alice's late reply to Ask #1, then start Ask #2 (`dm.ask(...)`); verify Ask #2 clears the stale buffer and does **not** match the late reply.
   - **Write-receipt pacing, `ExclusiveDm::ask` single-timer regression, & pure combinator table:** Using `tokio::time::pause()` and `ircgm::testing::test_harness_with` (`outbound_burst = 2`, `outbound_window = 8s`), enqueue 4 prompts at once and verify Ask #3's 10s `reply_timeout` starts **only when `OutboundSink`'s write receipt resolves at `t = 8s`** (expiring at `t = 18s`, not `t = 10s`!); verify `dm.ask(...).reply_timeout(120s)` and `.no_reply_timeout()` lengthen/disable an `ExclusiveDm`'s 30s `AskConfig` without `self.inbox`'s idle `read_limit` cutting it short at 30s; and run a table test confirming `optional()`, `settled()`, and `with_timeout()` **never** emit or suppress messages.
   - **Config-free `User`, `exclusive_many` / `exclusive_available`, `Huddle`, & static `Send`/`Sync` assertions:** Verify `User` is config-free while `AskConfig` shares settings cleanly; verify reversed-order concurrent `exclusive_many` calls never deadlock and return guards in caller order, duplicates fail fast with `Err(Error::InvalidInput)`, and a `QUIT` user fails `exclusive_many` with `Err(Error::UserLeft { .. })` (releasing already-acquired guards) while `exclusive_available` skips the offline/quitting user and returns the remaining online guards (`Ok(Vec::new())` if all offline); verify `bot.huddle` / `bot.huddle_available` relays `Verdict::Ignore` banter to online peers when `with_relay` is enabled, sends `Verdict::Reject` privately to `msg.sender` only, skips `.ignore_commands()` lines before relay, resolves `Verdict::Accept` in wire `seq` order, fails immediately with `UserLeft` when `0` online users remain, and releases all `dm_mutex`es on `Drop`; include static compile-time `Send + Sync` assertions on all core types.
4. **`room.ask_user`, `/nick` tracking, `TimeoutCtx`, & `.settled()` multi-player isolation test:**
   - Configure `room` with `room.with_ask_config(&AskConfig::new().reply_timeout(30s).timeout_message_fn(...))`, call `room.ask_user(&alice, ...).settled()`, inject `:Bob!u@h PRIVMSG #r :hit` (verify ignored), inject `:Alice!u@h NICK :Alice2`, advance 30s, and assert outbound `PRIVMSG #r` calls out `"Alice2"` by her new nick (`configured_timeout == Some(30s)`) and returns `Ok(Turn::TimedOut)`.
   - Verify `PART` or `KICK` of `Alice2` mid-turn immediately resolves `.settled()` to `Ok(Turn::Left { nick: "Alice2", .. })` and `486`/`716` resolves `.settled()` to `Ok(Turn::Unreachable { .. })` (`is_fatal() == false`) without aborting `try_join_all` / `FuturesUnordered` for remaining players, while `Error::Disconnected(_)` still returns `Err(Error::Disconnected(_))` (`is_fatal() == true`).
5. **`WorldState`, `005` (`CASEMAPPING` re-keying + `TARGMAX` `KICK`), batched moderation, `netsplit`, `user.account()`, & amortized `UserRegistry` GC test:**
   - Feed `005 PREFIX=(qaohv)~&@%+ MODES=4 TARGMAX=PRIVMSG:4,KICK:1 CHANMODES=eIbq,k,flj,CFLMPQScgimnprstuz CASEMAPPING=rfc1459`, multi-packet `353 RPL_NAMREPLY` + `366 RPL_ENDOFNAMES`, `332 RPL_TOPIC`, and parameterized `MODE #c +klo secret 50 Alice`; verify `room.members()` / `room.member(&u)` / `room.has_user(&u)` reflect `@`/`+` and preserve `User` identity across `MODE` and `NICK`, `room.voice_many` / `devoice_many` over 10 users emit `3` chunked `MODE #c +vvvv ...` / `+vv` commands (`4 + 4 + 2`), `Breakout::drop` emits individual `KICK`s when `TARGMAX` has `KICK:1` (or absent) and batches `KICK #c a,b,c` when `TARGMAX=KICK:3` is advertised, `room.topic()` tracks topic changes, and `room.kick` / `set_mode` / `voice*` / `devoice*` / `set_topic` fail synchronously with `Err(Error::NotChannelOp { .. })` when the bot lacks `+o`.
   - Verify dynamic `005 CASEMAPPING=ascii` after pre-existing `[Alice]` tracks/channels/inboxes exist cleanly re-keys `WorldState`, `UserRegistry`, and `InboxRegistry`.
   - Verify `[Alice]` and `{alice}` resolve to the same `User` (`u1 == u2` and `u1.eq_nick("{alice}")`), survive `:Alice!u@h NICK :Alice2` (`u1.nick() == "Alice2"`), allocate a fresh `UserTrack` on `QUIT` + rejoin while `Event::Quit { netsplit: true, .. }` detects `"*.net *.split"` vs normal quit messages (`netsplit == false`), update `u1.account()` across `extended-join` / `account-tag` / `ACCOUNT` (`account-notify` including `*` logout), and sweep dead `Weak<UserTrack>` entries via amortized `len > 2 * last_live_count` GC with zero `Drop` mutex re-entrancy.
6. **Acyclic `Bot` drop, `376`/`422` registration barrier, `OutboundSink` priority scheduling, `Breakout`, & `bot.quit` drain test:**
   - **Acyclic `Bot` lifetime & memory release:** Verify dropping the last `Bot` handle stops the pump immediately, clears `WorldState` and `InboxRegistry`, and drops `Arc<BotCore>` strong count to `0` even when `Member { User }` and unread `WireEvent { User }` handles existed (surviving `Room`/`User`/`Inbox` handles return `Err(Error::Disconnected(DisconnectReason::BotDropped))`), and verify the registration barrier waits for `CAP ACK/NAK` -> `CAP END` + `376`/`422` across multi-line `005`s (`handshake_timeout` fallback).
   - **`Breakout::invite_and_wait` vs `invite_available`:** Verify neither sends `INVITE` to an already-offline `User` (`invite_and_wait` fails immediately with `Err(Error::UserLeft { .. })`, whereas `invite_available` skips offline users, invites online ones, finishes early when invited users join/quit, and returns partial `Ok(joined)` on timeout or `Ok(Vec::new())` if none joined; also verify `room.invite(&user)` accepts `&User` directly).
   - **Priority scheduling, multi-stop `.or_until()` / `ok_or_pending()` / `race_all()`, & `bot.quit` drain:** Verify `Interactive` prompts (`PRIVMSG`) overtake queued `Background` `Breakout::drop` `KICK`/`PART` bursts under pacing while preserving per-class FIFO (and flushing same-channel `Background` `PART` before a subsequent `JOIN`); run a game holding a `Breakout` (with `"Alice"` and `"Bob"` joined) under `.until(bot.shutdown_requested())`, call `bot.request_shutdown()` (emitting a farewell `say` and dropping `Breakout`) followed by `bot.quit(Some("bye")).await`, and assert `TestServer::recv_sent` yields per-class FIFO (`Interactive` farewell `PRIVMSG`, `Background` `KICK`s for Alice and Bob -> `PART`), followed by `QUIT` after both queues drain!
7. **Indexed `InboxRegistry`, early unwatched-channel skip, `#[track_caller]` `Error::Lagged` hint, 75%-fill warning, & `.lossy()` test:**
   - Verify unwatched noisy channels perform zero `UserTrack` allocations, and multi-channel `Quit`/`NickChanged` events dispatch **at most once** per multi-source `InboxScope` slot (`last_dispatched_seq == seq`).
   - With `inbox_capacity = 4`, hold an unpolled `ExclusiveDm` for `"Alice"` while injecting 100 messages into `#lobby`; verify Alice's DM `Inbox` experiences **zero** lag!
   - Push 3 DMs (`75%` of `4`) and verify the one-time 75%-fill `tracing::warn!` fires; push 3 more (6 total) and verify `Err(Error::Lagged { scope, skipped: 2, opened_at })` names `"user Alice"`, reports the exact caller `file:line`, and includes `"Did you leave an Inbox open across an .await? Use .clear() or .lossy() if intended."`; then verify `u.inbox().lossy()` in a multi-voter `try_join_all` recovers from one flooding user without aborting other voters.
8. **Non-fatal `DeliveryFailed` (`Turn::Unreachable` on `401`/`404` & PM-block `486`/`492`/`531`/`716`), `Event::DeliveryFailed`, `BotLeftRoom`, non-occupant `ask_user`, & `echo-message` self-filter tests:**
   - Verify `room.ask(...)` aborts immediately with `Err(Error::DeliveryFailed { .. })` when `404 ERR_CANNOTSENDTOCHAN` is injected (or `Err(Error::BotLeftRoom { .. })` when the bot itself is `KICK`ed from `#room`), and `alice.ask(...)` aborts immediately with non-fatal `Err(Error::DeliveryFailed { .. })` (which `.settled()` maps to `Ok(Turn::Unreachable { .. })`) on `486` (`+R`), `492`, `531`, and `716` (`+g` `callerid`, target nick at `params[1]`), while `bot.inbox_dms().next_event()` surfaces `Event::DeliveryFailed` for bounced one-way `alice.say(...)` role DMs!
   - Verify `room.ask_user(&bob, ...)` on a synced `#room` where `Bob` is absent fails immediately with `Err(Error::UserLeft { .. })` without emitting `PRIVMSG`.
   - Inject `:BotNick!b@h PRIVMSG #room :!join` (simulating IRCv3 `echo-message`) and verify self-messages never trigger `wait_cmd` or `ask`.
9. **Formatting, dynamic self-hostmask wrapping (`396` cloak), `OutboundSink` pacing, & compile-checked Cookbook doctests (`irc-colors`):**
   - Verify `room.say("hello\r\nQUIT :injected")` splits safely into two `PRIVMSG` lines, and verify `OutboundSink` paces bursts exceeding `outbound_burst` across `outbound_window` while logging a `tracing::warn!`.
   - Inject `396` cloak with a 60-byte vhost and verify `Text::wrap` dynamically clamps emitted lines so `:<nick>!<user>@<vhost> PRIVMSG <target> :<chunk>\r\n` never exceeds `512` wire bytes while preserving active `irc_colors::Style` across chunks.
   - Verify inbound `"\x02!join\x02"` strips cleanly in `msg.text` / `msg.is_cmd("!join")` while round-tripping in `msg.to_styled()`.
   - **Compile-checked Cookbook doctests:** Include all 8 cookbook examples (§5.1–§5.8) as Rust doc-tests on `src/lib.rs` / module docs so `cargo test --doc` verifies every snippet compiles with zero errors or warnings!

---

## 7. Crate Layout & Dependencies

### Proposed Module Structure

```text
src/
├── lib.rs          # Root `pub use` re-exports (`irc`, `Text`), `fmt`, `prelude`, `future`, `testing`, static `Send`/`Sync` assertions, & Cookbook doctests
├── error.rs        # `#[non_exhaustive]` `Error` (`Lagged { scope, skipped, opened_at }` + hint, non-fatal `DeliveryFailed`) & `DisconnectReason`
├── casemap.rs      # `CaseMap` (`Rfc1459`, `StrictRfc1459`, `Ascii`, `str_eq`) and `FoldedStr`
├── message.rs      # `WireEvent`, `MessageKind` (`Privmsg`, `Action`, `Notice`), `ChatMessage` (`to_styled`), `Target::Dm`, `Event` (`Quit { netsplit }`, `DeliveryFailed`)
├── state.rs        # `WorldState` (`005` `MODES`, `TARGMAX` `KICK`, dynamic `CASEMAPPING` re-keying), `ChannelState`, `self_user_host`, `353`/`366`, & `MODE`
├── outbound.rs     # `OutboundSink` (`Interactive` vs `Background` priority scheduler, `WriteReceipt`s, rolling burst pacer, 512B `Text::wrap` budget)
├── pump.rs         # Background stream reader, `CAP` + `376`/`422` registration barrier, early unwatched-channel skip, self-echo & `NOTICE` filters, `seq` dispatch
├── bot.rs          # `Bot` (`BotOwner` clearing state on `Drop`, `session_id`), `BotCore`, `BotConfig`, `inbox_*`, `exclusive_*` & `huddle*`, `reply` mirroring `MessageKind`
├── testing.rs      # `pub mod testing` (`test_harness`, `test_harness_with`, `TestServer` with `recv_sent` / `try_recv_sent` & convenience helpers)
├── inbox.rs        # Concrete `Inbox` (`#[track_caller]`, `InboxScope`, `.accept_notices()`, 75% fill warning), indexed `InboxRegistry` (`all`/`by_channel`/`by_user`)
├── ask.rs          # `Verdict<T>`, `IntoVerdict`, unified `AskConfig` & `Ask` (`WriteReceipt` clock start, `warn_remaining`, `timeout_message*`, `ignore_commands`, `accept_notices`), `TimeoutCtx`
├── room.rs         # `Room` view (`with_ask_config`, `nicks`), `Member`, `invite(InviteTarget)`, sync moderation (`voice_many`/`devoice_many`), `ask` & `ask_user`
├── user.rs         # `UserRegistry` (amortized dead-`Weak` GC, `netsplit`), cycle-free `User` (`Weak<BotCore>`, `inbox_in`), `ExclusiveDm` (>15s lock-wait warning)
├── huddle.rs       # `Huddle` (`users`, `online_users`, `relay_to_others`, group `say`/`ask`, private `Verdict::Reject`, optional `with_relay`, RAII `Drop`)
├── breakout.rs     # `#[must_use] Breakout` (`to_room`, `with_ask_config`/`set_ask_config`), `BreakoutOptions` (`ask_config`), `InviteTarget`, `invite_*`, `Background` `Drop`
└── supervise.rs    # `Outcome`, `Turn` (incl. `Unreachable`), `race_all`, `future::{Until, OrUntil, WithTimeout, WithDeadline, Optional, Settled, MapOutput, OkOrPending, RaceAll}`, `GameFutureExt`
```

### Dependencies (`Cargo.toml`)

- `irc = "1"` — IRC protocol parser, `Client`, `ClientStream`, and `Sender`.
- `irc-colors = { version = "0.1", path = "../irc-colors" }` — rich IRC formatting (`Colorize`, `Text`, `Span`, `Style`), inbound formatting stripping/parsing (`Text::parse`), and byte-accurate outbound wrapping (`Text::wrap`).
- `tokio = { version = "1", features = ["sync", "time", "rt", "macros"] }` — `mpsc`, `watch`, `Notify`, `Mutex`, timers, task spawning.
- `futures-core = "0.3"` / `futures-util = "0.3"` — `Stream` implementation and `select_all` / `try_join_all` compatibility.
- `thiserror = "2"` — ergonomic `Error` derives.
- `tracing = "0.1"` — structured spans and wait diagnostics (`join`, `breakout`, `exclusive`, `Ask`, `.lossy()` lag).
- `fastrand = "2"` — lightweight random alphanumeric suffixes for `BreakoutNaming::RandomSuffix`.

**`[dev-dependencies]`:**
- `tokio = { version = "1", features = ["test-util", "macros", "rt"] }` — deterministic `current_thread` virtual-clock testing via `tokio::time::pause()` and `tokio::time::advance()`.
