# External Design Review Triage & Action Plan

This document tracks all items across **two external design reviews** (**#1–#36** from Review 1; **#37–#39** + deduplicated annotations from Review 2) of [`DESIGN.md`](../DESIGN.md), organized into three buckets so we can resolve them methodically without losing nuance or regressing key design invariants:

1. **Bucket 1 — Clear Catches & Mechanical Fixes (Auto-Accept):** Real bugs, compile errors, RFC rules, testability gaps, and doc accuracy fixes.
2. **Bucket 2 — Misunderstandings / Intentional Trade-offs (Push Back):** Suggestions that conflict with IRC game ergonomics or stem from misreading the use case (with minor doc clarifications noted where helpful).
3. **Bucket 3 — Focused Design Rounds (Discuss Together, One Round at a Time):** Architectural questions with genuine trade-offs across 4 rounds.

---

## Bucket 1: Clear Catches & Mechanical Fixes (Completed in `DESIGN.md`)

- [x] **#1 + #24 — Pump-stamped `Arc<Event>`, `sender_track` at message time, & in-band wire ordering:**
  - *Problem:* Updating `WorldState`/`UserRegistry` before broadcasting raw `Arc<Message>` means out-of-band lookups see *post-batch* state if multiple lines arrive in one TCP read (e.g., `Alice` sends `"hit"` and `/nick`s or `/quit`s in the same read; `Mallory` steals `"Alice"` before `msg.sender(&bot)` runs; `Inbox` checking `watch::Receiver<UserStatus>` aborts with `UserLeft` while `"hit"` is still unread in the ring buffer). Also, every `Inbox` was re-allocating `ChatMessage` and re-running `Text::parse`.
  - *Fix:* Have the pump assign a monotonic `seq: u64`, parse once into `Arc<Event>` (`Chat`, `Notice`, `Joined`, `Parted`, `Kicked`, `Quit`, `NickChanged`, `Raw`), stamp `ChatMessage` with `sender_track: Arc<UserTrack>` resolved at message-processing time (so `msg.sender(&bot)` is an infallible O(1) `Arc::clone` immune to rename/steal races, bounded by ring-buffer capacity), and process `NICK`/`PART`/`KICK`/`QUIT` **in-band** in `Inbox` (`Quit { at_seq }` drains events with `seq < at_seq` before returning `Err(Error::UserLeft { .. })`, while `watch` wakes tasks waiting on `dm_mutex` or newly subscribed after `at_seq`).
- [x] **#8 — `NOTICE` separation (RFC 2812 §3.3.2):**
  - *Problem:* Auto-replying to `NOTICE` with `PRIVMSG` causes bot-to-bot / service loops, and `NickServ` / bouncer notices could satisfy permissive `ask` matchers.
  - *Fix:* Restrict `ChatMessage` (and therefore `wait_map`, `wait_for`, `wait_cmd`, `next_chat`, `ask`, `ask_user`, and `bot.reply`) strictly to **`PRIVMSG` (including CTCP `ACTION`)**. Surface `NOTICE` separately via `NoticeMessage` / `Event::Notice` on `next_event` / `wait_event_map`.
- [x] **#20 — Consistent admin helper across cookbook examples:**
  - *Fix:* Align §5.1 and §5.5 on `account-tag`-first admin checks and note hostmask cloak caveats.
- [x] **#21 — Blackjack `on_timeout_fn` match arm type mismatch:**
  - *Problem:* `u.nick().bold().yellow() + "..."` has type `irc_colors::Text`, while `"⏳ Time's up!".yellow()` has type `irc_colors::Span`.
  - *Fix:* Wrap the second arm in `Text::from("⏳ Time's up!".yellow())`.
- [x] **#22 — `InviteTarget` unsized bound for `str`:**
  - *Problem:* `P: InviteTarget + 'a` rejects `str` because `P: Sized` is implicit.
  - *Fix:* Add `P: InviteTarget + ?Sized + 'a`.
- [x] **#23 — Non-existent `bot.whois(...)` in §2.2 prose:**
  - *Fix:* Replace with an async call on the same connection that exists in the API (e.g., `user.ask(...)`).
- [x] **#28b *(+ Review 2 §5b)* — Remove non-core `ClaimSet` / `ClaimGuard`:**
  - *Fix:* Removed `ClaimSet` and `ClaimGuard` from the core API. `ExclusiveDm` already serializes concurrent DM prompts per user, and `User: Clone + Eq + Ord + Hash` works directly in standard `HashSet<User>` / `HashMap<User, ...>` if an application wants cross-game enrollment tracking.
- [x] **#28c — Unify supervision entry point on `GameFutureExt::until`:**
  - *Fix:* Dropped redundant top-level `run_until(fut, stop)` free function so `.until(stop)` matches `.with_timeout()`, `.with_deadline()`, `.optional()`, and `.settled()` in one postfix place.
- [x] **#28d — `FutureExt` trait name collision:**
  - *Problem:* `ircgm::FutureExt` collides by name with `futures::FutureExt` (and `tokio-util` / `smol` extensions).
  - *Fix:* Rename to `GameFutureExt`.
- [x] **#29 — In-memory testability without `irc::client::Client`:**
  - *Problem:* `irc::client::Sender` cannot be standalone-constructed or inspected in unit tests without `irc::client::Client`.
  - *Fix:* Route outbound commands through a small internal `OutboundSink` (`Irc(irc::client::Sender) | Channel(mpsc::UnboundedSender<Command>)`) and expose `Bot::test_harness` returning `(Bot, TestServer)` with live inbound injection and outbound `Command` inspection.
- [x] **#30 — Missing `tokio` `test-util` dev-dependency:**
  - *Fix:* Specify `[dev-dependencies]` with `tokio = { version = "1", features = ["test-util", "macros", "rt-multi-thread"] }` for `tokio::time::pause()`.
- [x] **#31 — Acknowledge `WorldState` protocol scope & test surface:**
  - *Fix:* Clarify that while the async pump loop itself is ~40 lines, `WorldState` is a substantial protocol state machine (`005 ISUPPORT` `PREFIX`/`CHANTYPES`/`CASEMAPPING`, multi-packet `353`/`366` `NAMES` accumulation, channel `MODE` prefix deltas, `NICK`/`QUIT`/`PART`/`KICK`) with dedicated unit tests.
- [x] **#33 — Use `Error::UserLeft { .. }` struct-variant syntax consistently in docs:**
  - *Fix:* Replace shorthand `Err(Error::UserLeft)` prose references with `Err(Error::UserLeft { .. })`.
- [x] **#34 — Accurate ring-buffer retained memory estimate:**
  - *Fix:* Clarify that the 4,096-slot ring buffer array is ~32–64 KB of `Arc` pointers, while retaining 4,096 unique unread `Event`s on the heap under a stalled `Inbox` uses ~1–2 MB (still bounded and tiny).
- [x] **#35 — Document direct-style state trade-offs in §1:**
  - *Fix:* Add a "Trade-offs & Non-Goals" subsection noting that stack-frame state machines are ephemeral in-memory tasks (not serializable across restarts or externally inspectable for `!status` without mirroring fields into shared state).
- [x] **#36 — Document `tokio::select!` cancellation safety:**
  - *Fix:* Explicitly specify that all `Inbox` read methods (`wait_map`, `wait_for`, `wait_cmd`, `next_chat`, `next_event`, `next_raw`) are strictly cancel-safe in `tokio::select!` loops, while dropping an in-flight `Ask` cleanly aborts (releasing `Inbox` / `ExclusiveDm` without sending `on_timeout`) and is not resumable.

---

## Bucket 2: Misunderstandings / Intentional Trade-offs (Push Back)

- **#18 — "Lobby loop (§5.1) awaits `heist_mission` inline instead of `tokio::spawn`ing per game":**
  - *Clarification:* **Multiple concurrent loops in the same `Room` are 100% supported by `ircgm`** (`Room` is `Clone`, and every `room.inbox()` receives an independent fan-out of all room messages—so running a `!ping` loop, a `!stats` loop, and a game loop side-by-side in one channel works out of the box). Review 1 was only critiquing the *cookbook example* in §5.1 for awaiting `heist_mission` inline instead of `tokio::spawn`ing it: inline awaiting is a good default for *in-channel* games (so two Blackjack games don't talk over each other and both react to `"hit"` in `#lobby`), whereas `heist_mission` moves into a `Breakout` (`#vault-xxxx`) and *could* be `tokio::spawn`ed. Worth adding an explicit note in `DESIGN.md` explaining multi-loop fan-out (`!ping` + game loop) and the shared-listener caveat!
- **#25 — "Shrink `wait_*` family to just `wait_map` + `wait_cmd` + `StreamExt`":**
  - *Why we keep `wait_for`, `next_chat`, `next_event`:* These are ~20 lines of trivial wrappers that avoid forcing every user to import `futures::StreamExt` and unwrap `Option<Result<...>>` for the most common chat operations.
- **#26 & #27 — "`Vec<(User, Inbox)>` is a filter smell" & "`user.inbox()` bypasses DM serialization":**
  - *Why `user.inbox()` exists and is used in `collect_secret_votes`:* During a Mafia night vote, the bot sends **one public channel prompt** (`"Everyone DM me !vote <nick>"`), *not* individual serialized DM prompts. Each voter casts one vote concurrently (`try_join_all`), which requires one state per voter (a single merged 3-user inbox cannot express "at most 1 vote from each of Alice, Bob, and Carol") and must not serialize behind `dm_mutex`. *(We can still sharpen `user.inbox()`'s doc comment so callers know when to use `exclusive()`/`ask()` vs `inbox()`.)*
- **#28a — Minor surface trim (`InviteTarget`):**
  - *Why we keep it:* `InviteTarget` lets `invite_and_wait` accept `[&alice, &bob]` (`User`) or `["Alice"]` (`str`) ergonomically.
- **#32 — "§2.7 reads as a rebuttal ('Do not simplify...')":**
  - *Why we keep it:* Those explicit anti-simplification guardrails were written specifically so AI coding agents implementing or maintaining the crate do not "simplify" away load-bearing invariants (like post-lock reply timers or `ask_user`).
- **#37 *(Review 2 §4)* — "`bot.drain().await` waiting for active `ExclusiveDm` & `Breakout` counts to hit 0":**
  - *Why we push back on `ExclusiveDm`/`Breakout` ref-count draining:* `ircgm` intentionally has no global framework registry of "game trigger commands" (lobbies are plain user `loop`s), and an active game in `#lobby` (e.g., Quiz, Blackjack, or Mafia's daytime deliberation phase between two night phases) holds **zero** `Breakout`s or `ExclusiveDm`s—so waiting for `Breakout`/`ExclusiveDm` counts to reach 0 would kill active games mid-round! In direct-style `ircgm`, zero-interruption draining is naturally handled at the lobby loop (stop awaiting new `!heist` triggers on an admin `!drain` / `CancellationToken` while letting running game futures complete, then flush via `bot.quit(reason).await` from **#5**).

---

## Bucket 3: Focused Design Rounds (To Discuss Together)

### Round 1 — Group / Per-Player Failure Ergonomics & Recruitment
- [x] **#16 — ★ Expected per-player events (`TimedOut`, `UserLeft`) vs. fatal connection errors in group flows:**
  - *Issue:* In multi-player games (`blackjack_round`, `quiz_game`'s `select_all`, `collect_secret_votes`'s `try_join_all`), a single player disconnecting (`UserLeft`) or timing out (`TimedOut`) causes `?` to abort the entire multi-player table instead of skipping/eliminating just that player.
  - *Resolved in `DESIGN.md`:* Added `Turn<T> = Reply(T) | TimedOut | Left { nick, reason }`, `.settled()` on both `Ask` and `GameFutureExt`, and `Error::is_fatal()`, plus updated `blackjack_round`, `quiz_game`, `collect_secret_votes`, and `resolve_simultaneous_triggers`.
- [ ] **#19 — Bounded multi-player recruitment helper (`!join`) *(Low priority / deferred)*:**
  - *Issue:* Recruiting `min..=max` unique players within an overall wall-clock window (burst-safely, handling early `max` completion vs `min` threshold at deadline) is written in nearly every game and is easy to get wrong with per-iteration timeouts.
  - *Status:* Deferred for now (low priority; explicit `Inbox` + `HashSet<User>` + deadline combinator covers it).

### Round 2 — `AskConfig` Surface, `.timeout()` Naming, Deadlines, & `ExclusiveDm` Bounds
- [x] **#10 — ★ Bounding raw `ExclusiveDm` waits (`dm.inbox().next_chat().await`) & mutex queue wait:**
  - *Resolved in `DESIGN.md`:* `ExclusiveDm`'s internal `Inbox` inherits `user`'s `TimeLimit` (default `300s` per read, updatable via `dm.ask_timeout(...)` / `dm.ask_deadline(...)` / `dm.no_ask_timeout()`) so AFK bouncers cannot wedge `dm_mutex`, while normal `bot`/`room`/`user` `Inbox`es keep `read_limit = TimeLimit::None`. Outer queue + reply wall-clock bounds use `.with_timeout(dur)` / `.with_deadline(instant)`.
- [x] **#11 *(+ Review 2 §5a)* — `.timeout()` naming, `.optional()` inherent-vs-trait surface, & `ask(...).optional().timeout(...)` trap:**
  - *Resolved in `DESIGN.md`:* Renamed `GameFutureExt::timeout` → `.with_timeout(dur)` (keeping `.timeout(dur)` exclusively on `Ask`), turning `ask.optional().timeout(10s)` into a compile error and allowing `alice.ask(...).with_timeout(60s)` directly on `Ask`.
- [x] **#12 — Absolute deadlines (`Instant` / `deadline`):**
  - *Resolved in `DESIGN.md`:* Added `Ask::deadline(Instant)`, handle-level `.ask_deadline(Instant)` on `Room` / `Breakout` / `User` / `ExclusiveDm`, and `GameFutureExt::with_deadline(Instant)` (`tokio::time::timeout_at`), showcased in `collect_secret_votes`.
- [x] **#13 — Reply clock starts at `Sender` enqueue vs. wire flush under flood throttling:**
  - *Resolved in `DESIGN.md`:* Documented enqueue-time clock semantics and flood-throttling guidance in §2.7.
- [x] **#14 — `.optional()` + `on_timeout` double-announcement & drop silence:**
  - *Resolved in `DESIGN.md`:* `Ask::optional()` suppresses *inherited* handle `on_timeout` messages by default (only firing if `.on_timeout` / `.on_timeout_fn` was explicitly chained on that `Ask`); cancellation/drop never sends `on_timeout`.
- [x] **#15 — Context on `Error::TimedOut` vs. `Error::HandshakeTimeout`:**
  - *Resolved:* Kept `Error::TimedOut` unit-shaped for clean pattern matching (multi-player flows pair each player with `.settled() -> Turn<T>`, and `tracing` in **#39** logs target/user/duration).
- [x] **#17 — `AskConfig` ownership, `BotConfig` startup defaults, & `room` -> `User` config inheritance:**
  - *Resolved in `DESIGN.md`:* Kept `Bot` as a pure connection handle (`Arc<BotInner>`), put startup bot-wide defaults on `BotConfig` (`ask_timeout`, `no_ask_timeout`, `on_timeout`, `on_timeout_fn`, `silent_timeout`), and placed local overrides (including `.ask_deadline(Instant)`) strictly on the 4 conversation types (`Room` & `User` via `&self -> Self`; `Breakout` & `ExclusiveDm` via `mut self -> Self` in-place); `ChatMessage::sender(impl UserContext)` and `Room::user(nick)` inherit `room`'s `AskConfig` automatically, while helper functions take `room: &Room` (`&Path` vs `&TempDir`).

### Round 3 — Protocol Visibility: Delivery Failures, DM-Only Presence, IRCv3 Caps, Reconnect, & `Breakout` Drain
- [x] **#2 — ★ DM-only user presence limits (`NICK`/`QUIT` invisibility without a shared channel):**
  - *Resolved in `DESIGN.md`:* Documented that proactive `NICK`/`QUIT` requires sharing a channel with the bot, and added `401 ERR_NOSUCHNICK` handling (marking the `UserTrack` offline + fast-failing active DM waits without building `MONITOR` list-quota machinery).
- [x] **#3 — ★ Surfacing asynchronous outbound refusals (`401`, `404`, `442`) & bot kick/part:**
  - *Resolved in `DESIGN.md`:* Added in-band `Event::SendError` → `Error::DeliveryFailed { target, code, message }` (`401`/`403`/`404`/`442`) and `Event::BotLeftRoom` → `Error::BotLeftRoom { channel, reason }` on self `PART`/`KICK` so undeliverable `ask`s fail in milliseconds.
- [x] **#5 — ★ `Breakout::drop` flush on shutdown:**
  - *Resolved in `DESIGN.md`:* Unified `bot.quit(reason).await` to queue `QUIT` and drain previously queued outbound commands (such as `KICK` + `PART` from `Breakout::drop`) before closing the stream.
- [x] **#6 — Explicit reconnect / disconnect story:**
  - *Resolved in `DESIGN.md`:* Documented in §1 that `Bot` models a single IRC connection session (`Error::Disconnected` unwinds active futures cleanly; an outer supervisor loop reconnects).
- [x] **#7 — IRCv3 capability negotiation (`account-tag`, `multi-prefix`, `extended-join`, `echo-message` self-filtering) & SASL:**
  - *Resolved in `DESIGN.md`:* Added `BotConfig::request_caps` (`account-tag`, `multi-prefix`, `extended-join`, `userhost-in-names`, ignoring `NAK`s) and unconditional self-message filtering (`fold(sender) == fold(bot.current_nick)`) so `echo-message` can never self-trigger `ask` or `wait_cmd`.
- [x] **#9 — Edge-case semantics for `ask_user` / `room_inbox` when user or bot is not in the room:**
  - *Resolved in `DESIGN.md`:* If `WorldState` has synced `room` (`366`) and `user` (or the bot) is absent, `room_inbox` / `ask_user` fails immediately (`UserLeft` / `BotLeftRoom`) without sending a dead prompt; unjoined rooms fail fast on `404`/`442`.
- [x] **#38 *(Review 2 §5c)* — Dynamic self `nick!user@host` byte-budget calculation for `Text::wrap`:**
  - *Resolved in `DESIGN.md`:* `WorldState` tracks `self_user_host` dynamically (from self `JOIN`, `396` cloak, or `001`, with a conservative `76`-byte reserve beforehand) so `Text::wrap` guarantees `<= 512` wire bytes even under long vhosts/cloaks.

### Round 4 — `Inbox` Lag Isolation, Pump Filtering, & `tracing` Observability
- [x] **#4 *(+ Review 2 §2)* — ★ Global `broadcast` lag contamination → Pump-filtered per-`Inbox` channels:**
  - *Resolved in `DESIGN.md`:* Replaced the global `broadcast` ring buffer with a synchronous `Mutex<InboxRegistry>` (`Slab` of `Filter` + per-inbox bounded `mpsc::Sender<Arc<Event>>` + `AtomicU64` lag counter + cooperative 64-line burst yielding), isolating lag per `Inbox` and eliminating `O(N)` task wakeups on unrelated traffic while keeping `.inbox()` synchronous and race-free.
- [x] **#39 *(Review 2 §3)* — `tracing` spans & structured wait events for stack-frame observability:**
  - *Resolved in `DESIGN.md`:* Added §3.6 specifying async-safe `tracing` spans on `join`, `breakout_with`, and `exclusive`, plus structured `ircgm::ask` wait diagnostics (`target`, `user`, `timeout_ms`) for `tracing_subscriber` and `tokio-console`.
