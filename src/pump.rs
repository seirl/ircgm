use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use irc::proto::{CapSubCommand, Command, Message, Prefix};
use irc_colors::Text;
#[cfg(any(test, feature = "testing"))]
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::core::BotCore;
use crate::error::DisconnectReason;
use crate::message::{
    parse_privmsg_body, ChatMessage, Event, MessageKind, Target, WireEvent, WireKind, WireMeta,
};
use crate::user::{normalize_account, User};

/// Inbound feed of the in-memory [`crate::testing::TestServer`] transport.
#[cfg(any(test, feature = "testing"))]
pub(crate) enum PumpInput {
    Line(Box<Message>),
    Settle(oneshot::Sender<()>),
    Eof,
}

pub(crate) enum PumpSource {
    Irc(Box<irc::client::ClientStream>, Keepalive),
    #[cfg(any(test, feature = "testing"))]
    Test(mpsc::UnboundedReceiver<PumpInput>),
}

/// Idle-aware liveness tracker for live IRC streams: sends `PING` only after `ping_time` of
/// inbound silence, and treats any inbound frame within `ping_timeout` as proof of liveness.
pub(crate) struct Keepalive {
    ping_time: Duration,
    ping_timeout: Duration,
    awaiting_pong: bool,
    ping_seq: u64,
}

impl Keepalive {
    pub(crate) fn new(ping_time: Duration, ping_timeout: Duration) -> Self {
        Self {
            ping_time,
            ping_timeout,
            awaiting_pong: false,
            ping_seq: 0,
        }
    }

    async fn next_message(
        &mut self,
        stream: &mut irc::client::ClientStream,
        core: &BotCore,
    ) -> Result<Message, DisconnectReason> {
        loop {
            let wait = if self.awaiting_pong {
                self.ping_timeout
            } else {
                self.ping_time
            };
            match tokio::time::timeout(wait, stream.next()).await {
                Ok(Some(Ok(msg))) => {
                    self.awaiting_pong = false;
                    return Ok(msg);
                }
                Ok(Some(Err(err))) => return Err(DisconnectReason::Transport(err.to_string())),
                Ok(None) => return Err(DisconnectReason::Eof),
                Err(_) if !core.is_registered() => {}
                Err(_) if !self.awaiting_pong => {
                    self.awaiting_pong = true;
                    self.ping_seq += 1;
                    core.outbound
                        .enqueue_one(Command::PING(format!("ircgm-{}", self.ping_seq), None));
                }
                Err(_) => {
                    return Err(DisconnectReason::Transport(
                        "connection reset: no ping response".into(),
                    ));
                }
            }
        }
    }
}

pub(crate) struct RegistrationTracker {
    pub(crate) cap_pending: bool,
    pub(crate) motd_done: bool,
}

impl RegistrationTracker {
    fn is_ready(&self) -> bool {
        !self.cap_pending && self.motd_done
    }
}

fn is_services_pseudoclient(nick: &str) -> bool {
    const SERVICES: &[&str] = &[
        "NickServ", "ChanServ", "MemoServ", "OperServ", "HostServ", "BotServ", "Global", "SaslServ",
    ];
    SERVICES.iter().any(|s| nick.eq_ignore_ascii_case(s))
}

pub(crate) fn extract_numeric(cmd: &Command) -> Option<(u16, &[String])> {
    match cmd {
        Command::Response(resp, args) => Some((*resp as u16, args.as_slice())),
        Command::Raw(raw_cmd, args)
            if raw_cmd.len() == 3 && raw_cmd.bytes().all(|b| b.is_ascii_digit()) =>
        {
            let code = raw_cmd.parse::<u16>().ok()?;
            Some((code, args.as_slice()))
        }
        _ => None,
    }
}

struct ParsedTags {
    raw_account: Option<String>,
    is_bot: bool,
}

fn extract_tags(msg: &Message) -> ParsedTags {
    let mut out = ParsedTags {
        raw_account: None,
        is_bot: false,
    };
    let Some(tags) = &msg.tags else {
        return out;
    };
    for tag in tags {
        let k = tag.0.as_str();
        if k == "account" {
            if let Some(ref val) = tag.1 {
                out.raw_account = Some(val.clone());
            }
        } else if k == "bot" || k == "+bot" || k.ends_with("/bot") {
            out.is_bot = true;
        }
    }
    out
}

pub(crate) async fn run_pump(
    core: Arc<BotCore>,
    mut source: PumpSource,
    mut reg: RegistrationTracker,
) {
    if reg.is_ready() {
        core.mark_registered();
    }

    let mut seq: u64 = 0;
    let mut consecutive_ready: usize = 0;

    let end_reason = loop {
        if consecutive_ready >= 64 {
            consecutive_ready = 0;
            tokio::task::yield_now().await;
        }

        match &mut source {
            PumpSource::Irc(stream, keepalive) => {
                match keepalive.next_message(stream, &core).await {
                    Ok(msg) => {
                        consecutive_ready += 1;
                        seq += 1;
                        process_message(&core, msg, seq, &mut reg);
                    }
                    Err(reason) => break reason,
                }
            }
            #[cfg(any(test, feature = "testing"))]
            PumpSource::Test(rx) => {
                // Check if more items are already queued without blocking to count burst size:
                match rx.try_recv() {
                    Ok(item) => {
                        consecutive_ready += 1;
                        match item {
                            PumpInput::Line(msg) => {
                                seq += 1;
                                process_message(&core, *msg, seq, &mut reg);
                            }
                            PumpInput::Settle(done_tx) => {
                                let _ = done_tx.send(());
                            }
                            PumpInput::Eof => break DisconnectReason::Eof,
                        }
                    }
                    Err(mpsc::error::TryRecvError::Empty) => {
                        consecutive_ready = 0;
                        match rx.recv().await {
                            Some(PumpInput::Line(msg)) => {
                                consecutive_ready = 1;
                                seq += 1;
                                process_message(&core, *msg, seq, &mut reg);
                            }
                            Some(PumpInput::Settle(done_tx)) => {
                                let _ = done_tx.send(());
                            }
                            Some(PumpInput::Eof) | None => break DisconnectReason::Eof,
                        }
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        break DisconnectReason::Eof;
                    }
                }
            }
        }
    };

    // Preserve `DisconnectReason::Quit` if `bot.quit(...)` initiated this shutdown:
    core.mark_disconnected(end_reason);
}

fn process_message(
    core: &Arc<BotCore>,
    msg: Message,
    seq: u64,
    reg: &mut RegistrationTracker,
) {
    let received_at = Instant::now();
    let raw = Arc::new(msg);
    let tags = extract_tags(&raw);

    // Handle CAP negotiation (`CAP ACK` / `CAP NAK` -> `CAP END`):
    if let Command::CAP(_, ref sub, ref arg3, _) = raw.command {
        if matches!(sub, CapSubCommand::ACK | CapSubCommand::NAK) {
            let is_multiline_cont = arg3.as_deref() == Some("*");
            if reg.cap_pending && !is_multiline_cont {
                reg.cap_pending = false;
                core.outbound
                    .enqueue_one(Command::CAP(None, CapSubCommand::END, None, None));
                if reg.is_ready() {
                    core.mark_registered();
                }
            }
        }
    }

    // Check numerics first:
    if let Some((code, args)) = extract_numeric(&raw.command) {
        handle_numeric(core, &raw, code, args, seq, reg);
        return;
    }

    match &raw.command {
        Command::PRIVMSG(target, body) => {
            handle_chat_like(core, &raw, target, body, false, seq, received_at, &tags);
        }
        Command::NOTICE(target, body) => {
            handle_chat_like(core, &raw, target, body, true, seq, received_at, &tags);
        }
        Command::JOIN(chanlist, arg1, arg2) => {
            // `extended-join` sends `JOIN <channel> <account> :<realname>` (3 args -> `arg2.is_some()`).
            let ext_acct = if arg2.is_some() { arg1.as_deref() } else { None };
            handle_join(core, &raw, chanlist, ext_acct, seq, &tags);
        }
        Command::Raw(cmd, args) if cmd.eq_ignore_ascii_case("JOIN") && !args.is_empty() => {
            let ext_acct = if args.len() >= 3 {
                Some(args[1].as_str())
            } else {
                None
            };
            handle_join(core, &raw, &args[0], ext_acct, seq, &tags);
        }
        Command::PART(channel, reason) => {
            handle_part(core, &raw, channel, reason.clone(), seq, &tags);
        }
        Command::KICK(channel, target_nick, reason) => {
            handle_kick(core, &raw, channel, target_nick, reason.clone(), seq);
        }
        Command::QUIT(reason) => {
            handle_quit(core, &raw, reason.clone(), seq, &tags);
        }
        Command::NICK(new_nick) => {
            handle_nick(core, &raw, new_nick, seq, &tags);
        }
        Command::ACCOUNT(acct) => {
            handle_account(core, &raw, acct, seq);
        }
        Command::Raw(cmd, args) if cmd.eq_ignore_ascii_case("ACCOUNT") && !args.is_empty() => {
            handle_account(core, &raw, &args[0], seq);
        }
        Command::TOPIC(channel, topic) => {
            {
                let mut ws = core.state.write().unwrap();
                ws.observe_self_prefix(raw.prefix.as_ref());
                ws.handle_topic(channel, topic.clone());
            }
            dispatch_raw(core, &raw, seq, Some(channel));
        }
        Command::ChannelMODE(chan, _) => {
            handle_mode(core, &raw, chan, seq);
        }
        Command::Raw(cmd, args) if cmd.eq_ignore_ascii_case("MODE") && !args.is_empty() => {
            handle_mode(core, &raw, &args[0], seq);
        }
        _ => {
            core.state
                .write()
                .unwrap()
                .observe_self_prefix(raw.prefix.as_ref());
            dispatch_raw(core, &raw, seq, None);
        }
    }
}

fn handle_mode(core: &Arc<BotCore>, raw: &Arc<Message>, chan: &str, seq: u64) {
    let is_chan = {
        let mut ws = core.state.write().unwrap();
        ws.observe_self_prefix(raw.prefix.as_ref());
        let is_c = ws.is_channel_name(chan);
        ws.apply_mode_message(raw, &core.users);
        is_c
    };
    dispatch_raw(core, raw, seq, is_chan.then_some(chan));
}

fn resolve_chat_sender(core: &Arc<BotCore>, nick: &str, tags: &ParsedTags) -> User {
    let track = core.users.get_or_create(nick);
    if let Some(ref raw_acct) = tags.raw_account {
        track.set_account(normalize_account(raw_acct));
    }
    User::from_parts(Arc::downgrade(core), track)
}

#[allow(clippy::too_many_arguments)]
fn handle_chat_like(
    core: &Arc<BotCore>,
    raw: &Arc<Message>,
    target: &str,
    body: &str,
    is_notice: bool,
    seq: u64,
    received_at: Instant,
    tags: &ParsedTags,
) {
    let nick_prefix = match &raw.prefix {
        Some(Prefix::Nickname(nick, user, host)) => {
            Some((nick.as_str(), user.as_str(), host.as_str()))
        }
        _ if !is_notice => {
            dispatch_raw(core, raw, seq, None);
            return;
        }
        _ => None,
    };

    // Check self-echo & unwatched channel under WorldState read lock (upgrading only if self host needs recording):
    let (is_self, is_chan, chan_in_ws) = {
        let ws = core.state.read().unwrap();
        let is_self =
            nick_prefix.is_some_and(|(nick, _, _)| ws.casemap.str_eq(nick, &ws.current_nick));
        let needs_self_update = is_self && (ws.self_ident.is_none() || ws.self_host.is_none());
        let is_chan = ws.is_channel_name(target);
        let chan_in_ws = is_chan && ws.active_channel(target).is_some();
        drop(ws);
        if needs_self_update {
            core.state
                .write()
                .unwrap()
                .observe_self_prefix(raw.prefix.as_ref());
        }
        (is_self, is_chan, chan_in_ws)
    };

    if is_self {
        if let (Some((nick, _, _)), Some(raw_acct)) = (nick_prefix, &tags.raw_account) {
            core.users.update_account(nick, raw_acct);
        }
        dispatch_raw(core, raw, seq, is_chan.then_some(target));
        return;
    }

    if is_chan && !chan_in_ws && !core.inboxes.has_interest_in_channel(target) {
        // Early unwatched-channel skip: zero `UserTrack` / `ChatMessage` allocations!
        if let (Some((nick, _, _)), Some(raw_acct)) = (nick_prefix, &tags.raw_account) {
            core.users.update_account(nick, raw_acct);
        }
        return;
    }

    let msg_target = if is_chan {
        Target::Channel(target.to_owned())
    } else {
        Target::Dm
    };

    if !is_notice {
        let (nick, user, host) = nick_prefix.unwrap();
        let (kind, raw_text) = parse_privmsg_body(body);
        if kind == MessageKind::Privmsg && body.starts_with('\x01') {
            // Non-ACTION CTCP request (`\x01VERSION\x01`, etc.) stays `RawOnly`:
            dispatch_raw(core, raw, seq, is_chan.then_some(target));
            return;
        }

        let sender = resolve_chat_sender(core, nick, tags);
        let account = sender.account();
        let chat = ChatMessage {
            raw: Arc::clone(raw),
            received_at,
            sender,
            nick: nick.to_owned(),
            ident: (!user.is_empty()).then(|| user.to_owned()),
            host: (!host.is_empty()).then(|| host.to_owned()),
            account,
            target: msg_target,
            text: Text::parse(&raw_text).plain(),
            raw_text,
            kind,
        };
        dispatch_wire(
            core,
            raw,
            seq,
            WireKind::Event(Event::Chat(chat)),
            WireMeta::None,
            is_chan.then_some(target),
        );
    } else {
        let plain_text = Text::parse(body).plain();
        let (sender_opt, nick_opt, meta) = match nick_prefix {
            Some((nick, user, host)) => {
                let sender = resolve_chat_sender(core, nick, tags);
                let account = sender.account();
                let is_human_eligible = !user.is_empty()
                    && !host.is_empty()
                    && !tags.is_bot
                    && !is_services_pseudoclient(nick)
                    && !body.starts_with('\x01');
                let meta = if is_human_eligible {
                    WireMeta::HumanNotice(Box::new(ChatMessage {
                        raw: Arc::clone(raw),
                        received_at,
                        sender: sender.clone(),
                        nick: nick.to_owned(),
                        ident: Some(user.to_owned()),
                        host: Some(host.to_owned()),
                        account,
                        target: msg_target.clone(),
                        text: plain_text.clone(),
                        raw_text: body.to_owned(),
                        kind: MessageKind::Notice,
                    }))
                } else {
                    WireMeta::None
                };
                (Some(sender), Some(nick.to_owned()), meta)
            }
            None => (None, None, WireMeta::None),
        };
        let event = Event::Notice {
            sender: sender_opt,
            nick: nick_opt,
            target: msg_target,
            text: plain_text,
            raw: Arc::clone(raw),
        };
        dispatch_wire(
            core,
            raw,
            seq,
            WireKind::Event(event),
            meta,
            is_chan.then_some(target),
        );
    }
}

fn handle_join(
    core: &Arc<BotCore>,
    raw: &Arc<Message>,
    chanlist: &str,
    ext_account: Option<&str>,
    seq: u64,
    tags: &ParsedTags,
) {
    let Some(Prefix::Nickname(nick, user, host)) = &raw.prefix else {
        return;
    };
    let channel = chanlist.split(',').next().unwrap_or(chanlist);
    let track = core.users.get_or_create(nick);
    if let Some(acct) = ext_account.or(tags.raw_account.as_deref()) {
        track.set_account(normalize_account(acct));
    }

    core.state
        .write()
        .unwrap()
        .handle_join(channel, nick, Arc::clone(&track), raw.prefix.as_ref());

    let sender = User::from_parts(Arc::downgrade(core), track);
    dispatch_wire(
        core,
        raw,
        seq,
        WireKind::Event(Event::Joined {
            channel: channel.to_owned(),
            user: sender,
            nick: nick.clone(),
            ident: (!user.is_empty()).then(|| user.clone()),
            host: (!host.is_empty()).then(|| host.clone()),
        }),
        WireMeta::None,
        Some(channel),
    );
}

fn handle_part(
    core: &Arc<BotCore>,
    raw: &Arc<Message>,
    channel: &str,
    reason: Option<String>,
    seq: u64,
    tags: &ParsedTags,
) {
    let Some(Prefix::Nickname(nick, _, _)) = &raw.prefix else {
        return;
    };
    let track = core.users.get_or_create(nick);
    if let Some(ref raw_acct) = tags.raw_account {
        track.set_account(normalize_account(raw_acct));
    }

    let watched = core.inboxes.has_channel_listener(channel);
    let self_left = {
        let mut ws = core.state.write().unwrap();
        ws.observe_self_prefix(raw.prefix.as_ref());
        ws.handle_part_or_kick(channel, nick, track.id, reason.clone(), seq, watched)
    };

    let kind = if self_left {
        WireKind::BotLeftRoom {
            channel: channel.to_owned(),
            reason,
        }
    } else {
        WireKind::Event(Event::Parted {
            channel: channel.to_owned(),
            user: User::from_parts(Arc::downgrade(core), track),
            nick: nick.clone(),
            reason,
        })
    };
    dispatch_wire(core, raw, seq, kind, WireMeta::None, Some(channel));
}

fn handle_kick(
    core: &Arc<BotCore>,
    raw: &Arc<Message>,
    channel: &str,
    target_nick: &str,
    reason: Option<String>,
    seq: u64,
) {
    let by = match &raw.prefix {
        Some(Prefix::Nickname(n, _, _)) => n.clone(),
        Some(Prefix::ServerName(s)) => s.clone(),
        None => String::new(),
    };
    let track = core.users.get_or_create(target_nick);

    let watched = core.inboxes.has_channel_listener(channel);
    let self_left = {
        let mut ws = core.state.write().unwrap();
        ws.observe_self_prefix(raw.prefix.as_ref());
        ws.handle_part_or_kick(channel, target_nick, track.id, reason.clone(), seq, watched)
    };

    let kind = if self_left {
        WireKind::BotLeftRoom {
            channel: channel.to_owned(),
            reason,
        }
    } else {
        WireKind::Event(Event::Kicked {
            channel: channel.to_owned(),
            user: User::from_parts(Arc::downgrade(core), track),
            nick: target_nick.to_owned(),
            by,
            reason,
        })
    };
    dispatch_wire(core, raw, seq, kind, WireMeta::None, Some(channel));
}

fn handle_quit(
    core: &Arc<BotCore>,
    raw: &Arc<Message>,
    reason: Option<String>,
    seq: u64,
    tags: &ParsedTags,
) {
    let Some(Prefix::Nickname(nick, _, _)) = &raw.prefix else {
        return;
    };

    // Ensure a track exists before marking quit so `Event::Quit` carries a valid `User`:
    let ensured_track = core.users.get_or_create(nick);
    if let Some(ref raw_acct) = tags.raw_account {
        ensured_track.set_account(normalize_account(raw_acct));
    }

    let channels = core.state.write().unwrap().handle_quit(ensured_track.id);

    let track = core
        .users
        .mark_quit(nick, seq, reason.clone())
        .unwrap_or(ensured_track);

    dispatch_wire(
        core,
        raw,
        seq,
        WireKind::Event(Event::Quit {
            user: User::from_parts(Arc::downgrade(core), track),
            nick: nick.clone(),
            channels,
            reason,
        }),
        WireMeta::None,
        None,
    );
}

fn handle_nick(
    core: &Arc<BotCore>,
    raw: &Arc<Message>,
    new_nick: &str,
    seq: u64,
    tags: &ParsedTags,
) {
    let Some(Prefix::Nickname(old_nick, _, _)) = &raw.prefix else {
        return;
    };

    let track = core.users.rename(old_nick, new_nick);
    if let Some(ref raw_acct) = tags.raw_account {
        track.set_account(normalize_account(raw_acct));
    }

    let channels =
        core.state
            .write()
            .unwrap()
            .handle_nick(track.id, old_nick, new_nick, raw.prefix.as_ref());

    dispatch_wire(
        core,
        raw,
        seq,
        WireKind::Event(Event::NickChanged {
            user: User::from_parts(Arc::downgrade(core), track),
            old_nick: old_nick.clone(),
            new_nick: new_nick.to_owned(),
            channels,
        }),
        WireMeta::None,
        None,
    );
}

fn handle_account(core: &Arc<BotCore>, raw: &Arc<Message>, acct: &str, seq: u64) {
    let Some(Prefix::Nickname(nick, _, _)) = &raw.prefix else {
        return;
    };
    core.users.update_account(nick, acct);
    dispatch_raw(core, raw, seq, None);
}

fn handle_numeric(
    core: &Arc<BotCore>,
    raw: &Arc<Message>,
    code: u16,
    args: &[String],
    seq: u64,
    reg: &mut RegistrationTracker,
) {
    match code {
        1 => {
            // 001 RPL_WELCOME
            core.mark_welcomed();
            core.state.write().unwrap().handle_welcome(args);
            dispatch_raw(core, raw, seq, None);
        }
        5 => {
            // 005 RPL_ISUPPORT
            let changed_cm = core.state.write().unwrap().apply_isupport(args);
            if let Some(new_cm) = changed_cm {
                core.users.rekey_casemap(new_cm);
                core.inboxes.rekey_casemap(new_cm);
            }
            core.maybe_announce_bot();
            dispatch_raw(core, raw, seq, None);
        }
        324 => {
            // RPL_CHANNELMODEIS: `<client> <channel> <modes> ...`
            let chan_hint = args.get(1).map(String::as_str);
            core.state
                .write()
                .unwrap()
                .apply_mode_message(raw, &core.users);
            dispatch_raw(core, raw, seq, chan_hint);
        }
        331 => {
            // RPL_NOTOPIC: `<client> <channel> :No topic is set`
            let chan_hint = args.get(1).map(String::as_str);
            if let Some(ch) = chan_hint {
                core.state.write().unwrap().handle_topic(ch, None);
            }
            dispatch_raw(core, raw, seq, chan_hint);
        }
        332 => {
            // RPL_TOPIC: `<client> <channel> :<topic>`
            let chan_hint = args.get(1).map(String::as_str);
            if args.len() >= 3 {
                core.state
                    .write()
                    .unwrap()
                    .handle_topic(&args[1], Some(args[2].clone()));
            }
            dispatch_raw(core, raw, seq, chan_hint);
        }
        353 => {
            // RPL_NAMREPLY
            core.state
                .write()
                .unwrap()
                .handle_namreply(args, &core.users);
            let chan_hint = if args.len() >= 4 {
                Some(args[2].as_str())
            } else {
                args.get(1).map(String::as_str)
            };
            dispatch_raw(core, raw, seq, chan_hint);
        }
        366 => {
            // RPL_ENDOFNAMES
            let synced_chan = core.state.write().unwrap().handle_endofnames(args);
            let fallback = args.get(1).map(String::as_str);
            let hint = synced_chan.as_deref().or(fallback);
            dispatch_raw(core, raw, seq, hint);
        }
        376 | 422 => {
            // RPL_ENDOFMOTD / ERR_NOMOTD
            reg.motd_done = true;
            if reg.is_ready() {
                core.mark_registered();
            }
            dispatch_raw(core, raw, seq, None);
        }
        396 => {
            // RPL_HOSTHIDDEN
            core.state.write().unwrap().handle_host_hidden(args);
            dispatch_raw(core, raw, seq, None);
        }
        482 => {
            // ERR_CHANOPRIVSNEEDED: `<client> <channel> :You're not channel operator`
            let chan = args.get(1).map(String::as_str).unwrap_or("");
            let reason = args.last().map(String::as_str).unwrap_or("not channel op");
            tracing::warn!(
                channel = chan,
                message = reason,
                "IRC server rejected moderation command with 482 ERR_CHANOPRIVSNEEDED"
            );
            dispatch_raw(core, raw, seq, (!chan.is_empty()).then_some(chan));
        }
        405 | 471 | 473 | 474 | 475 | 476 | 477 => {
            // Join-refusal numerics: `<client> <channel> :<reason>`
            let chan_hint = args.get(1).map(String::as_str);
            fail_pending_join(core, code, args);
            dispatch_raw(core, raw, seq, chan_hint);
        }
        401 | 403 | 404 | 442 | 486 | 492 | 531 | 716 => {
            // Send-refusal / PM-block numerics: `<client> <target> ... :<reason>`
            if code == 403 {
                fail_pending_join(core, code, args);
            }
            let Some(target) = args.get(1).cloned() else {
                dispatch_raw(core, raw, seq, None);
                return;
            };
            let message = args
                .last()
                .cloned()
                .unwrap_or_else(|| format!("IRC error {code}"));

            let is_chan = code == 403 || core.state.read().unwrap().is_channel_name(&target);
            let (target_user_id, quit_track_id) = if !is_chan {
                core.users.resolve_send_error(&target, code, seq, &message)
            } else {
                (None, None)
            };

            if let Some(qid) = quit_track_id {
                core.state.write().unwrap().handle_quit(qid);
            }

            let meta = match target_user_id {
                Some(uid) => WireMeta::BounceUser(uid),
                None => WireMeta::None,
            };
            let chan_hint = is_chan.then_some(target.as_str());
            let event = Event::DeliveryFailed {
                target: target.clone(),
                code,
                message,
                raw: Arc::clone(raw),
            };
            dispatch_wire(core, raw, seq, WireKind::Event(event), meta, chan_hint);
        }
        _ => {
            dispatch_raw(core, raw, seq, None);
        }
    }
}

/// Resolves pending `Bot::join` waiters on a join-refusal numeric (`<client> <channel> :<reason>`).
fn fail_pending_join(core: &Arc<BotCore>, code: u16, args: &[String]) {
    let Some(channel) = args.get(1) else {
        return;
    };
    let message = args
        .last()
        .cloned()
        .unwrap_or_else(|| format!("join failed ({code})"));
    core.state
        .write()
        .unwrap()
        .fail_join(channel, code, message);
}

fn dispatch_wire(
    core: &Arc<BotCore>,
    raw: &Arc<Message>,
    seq: u64,
    kind: WireKind,
    meta: WireMeta,
    chan_hint: Option<&str>,
) {
    let wire = Arc::new(WireEvent {
        seq,
        raw: Arc::clone(raw),
        kind,
        meta,
    });
    core.inboxes.dispatch(&wire, chan_hint);
}

fn dispatch_raw(core: &Arc<BotCore>, raw: &Arc<Message>, seq: u64, chan_hint: Option<&str>) {
    dispatch_wire(core, raw, seq, WireKind::RawOnly, WireMeta::None, chan_hint);
}

#[cfg(test)]
#[path = "pump_test.rs"]
mod test;

