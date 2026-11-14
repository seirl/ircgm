use std::collections::HashMap;
use std::sync::Arc;

use irc::proto::{Command, Message, Prefix, Response};
use tokio::sync::watch;

use crate::casemap::{AtomicCaseMap, CaseMap, FoldedStr};
use crate::error::Error;
use crate::user::{UserRegistry, UserTrack};

/// Conservative `user@host` byte reserve (`76` bytes) used before the bot observes its own
/// relayed hostmask on the wire (`001`, self `JOIN`/`NICK`, or `396 RPL_HOSTHIDDEN`).
pub(crate) const DEFAULT_USER_HOST_RESERVE: usize = 76;

/// Fallback `user` (ident) reserve (`12` bytes) when `396 RPL_HOSTHIDDEN` reveals our vhost
/// before we have seen our ident in a `nick!user@host` prefix.
const DEFAULT_IDENT_RESERVE: usize = 12;

/// Parameter arity categories from `005 CHANMODES=A,B,C,D`.
#[derive(Clone, Debug)]
pub(crate) struct ChanModes {
    /// Category A: list modes (always take a parameter on `+` and `-`, e.g. `b,e,I`).
    pub(crate) list_a: String,
    /// Category B: modes that always take a parameter on `+` and `-` (e.g. `k`).
    pub(crate) param_always_b: String,
    /// Category C: modes that take a parameter on `+` only, never on `-` (e.g. `l`).
    pub(crate) param_set_c: String,
}

impl Default for ChanModes {
    fn default() -> Self {
        Self {
            list_a: "beI".into(),
            param_always_b: "k".into(),
            param_set_c: "l".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Occupant {
    pub(crate) track: Arc<UserTrack>,
    /// Ordered highest-to-lowest according to `WorldState::status_prefixes` (e.g. `['@', '+']`).
    pub(crate) prefixes: Vec<char>,
}

/// Lifecycle of the bot's presence in a channel.
#[derive(Clone, Debug)]
pub(crate) enum ChannelPhase {
    /// Self `JOIN` seen: `353` entries and any `JOIN`/`PART`/`MODE` arriving before `366` are
    /// staged here so none are lost when `366` commits the roster.
    Joining { staged: HashMap<u64, Occupant> },
    /// `366` committed `members`; `resync` stages a fresh `NAMES` reply while joined.
    Joined {
        members: HashMap<u64, Occupant>,
        resync: Option<HashMap<u64, Occupant>>,
    },
    /// The bot enqueued its own `PART` (`Room::part`, `Breakout` teardown) and awaits the echo;
    /// `Bot::join` must rejoin rather than hand out a `Room` that is about to be left.
    /// `evict_on_leave` marks ephemeral `Breakout` channels whose entry is dropped (instead of
    /// recorded as `Left`) once the echo arrives and nobody watches the channel.
    Parting {
        members: HashMap<u64, Occupant>,
        evict_on_leave: bool,
    },
    /// The bot itself left (`PART` / `KICK`) at wire sequence `at_seq`.
    Left {
        reason: Option<String>,
        at_seq: u64,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct ChannelEntry {
    /// Canonical channel name as last observed on the wire.
    pub(crate) name: String,
    pub(crate) topic: Option<String>,
    pub(crate) phase: ChannelPhase,
}

impl ChannelEntry {
    fn joining(name: String) -> Self {
        Self {
            name,
            topic: None,
            phase: ChannelPhase::Joining {
                staged: HashMap::new(),
            },
        }
    }

    /// `true` while the bot is joining, joined, or parting (not `Left`).
    pub(crate) fn is_active(&self) -> bool {
        !matches!(self.phase, ChannelPhase::Left { .. })
    }

    /// Committed roster once `366` synced the channel (`None` while `Joining`, `Parting`, or
    /// after `Left`).
    pub(crate) fn members(&self) -> Option<&HashMap<u64, Occupant>> {
        match &self.phase {
            ChannelPhase::Joined { members, .. } => Some(members),
            ChannelPhase::Joining { .. } | ChannelPhase::Parting { .. } | ChannelPhase::Left { .. } => {
                None
            }
        }
    }

    /// Best-known roster: committed members once `Joined` / `Parting`, or the staged roster while
    /// `Joining`.
    pub(crate) fn roster(&self) -> Option<&HashMap<u64, Occupant>> {
        match &self.phase {
            ChannelPhase::Joining { staged } => Some(staged),
            ChannelPhase::Joined { members, .. } | ChannelPhase::Parting { members, .. } => Some(members),
            ChannelPhase::Left { .. } => None,
        }
    }

    pub(crate) fn member_by_nick(&self, nick: &str) -> Option<&Occupant> {
        self.roster()?
            .values()
            .find(|occ| occ.track.casemap().str_eq(&occ.track.nick(), nick))
    }

    /// Wire sequence at which the bot left this channel, if it has.
    pub(crate) fn left_at_seq(&self) -> Option<u64> {
        match &self.phase {
            ChannelPhase::Left { at_seq, .. } => Some(*at_seq),
            ChannelPhase::Joining { .. } | ChannelPhase::Joined { .. } | ChannelPhase::Parting { .. } => {
                None
            }
        }
    }

    /// `Some(Error::BotLeftRoom { .. })` once the bot has left this channel.
    pub(crate) fn left_error(&self) -> Option<Error> {
        match &self.phase {
            ChannelPhase::Left { reason, .. } => Some(Error::BotLeftRoom {
                channel: self.name.clone(),
                reason: reason.clone(),
            }),
            ChannelPhase::Joining { .. } | ChannelPhase::Joined { .. } | ChannelPhase::Parting { .. } => {
                None
            }
        }
    }

    fn tracks_member(&self, track_id: u64) -> bool {
        match &self.phase {
            ChannelPhase::Joining { staged } => staged.contains_key(&track_id),
            ChannelPhase::Joined { members, resync } => {
                members.contains_key(&track_id)
                    || resync.as_ref().is_some_and(|r| r.contains_key(&track_id))
            }
            ChannelPhase::Parting { members, .. } => members.contains_key(&track_id),
            ChannelPhase::Left { .. } => false,
        }
    }

    fn for_each_roster_mut(&mut self, mut f: impl FnMut(&mut HashMap<u64, Occupant>)) {
        match &mut self.phase {
            ChannelPhase::Joining { staged } => f(staged),
            ChannelPhase::Joined { members, resync } => {
                f(members);
                if let Some(r) = resync {
                    f(r);
                }
            }
            ChannelPhase::Parting { members, .. } => f(members),
            ChannelPhase::Left { .. } => {}
        }
    }
}

/// What `WorldState::check_bot_in_channel` requires of the bot's presence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BotPresence {
    /// Chat, invites, asks, and replies: fail only once the bot is known to be out.
    Tracked,
    /// Moderation (`KICK`, `MODE`, `TOPIC`): the bot must be in the channel with `+o`.
    Op,
}

/// Resolution of an in-flight `Bot::join` shared by every coalesced waiter.
#[derive(Clone, Debug)]
pub(crate) enum JoinOutcome {
    /// `366` committed the roster under the server's canonical channel `name`.
    Joined { name: String },
    /// A join-refusal numeric (`403`, `405`, `471`..`477`) rejected the `JOIN`.
    Failed {
        channel: String,
        code: u16,
        message: String,
    },
    /// The bot was kicked (or parted) after its self `JOIN` but before `366`.
    Left {
        channel: String,
        reason: Option<String>,
    },
}

pub(crate) struct WorldState {
    pub(crate) casemap: CaseMap,
    shared_casemap: AtomicCaseMap,
    pub(crate) chan_types: Vec<char>,
    /// `(mode_char, prefix_symbol)` ordered highest to lowest privilege.
    pub(crate) status_prefixes: Vec<(char, char)>,
    pub(crate) chan_modes: ChanModes,
    pub(crate) max_modes: usize,
    pub(crate) targmax_kick: usize,
    pub(crate) current_nick: String,
    pub(crate) self_track_id: Option<u64>,
    pub(crate) self_ident: Option<String>,
    pub(crate) self_host: Option<String>,
    channels: HashMap<FoldedStr, ChannelEntry>,
    /// One outcome channel per channel with `Bot::join` waiters, resolved on `366` or refusal.
    pending_joins: HashMap<FoldedStr, PendingJoin>,
}

/// In-flight `Bot::join` shared by coalesced waiters (keeps the requested `name` so `005
/// CASEMAPPING` changes can re-key it). `exclusive` marks a `Bot::breakout_with` claim that
/// plain `Bot::join` callers must not coalesce onto.
struct PendingJoin {
    name: String,
    tx: watch::Sender<Option<JoinOutcome>>,
    exclusive: bool,
}

impl WorldState {
    pub(crate) fn new(initial_nick: impl Into<String>, shared_casemap: AtomicCaseMap) -> Self {
        Self {
            casemap: shared_casemap.load(),
            shared_casemap,
            chan_types: vec!['#', '&'],
            status_prefixes: vec![('o', '@'), ('v', '+')],
            chan_modes: ChanModes::default(),
            max_modes: 4,
            targmax_kick: 1,
            current_nick: initial_nick.into(),
            self_track_id: None,
            self_ident: None,
            self_host: None,
            channels: HashMap::new(),
            pending_joins: HashMap::new(),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.channels.clear();
        self.pending_joins.clear();
    }

    /// `true` if `channel` is `Joining`/`Joined` or has live `Bot::join` waiters (used to stop a
    /// `Breakout` from claiming a room someone else already holds).
    pub(crate) fn is_claimed(&self, channel: &str) -> bool {
        let in_use = self.channel(channel).is_some_and(|c| {
            matches!(c.phase, ChannelPhase::Joining { .. } | ChannelPhase::Joined { .. })
        });
        in_use
            || self
                .pending_joins
                .get(&self.casemap.fold(channel))
                .is_some_and(|p| p.tx.receiver_count() > 0)
    }

    /// Registers a `Bot::join` waiter for `channel`, coalescing onto any in-flight join.
    ///
    /// Returns the outcome receiver and whether the caller must send `JOIN` (`false` when another
    /// live waiter already sent it, or when the self `JOIN` already arrived and only `366` is
    /// pending). Fails with `Error::InvalidInput` instead of coalescing onto a live `exclusive`
    /// (`Breakout`) claim, whose room will be torn down when the `Breakout` drops.
    pub(crate) fn await_join(
        &mut self,
        channel: &str,
        exclusive: bool,
    ) -> Result<(watch::Receiver<Option<JoinOutcome>>, bool), Error> {
        let key = self.casemap.fold(channel);
        if let Some(pending) = self
            .pending_joins
            .get(&key)
            .filter(|p| p.tx.receiver_count() > 0)
        {
            if pending.exclusive {
                return Err(Error::InvalidInput(
                    "channel is being claimed by a Breakout",
                ));
            }
            return Ok((pending.tx.subscribe(), false));
        }
        let send_join = !matches!(
            self.channel(channel).map(|c| &c.phase),
            Some(ChannelPhase::Joining { .. })
        );
        let (tx, rx) = watch::channel(None);
        self.pending_joins.insert(
            key,
            PendingJoin {
                name: channel.to_owned(),
                tx,
                exclusive,
            },
        );
        Ok((rx, send_join))
    }

    /// Resolves pending `Bot::join` waiters for `channel` with a join-refusal numeric.
    pub(crate) fn fail_join(&mut self, channel: &str, code: u16, message: String) {
        if let Some(pending) = self.pending_joins.remove(&self.casemap.fold(channel)) {
            pending.tx.send_replace(Some(JoinOutcome::Failed {
                channel: channel.to_owned(),
                code,
                message,
            }));
        }
    }

    /// Marks `channel` `Parting` once the bot enqueues its own `PART`, so a concurrent
    /// `Bot::join` rejoins instead of returning a `Room` that is about to be left.
    pub(crate) fn mark_parting(&mut self, channel: &str, evict_on_leave: bool) {
        let Some(chan) = self.channels.get_mut(&self.casemap.fold(channel)) else {
            return;
        };
        let members = match &mut chan.phase {
            ChannelPhase::Joining { staged } => std::mem::take(staged),
            ChannelPhase::Joined { members, .. } => std::mem::take(members),
            ChannelPhase::Parting { .. } | ChannelPhase::Left { .. } => return,
        };
        chan.phase = ChannelPhase::Parting {
            members,
            evict_on_leave,
        };
    }

    /// Looks up `channel` (case-folded under the active [`CaseMap`]) in any phase.
    pub(crate) fn channel(&self, channel: &str) -> Option<&ChannelEntry> {
        self.channels.get(&self.casemap.fold(channel))
    }

    /// Looks up `channel` only while the bot is joining or joined (not `Left`).
    pub(crate) fn active_channel(&self, channel: &str) -> Option<&ChannelEntry> {
        self.channel(channel).filter(|c| c.is_active())
    }

    fn active_channel_mut(&mut self, channel: &str) -> Option<&mut ChannelEntry> {
        let key = self.casemap.fold(channel);
        self.channels.get_mut(&key).filter(|c| c.is_active())
    }

    /// The bot's own [`Occupant`] in `chan`'s best-known roster.
    pub(crate) fn self_occupant<'a>(&'a self, chan: &'a ChannelEntry) -> Option<&'a Occupant> {
        match self.self_track_id {
            Some(self_id) => chan.roster()?.get(&self_id),
            None => chan.member_by_nick(&self.current_nick),
        }
    }

    /// Single source of truth for whether the bot may act in `channel`:
    /// - any `need`: `Left`, or a synced roster without the bot, -> `Error::BotLeftRoom`;
    /// - `BotPresence::Op` additionally rejects untracked (never joined) channels with
    ///   `Error::BotLeftRoom` and a synced roster where the bot lacks `+o` with
    ///   `Error::NotChannelOp`.
    pub(crate) fn check_bot_in_channel(&self, channel: &str, need: BotPresence) -> Result<(), Error> {
        let not_in_channel = || Error::BotLeftRoom {
            channel: channel.to_owned(),
            reason: None,
        };
        let Some(chan) = self.channel(channel) else {
            return match need {
                BotPresence::Tracked => Ok(()),
                BotPresence::Op => Err(not_in_channel()),
            };
        };
        if let Some(err) = chan.left_error() {
            return Err(err);
        }
        if chan.members().is_some() {
            let self_occ = self.self_occupant(chan).ok_or_else(not_in_channel)?;
            if need == BotPresence::Op && !self.occupant_is_op(self_occ) {
                return Err(Error::NotChannelOp {
                    channel: channel.to_owned(),
                });
            }
        }
        Ok(())
    }

    /// Returns `true` if `target` starts with one of the server's `005 CHANTYPES` prefixes.
    pub(crate) fn is_channel_name(&self, target: &str) -> bool {
        target
            .chars()
            .next()
            .is_some_and(|c| self.chan_types.contains(&c))
    }

    /// Returns `"user@host"` once both ident and host have been observed on the wire.
    pub(crate) fn self_user_host(&self) -> Option<String> {
        match (&self.self_ident, &self.self_host) {
            (Some(u), Some(h)) => Some(format!("{u}@{h}")),
            _ => None,
        }
    }

    /// Exact or conservatively reserved byte length of `<user>@<host>` for 512-byte wire budgeting.
    pub(crate) fn relayed_user_host_len(&self) -> usize {
        match (&self.self_ident, &self.self_host) {
            (Some(u), Some(h)) => u.len() + 1 + h.len(),
            (None, Some(h)) => DEFAULT_IDENT_RESERVE + 1 + h.len(),
            _ => DEFAULT_USER_HOST_RESERVE,
        }
    }

    /// Computes the maximum safe payload byte budget for an outbound `PRIVMSG` / `NOTICE` / `TOPIC`
    /// so the server-relayed `:<nick>!<user>@<host> <CMD> <target> :<payload>\r\n` never exceeds 512 bytes.
    pub(crate) fn payload_budget(
        &self,
        cmd_name: &str,
        target: &str,
        max_payload_bytes: usize,
        is_action: bool,
    ) -> usize {
        let cmd_overhead = 2 + cmd_name.len(); // " <CMD> "
        let overhead = 1
            + self.current_nick.len()
            + 1
            + self.relayed_user_host_len()
            + cmd_overhead
            + target.len()
            + 4; // " :" + "\r\n"
        let wire_max = 512usize.saturating_sub(overhead);
        let mut budget = max_payload_bytes.min(wire_max);
        if is_action {
            budget = budget.saturating_sub(9); // "\x01ACTION " + "\x01"
        }
        budget.max(1)
    }

    pub(crate) fn observe_self_prefix(&mut self, prefix: Option<&Prefix>) {
        if let Some(Prefix::Nickname(nick, user, host)) = prefix {
            if self.casemap.str_eq(nick, &self.current_nick) {
                if !user.is_empty() {
                    self.self_ident = Some(user.clone());
                }
                if !host.is_empty() {
                    self.self_host = Some(host.clone());
                }
            }
        }
    }

    pub(crate) fn handle_welcome(&mut self, args: &[String]) {
        if let Some(nick) = args.first() {
            if !nick.is_empty() && !self.casemap.str_eq(&self.current_nick, nick) {
                self.current_nick = nick.clone();
                self.self_track_id = None;
            } else if !nick.is_empty() {
                self.current_nick = nick.clone();
            }
        }
        // Check trailing welcome text for `<nick>!<user>@<host>`:
        if let Some(trailing) = args.last() {
            for word in trailing.split_whitespace().rev() {
                let word = word.trim_end_matches(['.', '!', ',']);
                if let Some((n, uh)) = word.split_once('!') {
                    if let Some((u, h)) = uh.split_once('@') {
                        if self.casemap.str_eq(n, &self.current_nick)
                            && !u.is_empty()
                            && !h.is_empty()
                        {
                            self.self_ident = Some(u.to_owned());
                            self.self_host = Some(h.to_owned());
                            break;
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn handle_host_hidden(&mut self, args: &[String]) {
        // `396 <nick> <vhost> :is now your displayed host`
        if args.len() >= 2 && !args[1].is_empty() {
            self.self_host = Some(args[1].clone());
        }
    }

    /// Applies `005 RPL_ISUPPORT` parameters. Returns `Some(new_casemap)` if `CASEMAPPING` changed
    /// so the caller can re-key `UserRegistry` and `InboxRegistry` in lockstep.
    pub(crate) fn apply_isupport(&mut self, args: &[String]) -> Option<CaseMap> {
        let mut changed_casemap = None;
        if args.len() <= 1 {
            return None;
        }
        let tokens = &args[1..];
        let end = if tokens.last().is_some_and(|t| t.contains(' ')) {
            tokens.len() - 1
        } else {
            tokens.len()
        };

        for token in &tokens[..end] {
            if let Some(negated) = token.strip_prefix('-') {
                // Negated ISUPPORT parameter resets to default:
                if negated.eq_ignore_ascii_case("TARGMAX") {
                    self.targmax_kick = 1;
                } else if negated.eq_ignore_ascii_case("MODES") {
                    self.max_modes = 4;
                }
                continue;
            }

            let (key, val_opt) = match token.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (token.as_str(), None),
            };

            if key.eq_ignore_ascii_case("CASEMAPPING") {
                if let Some(val) = val_opt {
                    if let Some(new_cm) = CaseMap::from_isupport(val) {
                        if new_cm != self.casemap {
                            self.rekey_casemap(new_cm);
                            changed_casemap = Some(new_cm);
                        }
                    }
                }
            } else if key.eq_ignore_ascii_case("CHANTYPES") {
                if let Some(val) = val_opt {
                    self.chan_types = val.chars().collect();
                }
            } else if key.eq_ignore_ascii_case("PREFIX") {
                if let Some(val) = val_opt {
                    if val.is_empty() {
                        self.status_prefixes.clear();
                    } else if let Some(rest) = val.strip_prefix('(') {
                        if let Some((modes, syms)) = rest.split_once(')') {
                            let m_chars: Vec<char> = modes.chars().collect();
                            let s_chars: Vec<char> = syms.chars().collect();
                            if m_chars.len() == s_chars.len() {
                                self.status_prefixes =
                                    m_chars.into_iter().zip(s_chars).collect();
                            }
                        }
                    }
                }
            } else if key.eq_ignore_ascii_case("CHANMODES") {
                if let Some(val) = val_opt {
                    let parts: Vec<&str> = val.split(',').collect();
                    if parts.len() >= 4 {
                        self.chan_modes = ChanModes {
                            list_a: parts[0].to_owned(),
                            param_always_b: parts[1].to_owned(),
                            param_set_c: parts[2].to_owned(),
                        };
                    }
                }
            } else if key.eq_ignore_ascii_case("MODES") {
                match val_opt {
                    Some("") => self.max_modes = 12,
                    Some(v) => {
                        if let Ok(n) = v.parse::<usize>() {
                            if n >= 1 {
                                self.max_modes = n;
                            }
                        }
                    }
                    None => {}
                }
            } else if key.eq_ignore_ascii_case("TARGMAX") {
                self.targmax_kick = 1;
                if let Some(val) = val_opt {
                    for pair in val.split(',').filter(|s| !s.is_empty()) {
                        if let Some((cmd, limit_str)) = pair.split_once(':') {
                            if cmd.eq_ignore_ascii_case("KICK") {
                                if limit_str.is_empty() {
                                    // Empty limit means unlimited targets; cap at 8 to stay well within 512B.
                                    self.targmax_kick = 8;
                                } else if let Ok(n) = limit_str.parse::<usize>() {
                                    if n >= 1 {
                                        self.targmax_kick = n;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        changed_casemap
    }

    /// Rebuilds all `channels` `FoldedStr` keys under `new_map` (rosters are keyed by immutable
    /// `UserTrack::id`, so no member re-keying is needed).
    pub(crate) fn rekey_casemap(&mut self, new_map: CaseMap) {
        self.casemap = new_map;
        self.shared_casemap.store(new_map);
        let old_channels = std::mem::take(&mut self.channels);
        self.channels = old_channels
            .into_values()
            .map(|chan| (new_map.fold(&chan.name), chan))
            .collect();
        let old_pending = std::mem::take(&mut self.pending_joins);
        self.pending_joins = old_pending
            .into_values()
            .map(|pending| (new_map.fold(&pending.name), pending))
            .collect();
    }

    pub(crate) fn is_status_symbol(&self, sym: char) -> bool {
        self.status_prefixes.iter().any(|&(_, s)| s == sym)
    }

    pub(crate) fn mode_for_symbol(&self, sym: char) -> Option<char> {
        self.status_prefixes
            .iter()
            .find_map(|&(m, s)| (s == sym).then_some(m))
    }

    pub(crate) fn symbol_for_mode(&self, mode: char) -> Option<char> {
        self.status_prefixes
            .iter()
            .find_map(|&(m, s)| (m == mode).then_some(s))
    }

    pub(crate) fn occupant_is_op(&self, occ: &Occupant) -> bool {
        occ.prefixes.iter().any(|&sym| {
            matches!(sym, '@' | '&' | '~')
                || matches!(self.mode_for_symbol(sym), Some('o' | 'a' | 'q'))
        })
    }

    pub(crate) fn occupant_is_voiced(&self, occ: &Occupant) -> bool {
        occ.prefixes
            .iter()
            .any(|&sym| sym == '+' || self.mode_for_symbol(sym) == Some('v'))
    }

    fn sort_and_dedup_prefixes(&self, prefixes: &mut Vec<char>) {
        prefixes.sort_by_key(|&sym| {
            self.status_prefixes
                .iter()
                .position(|&(_, s)| s == sym)
                .unwrap_or(usize::MAX)
        });
        prefixes.dedup();
    }

    pub(crate) fn handle_join(
        &mut self,
        channel: &str,
        nick: &str,
        track: Arc<UserTrack>,
        prefix: Option<&Prefix>,
    ) {
        self.observe_self_prefix(prefix);
        let chan_key = self.casemap.fold(channel);
        let is_self = self.casemap.str_eq(nick, &self.current_nick);

        if is_self {
            self.self_track_id = Some(track.id);
            let chan = self
                .channels
                .entry(chan_key)
                .or_insert_with(|| ChannelEntry::joining(channel.to_owned()));
            chan.name = channel.to_owned();
            if !chan.is_active() {
                *chan = ChannelEntry::joining(channel.to_owned());
            }
            chan.for_each_roster_mut(|map| {
                map.entry(track.id).or_insert_with(|| Occupant {
                    track: Arc::clone(&track),
                    prefixes: Vec::new(),
                });
            });
        } else if let Some(chan) = self.active_channel_mut(channel) {
            chan.for_each_roster_mut(|map| {
                map.insert(
                    track.id,
                    Occupant {
                        track: Arc::clone(&track),
                        prefixes: Vec::new(),
                    },
                );
            });
        }
    }

    /// Removes `track_id` from `channel` (or marks the channel `Left` if `nick` is the bot).
    /// When the bot completes a `Breakout` teardown `PART` (`Parting { evict_on_leave: true }`)
    /// and `watched` is `false` (no channel-scoped `Inbox` needs the `Left` state), the entry is
    /// evicted instead so ephemeral `Breakout` channels do not accumulate forever.
    /// Returns `true` if the bot itself left the channel.
    pub(crate) fn handle_part_or_kick(
        &mut self,
        channel: &str,
        nick: &str,
        track_id: u64,
        reason: Option<String>,
        at_seq: u64,
        watched: bool,
    ) -> bool {
        if self.casemap.str_eq(nick, &self.current_nick) {
            let chan_key = self.casemap.fold(channel);
            let chan = self
                .channels
                .entry(chan_key.clone())
                .or_insert_with(|| ChannelEntry::joining(channel.to_owned()));
            let was_joining = matches!(chan.phase, ChannelPhase::Joining { .. });
            if !watched
                && matches!(
                    chan.phase,
                    ChannelPhase::Parting {
                        evict_on_leave: true,
                        ..
                    }
                )
            {
                self.channels.remove(&chan_key);
                return true;
            }
            chan.topic = None;
            chan.phase = ChannelPhase::Left {
                reason: reason.clone(),
                at_seq,
            };
            let name = chan.name.clone();
            // Kicked between our self `JOIN` and `366`: resolve `Bot::join` waiters now instead
            // of letting them run out `handshake_timeout`.
            if was_joining {
                if let Some(pending) = self.pending_joins.remove(&chan_key) {
                    pending.tx.send_replace(Some(JoinOutcome::Left {
                        channel: name,
                        reason,
                    }));
                }
            }
            true
        } else {
            if let Some(chan) = self.active_channel_mut(channel) {
                chan.for_each_roster_mut(|map| {
                    map.remove(&track_id);
                });
            }
            false
        }
    }

    /// Removes `track_id` from all joined channels and returns the channels they occupied.
    pub(crate) fn handle_quit(&mut self, track_id: u64) -> Vec<String> {
        let mut occupied = Vec::new();
        for chan in self.channels.values_mut() {
            let mut removed = false;
            chan.for_each_roster_mut(|map| {
                removed |= map.remove(&track_id).is_some();
            });
            if removed {
                occupied.push(chan.name.clone());
            }
        }
        occupied.sort();
        occupied
    }

    /// Updates `current_nick` if the bot itself renamed, and returns the list of channels
    /// `track_id` currently occupies (`Occupant` reads live `UserTrack::nick()`, so no map
    /// mutations are needed).
    pub(crate) fn handle_nick(
        &mut self,
        track_id: u64,
        old_nick: &str,
        new_nick: &str,
        prefix: Option<&Prefix>,
    ) -> Vec<String> {
        self.observe_self_prefix(prefix);
        if self.casemap.str_eq(old_nick, &self.current_nick) {
            self.current_nick = new_nick.to_owned();
            self.self_track_id = Some(track_id);
        }
        let mut occupied: Vec<String> = self
            .channels
            .values()
            .filter(|chan| chan.tracks_member(track_id))
            .map(|chan| chan.name.clone())
            .collect();
        occupied.sort();
        occupied
    }

    pub(crate) fn handle_topic(&mut self, channel: &str, topic: Option<String>) {
        if let Some(chan) = self.active_channel_mut(channel) {
            chan.topic = topic.filter(|t| !t.is_empty());
        }
    }

    pub(crate) fn handle_namreply(&mut self, args: &[String], users: &UserRegistry) {
        // `353 <client> ("="|"*"|"@") <channel> :<entries>` (or `<client> <channel> :<entries>`)
        let (channel, entries) = if args.len() >= 4 {
            (&args[2], &args[3])
        } else if args.len() == 3 {
            (&args[1], &args[2])
        } else {
            return;
        };

        if self.active_channel(channel).is_none() {
            return;
        }

        let mut parsed = Vec::new();
        for raw_entry in entries.split_whitespace() {
            let mut prefixes = Vec::new();
            let mut rest = raw_entry;
            while let Some(ch) = rest.chars().next() {
                if self.is_status_symbol(ch) {
                    prefixes.push(ch);
                    rest = &rest[ch.len_utf8()..];
                } else {
                    break;
                }
            }
            self.sort_and_dedup_prefixes(&mut prefixes);

            // Strip optional `userhost-in-names` `!user@host` tail:
            let (nick, uh_opt) = match rest.split_once('!') {
                Some((n, uh)) => (n, Some(uh)),
                None => (rest, None),
            };
            if nick.is_empty() {
                continue;
            }
            if self.casemap.str_eq(nick, &self.current_nick) {
                if let Some((u, h)) = uh_opt.and_then(|uh| uh.split_once('@')) {
                    if !u.is_empty() {
                        self.self_ident = Some(u.to_owned());
                    }
                    if !h.is_empty() {
                        self.self_host = Some(h.to_owned());
                    }
                }
            }

            let track = users.get_or_create(nick);
            if self.casemap.str_eq(nick, &self.current_nick) {
                self.self_track_id = Some(track.id);
            }
            parsed.push(Occupant { track, prefixes });
        }

        if let Some(chan) = self.active_channel_mut(channel) {
            let staged = match &mut chan.phase {
                ChannelPhase::Joining { staged } => staged,
                ChannelPhase::Joined { resync, .. } => resync.get_or_insert_with(HashMap::new),
                ChannelPhase::Parting { .. } | ChannelPhase::Left { .. } => return,
            };
            for occ in parsed {
                staged.insert(occ.track.id, occ);
            }
        }
    }

    /// Commits staged `353` entries atomically on `366 RPL_ENDOFNAMES` (`Joining` -> `Joined`, or
    /// swapping in a `Joined` resync) and returns the canonical channel name if active.
    pub(crate) fn handle_endofnames(&mut self, args: &[String]) -> Option<String> {
        let channel = if args.len() >= 2 {
            &args[1]
        } else {
            args.first()?
        };
        let key = self.casemap.fold(channel);
        let chan = self.channels.get_mut(&key).filter(|c| c.is_active())?;
        match &mut chan.phase {
            ChannelPhase::Joining { staged } => {
                let members = std::mem::take(staged);
                chan.phase = ChannelPhase::Joined {
                    members,
                    resync: None,
                };
            }
            ChannelPhase::Joined { members, resync } => {
                if let Some(fresh) = resync.take() {
                    *members = fresh;
                }
            }
            ChannelPhase::Parting { .. } | ChannelPhase::Left { .. } => {}
        }
        let name = chan.name.clone();
        if matches!(chan.phase, ChannelPhase::Joined { .. }) {
            if let Some(pending) = self.pending_joins.remove(&key) {
                pending.tx.send_replace(Some(JoinOutcome::Joined { name: name.clone() }));
            }
        }
        Some(name)
    }

    /// Applies a parameterized channel `MODE` sequence (`["+klo", "secret", "50", "Alice", "-v", "Bob"]`)
    /// respecting `005 PREFIX` and `005 CHANMODES` arity rules.
    pub(crate) fn apply_channel_mode_tokens<S: AsRef<str>>(
        &mut self,
        channel: &str,
        tokens: &[S],
        users: &UserRegistry,
    ) {
        if self.active_channel(channel).is_none() {
            return;
        }

        let mut updates: Vec<(u64, bool, char)> = Vec::new();
        let mut idx = 0;
        while idx < tokens.len() {
            let mode_word = tokens[idx].as_ref();
            idx += 1;
            if mode_word.is_empty() {
                continue;
            }
            let mut adding = true;
            for ch in mode_word.chars() {
                match ch {
                    '+' => adding = true,
                    '-' => adding = false,
                    _ => {
                        if let Some(sym) = self.symbol_for_mode(ch) {
                            if idx < tokens.len() {
                                let target_nick = tokens[idx].as_ref();
                                idx += 1;
                                if let Some(track) = users.get_existing(target_nick) {
                                    updates.push((track.id, adding, sym));
                                }
                            }
                        } else if self.chan_modes.list_a.contains(ch)
                            || self.chan_modes.param_always_b.contains(ch)
                        {
                            if idx < tokens.len() {
                                idx += 1;
                            }
                        } else if self.chan_modes.param_set_c.contains(ch) {
                            if adding && idx < tokens.len() {
                                idx += 1;
                            }
                        } else {
                            // Category D or unknown flag mode: 0 parameters.
                        }
                    }
                }
            }
        }

        let order: Vec<char> = self.status_prefixes.iter().map(|&(_, s)| s).collect();
        if let Some(chan) = self.active_channel_mut(channel) {
            for (target_id, adding, sym) in updates {
                chan.for_each_roster_mut(|map| {
                    if let Some(occ) = map.get_mut(&target_id) {
                        if adding {
                            if !occ.prefixes.contains(&sym) {
                                occ.prefixes.push(sym);
                            }
                        } else {
                            occ.prefixes.retain(|&s| s != sym);
                        }
                        occ.prefixes.sort_by_key(|&s| {
                            order.iter().position(|&x| x == s).unwrap_or(usize::MAX)
                        });
                    }
                });
            }
        }
    }

    /// Extracts channel `MODE` tokens from an inbound `irc::proto::Message` (`ChannelMODE`,
    /// `Raw("MODE")`, or `324 RPL_CHANNELMODEIS`) and applies them via `apply_channel_mode_tokens`.
    pub(crate) fn apply_mode_message(&mut self, msg: &Message, users: &UserRegistry) {
        match &msg.command {
            Command::ChannelMODE(chan, modes) => {
                if self.is_channel_name(chan) && !modes.is_empty() {
                    let mut flags = String::new();
                    let mut tokens = Vec::with_capacity(1 + modes.len());
                    tokens.push(String::new());
                    for m in modes {
                        flags.push_str(&m.flag());
                        if let Some(arg) = m.arg() {
                            tokens.push(arg.to_owned());
                        }
                    }
                    tokens[0] = flags;
                    self.apply_channel_mode_tokens(chan, &tokens, users);
                }
            }
            Command::Raw(cmd, args) if cmd.eq_ignore_ascii_case("MODE") && args.len() >= 2 => {
                let chan = &args[0];
                if self.is_channel_name(chan) {
                    self.apply_channel_mode_tokens(chan, &args[1..], users);
                }
            }
            Command::Response(Response::RPL_CHANNELMODEIS, args) if args.len() >= 3 => {
                // `324 <client> <channel> <modes> [mode_params...]`
                let chan = &args[1];
                if self.is_channel_name(chan) {
                    self.apply_channel_mode_tokens(chan, &args[2..], users);
                }
            }
            _ => {}
        }
    }
}

/// Parses an inbound IRC wire line into [`Message`] using the same `irc-proto` parser as live
/// `ClientStream` frames.
pub(crate) fn parse_irc_message(
    raw_line: &str,
) -> std::result::Result<Message, irc::proto::error::ProtocolError> {
    raw_line.parse()
}

#[cfg(test)]
#[path = "state_test.rs"]
mod test;
