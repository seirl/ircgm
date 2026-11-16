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
    parse_privmsg_body, ChatMessage, Event, MessageKind, Target, WireEvent, WireKind,
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

    async fn next_message<S, E>(
        &mut self,
        stream: &mut S,
        core: &BotCore,
    ) -> Result<Message, DisconnectReason>
    where
        S: futures_util::Stream<Item = Result<Message, E>> + Unpin,
        E: std::fmt::Display,
    {
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
    core.observe_inbound_actor(raw.prefix.as_ref(), tags.raw_account.as_deref());

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
            core.state
                .write()
                .unwrap()
                .handle_topic(channel, topic.clone());
            dispatch_raw(core, &raw, seq, Some(channel));
        }
        Command::ChannelMODE(chan, _) => {
            handle_mode(core, &raw, chan, seq);
        }
        Command::Raw(cmd, args) if cmd.eq_ignore_ascii_case("MODE") && !args.is_empty() => {
            handle_mode(core, &raw, &args[0], seq);
        }
        _ => {
            dispatch_raw(core, &raw, seq, None);
        }
    }
}

fn handle_mode(core: &Arc<BotCore>, raw: &Arc<Message>, chan: &str, seq: u64) {
    let is_chan = {
        let mut ws = core.state.write().unwrap();
        let is_c = ws.is_channel_name(chan);
        ws.apply_mode_message(raw, &core.users);
        is_c
    };
    dispatch_raw(core, raw, seq, is_chan.then_some(chan));
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

    let (is_self, is_chan, chan_in_ws) = {
        let ws = core.state.read().unwrap();
        let is_self =
            nick_prefix.is_some_and(|(nick, _, _)| ws.casemap.str_eq(nick, &ws.current_nick));
        let is_chan = ws.is_channel_name(target);
        let chan_in_ws = is_chan && ws.active_channel(target).is_some();
        (is_self, is_chan, chan_in_ws)
    };

    if is_self {
        dispatch_raw(core, raw, seq, is_chan.then_some(target));
        return;
    }

    if is_chan && !chan_in_ws && !core.inboxes.has_interest_in_channel(target) {
        // Early unwatched-channel skip: zero `UserTrack` / `ChatMessage` allocations!
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
            // Non-ACTION CTCP request (`\x01VERSION\x01`, etc.) stays `Raw`:
            dispatch_raw(core, raw, seq, is_chan.then_some(target));
            return;
        }

        let sender = core.resolve_user(nick, tags.raw_account.as_deref());
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
        dispatch_event(core, raw, seq, Event::Chat(chat));
    } else {
        let plain_text = Text::parse(body).plain();
        let (sender_opt, nick_opt, human_chat) = match nick_prefix {
            Some((nick, user, host)) => {
                let sender = core.resolve_user(nick, tags.raw_account.as_deref());
                let account = sender.account();
                let is_human_eligible = !user.is_empty()
                    && !host.is_empty()
                    && !tags.is_bot
                    && !is_services_pseudoclient(nick)
                    && !body.starts_with('\x01');
                let chat = is_human_eligible.then(|| {
                    Box::new(ChatMessage {
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
                    })
                });
                (Some(sender), Some(nick.to_owned()), chat)
            }
            None => (None, None, None),
        };
        let event = Event::Notice {
            sender: sender_opt,
            nick: nick_opt,
            target: msg_target,
            text: plain_text,
            raw: Arc::clone(raw),
        };
        let kind = match human_chat {
            Some(chat) => WireKind::HumanNotice { event, chat },
            None => WireKind::Event(event),
        };
        dispatch_wire(core, raw, seq, kind);
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
    let track = core
        .users
        .get_or_create_with_account(nick, ext_account.or(tags.raw_account.as_deref()));
    core.state
        .write()
        .unwrap()
        .handle_join(channel, nick, Arc::clone(&track));
    dispatch_event(
        core,
        raw,
        seq,
        Event::Joined {
            channel: channel.to_owned(),
            user: User::from_parts(Arc::downgrade(core), track),
            nick: nick.clone(),
            ident: (!user.is_empty()).then(|| user.clone()),
            host: (!host.is_empty()).then(|| host.clone()),
        },
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
    let kind = match core.record_channel_departure(
        channel,
        nick,
        tags.raw_account.as_deref(),
        reason.clone(),
        seq,
    ) {
        Some(user) => WireKind::Event(Event::Parted {
            channel: channel.to_owned(),
            user,
            nick: nick.clone(),
            reason,
        }),
        None => WireKind::BotLeftRoom {
            channel: channel.to_owned(),
            reason,
        },
    };
    dispatch_wire(core, raw, seq, kind);
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
    let kind =
        match core.record_channel_departure(channel, target_nick, None, reason.clone(), seq) {
            Some(user) => WireKind::Event(Event::Kicked {
                channel: channel.to_owned(),
                user,
                nick: target_nick.to_owned(),
                by,
                reason,
            }),
            None => WireKind::BotLeftRoom {
                channel: channel.to_owned(),
                reason,
            },
        };
    dispatch_wire(core, raw, seq, kind);
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
    let ensured = core
        .users
        .get_or_create_with_account(nick, tags.raw_account.as_deref());
    let channels = core.state.write().unwrap().handle_quit(ensured.id);
    let track = core
        .users
        .mark_quit(nick, seq, reason.clone())
        .unwrap_or(ensured);
    dispatch_event(
        core,
        raw,
        seq,
        Event::Quit {
            user: User::from_parts(Arc::downgrade(core), track),
            nick: nick.clone(),
            channels,
            reason,
        },
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
    let channels = core
        .state
        .write()
        .unwrap()
        .handle_nick(track.id, old_nick, new_nick);
    dispatch_event(
        core,
        raw,
        seq,
        Event::NickChanged {
            user: User::from_parts(Arc::downgrade(core), track),
            old_nick: old_nick.clone(),
            new_nick: new_nick.to_owned(),
            channels,
        },
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
            core.apply_isupport(args);
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
            let scope = core.resolve_bounce(&target, code, seq, &message);
            let wire = Arc::new(WireEvent::bounce(
                seq,
                Arc::clone(raw),
                target,
                code,
                message,
                scope,
            ));
            core.inboxes.dispatch(&wire);
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

fn dispatch_wire(core: &Arc<BotCore>, raw: &Arc<Message>, seq: u64, kind: WireKind) {
    let wire = Arc::new(WireEvent::new(seq, Arc::clone(raw), kind));
    core.inboxes.dispatch(&wire);
}

fn dispatch_event(core: &Arc<BotCore>, raw: &Arc<Message>, seq: u64, event: Event) {
    dispatch_wire(core, raw, seq, WireKind::Event(event));
}

fn dispatch_raw(core: &Arc<BotCore>, raw: &Arc<Message>, seq: u64, chan_hint: Option<&str>) {
    dispatch_wire(
        core,
        raw,
        seq,
        WireKind::Raw {
            channel: chan_hint.map(str::to_owned),
        },
    );
}

#[cfg(test)]
#[path = "pump_test.rs"]
mod test;

