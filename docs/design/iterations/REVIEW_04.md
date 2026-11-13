# API Evaluation & Holes Found Implementing One Night Ultimate Werewolf

Writing [`bot.rs`](./bot.rs), [`bot_with_dag.rs`](./bot_with_dag.rs), and [`bot_with_plugin_dag/`](./bot_with_plugin_dag/mod.rs) (43 role plugins across Base ONUW, Daybreak, Vampire, Alien, and Super Villains) against [DESIGN.md](../../design/DESIGN.md) without scaffolding or running code surfaced **8 substantive API/protocol holes**, **6 spec-drift omissions inside `DESIGN.md` §4**, and **7 candid API-ergonomics simplifications**—all cheap to resolve *before* implementing `ircgm`.

---

## 1. Blatant API & Protocol Holes

### 1.1 Silent Failure of One-Way Secret DMs (`User::say`) + Missing PM-Block Numerics
- **Where it hit in `bot.rs`:** `recruit_players` (DM check) and `deal_initial_cards` / passive Night roles (`Werewolves`, `Minion`, `Masons`, `Insomniac`).
- **The Problem:**
  1. In ONUW, half the roles (`Villager`, `Tanner`, `Hunter`, `Mason`, `Minion`, `Insomniac`, multi-wolf `Werewolves`) never receive an interactive `user.ask(...)`—they **only** receive secret information via one-way `user.say(...)`. Because `user.say(...)` synchronously enqueues to `OutboundSink`, it returns `Ok(())` even if the server immediately bounces the `PRIVMSG`. A player with PMs blocked plays the entire 8-minute game blind without the bot noticing.
  2. Even when `bot.rs` works around this by running a mandatory pre-game `user.ask("Reply ok...", ...)` DM check at the end of Lobby, **`DESIGN.md` §3.2 only converts `401`, `403`, `404`, and `442` into `Event::SendError` (`Error::DeliveryFailed`)**. On modern IRC networks (Libera, OFTC, Ergo, UnrealIRCd, InspIRCd), blocked DMs commonly return:
     - `486 ERR_NONONREG` (*"You must identify to a registered nick to private message..."*, `+R`)
     - `716 RPL_TARGUMODEG` (*"<nick> is in +g mode (server-side ignore)"*, `callerid` — default on many networks!)
     - `531 ERR_CANTSENDTOUSER` / `492 ERR_NOCTCP`
     Because the pump ignores these numerics, `user.ask(...)` hangs for the full timeout instead of failing in milliseconds with `Err(Error::DeliveryFailed { .. })`.
- **Recommended Fix before Implementing:**
  - Expand the pump's `Event::SendError` numeric table to include `486`, `492`, `531`, and `716` (note: on `716`, `target` in numeric parameters is typically `params[1]`, after our own nick in `params[0]`—test exact parameter positions per numeric!).
  - Consider exposing `RoomEvent::DeliveryFailed { target, code, message }` (or `User::last_delivery_error()`) on `Inbox::next_event()` so games sending one-way secret role DMs can detect bounced cards.

### 1.2 No RAII Moderation Guard for Persistent Channels + Unbatched `+v`/`-v` Modes
- **Where it hit in `bot.rs`:** `ChannelModerationGuard` (`+m` + `+v`/`-v` + topic updates on `#werewolf`).
- **The Problem:**
  1. `Breakout` has first-class cancellation-safe `Drop` cleanup, which works great for heist vaults. However, social deduction games (`Mafia`, `ONUW`) are almost always played in a **persistent public channel** (`#werewolf`) so spectators can watch Day debates and Dawn reveals while `+m` + `+v` keeps spectators and sleeping players from talking out of turn.
  2. If a game runs under `.until(stop)` (admin `!stop` or `bot.shutdown_requested()`) or unwinds via `?` during Night while `#werewolf` is `+m` and all players are `-v`, **`Room` has no cleanup guard—leaving `#werewolf` permanently muted (`+m`) after the game aborts** unless every bot author invents `ChannelModerationGuard` by hand.
  3. Worse, `Room` only exposes singular `room.voice(&user)` and `room.devoice(&user)`. Transitioning 10 players between Night (`10x -v`), Day (`10x +v`), and Vote (`10x -v`) queues **10 separate `MODE #werewolf +v <nick>` commands** instead of batching by `005 MODES=4` (`3` lines of `MODE #werewolf +vvvv ...`), burning two-thirds of `irc` v1.x's `15 msgs / 8s` burst window on mode changes alone!
- **Recommended Fix before Implementing:**
  - Add batched **`room.voice_many(&users)`** and **`room.devoice_many(&users)`** (chunked automatically by `WorldState`'s `005 MODES` parameter limit, defaulting to `4`).
  - Either provide an opt-in RAII `ModerationGuard` (`room.moderate()`) that tracks applied modes/voices/topic and reverts them synchronously on `Drop`, or include `ChannelModerationGuard` as a first-class recipe/helper.

### 1.3 Outbound Burst Throttling (`15 msgs / 8s`) vs. `N`-Player DM Fan-Out & `Ask` Timers
- **Where it hit in `bot.rs`:** `deal_initial_cards` (Setup → Night) and `run_voting_phase` (Day → Simultaneous DM Vote).
- **The Problem:**
  - At the start of Voting in a 10-player game, the bot simultaneously devoices 10 players, announces voting in `#werewolf`, and polls 10 `voter.ask(...)` futures via `try_join_all`. Even with batched `devoice_many` (`3` lines) + `1` channel `PRIVMSG` + `10` DM `PRIVMSG`s (`14`–`24` lines if any prompt wraps), `irc` v1.x's client-side burst throttle (`max_messages_in_burst = 15`, `burst_window_length = 8s`) stalls messages `16+` for several seconds inside `ClientStream`.
  - Meanwhile, **`Ask` starts its `reply_timeout` clock as soon as `OutboundSink` enqueues the message** (`DESIGN.md` §2.7), and `DESIGN.md` §3.5 only logs a `tracing::warn!` when a *single* `say()` wraps past 15 lines—never when 20 separate 1-line `say`/`ask` calls burst in the same tick!
- **Recommended Fix before Implementing:**
  - Track aggregate outbound enqueue burst depth in `OutboundSink` (not just single-call line count) for the `tracing::warn!` diagnostic.
  - Document configuring `irc::client::data::Config { max_messages_in_burst, .. }` (or raise `BotConfig::new`'s default if appropriate for modern IRCv3 servers with `fakelag` allowances) for `8+`-player DM games.

### 1.4 Resolving Player Nickname Inputs & `Room` Membership by `&User`
- **Where it hit in `bot.rs`:** `resolve_player_token` (used in Doppelgänger, Seer, Robber, Troublemaker, and Voting) and `is_active_in_room`.
- **The Problem:**
  - Cookbook §5.4 (`collect_secret_votes`) stopped at returning `Option<String>` for `!vote <nick>`. The moment `bot.rs` had to resolve that string to an enrolled player in `players: &[PlayerSlot]`:
    1. `p.user.nick() == token` is **wrong** on IRC (fails on case and RFC 1459 `[Alice]` vs `{alice}`) and clones a `String` out of `RwLock` on every iteration.
    2. `bot.user(token)` is **wrong** for input parsing because typos allocate transient `UserTrack` entries in `UserRegistry` and match arbitrary non-playing users on the network.
    3. `CaseMap` is returned by `bot.casemap()`, **but `CaseMap`'s methods are never specified anywhere in `DESIGN.md` §4**.
    4. Similarly, checking whether an enrolled `user: &User` is still in `room: &Room` required `room.has_nick(&user.nick())` because `Room` lacks `room.has_user(&User) -> bool` (or `room.member(&User) -> Option<Member>`).
- **Recommended Fix before Implementing:**
  - Specify `CaseMap` publicly in §4 (`CaseMap::eq(&self, a: &str, b: &str) -> bool`, `CaseMap::fold(&self, s: &str) -> FoldedStr`) and re-export `CaseMap`.
  - Add **`User::eq_nick(&self, nick: &str) -> bool`** (zero-allocation read-locked comparison using `BotCore`'s active `CaseMap`) and **`Room::has_user(&self, user: &User) -> bool`** / **`Room::member(&self, user: &User) -> Option<Member>`** (direct `UserTrack::id` lookup).

### 1.5 `ExclusiveDm::ask` Dual-Timer Conflict When Lengthening Step-2 Deadlines (Found in `bot_with_dag.rs`)
- **Where it hit in [`bot_with_dag.rs`](./bot_with_dag.rs):** `run_doppel_dag_branch` (Step 1a shares `stage1_deadline`, then Step 1b extends the deadline to `stage1_deadline.max(now + 25s)` via `dm.ask(...).reply_deadline(step1b_deadline)` so a player who used 38s of 40s on Step 1a still gets 25s for Step 1b).
- **The Problem:**
  - In `DESIGN.md` §4.6 (lines 809–837), `ExclusiveDm` stores `inbox: Inbox`, and calling `dm.with_ask_config(stage1_cfg)` / `dm.with_ask_deadline(d1)` updates **both** `self.ask_config` **and `self.inbox`'s internal `read_limit` (`TimeLimit`)**!
  - Unlike `Room::ask` (which opens a fresh `Inbox` with `read_limit = None`), `ExclusiveDm::ask` reuses `self.inbox`. If `ExclusiveDm::ask` polls `self.inbox.wait_map(...)` while `self.inbox`'s `read_limit` is still set to `d1` (or `BotConfig`'s 300s default), **`self.inbox` will expire early at `d1` even when the caller explicitly lengthened the prompt via `.reply_deadline(d2)` (`d2 > d1`), `.reply_timeout(600s)`, or `.no_reply_timeout()`**—and worse, an early `Inbox` read timeout won't trigger `Ask`'s `on_timeout` handler if `Ask` thinks its own timer hasn't fired yet!
- **Recommended Fix before Implementing:**
  - Inside `ExclusiveDm::ask`, `Ask::poll` must poll raw events on `self.inbox` **bypassing `self.inbox.read_limit`** so `Ask`'s own resolved deadline is the *single* source of truth during `dm.ask(...)` (`self.inbox.read_limit` should apply **only** when user code reads `dm.inbox().next_chat()` / `wait_*` directly!). Add a regression test in §6 for `dm.with_ask_timeout(10s)` followed by `dm.ask(...).reply_timeout(60s)`.

### 1.6 `&mut ExclusiveDm` and `&mut Inbox` Cannot Reconfigure Timeouts (`self` by-value Trap Found in `bot_with_plugin_dag/`)
- **Where it hit in [`bot_with_plugin_dag/plugin.rs`](./bot_with_plugin_dag/plugin.rs):** `RolePlugin::prompt_seat(&self, dm: &mut ExclusiveDm, ...)` (so `Doppelgänger` can pass its already-locked `&mut ExclusiveDm` straight into `copied_role.prompt_seat(...)` without deadlocking `dm_mutex`).
- **The Problem:**
  1. Every modifier on `ExclusiveDm` (`with_ask_timeout(mut self) -> Self`, `with_ask_deadline`, `with_ignore_commands`) and every modifier on `Inbox` (`with_read_timeout(self) -> Self`, `with_read_deadline`, `without_read_timeout`, `ignore_commands`) takes **`self` by value** with **zero `&mut self` setter equivalents**!
  2. Worse, `ExclusiveDm::inbox(&mut self) -> &mut Inbox` *only* ever returns `&mut Inbox`—meaning **`dm.inbox().with_read_timeout(...)` is an immediate compile error (`cannot move out of *dm.inbox()`)**, and any helper or trait method receiving `dm: &mut ExclusiveDm` cannot adjust `dm`'s read limit or `ignore_commands` for multi-line `dm.inbox().next_chat()` reads!
  3. Also, boxing `dm.ask(...)` inside `Pin<Box<dyn Future<...> + Send + 'a>>` (`BoxFuture`) relies on `Bot`, `Room`, `User`, `AskConfig`, and `CaseMap` being `Send + Sync` and `ExclusiveDm`, `Inbox`, `Breakout`, and `Ask` being `Send`—worth locking down with compile-time auto-trait assertions in §6.
- **Recommended Fix before Implementing:**
  - Add `&mut self` setters (`set_read_timeout(&mut self, ...)`, `set_read_deadline(&mut self, ...)`, `clear_read_timeout(&mut self)`, `set_ignore_commands(&mut self, bool)`) on `Inbox` and corresponding `set_ask_*(&mut self, ...)` setters on `ExclusiveDm` alongside the `with_*` builders.
  - Add static `assert_send_sync` / `assert_send` tests to the test suite.

### 1.7 Secret Pack Deliberation: `Breakout::invite_and_wait` & Multi-DM Relay Frictions (Even With Auto-Join!)
- **Where it hit in [`bot_with_plugin_dag/roles/vampire/vampire.rs`](./bot_with_plugin_dag/roles/vampire/vampire.rs):** Multi-player Vampire coven wake in Dusk (identical to classic Werewolf night kills where Wolves argue and pick one victim).
- **Even assuming 100% client auto-join on `INVITE` (or server-forced `SAJOIN`), three `ircgm` API frictions remain:**
  1. **Single disconnected teammate aborts both `Breakout::invite_and_wait` AND `Bot::exclusive_many`:**
     - If 1 of 3 Wolves/Vampires quit IRC 2 seconds earlier, `den.invite_and_wait(&wolves, 5s).await?` and `bot.exclusive_many(&wolves).await?` **both fail the entire batch with `Err(Error::UserLeft)`** (dropping `den` and kicking the 2 online Wolves who already auto-joined!).
     - Worse, if `invite_and_wait` hits a lagged bouncer and returns `Err(Error::HandshakeTimeout)`, **`.settled()` does not catch `HandshakeTimeout`** (only `TimedOut` and `UserLeft`), so callers can't use `.settled()` to proceed with whichever teammates made it in.
  2. **`Room::invite(&self, nick: &str)` signature asymmetry:**
     - `Room::kick`, `Room::voice`, and `Room::devoice` all take `&User`, whereas `Room::invite` takes `&str` (`DESIGN.md` §4.5 line 760) while `Breakout::invite_and_wait` takes `P: InviteTarget`. Adding a newly converted Wolf (`Cursed` / `Alpha Wolf`) to a persistent multi-night `#wolves` `Breakout` requires `den.invite(&new_wolf.nick())` instead of `den.invite(&new_wolf)`.
  3. **Missing first-class multi-DM relay (`Huddle`) + outbound burst cost:**
     - Spinning up ephemeral `Breakout` rooms in parallel (e.g. Wolves + Masons + Coven) costs `JOIN` + `MODE +is` + `INVITE`s + `KICK`s + `PART` against `irc`'s `15 msgs / 8s` burst budget, whereas bot-relayed DMs need zero channel protocol commands—yet building a DM relay today (`bot.users_inbox`) forces every game to hand-roll online-lock retries, peer whisper echoing, and `Ask` plumbing.
- **Recommended Fixes before Implementing:**
  - On `Breakout` / `Room`: make **`Room::invite` accept `impl InviteTarget`** (`&User`, `&str`, `&String`), and add **`den.invite_online(&users, timeout) -> Result<Vec<User>>`** (skipping offline users and returning the subset that joined before `timeout`, failing only if `0` joined or on fatal error).
  - Add a first-class **`Huddle`** handle (`bot.huddle(&users).await?`) mirroring `Room`/`Breakout`: locks `ExclusiveDm` on all *online* members in `UserTrack::id` order, exposes `.say(...)` broadcast, `.ask(prompt, matcher)`, `.inbox()` / `.user_inbox(&u)`, and optional peer chat relay. (Per-member voting & consensus then stay pure game logic on top of `user_inbox` / `inbox`, just like `w.room_inbox(&den)`!)

### 1.8 Cross-Player `User::say` Interleaving During Parallel `Ask`s (Found in `roles/super_villains/role_retriever.rs`)
- **Where it hit in [`bot_with_plugin_dag/roles/super_villains/role_retriever.rs`](./bot_with_plugin_dag/roles/super_villains/role_retriever.rs) & [`curator.rs`](./bot_with_plugin_dag/roles/daybreak/curator.rs):** One player's night action sends an unsolicited secret notification (`victim.say("Your card is now ...")`) to *another* player.
- **The Subtlety:**
  - `User::say` intentionally bypasses `dm_mutex` (so `Role Retriever` notifying `Alice` while `Alice` holds `ExclusiveDm` for her own role does **not** deadlock—a good design choice!).
  - However, in a parallel Night DAG, if `Role Retriever` called `victim.say(...)` inside `prompt_seat`, that notification could arrive in `victim`'s DM window right in the middle of `victim`'s own active `Ask` prompt (and before upstream swaps have even settled!). Deferring cross-player notifications to `resolve_seat` solved both causal accuracy and most interleaving, and is worth calling out in `ExclusiveDm`'s docs (*"`ExclusiveDm` serializes `ask` sessions; one-way `User::say` notifications from other tasks may still be delivered concurrently"*).

---

## 2. Internal Spec Drift in `DESIGN.md` (Fix Before Coding)

While writing `bot.rs`, we noticed that `REVIEW_03.md` decisions were added to `DESIGN.md` §1–§3 and Table 4.1, **but not yet propagated into the Rust signature blocks in `DESIGN.md` §4.2–§4.10**:

1. **Crate root re-exports (§4.2, line 415):** Missing `Match`, `MatchOutcome`, `CaseMap`, `Member`, `DisconnectReason`, `Outcome`, `Turn`, `GameFutureExt`, `InboxScope`.
2. **`ChatMessage` (§4.3, lines 566–583):** Missing `pub seq: u64`, `pub server_time: Option<SystemTime>`, and `pub received_at: tokio::time::Instant` (promised in `REVIEW_03.md` §6.1.1 & §6.1.5). Also `RoomEvent` (line 598) is missing `#[non_exhaustive]`.
3. **`Inbox` (§4.4, lines 649–689):** Missing `pub fn ignore_commands(self) -> Self` (listed in Table 4.1) and explicit departure semantics for **mixed** `InboxScope` filters containing *both* `.room(&room)` and `.dms_from(&users)` (what happens in `bot.rs`'s `info_responder` if all `users` quit while `room` is still active, or `BotLeftRoom` fires while DMs are still active? Suggestion: `BotLeftRoom` is an error if any scoped room loses the bot; `UserLeft` only fires on a pure `dms_from` scope when *all* scoped DM users have quit, never when an active `room` remains in scope). Also explicitly state that `room.inbox()` delivers `RoomEvent::Quit` and `RoomEvent::NickChanged` whenever `channels` includes `room`.
4. **`Room` (§4.5, lines 701–747):** Missing `Member` struct definition and all moderation/membership signatures listed in Table 4.1 (`members`, `bot_is_op`, `kick`, `set_mode`, `voice`, `devoice`, `topic`, `set_topic`).
5. **`User` & `ExclusiveDm` (§4.6, lines 764–838):** Missing `User::account(&self) -> Option<String>` and `ExclusiveDm::with_ignore_commands(self) -> Self` (both listed in Table 4.1).
6. **`Ask` & `AskConfig` (§4.7, lines 862–915) & `Error` (§4.10, lines 1056–1110):**
   - §4.7 still types `matcher` as `FnMut(&ChatMessage) -> Option<T>` instead of generic `R: MatchOutcome` (`Option<T>` + `Match<T>`), and omits `Ask::warn_at`, `Ask::ignore_commands`, and `AskConfig::ignore_commands`.
   - §4.10 still defines unit `Error::Disconnected` (and omits `#[non_exhaustive]`), contradicting `Error::Disconnected(DisconnectReason::BotDropped)` in §1 (line 69) and `DisconnectReason::Eof` in §4.2 (line 477).

---

## 3. Ergonomics & "This Could Be Much Simpler" Critique

Beyond strict bugs, several parts of the surface API felt over-engineered in one corner (`Ask` timeout inheritance) while forcing tedious manual boilerplate in the places every multiplayer IRC game actually spends its lines:

### 3.1 Four Different Spellings of "Timeout", "No Timeout", and "Silent" Across Five Types
While writing the bots, we constantly had to flip back to `DESIGN.md` §4 just to check what each method was named on *this* struct:

| Concept | `BotConfig` | `Room` | `ExclusiveDm` / `Breakout` | `Ask` / `AskConfig` | `Inbox` |
|---|---|---|---|---|---|
| Set timeout | `with_ask_timeout` | `ask_timeout` | `with_ask_timeout` | `reply_timeout` | `with_read_timeout` |
| Disable timeout | `with_no_ask_timeout` | `no_ask_timeout` | `with_no_ask_timeout` | `no_reply_timeout` | `without_read_timeout` |
| Suppress msg | `with_silent_timeout` | `silent_timeout` | `with_silent_timeout` | `silent` | — |

Having **`no_ask_timeout`, `with_no_ask_timeout`, `no_reply_timeout`, and `without_read_timeout`** (plus `silent` vs `silent_timeout` vs `with_silent_timeout`) in one small crate is an artifact of solving micro-collisions one review at a time. Worse, `AskConfig` still lacked `.warn_at(...)`, so we had to repeat `.warn_at(Duration::from_secs(10), ...)` at every `ask` call site anyway!
- **Simpler:** Centralize prompt config on **`AskConfig`** (including `.warn_at(...)` and clear merge/override rules on `.with_config(&cfg)`), cut ~40% of the duplicated builder methods across `BotConfig` / `Room` / `Breakout` / `ExclusiveDm`, and unify vocabulary (`timeout` / `no_timeout` / `silent` everywhere except `GameFutureExt::with_timeout`).

### 3.2 `User::ask` and `ExclusiveDm::ask` Can't Share Code — AND `User::ask` Silently Self-Deadlocks Inside `ExclusiveDm`
- `User::ask(&self, ...)` and `ExclusiveDm::ask(&mut self, ...)` share no trait (`DmTarget` / `PromptSession`). In `bot.rs`, standalone `Seer` (`seer_user.ask(...)`) and `Doppelgänger-Seer` (`dm.ask(...)`) couldn't share their `.ask(...)` call unless every role manually opened `user.exclusive().await?` upfront.
- Worse: **if code holds `let mut dm = alice.exclusive().await?` and calls any helper `foo(&alice)` that internally calls `alice.ask(...)`, the task silently deadlocks forever** with no compiler warning and no runtime panic!

### 3.3 Lopsided Scope: 35 Timeout Methods on `Ask`, Zero Help for Lobbies or Countdown Timers
- Asking a single Night question took **6 lines**.
- Recruiting `3..=10` players (`!join`, `!leave`, `!go`, automatically dropping `Parted`/`Kicked`/`Quit` users, 30s warning) took **~115 lines** of manual `tokio::select!`, `Instant` math, and `RoomEvent` boilerplate.
- Running a 5-minute Day timer with `2m / 1m / 15s` warnings + `!ready` early-vote consensus took **~95 lines** of `sleep_until`, `saturating_duration_since`, and `milestones.remove(0)`.
- Almost *every* multiplayer IRC game (Mafia, ONUW, Poker, Trivia, Coup, Secret Hitler) needs a join/leave/quit-aware lobby and a milestone countdown timer. Adding a tiny `Countdown` stream/helper (yielding `Tick::Warning(Duration)` / `Tick::Expired`) would delete ~100 lines of error-prone time math per bot.

### 3.4 Writing `Match::Reject(Text::from("...".yellow()))` 30+ Times & `irc-colors` Concatenation Noise
- Every outbound method (`say`, `reply`, `ask`, `on_timeout`) accepts `impl Into<Text>`, so `"..." .yellow()` (`Span`) works directly. Because `Match::Reject(Text)` is an enum variant rather than a constructor function, Rust won't call `.into()`, forcing `Match::Reject(Text::from("...".yellow()))` on every rejection arm across all 12 role files! Adding **`Match::reject(impl Into<Text>)`** eliminates every `Text::from(...)` wrapper.
- Also note: if a bot author writes `_ => Match::reject("Invalid!")` on an **open-floor `room.ask(...)`** in a 40-person channel, spectator chatter will trigger 40 rejection lines—worth a prominent doc warning to return `Match::Ignore` for unrelated channel chat.
- Finally, `irc-colors` `+` chaining looks clean in 2-span cookbook snippets, and turns into awkward `format!(...) + span + " literal "` gymnastics in real games with seat numbers, counts, and plurals (especially since `"a" + "b"` doesn't compile unless the left-most operand is already a `Span`/`Text`/`String`). A small `irc_text!` formatting macro would cut half the visual noise.

### 3.5 Over-Ceremonious Setup Just to Allow `!deck` / `!help` During Active Prompts
Letting a player type `!deck` or `!players` in DM while `seer.ask(...)` is waiting required **four separate moving parts**:
1. `.with_command_prefixes(["!"])` on `BotConfig`,
2. `.ignore_commands()` on `AskConfig` *and* `.with_ignore_commands()` on `ExclusiveDm`,
3. Constructing `InboxScope::new().room(room).dms_from(&players).lossy()`, and
4. Writing an infinite `async` loop (with `#[allow(unreachable_code)] Ok::<(), Error>(())` for type inference) raced against `game_flow` in `tokio::select!`.

### 3.6 Threading `casemap: CaseMap` Everywhere Because `User` / `Room` Hide Session Helpers
`User` and `Room` already hold `Arc<BotCore>` internally (rightly avoiding `Arc<BotOwner>` so pump drop-liveness stays exclusively on `Bot`). However, because read-only identity helpers weren't exposed on `User` and `Room`, we had to thread `bot.casemap()` down 5–6 helper layers just to match nicknames (`user.eq_nick(token)` kills the parameter entirely) and write `room.has_nick(&user.nick())` instead of `room.has_user(&user)`. Also, in public channels where `@` chanop is optional, every moderation call had to be wrapped in `if room.bot_is_op() { let _ = ...; }` so `?` wouldn't abort the game on `Err(Error::NotChannelOp)`.

### 3.7 `.until(stop)` Breaks Down as Soon as You Have Two Stop Signals
`game.until(admin_stop)` and `game.until(bot.shutdown_requested())` each look great alone in `DESIGN.md`. The moment a real `lobby_loop` needs **both** admin `!stop` *and* graceful shutdown (`bot.shutdown_requested()`), chaining `.until(a).until(b)` nests `Outcome<Outcome<Result<()>, ChatMessage>, ()>`, forcing us right back to a manual `tokio::select! { biased; ... }` block anyway.

---

## 4. What Worked Exceptionally Well

- **Direct-style sequential Night Phase (`run_night_phase`):** Writing 9 ordered, interdependent night steps—including conditional Lone Wolf center peeking, dynamic Werewolf/Mason pack lists after Doppelgänger, and card swaps mutating shared `TableState`—as a single linear `async fn` with zero state-machine enums was genuinely a joy to read and write.
- **`ExclusiveDm` on the Doppelgänger trap (`run_doppelganger_step`):** Holding one `ExclusiveDm` with `.with_ask_deadline(step_deadline)` across Step 1a (*"Who do you copy?"*) and conditional Step 1b (*immediate Seer/Robber/Troublemaker/Drunk follow-up*) solved the hardest rule in ONUW cleanly with zero risk of interleaved prompts.
- **Simultaneous Secret Voting (`try_join_all` + `voter.ask` + `Match::Reject` + `.settled()`):** Every voter gets private validation feedback on typos/self-votes, AFK or disconnected voters degrade cleanly to `None` (`abstained`) via `.settled()`, and voting finishes the instant the last player locks in.
- **Out-of-band `!deck` / `!players` responder (`InboxScope` + `.ignore_commands()` + `bot.reply`):** Combining `.ignore_commands()` on active `Ask`s with a concurrent `bot.inbox_for(InboxScope::new().room(room).dms_from(&enrolled_users))` loop answered player queries mid-prompt in both `#werewolf` and DMs without deadlocking `dm_mutex` or triggering `Match::Reject`.
- **Mid-game `/nick` resilience (`User: Eq + Hash` + live `.nick()`):** Storing `User` in `PlayerSlot`, `HashSet<User>` (`ready_voters`), and `ChannelModerationGuard` meant `/nick` changes mid-Night or mid-Day required zero manual nick-tracking code anywhere in the game logic.

---

## 5. Model Flexibility Stress-Test: Daybreak, Vampire, Alien, & Super Villains

Extending [`bot_with_plugin_dag/`](./bot_with_plugin_dag/mod.rs) from 12 base roles to **43 plugins** covering every distinct mechanic across all 4 expansions required **zero concrete role special-cases in [`dag.rs`](./bot_with_plugin_dag/dag.rs)**:
- **2-Stage Dusk (`wake_order < 100`) → Night (`wake_order >= 100`) DAG:** Automatically runs `Copycat` / `Doppelgänger`, `Sentinel` shields, and Vampire Dusk Marks (`Mark::Vampire`, `Mark::Fear`, `Mark::Clarity`, `Mark::Love`, `Mark::Assassin`, `Mark::Disease`) before Night—suppressing Night wakes for `Mark::Fear` holders and passing `Sentinel`'s shield set into Night `PromptCtx`s—while collapsing to **0ms overhead** when no Dusk/Copycat cards are in the deck.
- **Richer Tabletop State on `BoardCards`:** Adding `shielded`, `marks`, `artifacts`, `face_up_players` / `face_up_center`, `viewed_or_moved_actors` (for `Aura Seer`), `extra_evil_card` (for `Alpha Wolf` / `Temptress`), `blob_seats`, and `oracle_omen` (dynamic Alien app state between `Oracle` and `Psychic`) let every expansion plugin compose purely through `BoardCards` + `RoleTag`s.
- **Mid-Night Role Transformation (`Card::Transformed`) & Global Win Overrides (`check_team_win_override`):** Cleanly expressed `Paranormal Investigator` transforming mid-prompt upon spotting a threat/Tanner, and Alien `Leader` flipping team victory when all Aliens point at the Leader.
