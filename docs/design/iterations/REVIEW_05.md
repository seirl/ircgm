DECISIONS (do not revisit)
- No v1-scope cuts. Keep the full public API surface.
- Huddle stays a separate public type from ExclusiveDm (the relay is the point). Sharing a private DM-session core underneath both is allowed.
- Keep Inbox read timeouts (with_read_timeout etc.). They are what stops a looped dm.inbox() read from holding dm_mutex forever.
- Keep Outcome/or_until/race_all, optional()/settled(), and the strict/lenient method pairs as they are.

MUST CHANGE (spec-level)
1. Outbound scheduling (rewrite §3.5, adjust §2.7 and §6.6):
   - OutboundSink owns the outbound queue and paces/prioritizes it. Set irc's burst limits high so irc's throttle never sits in front of our scheduler. This reverses the earlier "no redundant token bucket" decision.
   - send returns a receipt that resolves when the line is actually written. Ask starts its reply clock on the receipt, not on enqueue.
   - Two priority classes: interactive (PRIVMSG/NOTICE/INVITE/MODE) and background (KICK/PART). FIFO within a class, so per-target ordering holds. QUIT drains both.
   - Batch KICK a,b,c only where 005 TARGMAX allows it. Do not assume it (some networks advertise KICK:1).
   - Update the §6.6 shutdown test: a later PRIVMSG may now precede queued KICKs, so assert per-class FIFO instead of global FIFO.
   - close().await is the awaitable eviction path; priority is the fix for Breakout::drop bursts starving prompts.
2. Registration barrier (rewrite part of §3.2):
   - Connect completes after CAP ACK/NAK and after 376/422, with handshake_timeout as the fallback. "First 005" is not sufficient because ISUPPORT spans multiple lines.
   - If CASEMAPPING changes after tracks exist, rebuild folded keys in UserRegistry and WorldState.
   - Verify in the spike whether irc auto-joins Config.channels before this barrier, and whether irc's own CAP/SASL handling races our CAP REQ (prefer Config.capabilities if the cap set fits).
3. Arc cycle: BotCore -> WorldState -> Member{User} -> Arc<BotCore>, and InboxRegistry -> mpsc buffers -> Event{User} -> Arc<BotCore>. Fix by clearing WorldState and InboxRegistry in BotOwner::drop (or make User hold Weak<BotCore>). Correct the "zero Arc cycles" claim.
4. DeliveryFailed must not be fatal. Make it per-player: add Turn::Unreachable{nick, code} (or fold into Left with a reason) and have settled() map it. Update Error::is_fatal and the Quiplash/vote examples.
5. ChatMessage: replace is_action: bool with kind: Privmsg | Action | Notice. bot.reply mirrors the kind, and a Notice is never answered with a PRIVMSG (RFC 2812, prevents bot loops).
6. NOTICE-as-chat is opt-in per Inbox/Ask/AskConfig via .accept_notices(), not a global BotConfig flag. Accept only notices whose prefix is a real nick!user@host; consider honoring the IRCv3 `bot` tag. Server and services notices stay excluded.

SHOULD CHANGE (smaller)
7. Identity: add netsplit: bool to UserStatus::Quit (heuristic: reason looks like "a.b c.d"). Document that a QUIT then rejoin yields a new UserTrack, and recommend keying long games on account(). State the invariant that DM-only users sharing no channel with the bot are best-effort for NICK/QUIT, so recruit through a channel.
8. UserRegistry GC: amortized sweep of dead Weak entries when len > 2x the last live count. Do not remove entries from Drop (it can re-enter the registry mutex).
9. Inbox registry: index slots by channel, user id, and an All list, with per-event dedupe by slot id (Quit/NickChanged carry multiple channels; multi-source slots sit in several buckets; one user bucket per slot). Use it for an early "does any slot care?" check before Text::parse and UserTrack allocation. This is an optimization, not a correctness item.
10. Error::Lagged message should include a hint: "Did you leave an Inbox open across an .await? Use .clear() or .lossy() if intended." Add #[track_caller] to every .inbox() so the error reports the creation site. Add a one-time tracing::warn! at 75% fill.
11. Docs/cookbook:
   - One rule: a task holding any ExclusiveDm must not await exclusive()/ask() on another user except via exclusive_many. Add a "lock wait exceeded Ns" tracing::warn! naming the holder.
   - Clarify that the AFK protection covers read time only. A guard held across sleep/DB/unrelated awaits is not covered; the answer is outer with_timeout.
   - Add a cookbook example: DM ask(...).settled(), then room.say(...) on Turn::TimedOut, to show on_timeout is local to the prompt's target.
12. Fix the §5.4 compile bug: `if let Turn::Reply((killer, ref victim)) = pick` partially moves pick before pick.into_reply(). Also note leader.inbox().wait_cmd("!abort") in §5.1 only hears DMs. Make all cookbook examples compile-checked doctests.

NOT CHANGING (reviewer concerns already covered or overstated)
- Mixed-locking deadlocks: already in §2.4 item 3 (docs addition above only).
- Inbox filter cost was overstated; the index is an optimization only.
- QUIT already removes registry keys; dead Weaks come mainly from PART/KICK of users nobody holds.

