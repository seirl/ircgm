use std::sync::Arc;

use futures_util::StreamExt;
use irc::proto::{CapSubCommand, Command, Message, Prefix};
use irc_colors::Text;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::bot::BotCore;
use crate::error::DisconnectReason;
use crate::message::{
    parse_privmsg_body, parse_server_time, ChatMessage, MessageKind, Target, WireEvent, WireKind,
};
use crate::outbound::Priority;
use crate::user::{normalize_account, User};

pub(crate) enum PumpInput {
    Line(Box<Message>),
    Settle(oneshot::Sender<()>),
    Eof,
}

pub(crate) enum PumpSource {
    Irc(Box<irc::client::ClientStream>),
    Test(mpsc::UnboundedReceiver<PumpInput>),
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

fn extract_numeric(cmd: &Command) -> Option<(u16, &[String])> {
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
    server_time: Option<std::time::SystemTime>,
    raw_account: Option<String>,
    is_bot: bool,
}

fn extract_tags(msg: &Message) -> ParsedTags {
    let mut out = ParsedTags {
        server_time: None,
        raw_account: None,
        is_bot: false,
    };
    let Some(tags) = &msg.tags else {
        return out;
    };
    for tag in tags {
        let k = tag.0.as_str();
        if k == "time" {
            if let Some(ref val) = tag.1 {
                out.server_time = parse_server_time(val);
            }
        } else if k == "account" {
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
            PumpSource::Irc(stream) => match stream.next().await {
                Some(Ok(msg)) => {
                    consecutive_ready += 1;
                    seq += 1;
                    process_message(&core, msg, seq, &mut reg);
                }
                Some(Err(err)) => {
                    break DisconnectReason::Transport(err.to_string());
                }
                None => {
                    break DisconnectReason::Eof;
                }
            },
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
    core.mark_disconnected_if_unset(end_reason);
    core.inboxes.clear();
}

fn process_message(
    core: &Arc<BotCore>,
    msg: Message,
    seq: u64,
    reg: &mut RegistrationTracker,
) {
    let received_at = Instant::now();
    let tags = extract_tags(&msg);

    // Handle PING automatically on raw `from_client` / `TestServer` if needed (`ClientStream` also handles PING,
    // so only auto-respond on `Test`? No, `irc` handles PING inside `ClientStream`; don't double-PONG!).

    // Handle CAP negotiation (`CAP ACK` / `CAP NAK` -> `CAP END`):
    if let Command::CAP(_, ref sub, ref arg3, _) = msg.command {
        if matches!(sub, CapSubCommand::ACK | CapSubCommand::NAK) {
            let is_multiline_cont = arg3.as_deref() == Some("*");
            if reg.cap_pending && !is_multiline_cont {
                reg.cap_pending = false;
                let cm = core.casemap();
                let _ = core.outbound.enqueue_one(
                    Priority::Interactive,
                    Command::CAP(None, CapSubCommand::END, None, None),
                    cm,
                );
                if reg.is_ready() {
                    core.mark_registered();
                }
            }
        }
    }

    // Check numerics first:
    if let Some((code, args)) = extract_numeric(&msg.command) {
        handle_numeric(core, &msg, code, args, seq, reg);
        return;
    }

    match &msg.command {
        Command::PRIVMSG(target, body) => {
            handle_privmsg(core, &msg, target, body, seq, received_at, &tags);
        }
        Command::NOTICE(target, body) => {
            handle_notice(core, &msg, target, body, seq, received_at, &tags);
        }
        Command::JOIN(chanlist, arg1, arg2) => {
            // `extended-join` sends `JOIN <channel> <account> :<realname>` (3 args -> `arg2.is_some()`).
            let ext_acct = if arg2.is_some() { arg1.as_deref() } else { None };
            handle_join(core, &msg, chanlist, ext_acct, seq, &tags);
        }
        Command::Raw(cmd, args) if cmd.eq_ignore_ascii_case("JOIN") && !args.is_empty() => {
            let ext_acct = if args.len() >= 3 {
                Some(args[1].as_str())
            } else {
                None
            };
            handle_join(core, &msg, &args[0], ext_acct, seq, &tags);
        }
        Command::PART(channel, reason) => {
            handle_part(core, &msg, channel, reason.clone(), seq, &tags);
        }
        Command::KICK(channel, target_nick, reason) => {
            handle_kick(core, &msg, channel, target_nick, reason.clone(), seq);
        }
        Command::QUIT(reason) => {
            handle_quit(core, &msg, reason.clone(), seq, &tags);
        }
        Command::NICK(new_nick) => {
            handle_nick(core, &msg, new_nick, seq, &tags);
        }
        Command::ACCOUNT(acct) => {
            handle_account(core, &msg, acct, seq);
        }
        Command::Raw(cmd, args) if cmd.eq_ignore_ascii_case("ACCOUNT") && !args.is_empty() => {
            handle_account(core, &msg, &args[0], seq);
        }
        Command::TOPIC(channel, topic) => {
            let cm = {
                let mut ws = core.state.write().unwrap();
                ws.observe_self_prefix(msg.prefix.as_ref());
                ws.handle_topic(channel, topic.clone());
                ws.casemap
            };
            let chan = channel.clone();
            let raw = Arc::new(msg);
            let wire = Arc::new(WireEvent {
                seq,
                raw,
                kind: WireKind::RawOnly,
            });
            core.inboxes.dispatch(&wire, Some(&chan), cm);
        }
        Command::ChannelMODE(chan, _) => {
            let (cm, is_chan) = {
                let mut ws = core.state.write().unwrap();
                ws.observe_self_prefix(msg.prefix.as_ref());
                let is_c = ws.is_channel_name(chan);
                ws.apply_mode_message(&msg);
                (ws.casemap, is_c)
            };
            let chan_owned = chan.clone();
            let raw = Arc::new(msg);
            let wire = Arc::new(WireEvent {
                seq,
                raw,
                kind: WireKind::RawOnly,
            });
            core.inboxes
                .dispatch(&wire, is_chan.then_some(chan_owned.as_str()), cm);
        }
        Command::Raw(cmd, args) if cmd.eq_ignore_ascii_case("MODE") && !args.is_empty() => {
            let chan_owned = args[0].clone();
            let (cm, is_chan) = {
                let mut ws = core.state.write().unwrap();
                ws.observe_self_prefix(msg.prefix.as_ref());
                let is_c = ws.is_channel_name(&chan_owned);
                ws.apply_mode_message(&msg);
                (ws.casemap, is_c)
            };
            let raw = Arc::new(msg);
            let wire = Arc::new(WireEvent {
                seq,
                raw,
                kind: WireKind::RawOnly,
            });
            core.inboxes
                .dispatch(&wire, is_chan.then_some(chan_owned.as_str()), cm);
        }
        _ => {
            let cm = {
                let mut ws = core.state.write().unwrap();
                ws.observe_self_prefix(msg.prefix.as_ref());
                ws.casemap
            };
            let raw = Arc::new(msg);
            let wire = Arc::new(WireEvent {
                seq,
                raw,
                kind: WireKind::RawOnly,
            });
            core.inboxes.dispatch(&wire, None, cm);
        }
    }
}

fn handle_privmsg(
    core: &Arc<BotCore>,
    msg: &Message,
    target: &str,
    body: &str,
    seq: u64,
    received_at: Instant,
    tags: &ParsedTags,
) {
    let Some(Prefix::Nickname(nick, user, host)) = &msg.prefix else {
        let cm = core.casemap();
        let wire = Arc::new(WireEvent {
            seq,
            raw: Arc::new(msg.clone()),
            kind: WireKind::RawOnly,
        });
        core.inboxes.dispatch(&wire, None, cm);
        return;
    };

    // Check self-echo & unwatched channel under WorldState lock:
    let (cm, is_self, is_chan, chan_in_ws) = {
        let mut ws = core.state.write().unwrap();
        let is_self = ws.casemap.str_eq(nick, &ws.current_nick);
        if is_self {
            ws.observe_self_prefix(msg.prefix.as_ref());
        }
        let is_chan = ws.is_channel_name(target);
        let chan_in_ws = is_chan && ws.channels.contains_key(&ws.casemap.fold(target));
        (ws.casemap, is_self, is_chan, chan_in_ws)
    };

    if is_self {
        if let Some(ref raw_acct) = tags.raw_account {
            core.users.update_account(nick, raw_acct, false);
        }
        let wire = Arc::new(WireEvent {
            seq,
            raw: Arc::new(msg.clone()),
            kind: WireKind::RawOnly,
        });
        core.inboxes
            .dispatch(&wire, is_chan.then_some(target), cm);
        return;
    }

    if is_chan && !chan_in_ws && !core.inboxes.has_interest_in_channel(&cm.fold(target)) {
        // Early unwatched-channel skip: zero `UserTrack` / `ChatMessage` allocations!
        if let Some(ref raw_acct) = tags.raw_account {
            core.users.update_account(nick, raw_acct, false);
        }
        return;
    }

    let (kind, raw_text) = parse_privmsg_body(body);
    if kind == MessageKind::Privmsg && body.starts_with('\x01') {
        // Non-ACTION CTCP request (`\x01VERSION\x01`, etc.) stays `RawOnly`:
        let wire = Arc::new(WireEvent {
            seq,
            raw: Arc::new(msg.clone()),
            kind: WireKind::RawOnly,
        });
        core.inboxes
            .dispatch(&wire, is_chan.then_some(target), cm);
        return;
    }

    let track = core.users.get_or_create(nick);
    if let Some(ref raw_acct) = tags.raw_account {
        track.set_account(normalize_account(raw_acct));
    }
    let account = track.account();
    let sender = User::from_parts(Arc::downgrade(core), track);
    let plain_text = Text::parse(&raw_text).plain();
    let raw_arc = Arc::new(msg.clone());

    let chat = ChatMessage {
        raw: Arc::clone(&raw_arc),
        seq,
        received_at,
        server_time: tags.server_time,
        sender,
        nick: nick.clone(),
        ident: (!user.is_empty()).then(|| user.clone()),
        host: (!host.is_empty()).then(|| host.clone()),
        account,
        target: if is_chan {
            Target::Channel(target.to_owned())
        } else {
            Target::Dm
        },
        text: plain_text,
        raw_text,
        kind,
    };

    let wire = Arc::new(WireEvent {
        seq,
        raw: raw_arc,
        kind: WireKind::Chat(chat),
    });
    core.inboxes
        .dispatch(&wire, is_chan.then_some(target), cm);
}

fn handle_notice(
    core: &Arc<BotCore>,
    msg: &Message,
    target: &str,
    body: &str,
    seq: u64,
    received_at: Instant,
    tags: &ParsedTags,
) {
    let (cm, is_chan, chan_in_ws, is_self) = {
        let mut ws = core.state.write().unwrap();
        let is_self = match &msg.prefix {
            Some(Prefix::Nickname(nick, _, _)) => ws.casemap.str_eq(nick, &ws.current_nick),
            _ => false,
        };
        if is_self {
            ws.observe_self_prefix(msg.prefix.as_ref());
        }
        let is_chan = ws.is_channel_name(target);
        let chan_in_ws = is_chan && ws.channels.contains_key(&ws.casemap.fold(target));
        (ws.casemap, is_chan, chan_in_ws, is_self)
    };

    if is_self {
        let wire = Arc::new(WireEvent {
            seq,
            raw: Arc::new(msg.clone()),
            kind: WireKind::RawOnly,
        });
        core.inboxes
            .dispatch(&wire, is_chan.then_some(target), cm);
        return;
    }

    if is_chan && !chan_in_ws && !core.inboxes.has_interest_in_channel(&cm.fold(target)) {
        if let Some(Prefix::Nickname(nick, _, _)) = &msg.prefix {
            if let Some(ref raw_acct) = tags.raw_account {
                core.users.update_account(nick, raw_acct, false);
            }
        }
        return;
    }

    let msg_target = if is_chan {
        Target::Channel(target.to_owned())
    } else {
        Target::Dm
    };
    let plain_text = Text::parse(body).plain();
    let raw_arc = Arc::new(msg.clone());

    let (sender_opt, nick_opt, human_chat) = match &msg.prefix {
        Some(Prefix::Nickname(nick, user, host)) => {
            let track = core.users.get_or_create(nick);
            if let Some(ref raw_acct) = tags.raw_account {
                track.set_account(normalize_account(raw_acct));
            }
            let account = track.account();
            let sender = User::from_parts(Arc::downgrade(core), track);

            let is_human_eligible = !user.is_empty()
                && !host.is_empty()
                && !tags.is_bot
                && !is_services_pseudoclient(nick)
                && !body.starts_with('\x01');

            let chat_opt = is_human_eligible.then(|| ChatMessage {
                raw: Arc::clone(&raw_arc),
                seq,
                received_at,
                server_time: tags.server_time,
                sender: sender.clone(),
                nick: nick.clone(),
                ident: Some(user.clone()),
                host: Some(host.clone()),
                account,
                target: msg_target.clone(),
                text: plain_text.clone(),
                raw_text: body.to_owned(),
                kind: MessageKind::Notice,
            });
            (Some(sender), Some(nick.clone()), chat_opt)
        }
        _ => (None, None, None),
    };

    let wire = Arc::new(WireEvent {
        seq,
        raw: raw_arc,
        kind: WireKind::Notice {
            sender: sender_opt,
            nick: nick_opt,
            target: msg_target,
            text: plain_text,
            human_chat,
        },
    });
    core.inboxes
        .dispatch(&wire, is_chan.then_some(target), cm);
}

fn handle_join(
    core: &Arc<BotCore>,
    msg: &Message,
    chanlist: &str,
    ext_account: Option<&str>,
    seq: u64,
    tags: &ParsedTags,
) {
    let Some(Prefix::Nickname(nick, user, host)) = &msg.prefix else {
        return;
    };
    let channel = chanlist.split(',').next().unwrap_or(chanlist);
    let track = core.users.get_or_create(nick);
    if let Some(acct) = ext_account.or(tags.raw_account.as_deref()) {
        track.set_account(normalize_account(acct));
    }

    let cm = {
        let mut ws = core.state.write().unwrap();
        ws.handle_join(channel, nick, Arc::clone(&track), msg.prefix.as_ref());
        ws.casemap
    };

    let sender = User::from_parts(Arc::downgrade(core), track);
    let wire = Arc::new(WireEvent {
        seq,
        raw: Arc::new(msg.clone()),
        kind: WireKind::Joined {
            channel: channel.to_owned(),
            user: sender,
            nick: nick.clone(),
            ident: (!user.is_empty()).then(|| user.clone()),
            host: (!host.is_empty()).then(|| host.clone()),
        },
    });
    core.inboxes.dispatch(&wire, Some(channel), cm);
}

fn handle_part(
    core: &Arc<BotCore>,
    msg: &Message,
    channel: &str,
    reason: Option<String>,
    seq: u64,
    tags: &ParsedTags,
) {
    let Some(Prefix::Nickname(nick, _, _)) = &msg.prefix else {
        return;
    };
    let track = core.users.get_or_create(nick);
    if let Some(ref raw_acct) = tags.raw_account {
        track.set_account(normalize_account(raw_acct));
    }

    let (cm, self_left) = {
        let mut ws = core.state.write().unwrap();
        ws.observe_self_prefix(msg.prefix.as_ref());
        let left = ws.handle_part_or_kick(channel, nick, reason.clone());
        (ws.casemap, left)
    };

    let kind = if self_left {
        WireKind::BotLeftRoom {
            channel: channel.to_owned(),
            reason,
        }
    } else {
        WireKind::Parted {
            channel: channel.to_owned(),
            user: User::from_parts(Arc::downgrade(core), track),
            nick: nick.clone(),
            reason,
        }
    };

    let wire = Arc::new(WireEvent {
        seq,
        raw: Arc::new(msg.clone()),
        kind,
    });
    core.inboxes.dispatch(&wire, Some(channel), cm);
}

fn handle_kick(
    core: &Arc<BotCore>,
    msg: &Message,
    channel: &str,
    target_nick: &str,
    reason: Option<String>,
    seq: u64,
) {
    let by = match &msg.prefix {
        Some(Prefix::Nickname(n, _, _)) => n.clone(),
        Some(Prefix::ServerName(s)) => s.clone(),
        None => String::new(),
    };
    let track = core.users.get_or_create(target_nick);

    let (cm, self_left) = {
        let mut ws = core.state.write().unwrap();
        ws.observe_self_prefix(msg.prefix.as_ref());
        let left = ws.handle_part_or_kick(channel, target_nick, reason.clone());
        (ws.casemap, left)
    };

    let kind = if self_left {
        WireKind::BotLeftRoom {
            channel: channel.to_owned(),
            reason,
        }
    } else {
        WireKind::Kicked {
            channel: channel.to_owned(),
            user: User::from_parts(Arc::downgrade(core), track),
            nick: target_nick.to_owned(),
            by,
            reason,
        }
    };

    let wire = Arc::new(WireEvent {
        seq,
        raw: Arc::new(msg.clone()),
        kind,
    });
    core.inboxes.dispatch(&wire, Some(channel), cm);
}

fn handle_quit(
    core: &Arc<BotCore>,
    msg: &Message,
    reason: Option<String>,
    seq: u64,
    tags: &ParsedTags,
) {
    let Some(Prefix::Nickname(nick, _, _)) = &msg.prefix else {
        return;
    };

    // Ensure a track exists before marking quit so `Event::Quit` carries a valid `User`:
    let ensured_track = core.users.get_or_create(nick);
    if let Some(ref raw_acct) = tags.raw_account {
        ensured_track.set_account(normalize_account(raw_acct));
    }

    let (cm, channels) = {
        let mut ws = core.state.write().unwrap();
        let chans = ws.handle_quit(nick);
        (ws.casemap, chans)
    };

    let (track, netsplit) = core
        .users
        .mark_quit(nick, seq, reason.clone(), true)
        .unwrap_or((ensured_track, false));

    let wire = Arc::new(WireEvent {
        seq,
        raw: Arc::new(msg.clone()),
        kind: WireKind::Quit {
            user: User::from_parts(Arc::downgrade(core), track),
            nick: nick.clone(),
            channels,
            reason,
            netsplit,
        },
    });
    core.inboxes.dispatch(&wire, None, cm);
}

fn handle_nick(
    core: &Arc<BotCore>,
    msg: &Message,
    new_nick: &str,
    seq: u64,
    tags: &ParsedTags,
) {
    let Some(Prefix::Nickname(old_nick, _, _)) = &msg.prefix else {
        return;
    };

    let track = core.users.rename(old_nick, new_nick);
    if let Some(ref raw_acct) = tags.raw_account {
        track.set_account(normalize_account(raw_acct));
    }

    let (cm, channels) = {
        let mut ws = core.state.write().unwrap();
        let chans = ws.handle_nick(old_nick, new_nick, msg.prefix.as_ref());
        (ws.casemap, chans)
    };

    let wire = Arc::new(WireEvent {
        seq,
        raw: Arc::new(msg.clone()),
        kind: WireKind::NickChanged {
            user: User::from_parts(Arc::downgrade(core), track),
            old_nick: old_nick.clone(),
            new_nick: new_nick.to_owned(),
            channels,
        },
    });
    core.inboxes.dispatch(&wire, None, cm);
}

fn handle_account(core: &Arc<BotCore>, msg: &Message, acct: &str, seq: u64) {
    let Some(Prefix::Nickname(nick, _, _)) = &msg.prefix else {
        return;
    };
    let (cm, in_any_channel) = {
        let ws = core.state.read().unwrap();
        (ws.casemap, !ws.channels_for_nick(nick).is_empty())
    };
    core.users.update_account(nick, acct, in_any_channel);

    let wire = Arc::new(WireEvent {
        seq,
        raw: Arc::new(msg.clone()),
        kind: WireKind::RawOnly,
    });
    core.inboxes.dispatch(&wire, None, cm);
}

fn handle_numeric(
    core: &Arc<BotCore>,
    msg: &Message,
    code: u16,
    args: &[String],
    seq: u64,
    reg: &mut RegistrationTracker,
) {
    match code {
        1 => {
            // 001 RPL_WELCOME
            let cm = {
                let mut ws = core.state.write().unwrap();
                ws.handle_welcome(args);
                ws.casemap
            };
            dispatch_raw(core, msg, seq, None, cm);
        }
        5 => {
            // 005 RPL_ISUPPORT
            let (changed_cm, cm) = {
                let mut ws = core.state.write().unwrap();
                let changed = ws.apply_isupport(args);
                (changed, ws.casemap)
            };
            if let Some(new_cm) = changed_cm {
                core.users.rekey_casemap(new_cm);
                core.inboxes.rekey_casemap(new_cm);
            }
            dispatch_raw(core, msg, seq, None, cm);
        }
        324 => {
            // RPL_CHANNELMODEIS: `<client> <channel> <modes> ...`
            let chan_hint = args.get(1).map(String::as_str);
            let cm = {
                let mut ws = core.state.write().unwrap();
                ws.apply_mode_message(msg);
                ws.casemap
            };
            dispatch_raw(core, msg, seq, chan_hint, cm);
        }
        331 => {
            // RPL_NOTOPIC: `<client> <channel> :No topic is set`
            let chan_hint = args.get(1).map(String::as_str);
            let cm = {
                let mut ws = core.state.write().unwrap();
                if let Some(ch) = chan_hint {
                    ws.handle_topic(ch, None);
                }
                ws.casemap
            };
            dispatch_raw(core, msg, seq, chan_hint, cm);
        }
        332 => {
            // RPL_TOPIC: `<client> <channel> :<topic>`
            let chan_hint = args.get(1).map(String::as_str);
            let cm = {
                let mut ws = core.state.write().unwrap();
                if args.len() >= 3 {
                    ws.handle_topic(&args[1], Some(args[2].clone()));
                }
                ws.casemap
            };
            dispatch_raw(core, msg, seq, chan_hint, cm);
        }
        353 => {
            // RPL_NAMREPLY
            let cm = {
                let mut ws = core.state.write().unwrap();
                ws.handle_namreply(args, &core.users);
                ws.casemap
            };
            let chan_hint = if args.len() >= 4 {
                Some(args[2].as_str())
            } else {
                args.get(1).map(String::as_str)
            };
            dispatch_raw(core, msg, seq, chan_hint, cm);
        }
        366 => {
            // RPL_ENDOFNAMES
            let (synced_chan, cm) = {
                let mut ws = core.state.write().unwrap();
                let ch = ws.handle_endofnames(args);
                (ch, ws.casemap)
            };
            let fallback = args.get(1).map(String::as_str);
            let hint = synced_chan.as_deref().or(fallback);
            dispatch_raw(core, msg, seq, hint, cm);
        }
        376 | 422 => {
            // RPL_ENDOFMOTD / ERR_NOMOTD
            reg.motd_done = true;
            if reg.is_ready() {
                core.mark_registered();
            }
            let cm = core.casemap();
            dispatch_raw(core, msg, seq, None, cm);
        }
        396 => {
            // RPL_HOSTHIDDEN
            let cm = {
                let mut ws = core.state.write().unwrap();
                ws.handle_host_hidden(args);
                ws.casemap
            };
            dispatch_raw(core, msg, seq, None, cm);
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
            let cm = core.casemap();
            dispatch_raw(core, msg, seq, (!chan.is_empty()).then_some(chan), cm);
        }
        405 | 471 | 473 | 474 | 475 | 476 | 477 => {
            // Join-refusal numerics: `<client> <channel> :<reason>`
            let chan_hint = args.get(1).map(String::as_str);
            let cm = core.casemap();
            dispatch_raw(core, msg, seq, chan_hint, cm);
        }
        401 | 403 | 404 | 442 | 486 | 492 | 531 | 716 => {
            // Send-refusal / PM-block numerics: `<client> <target> ... :<reason>`
            let Some(target) = args.get(1).cloned() else {
                let cm = core.casemap();
                dispatch_raw(core, msg, seq, None, cm);
                return;
            };
            let message = args
                .last()
                .cloned()
                .unwrap_or_else(|| format!("IRC error {code}"));

            let (cm, is_chan) = {
                let mut ws = core.state.write().unwrap();
                let is_c = ws.is_channel_name(&target);
                if code == 401 && !is_c {
                    ws.handle_quit(&target);
                }
                (ws.casemap, is_c)
            };

            let target_user_id = if !is_chan {
                if code == 401 {
                    core.users
                        .mark_quit(&target, seq, Some(message.clone()), false)
                        .map(|(t, _)| t.id)
                } else {
                    core.users.get_existing(&target).map(|t| t.id)
                }
            } else {
                None
            };

            let wire = Arc::new(WireEvent {
                seq,
                raw: Arc::new(msg.clone()),
                kind: WireKind::SendError {
                    target: target.clone(),
                    target_user_id,
                    code,
                    message,
                },
            });
            core.inboxes
                .dispatch(&wire, is_chan.then_some(target.as_str()), cm);
        }
        _ => {
            let cm = core.casemap();
            dispatch_raw(core, msg, seq, None, cm);
        }
    }
}

fn dispatch_raw(
    core: &Arc<BotCore>,
    msg: &Message,
    seq: u64,
    chan_hint: Option<&str>,
    cm: crate::casemap::CaseMap,
) {
    let wire = Arc::new(WireEvent {
        seq,
        raw: Arc::new(msg.clone()),
        kind: WireKind::RawOnly,
    });
    core.inboxes.dispatch(&wire, chan_hint, cm);
}
