use std::collections::HashMap;
use std::sync::Arc;

use irc::proto::{Command, Message, Prefix, Response};

use crate::casemap::{CaseMap, FoldedStr};
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
    pub(crate) nick: String,
    /// Ordered highest-to-lowest according to `WorldState::status_prefixes` (e.g. `['@', '+']`).
    pub(crate) prefixes: Vec<char>,
}

#[derive(Clone, Debug)]
struct StagedName {
    track: Arc<UserTrack>,
    nick: String,
    prefixes: Vec<char>,
}

#[derive(Clone, Debug)]
pub(crate) struct ChannelState {
    pub(crate) name: String,
    pub(crate) topic: Option<String>,
    pub(crate) names_synced: bool,
    pub(crate) members: HashMap<FoldedStr, Occupant>,
    staging_names: Vec<StagedName>,
    has_staged_batch: bool,
}

impl ChannelState {
    fn new(name: String) -> Self {
        Self {
            name,
            topic: None,
            names_synced: false,
            members: HashMap::new(),
            staging_names: Vec::new(),
            has_staged_batch: false,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LostChannel {
    pub(crate) name: String,
    pub(crate) reason: Option<String>,
}

pub(crate) struct WorldState {
    pub(crate) casemap: CaseMap,
    pub(crate) chan_types: Vec<char>,
    /// `(mode_char, prefix_symbol)` ordered highest to lowest privilege.
    pub(crate) status_prefixes: Vec<(char, char)>,
    pub(crate) chan_modes: ChanModes,
    pub(crate) max_modes: usize,
    pub(crate) targmax_kick: usize,
    pub(crate) current_nick: String,
    pub(crate) self_ident: Option<String>,
    pub(crate) self_host: Option<String>,
    pub(crate) channels: HashMap<FoldedStr, ChannelState>,
    pub(crate) lost_channels: HashMap<FoldedStr, LostChannel>,
}

impl WorldState {
    pub(crate) fn new(initial_nick: impl Into<String>) -> Self {
        Self {
            casemap: CaseMap::default(),
            chan_types: vec!['#', '&'],
            status_prefixes: vec![('o', '@'), ('v', '+')],
            chan_modes: ChanModes::default(),
            max_modes: 4,
            targmax_kick: 1,
            current_nick: initial_nick.into(),
            self_ident: None,
            self_host: None,
            channels: HashMap::new(),
            lost_channels: HashMap::new(),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.channels.clear();
        self.lost_channels.clear();
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
            if !nick.is_empty() {
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

    /// Rebuilds all `channels` and occupant `FoldedStr` keys under `new_map`.
    pub(crate) fn rekey_casemap(&mut self, new_map: CaseMap) {
        self.casemap = new_map;
        let old_channels = std::mem::take(&mut self.channels);
        let mut new_channels = HashMap::with_capacity(old_channels.len());
        for (_, mut chan) in old_channels {
            let old_members = std::mem::take(&mut chan.members);
            let mut new_members = HashMap::with_capacity(old_members.len());
            for (_, occ) in old_members {
                new_members.insert(new_map.fold(&occ.nick), occ);
            }
            chan.members = new_members;
            new_channels.insert(new_map.fold(&chan.name), chan);
        }
        self.channels = new_channels;
        let old_lost = std::mem::take(&mut self.lost_channels);
        let mut new_lost = HashMap::with_capacity(old_lost.len());
        for (_, lost) in old_lost {
            new_lost.insert(new_map.fold(&lost.name), lost);
        }
        self.lost_channels = new_lost;
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
            self.lost_channels.remove(&chan_key);
            let chan = self
                .channels
                .entry(chan_key)
                .or_insert_with(|| ChannelState::new(channel.to_owned()));
            chan.name = channel.to_owned();
            let user_key = self.casemap.fold(nick);
            chan.members.entry(user_key).or_insert(Occupant {
                track,
                nick: nick.to_owned(),
                prefixes: Vec::new(),
            });
        } else if let Some(chan) = self.channels.get_mut(&chan_key) {
            let user_key = self.casemap.fold(nick);
            chan.members.insert(
                user_key,
                Occupant {
                    track,
                    nick: nick.to_owned(),
                    prefixes: Vec::new(),
                },
            );
        }
    }

    /// Removes `nick` from `channel` (or removes the entire `ChannelState` if `nick` is the bot).
    /// Returns `true` if the bot itself left the channel.
    pub(crate) fn handle_part_or_kick(
        &mut self,
        channel: &str,
        nick: &str,
        reason: Option<String>,
    ) -> bool {
        let chan_key = self.casemap.fold(channel);
        if self.casemap.str_eq(nick, &self.current_nick) {
            let canonical = self
                .channels
                .remove(&chan_key)
                .map(|c| c.name)
                .unwrap_or_else(|| channel.to_owned());
            self.lost_channels.insert(
                chan_key,
                LostChannel {
                    name: canonical,
                    reason,
                },
            );
            true
        } else {
            if let Some(chan) = self.channels.get_mut(&chan_key) {
                let user_key = self.casemap.fold(nick);
                chan.members.remove(&user_key);
            }
            false
        }
    }

    /// Returns the list of channel names `nick` currently occupies in `WorldState`.
    pub(crate) fn channels_for_nick(&self, nick: &str) -> Vec<String> {
        let user_key = self.casemap.fold(nick);
        let mut out: Vec<String> = self
            .channels
            .values()
            .filter(|c| c.members.contains_key(&user_key))
            .map(|c| c.name.clone())
            .collect();
        out.sort();
        out
    }

    /// Removes `nick` from all joined channels and returns the channels they occupied.
    pub(crate) fn handle_quit(&mut self, nick: &str) -> Vec<String> {
        let user_key = self.casemap.fold(nick);
        let mut occupied = Vec::new();
        for chan in self.channels.values_mut() {
            if chan.members.remove(&user_key).is_some() {
                occupied.push(chan.name.clone());
            }
        }
        occupied.sort();
        occupied
    }

    /// Updates `old_nick -> new_nick` across `current_nick` and all joined channels, returning
    /// the list of channels `old_nick` occupied.
    pub(crate) fn handle_nick(
        &mut self,
        old_nick: &str,
        new_nick: &str,
        prefix: Option<&Prefix>,
    ) -> Vec<String> {
        self.observe_self_prefix(prefix);
        if self.casemap.str_eq(old_nick, &self.current_nick) {
            self.current_nick = new_nick.to_owned();
        }
        let old_key = self.casemap.fold(old_nick);
        let new_key = self.casemap.fold(new_nick);
        let mut occupied = Vec::new();
        for chan in self.channels.values_mut() {
            if let Some(mut occ) = chan.members.remove(&old_key) {
                occ.nick = new_nick.to_owned();
                occupied.push(chan.name.clone());
                chan.members.insert(new_key.clone(), occ);
            }
        }
        occupied.sort();
        occupied
    }

    pub(crate) fn handle_topic(&mut self, channel: &str, topic: Option<String>) {
        let chan_key = self.casemap.fold(channel);
        if let Some(chan) = self.channels.get_mut(&chan_key) {
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

        let chan_key = self.casemap.fold(channel);
        if !self.channels.contains_key(&chan_key) {
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
            parsed.push(StagedName {
                track,
                nick: nick.to_owned(),
                prefixes,
            });
        }

        if let Some(chan) = self.channels.get_mut(&chan_key) {
            chan.has_staged_batch = true;
            chan.staging_names.extend(parsed);
        }
    }

    /// Commits staged `353` entries atomically on `366 RPL_ENDOFNAMES` and returns the canonical
    /// channel name if tracked.
    pub(crate) fn handle_endofnames(&mut self, args: &[String]) -> Option<String> {
        let channel = if args.len() >= 2 {
            &args[1]
        } else {
            args.first()?
        };
        let chan_key = self.casemap.fold(channel);
        let casemap = self.casemap;
        let chan = self.channels.get_mut(&chan_key)?;

        if chan.has_staged_batch {
            let staged = std::mem::take(&mut chan.staging_names);
            chan.has_staged_batch = false;
            let mut new_members = HashMap::with_capacity(staged.len());
            for item in staged {
                new_members.insert(
                    casemap.fold(&item.nick),
                    Occupant {
                        track: item.track,
                        nick: item.nick,
                        prefixes: item.prefixes,
                    },
                );
            }
            chan.members = new_members;
        }
        chan.names_synced = true;
        Some(chan.name.clone())
    }

    /// Applies a parameterized channel `MODE` sequence (`["+klo", "secret", "50", "Alice", "-v", "Bob"]`)
    /// respecting `005 PREFIX` and `005 CHANMODES` arity rules.
    pub(crate) fn apply_channel_mode_tokens<S: AsRef<str>>(&mut self, channel: &str, tokens: &[S]) {
        let chan_key = self.casemap.fold(channel);
        if !self.channels.contains_key(&chan_key) {
            return;
        }

        let mut updates: Vec<(FoldedStr, bool, char)> = Vec::new();
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
                                updates.push((self.casemap.fold(target_nick), adding, sym));
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

        for (target_key, adding, sym) in updates {
            if let Some(chan) = self.channels.get_mut(&chan_key) {
                if let Some(occ) = chan.members.get_mut(&target_key) {
                    if adding {
                        if !occ.prefixes.contains(&sym) {
                            occ.prefixes.push(sym);
                        }
                    } else {
                        occ.prefixes.retain(|&s| s != sym);
                    }
                }
            }
            // Keep prefixes sorted by `status_prefixes` privilege rank:
            let order: Vec<char> = self.status_prefixes.iter().map(|&(_, s)| s).collect();
            if let Some(chan) = self.channels.get_mut(&chan_key) {
                if let Some(occ) = chan.members.get_mut(&target_key) {
                    occ.prefixes.sort_by_key(|&s| {
                        order.iter().position(|&x| x == s).unwrap_or(usize::MAX)
                    });
                }
            }
        }
    }

    /// Extracts channel `MODE` tokens from an `irc::proto::Message` (`ChannelMODE` or `Raw("MODE")`)
    /// and applies them via `apply_channel_mode_tokens`.
    pub(crate) fn apply_mode_message(&mut self, msg: &Message) {
        match &msg.command {
            Command::ChannelMODE(chan, modes) => {
                if !self.is_channel_name(chan) {
                    return;
                }
                let mut flags = String::new();
                let mut args: Vec<String> = Vec::new();
                for m in modes {
                    flags.push_str(&m.flag());
                    if let Some(arg) = m.arg() {
                        args.push(arg.to_owned());
                    }
                }
                let mut tokens = Vec::with_capacity(1 + args.len());
                tokens.push(flags);
                tokens.extend(args);
                self.apply_channel_mode_tokens(chan, &tokens);
            }
            Command::Raw(cmd, args) if cmd.eq_ignore_ascii_case("MODE") && args.len() >= 2 => {
                let chan = &args[0];
                if self.is_channel_name(chan) {
                    self.apply_channel_mode_tokens(chan, &args[1..]);
                }
            }
            Command::Response(Response::RPL_CHANNELMODEIS, args) if args.len() >= 3 => {
                // `324 <client> <channel> <modes> [mode_params...]`
                let chan = &args[1];
                if self.is_channel_name(chan) {
                    self.apply_channel_mode_tokens(chan, &args[2..]);
                }
            }
            _ => {}
        }
    }
}

/// Parses an inbound IRC wire line into [`Message`], preserving all raw arguments on channel
/// `MODE` frames (`irc-proto`'s static `Mode::as_channel_modes` otherwise drops interleaved
/// mode blocks like `+klo k 50 Alice -v Bob` and mis-assigns custom `005 CHANMODES` parameters).
pub(crate) fn parse_irc_message(
    raw_line: &str,
) -> std::result::Result<Message, irc::proto::error::ProtocolError> {
    let mut msg = raw_line.parse::<Message>()?;
    if matches!(msg.command, Command::ChannelMODE(..)) {
        if let Some(raw_args) = extract_raw_command_args(raw_line, "MODE") {
            if raw_args.len() >= 2 {
                msg.command = Command::Raw("MODE".into(), raw_args);
            }
        }
    }
    Ok(msg)
}

fn extract_raw_command_args(raw_line: &str, expected_cmd: &str) -> Option<Vec<String>> {
    let mut s = raw_line.trim_end_matches(['\r', '\n']);
    if s.starts_with('@') {
        s = s.split_once(' ')?.1.trim_start_matches(' ');
    }
    if s.starts_with(':') {
        s = s.split_once(' ')?.1.trim_start_matches(' ');
    }
    let (cmd, mut rest) = match s.split_once(' ') {
        Some((c, r)) => (c, r),
        None => (s, ""),
    };
    if !cmd.eq_ignore_ascii_case(expected_cmd) {
        return None;
    }
    let mut args = Vec::new();
    while !rest.is_empty() {
        rest = rest.trim_start_matches(' ');
        if rest.is_empty() {
            break;
        }
        if let Some(trailing) = rest.strip_prefix(':') {
            args.push(trailing.to_owned());
            break;
        }
        match rest.split_once(' ') {
            Some((tok, tail)) => {
                args.push(tok.to_owned());
                rest = tail;
            }
            None => {
                args.push(rest.to_owned());
                break;
            }
        }
    }
    Some(args)
}

#[cfg(test)]
#[path = "state_test.rs"]
mod test;
