# Consolidated design review: `ircgm`

*Hand-off for the implementing agent. This merges two independent reviews of the latest spec and the author's own notes. Section 1 is a decision list to apply to the spec before coding. Section 2 is for implementation. Section 3 is deferred. Section 4 lists suggestions that were considered and **rejected**, so don't re-litigate them.*

---

## 0. Verdict

The architecture is sound. **Do not redesign these**, because they are the hard parts and they are correct:

- The connection pump with pump-filtered, per-`Inbox` bounded `mpsc` channels (no global `broadcast`).
- `wait_*` living **only** on `Inbox`, with the "create the inbox before you speak" discipline.
- Wire-order identity stamping (`seq`, `Arc<UserTrack>` captured at arrival) and in-band handling of `NICK`/`QUIT`/`PART`.
- The per-user DM mutex (`ExclusiveDm`) and queue-fair `Ask` timers (the clock starts after the prompt is sent).
- RAII only for exclusive resources (`Breakout`, `ExclusiveDm`), with `Room`/`User` as cheap handles.
- `User` as session identity (`Eq/Ord/Hash` by `UserTrack::id`), surviving `/nick` and case-folding.
- Errors-as-control-flow, so linear game code is `room.ask(...).await?` and `TimedOut` unwinds through RAII guards.
- The testing strategy (socketless harness, virtual time).

The remaining issues are mostly API-shape inconsistencies, a few real bugs, and two missing capabilities (multi-user locking and graceful shutdown).

---

## 1. Resolve in the spec BEFORE writing code (public API shape, painful to change later)

### 1.1 `- [x]` Config living on `User` (highest priority)

**Problem.** `User` is the long-lived identity (stored in `HashSet<User>`, reused across rounds) but now carries an `Arc<AskConfig>` snapshot taken at creation.
- `msg.sender(&room)` snapshots the room's config into the user. If the `User` is kept across phases, it silently keeps the old timeout and `on_timeout` message.
- `HashSet::insert` dedups by id, so whichever clone was inserted first wins. Behavior depends on insertion order.
- Worst case: `ask_deadline(Instant)` on a stored `User` becomes expired in the next round, so every `ask` fails instantly with `TimedOut`.

**Required change (minimum).** Forbid absolute `Deadline` on stored identity handles. `User` may carry only a relative `Timeout`/`on_timeout`. Keep `.reply_deadline(instant)` on the per-call `Ask` (and on `Room`/`Breakout`/`ExclusiveDm`, which are phase-scoped). Document that `Eq/Hash` ignore config.

**Preferred (if you accept the churn).** Make `User` config-free. DM prompts take config from the per-call `Ask` builder or from an explicit context. `room.ask_user` already uses the *room's* config, which is the right model.

### 1.2 `- [x]` One rule for timeout messages and result-mapping combinators

**Problem.** The current semantics are inconsistent:
- `Ask::optional` suppresses *inherited* `on_timeout` messages, but `GameFutureExt::optional` cannot (it only sees the result), so `ask.with_timeout(x).optional()` behaves differently from `ask.optional()`.
- `settled()` does *not* suppress the message (the Blackjack example relies on that), which is inconsistent with `optional()`.
- The outer `with_timeout` returns `Error::TimedOut` **without** sending `on_timeout`. The same error now has two origins with different side effects.

**Required change.** Adopt a single rule:
> `on_timeout` fires only when the `Ask`'s own reply timer expires, controlled solely by `on_timeout` / `on_timeout_fn` / `silent()` (explicit or inherited). Combinators (`optional`, `settled`, `with_timeout`, `with_deadline`) are **pure result mappers** and never change messaging.

Consequences to implement:
- Delete the inherent `Ask::optional`/`Ask::settled`; keep only the `GameFutureExt` versions (one implementation, one semantics).
- Update the cookbook: where silence was intended with `.optional()`, add `.silent()` explicitly.
- Document that `with_timeout` bounds queue + reply and sends no message.
- Optional (low cost): give the outer wall-clock timeout a distinct internal origin (e.g. `Error::Deadline`, or a flag on `TimedOut`) so callers can tell the two apart. Keep it non-fatal and handled by `settled()`.

### 1.3 `- [x]` Rename the timer methods on `Ask`

Rename `Ask::timeout` → `Ask::reply_timeout` and `Ask::deadline` → `Ask::reply_deadline`, so they sit visibly apart from `with_timeout`/`with_deadline` in autocomplete. Apply the same prefix to the handle-level setters for consistency (`ask_timeout`/`ask_deadline` can stay, since they already read as config).

### 1.4 `- [x]` `RoomEvent` must carry channel and stable identity

**Problem.** `Joined`/`Parted`/`Kicked`/`Quit` have no `channel`, so they are unusable from `bot.inbox()` or a multi-channel `select!`. They carry only nick strings, losing the stable `User` identity the rest of the design preserves. Adding fields to a public enum later is breaking.

**Required change.**
- Add `channel: String` to `Joined`/`Parted`/`Kicked`. For `Quit`/`NickChanged`, add the affected channels if cheap to compute from `WorldState` at stamp time, or document that they are connection-wide.
- Add a stamped `User` (or `UserId`) to each membership variant, using the same wire-time stamping as `ChatMessage`.
- Rename `ChatMessage::user: Option<String>` → `ident`, to avoid confusion with `sender() -> User`.

### 1.5 `- [x]` `TimeoutCtx`: owned vs borrowed (decide, low stakes)

Either is fine. Owned (`Target` and `User` are cheap clones on a cold path) simplifies closures. If you go owned, the bound is `F: Fn(TimeoutCtx) -> M, M: Into<Text>`. Keep the generic `M`; nested `impl Trait` inside `Fn` sugar is not valid Rust. Also fix `duration`: it is ill-defined for absolute deadlines ("configured duration, or remaining duration"). Either expose `limit: TimeLimit`, or document that it is unsuitable for user-facing text.

### 1.6 `- [x]` Guard-level config naming on `Breakout` / `ExclusiveDm`

`vault.ask_timeout(..)` returns a `Breakout` on an owned guard (`mut self`) but a plain `Room` on `&Breakout` (auto-deref to `Room::ask_timeout(&self)`). Same-looking line, different type. Rename the guard-level methods (`with_ask_timeout`, etc., or a single `.configure(|c| ...)`), so the distinction is visible at the call site.

---

## 2. Implementation-time items (real bugs or missing capabilities, fix in place)

### 2.1 Bugs and correctness

- [x] **1. `ExclusiveDm::ask` can consume stale input.** The inbox is reused across asks. A late reply to ask #1 (or text typed before the prompt) satisfies ask #2. **Fix:** call `inbox.clear()` immediately before `say(prompt)` in `ExclusiveDm::ask`. Add a test. *(Spec updated in `DESIGN.md` §4.6 & §6.)*
- [x] **2. Pump task may hold a strong `Arc<BotInner>`.** Then "last handle dropped → pump stops" never fires. The pump should own only the inner state it needs (a separate core, or a `Weak`). You'll hit this on the first drop test. *(Spec updated in `DESIGN.md` §1, §3.2, §4.1: `Bot(Arc<BotOwner>)` owns `AbortOnDrop`; `Room`, `User`, `Inbox`, and pump hold `Arc<BotCore>`.)*
- [x] **3. Lock acquisition has no deadline.** The 300s default bounds *reads* inside an exchange, but `exclusive()` acquisition itself is unbounded, so an A→B / B→A double-lock is a permanent deadlock (not a 5-minute stall). See 2.2. *(Spec updated in `DESIGN.md` §2.4 & §4.6.)*
- [x] **4. Per-user `Lagged` is fatal.** In a `try_join_all` vote, one flooding user's lagged inbox aborts everyone via `?`. For user-scoped inboxes (`user.inbox()`, `user.room_inbox()`), fold lag into the non-fatal per-player path handled by `settled()` (e.g. `Error::Lagged` stays fatal for `bot.inbox()`/`room.inbox()`, but becomes a per-player interruption when scoped to one user). Decide this early because it changes `is_fatal` semantics. *(Resolved via §7.2 #4: kept `Lagged` fatal & `Turn<T>` 3-variant, enriched `Error::Lagged { scope, skipped }`, and updated `collect_secret_votes` to `u.inbox().lossy()`.)*
- [x] **5. Hidden per-inbox read limit.** `Inbox` from `ExclusiveDm::inbox()` has a read limit, but `room.inbox()` does not. Same type, same `wait_cmd`, different behavior by origin. Make it explicit (`inbox.with_read_limit(..)`), and document loudly on `ExclusiveDm::inbox()`. *(Spec updated in `DESIGN.md` §4.4 & §4.6.)*
- [x] **6. Test determinism.** "Inject back-to-back before polling" is racy if the pump is a spawned task on a multi-threaded runtime (dev-deps list `rt-multi-thread`). Use a `current_thread` runtime, **or** give `TestServer` a `settle().await` that returns once the pump has drained all injected lines. *(Spec updated in `DESIGN.md` §4.2, §6, & §7 with both `current_thread` runtime and `TestServer::settle().await`.)*
- [x] **7. Heist example bug.** `invite_and_wait` fails with `HandshakeTimeout`, but `lobby` only special-cases `Error::TimedOut`, so an invite timeout falls into the generic "ended early" branch. Handle it explicitly or fix the example. *(Spec updated in `DESIGN.md` §5.1.)*

### 2.2 `- [x]` `exclusive_many` (additive, cheap)

Add `bot.exclusive_many(&[&User]) -> Result<Vec<ExclusiveDm>>`:
- Sort by `UserTrack::id`.
- **Deduplicate**, so passing the same user twice cannot self-deadlock.
- Acquire in canonical order; return guards in the caller's original order.
- If any user has quit, fail with `UserLeft` and release already-acquired guards.

Prefer the slice/Vec form over a tuple or variadic API. This removes a footgun the spec currently hands to documentation ("use `User: Ord` for lock order"). Keep that documentation as a fallback note.

### 2.3 `- [x]` Graceful shutdown signal (missing capability)

`bot.quit()` is described as enabling zero-interruption deploys, but nothing tells suspended game futures to wrap up.
- Add `bot.shutdown_requested()` (a future backed by a `tokio::sync::watch`), which fires **before** `QUIT` is queued, so games can announce "restarting, back soon" while the socket is alive.
- Usage: `game.until(bot.shutdown_requested())`.
- Do **not** expose `tokio_util::CancellationToken` in the public API (a needless public dependency; `watch` suffices).
- Consider a distinct error/outcome for graceful shutdown vs `Disconnected`, so callers can tell planned from unplanned.
- Specify ordering: shutdown signal → grace period (bounded by `handshake_timeout` or a configurable drain) → `QUIT`.

### 2.4 `- [x]` Outbound flood control (documentation and metrics, not a new limiter)

The `irc` crate already has built-in outbound burst throttling (burst window and max-messages-in-burst settings; **verify the actual defaults and that `Sender` honors them in the version you target**). Do **not** build a second token bucket. The real residue:
- The unbounded queue means enqueue time ≠ wire time, which affects when the `Ask` reply clock starts and how long `Breakout::drop`'s KICK burst takes to flush. Document this, and consider starting the `Ask` clock on flush if the `irc` API allows, or at least exposing outbound queue depth (metric/`tracing`).
- Consider a bounded or soft-capped outbound queue (warn or error past N pending lines) to protect against a buggy `loop { room.say(..) }` growing memory.

### 2.5 `- [x]` Smaller API ergonomics (cheap now, fine to batch)

- [x] `msg.is_in(&room)` instead of `m.is_in(room.name(), bot.casemap())`.
- [x] Add `bot.dm_inbox()` (DM-only filter) for admin command listeners. `bot.inbox()` wakes on every line on the connection; keep it as the escape hatch.
- [x] State plainly in docs: two concurrent `room.ask_user` calls to the **same player** in the same channel both match one reply (no mutex analogue to `dm_mutex`; acceptable, but call it out since DMs get special treatment).
- [x] Note that the `Ask` clock starts when the prompt is *enqueued*, and flood-throttled long prompts can make that earlier than wire time.

---

## 3. Deferred (not blockers; revisit after v1)

- **Reconnect-surviving player identity.** The problem is real: after a `QUIT` and reconnect, a player gets a new `UserTrack::id`. See the rejection in §4 for why `Eq/Hash` must not change. Post-v1: expose `user.account()`/`msg.account` and let games hold `AccountName -> PlayerState` with an explicit "rebind player to new `User`" step. In-flight asks fail with `UserLeft` on `QUIT` regardless, so this needs game-level decisions anyway.
- Lazy `UserTrack` stamping (skip allocate/lookup when no inbox slot matches the line). A pure optimization, so measure first.
- Trim the `Ask` builder surface (`silent`, `on_timeout` vs `on_timeout_fn` pair, etc.) once §1.2 lands.
- Layer the crate into a core (`Bot`/`Room`/`User`/`Inbox`/state/pump) and a games layer (`Ask`/`ExclusiveDm`/`Turn`/`Breakout`/`GameFutureExt`).
- `BreakoutNaming::Words` (e.g. `#vault-crimson-fox`) as optional flavor. It needs collision retry on `471`/`473` since the keyspace is smaller, and the "guaranteed empty channel" property depends on uniqueness. Keep random alphanumeric as the default.
- Move "why we rejected X" rationale into ADRs and keep the spec normative and scannable.
- Optional `Error` restructuring: a type-level split (e.g. an `Interrupted` variant family vs fatal) *inside* `Error`, keeping `Result<T>` and `?`.

---

## 4. Considered and REJECTED (do not implement)

1. **`Result<Turn<T>, FatalError>` as the return type of `Ask`.** Rejected. It forces a `match` at every step of a linear quest and sacrifices the library's best property: `room.ask(..).await?` with `TimedOut`/`UserLeft` propagating via `?` to trigger `Breakout`/`ExclusiveDm` RAII cleanup. The legitimate concern (conflating "slow player" with "socket died") is already handled by `is_fatal()` and `settled()`.
2. **Removing `AskConfig` from `Room` / replacing it with a scoped `Phase` builder.** Rejected. `Room` is a view with no channel state, so a "fast room" and "slow room" cannot disagree about the channel. A scoped builder reintroduces the rightward indentation §2.7 deliberately rejected. The valid kernel is config on the long-lived `User` (see §1.1), not on `Room`.
3. **Making `User`'s `Eq`/`Hash` check `account` first, falling back to session id.** Rejected. A user's hash would change when they identify mid-life, corrupting any `HashSet<User>`/`HashMap<User, _>` holding them, and equality becomes non-transitive across authenticated and unauthenticated handles. Keep `User` = session identity; handle accounts at the application layer (§3).
4. **A custom outbound token-bucket in the pump.** Rejected as duplicative of `irc`'s built-in throttle (see §2.4 for the real residual work).
5. **`tracing::warn!` on every silent timeout.** Rejected. It would be noisy for `.optional()`/`.settled()` flows where silence is intentional, and the premise ("the game silently halts") is wrong, since the author receives `Err(TimedOut)`. Keep the `debug!` event. Optionally add an opt-in `BotConfig` flag if a lint-like behavior is wanted.
6. **Exposing `CancellationToken` publicly.** Rejected in favor of a `watch`-backed `shutdown_requested()` (§2.3).

---

## 5. Tests to add or adjust *(specified in `DESIGN.md` §6; to verify in crate test suite)*

- [x] **Stale-inbox regression:** ask #1 times out; the user replies late; ask #2 (same `ExclusiveDm`) must *not* match the late reply.
- [x] **Deadline-on-stored-handle:** a `User` reused across rounds never inherits an expired absolute deadline (or the type forbids it).
- [x] **Combinator semantics (§1.2):** a table test showing `optional`, `settled`, and `with_timeout` never emit or suppress messages; only `on_timeout`/`silent()` do.
- [x] **`exclusive_many`:** reversed-order concurrent calls on the same pair do not deadlock; a duplicate user fails fast with `InvalidInput` without self-deadlocking; a quit user fails and releases the others.
- [x] **Shutdown:** `shutdown_requested()` fires before `QUIT` is observed on the wire; games can send a farewell message before the stream closes.
- [x] **User-scoped lag & diagnostics:** an overflowed inbox reports *which* scope lagged (`Error::Lagged { scope, skipped }`), and a `u.inbox().lossy()` voter in `try_join_all` recovers without aborting other players while strict room/bot-scoped lag remains fatal.
- [x] **Pump lifetime:** dropping the last `Bot` handle stops the pump immediately (even with buffered events or surviving `Room`/`User` handles).
- [x] **Determinism:** run the burst and `NICK`/`QUIT` batching tests on a `current_thread` runtime and via `TestServer::settle()`.
- [x] **Event identity:** `RoomEvent::Parted`/`Kicked`/`Joined` from `bot.inbox()` carry the channel and a stable `User`.

---

## 6. Suggested order of work

1. Apply §1.1 through §1.6 to the spec (about an hour; these are all small edits).
2. Build the core: pump, `WorldState`, `UserRegistry`, `Inbox`, test harness (with `settle()` or a `current_thread` runtime) and the pump-lifetime test.
3. Build `Ask`, `ExclusiveDm` (with the `clear()` fix), `Breakout`, and `GameFutureExt` under the §1.2 semantics.
4. Add `exclusive_many` and `shutdown_requested()`.
5. Handle the §2.1 and §2.5 items as you reach the relevant code; defer §3.

---

## 7. Design Response & Finalized Resolution (Initial Analysis + Counter-Review Synthesis)

Below is our point-by-point response to Sections 1–5, incorporating the follow-up counter-review so every design trade-off is settled cleanly before implementation.

### 7.1 Accepted Redesigns & Where Going All-In Simplifies the API

1. **§1.1 + §1.4 — Make `User` 100% Config-Free + Stamp `sender: User` Directly + Public `AskConfig`:**
   - **Decision:** Adopt the **Preferred** option in §1.1—`User` carries **zero** `AskConfig`.
   - **Rationale:** `User: Eq + Ord + Hash` promises that equal handles are interchangeable (`HashSet::insert` retains the first inserted handle). Carrying config on `User` created hidden behavioral divergence between equal `User` clones, risked leaking channel-specific `on_timeout` messages (`msg.sender(&vault)`) into later private DM rounds, and risked expired `Instant` deadlines persisting across rounds.
   - **Payoff for §1.4:** The *only* reason `ChatMessage::sender` previously took a context parameter (`UserContext`) and `Room::user(nick)` existed was to inherit `AskConfig`. With a config-free `User`, the pump stamps **`pub sender: User`** directly onto `ChatMessage`, renames `ChatMessage::user` → `pub ident: Option<String>`, and stamps **`user: User`** + `channel: String` (on `Joined`/`Parted`/`Kicked`) or `channels: Vec<String>` (on `Quit`/`NickChanged`) onto `RoomEvent` (plus `sender: Option<User>` on `RoomEvent::Notice`). Eight redundant API items disappear (`6` config methods on `User`, `Room::user`, and `UserContext`).
   - **Reusable plain-data `AskConfig` (from counter-review):** To avoid repeating `.reply_timeout(...)` + `.on_timeout(...)` across N players' DM prompts (e.g., Quiz or Mafia DM rounds) now that `User` is config-free, we expose **`pub struct AskConfig`** as plain reusable value data (`AskConfig::new().reply_timeout(...).on_timeout(...)`) accepted by **`Ask::with_config(&cfg)`** (as well as `Room::with_ask_config(&cfg)`, `Breakout::with_ask_config(&cfg)`, `ExclusiveDm::with_ask_config(&cfg)`, and `BotConfig::with_ask_config(cfg)`). Plain data, zero hidden state on `User`.

2. **§1.2 — Single Rule for Timeout Messages & Pure Combinators (Keeping `Error::TimedOut` Unified):**
   - **Decision:** Adopt the single rule verbatim:
     > `on_timeout` fires **if and only if** `Ask`'s own reply timer expires, controlled strictly by `on_timeout` / `on_timeout_fn` / `silent()` (inherited or explicit). Combinators (`optional`, `settled`, `with_timeout`, `with_deadline`) live **only** on `GameFutureExt` and are pure result/future wrappers that never alter messaging.
   - Inherent `Ask::optional` and `Ask::settled` are deleted (leaving one implementation on `GameFutureExt`). Cookbook examples that use `.optional()` where an inherited `on_timeout` might exist add `.silent()` explicitly, and Mafia's `resolve_simultaneous_triggers` updates from `alice.ask_timeout(20s)` to `.reply_timeout(Duration::from_secs(20)).silent().optional()` (or shared `AskConfig`).
   - **Pushback (accepted in counter-review) on `Error::Deadline`:** Keep **`Error::TimedOut` unified** rather than splitting out `Error::Deadline` for `with_timeout` / `with_deadline`, because callers matching `Err(Error::TimedOut)` on `inbox.wait_cmd(...).with_timeout(30s)` would otherwise silently miss outer timeouts. We document clearly that outer `with_timeout` / `with_deadline` bounds total wall-clock time (queue + reply) and cancels the inner future without sending `on_timeout`.

3. **§1.3 — `Ask::reply_timeout` / `Ask::reply_deadline` / `Ask::no_reply_timeout`:**
   - **Decision:** Accepted. In Rust, `.timeout()` and `.with_timeout()` on the same `Ask` receiver look like two builder-naming conventions for the same operation in IDE autocomplete. Naming `Ask`'s inner post-send timers `reply_timeout`, `reply_deadline`, and `no_reply_timeout` makes the queue-fair post-send boundary self-documenting.

4. **§1.5 — `TimeoutCtx` Fields:**
   - **Decision:** Keep borrowed `&TimeoutCtx<'_>` (zero cloning on `Target`, ergonomic `match ctx.user { Some(u) => ... }`) and replace the ambiguous `duration: Duration` with **`pub configured_timeout: Option<Duration>`** (`Some(d)` for relative timeouts, `None` for absolute `Instant` deadlines) + **`pub elapsed: Duration`**.

5. **§1.6 — `with_*` Naming on Owning Guards/Builders (`Breakout`, `ExclusiveDm`, `BotConfig`):**
   - **Decision:** Accepted. Beyond the return-type mismatch between `Breakout` and `&Breakout`, having `Breakout::ask_timeout(self)` share a name with `Room::ask_timeout(&self)` created an accidental-move RAII footgun (`{ let fast = vault.ask_timeout(10s); ... }` moving `vault` and kicking/parting at the end of the inner block!).
   - `Room` keeps `&self -> Room` view methods (`ask_timeout`, `ask_deadline`, `no_ask_timeout`, `on_timeout`, `on_timeout_fn`, `silent_timeout`), while owning builders (`BotConfig`, `Breakout`, `ExclusiveDm`) use **`with_ask_timeout`, `with_ask_deadline`, `with_no_ask_timeout`, `with_on_timeout`, `with_on_timeout_fn`, `with_silent_timeout`** (`mut self -> Self`). Now `vault.ask_timeout(10s)` always borrows `&vault` via `Deref` and returns a non-owning `Room` view without moving `Breakout`!

---

### 7.2 Pushbacks, Refinements, & Ownership Architecture (Section 2)

1. **§2.1 #1 — `ExclusiveDm::ask` Stale-Input Fix (`inbox.clear()`):**
   - **Accepted.** `ExclusiveDm::ask` calls `self.inbox.clear()` immediately before queueing `prompt` so late replies from a prior timed-out prompt or unsolicited pre-prompt lines never satisfy the next `ask`. Documented explicitly as intentionally discarding pre-prompt lines (whereas raw `dm.inbox()` preserves multi-line bursts), with a dedicated regression test.

2. **§2.1 #2 — `BotOwner` vs. `BotCore` Ownership Split (Refined via Counter-Review):**
   - **Refinement accepted:** Instead of `User`/`Room` holding `Arc<BotHandle>` and the pump using `Weak<BotHandle>` (where unread events containing `User` inside an open `Inbox` buffer would still hold strong refs to `BotHandle`), **`Bot` (`Arc<BotOwner>`) is the sole owner of pump liveness (`AbortOnDrop`)**, while **`Room`, `User`, `Inbox`, and the pump task hold only `Arc<BotCore>`** (removing unused `Room::bot()` / `User::bot()`).
   - **Consequences:** Zero `Weak` upgrades in the pump, zero buffered-event retention loops, dropping the last `Bot` handle stops the pump **immediately**, and any `Room` / `User` / `Inbox` outliving `Bot` cleanly returns `Err(Error::Disconnected)` (explicitly documented).

3. **§2.1 #3 & §2.2 — `bot.exclusive_many(&[&User])` & Unbounded Lock Acquisition Docs:**
   - **Refinement (accepted in counter-review):** "Deduplicate and return guards in caller order" was a Rust type contradiction because `ExclusiveDm` is `!Clone`. Instead, `bot.exclusive_many(&[&User]) -> Result<Vec<ExclusiveDm>>` sorts indices by `UserTrack::id`, **rejects duplicate users upfront with `Err(Error::InvalidInput("duplicate user in exclusive_many"))`** (acquiring zero locks), acquires locks in canonical `id` order racing `QUIT` (releasing already-acquired guards if any target quit), and returns `guards` in exact caller order (`guards.len() == users.len()`).
   - **Acquisition bound docs:** Document explicitly that `exclusive()` / `exclusive_many()` wait unboundedly on current lock holders (bounded indirectly by the default 300s read timeout unless a holder opted into `with_no_ask_timeout()`), that `.with_timeout(dur)` / `.with_deadline(instant)` caps queue + reply wall-clock time, and that calling `exclusive()` / `exclusive_many()` for user `X` from a task *already* holding `X`'s `ExclusiveDm` will self-deadlock.

4. **§2.1 #4 — Per-User `Error::Lagged` vs. `.settled()` (Converged Compromise):**
   - **Pushback + refinement:** We keep `Error::Lagged` fatal (`is_fatal() == true`) and keep `Turn<T>` cleanly 3-variant (`Reply`, `TimedOut`, `Left`), rather than adding a 4th `Turn::Lagged` arm to every game's `match` or misreporting an online flooder as `Left`.
   - However, per the counter-review, we do **not** claim active inboxes can never lag (a task holding an open inbox across a slow DB `.await` or `sleep` can fall behind), and we enrich **`Error::Lagged { scope: String, skipped: u64 }`** so any flood-induced abort names the exact lagging scope (`"bot"`, `"dm"`, `"room #lobby"`, `"user Alice"`, `"user Alice in #lobby"`). For multi-player public-prompt collection (`collect_secret_votes`), `u.inbox().lossy()` opts per-voter inboxes into warn-and-skip so one spammer cannot abort `try_join_all`.

5. **§2.1 #5 — Explicit `Inbox` Read Limits:**
   - **Accepted.** Expose `Inbox::with_read_timeout(Duration)`, `Inbox::with_read_deadline(Instant)`, and `Inbox::without_read_timeout()`, and document prominently on `ExclusiveDm::inbox()` that its inbox starts with `BotConfig`'s default `ask_timeout` (300s per read, overridable via `dm.with_ask_timeout(...)` / `dm.with_ask_deadline(...)` / `dm.with_no_ask_timeout()`).

6. **§2.1 #6 — Test Determinism (`current_thread` + `TestServer::settle().await`):**
   - **Accepted.** Switch `[dev-dependencies]` to Tokio's `current_thread` runtime (`["rt", "macros", "test-util"]`) **and** add an in-band barrier `TestServer::settle(&self)` that awaits the pump draining all prior injected lines.

7. **§2.1 #7 — Heist Example `HandshakeTimeout`:**
   - **Accepted.** Handle `Outcome::Completed(Err(Error::HandshakeTimeout { .. }))` explicitly in §5.1's `lobby` (`"never showed up at the vault"`).

8. **§2.3 — Cooperative Graceful Shutdown (`bot.request_shutdown()`, `bot.is_shutting_down()`, `bot.shutdown_requested()`):**
   - **Accepted.** Backed by `tokio::sync::watch` (no `tokio-util` dependency). `bot.request_shutdown()` fires all `bot.shutdown_requested()` futures *before* `QUIT` is queued so running games (`game.until(bot.shutdown_requested())` → `Outcome::Stopped(())`) can send farewell messages and drop `Breakout`s while the socket is open; `bot.quit(reason).await` also triggers `request_shutdown()`, yields once so ready cleanup runs first, and then queues `QUIT` and drains the stream (documenting interaction between large `Breakout::drop` `KICK` bursts, `irc`'s `15 msgs / 8s` throttle, and `handshake_timeout`).

9. **§2.4 — Outbound Flood Control Facts & Multi-Line Warning:**
   - **Accepted.** Verified `irc` v1.x defaults (`max_messages_in_burst = 15`, `burst_window_length = 8s` inside `ClientStream`). Documented enqueue-vs-wire timing and `Breakout::drop` drain sizing, plus a `tracing::warn!` when a single outbound message wraps into > `15` IRC lines.

10. **§2.5 — Smaller Ergonomics:**
    - **Accepted all 4:** `msg.is_in(&room)`, `bot.dm_inbox()` (`Filter::AllDms`), explicit doc note on concurrent `room.ask_user` to the same player in the same channel, and enqueue-time clock docs.
