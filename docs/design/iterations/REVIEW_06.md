RENAMES
- User::room_inbox(&room) -> user.inbox_in(&room)
- Bot inbox family -> inbox_all(), inbox_dms(), inbox_users(..), inbox_rooms(..), inbox_scope(scope). Keep room.inbox() and user.inbox().
- Public RoomEvent -> Event. Rename the internal Event to WireEvent.
- Target::Direct -> Target::Dm
- Match<T> -> Verdict<T>; MatchOutcome -> IntoVerdict; Match::reject() -> Verdict::reject()
- warn_at -> warn_remaining (AskConfig and Ask)
- on_timeout / on_timeout_fn / silent -> timeout_message / timeout_message_fn / no_timeout_message (AskConfig and Ask)
- GameFutureExt::map_to -> map_output; ok_or_never -> ok_or_pending
- RunUntil -> Until; OrStop -> OrUntil; adapters named after their methods (WithTimeout, WithDeadline, Optional, Settled, MapOutput, OkOrPending, RaceAll)
- Breakout::room() -> to_room()
- CaseMap::eq -> str_eq
- Room::occupants() -> remove (members() and nicks() cover it)
- Huddle::members()/online_members() -> users()/online_users()
- huddle_available / exclusive_available / invite_available -> huddle_online / exclusive_online / invite_online
- TestServer::recv_cmd / try_recv_cmd -> recv_sent / try_recv_sent. Reserve "cmd" for chat commands (is_cmd, cmd_args, wait_cmd, command_prefixes).
- Turn::Left and Error::UserLeft -> use the same name in both
- Ask::with_config -> align with ExclusiveDm/Huddle (choose either .config(&cfg) on builders or with_ask_config everywhere, and document the rule)
- Room::ask_config(&self, cfg) -> Room::with_ask_config(&self, cfg)

NAMESPACING
- Add `pub use irc;` at the root.
- Do not re-export Text, Span, Style, Color, Colorize bare at the root. Expose them as ircgm::fmt (and ircgm::irc_colors).
- Add ircgm::prelude: common types, plus `GameFutureExt as _` and `Colorize as _`.
- Put combinator adapter types in a pub mod ircgm::future, not at the root.
- Move TestServer and Bot::test_harness* into ircgm::testing behind a `testing` feature.
- Document: do not glob-import ircgm and irc together (User collision); do not bring colored::Colorize and ircgm's Colorize into the same scope.
- Add a "check crates.io availability for ircgm and irc-colors" item to the pre-0.1 checklist.

CONVENTIONS (add a short "Naming rules" section)
- Builder types (AskConfig, Ask, BreakoutOptions): bare consuming method names.
- Long-lived handles: with_* (consuming), set_* (&mut), without_* (consuming off-switch).
- to_* for clones/expensive conversions; *_with for constructors that take config; "cmd" means chat command only.
- Breakout: add #[must_use]. Prefer configuring ask config via BreakoutOptions at creation; offer set_ask_config(&mut self) afterwards. Note the Deref temporary-drop footgun in the docs.

OPTIONAL (decide, then apply consistently): User::exclusive -> lock_dm (exclusive_many -> lock_dms, exclusive_online -> lock_dms_online); Turn -> Answer; ChatMessage::styled -> to_styled; Huddle::relay_other -> relay_to_others.

