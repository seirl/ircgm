use std::time::Duration;

use super::*;
use crate::casemap::AtomicCaseMap;
use crate::outbound::{OutboundSink, RawTransport};
use crate::state::parse_irc_message;

fn test_core_with_announce(
    out_tx: mpsc::UnboundedSender<Command>,
    announce_bot: bool,
) -> Arc<BotCore> {
    let casemap = AtomicCaseMap::default();
    let outbound = OutboundSink::new(RawTransport::test_only(out_tx));
    BotCore::new(
        "gm".into(),
        casemap,
        outbound,
        1024,
        vec!["!".into()],
        Duration::from_secs(15),
        450,
        announce_bot,
    )
}

fn test_core(out_tx: mpsc::UnboundedSender<Command>) -> Arc<BotCore> {
    test_core_with_announce(out_tx, true)
}

#[test]
fn is_services_pseudoclient_matches_case_insensitively() {
    assert!(is_services_pseudoclient("NickServ"));
    assert!(is_services_pseudoclient("nickserv"));
    assert!(is_services_pseudoclient("CHANSERV"));
    assert!(is_services_pseudoclient("SaslServ"));
    assert!(!is_services_pseudoclient("alice"));
    assert!(!is_services_pseudoclient("NickServer"));
}

#[test]
fn extract_numeric_handles_response_and_three_digit_raw() {
    let msg_resp = parse_irc_message(":srv 376 gm :End of /MOTD\r\n").unwrap();
    let (code, args) = extract_numeric(&msg_resp.command).unwrap();
    assert_eq!(code, 376);
    assert_eq!(args[0], "gm");

    let msg_716 = parse_irc_message(":srv 716 gm alice :is in +g mode\r\n").unwrap();
    let (code716, args716) = extract_numeric(&msg_716.command).unwrap();
    assert_eq!(code716, 716);
    assert_eq!(args716[1], "alice");

    let msg_priv = parse_irc_message(":a!b@c PRIVMSG #x :hi\r\n").unwrap();
    assert!(extract_numeric(&msg_priv.command).is_none());
}

#[test]
fn extract_tags_parses_account_and_bot_flags() {
    let msg = parse_irc_message(
        "@time=2026-04-06T00:00:01.000Z;account=AliceAcct;draft/bot :a!b@c PRIVMSG #x :hi\r\n",
    )
    .unwrap();
    let tags = extract_tags(&msg);
    assert_eq!(tags.raw_account.as_deref(), Some("AliceAcct"));
    assert!(tags.is_bot);

    let human_msg = parse_irc_message(":a!b@c PRIVMSG #x :hi\r\n").unwrap();
    let human_tags = extract_tags(&human_msg);
    assert!(human_tags.raw_account.is_none());
    assert!(!human_tags.is_bot);
}

#[tokio::test]
async fn multiline_cap_ack_waits_for_final_ack_and_motd_before_marking_registered() {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    let core = test_core(out_tx);

    let mut reg = RegistrationTracker {
        cap_pending: true,
        motd_done: false,
    };

    // Multiline continuation (`CAP gm ACK * :multi-prefix`) must NOT send `CAP END` yet:
    let ack_part1 = parse_irc_message(":srv CAP gm ACK * :multi-prefix\r\n").unwrap();
    process_message(&core, ack_part1, 1, &mut reg);
    assert!(reg.cap_pending);
    assert!(out_rx.try_recv().is_err());
    assert!(!core.is_registered());

    // Final `CAP gm ACK :server-time` sends `CAP END`, still waiting on MOTD:
    let ack_part2 = parse_irc_message(":srv CAP gm ACK :server-time\r\n").unwrap();
    process_message(&core, ack_part2, 2, &mut reg);
    assert!(!reg.cap_pending);
    assert!(!core.is_registered());
    assert_eq!(
        out_rx.try_recv().ok(),
        Some(Command::CAP(None, CapSubCommand::END, None, None))
    );

    // `422 ERR_NOMOTD` completes registration:
    let nomotd = parse_irc_message(":srv 422 gm :MOTD File is missing\r\n").unwrap();
    process_message(&core, nomotd, 3, &mut reg);
    assert!(reg.motd_done);
    assert!(core.is_registered());
}

#[tokio::test]
async fn self_echo_notice_updates_tracked_account_without_emitting_chat_event() {
    let (out_tx, _out_rx) = mpsc::unbounded_channel();
    let core = test_core(out_tx);
    let self_track = core.users.get_or_create("gm");
    assert_eq!(self_track.account(), None);

    let mut reg = RegistrationTracker {
        cap_pending: false,
        motd_done: true,
    };
    let notice =
        parse_irc_message("@account=GmAcct :gm!~gm@cloak NOTICE #lobby :status\r\n").unwrap();
    process_message(&core, notice, 1, &mut reg);

    assert_eq!(self_track.account().as_deref(), Some("GmAcct"));
    assert_eq!(
        core.state.read().unwrap().self_user_host().as_deref(),
        Some("~gm@cloak")
    );
}

#[tokio::test]
async fn isupport_bot_mode_announces_on_registration() {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    let core = test_core_with_announce(out_tx, true);
    let mut reg = RegistrationTracker {
        cap_pending: false,
        motd_done: false,
    };

    // Server assigns `gm_` in 001 and advertises `BOT=B` in 005:
    let welcome = parse_irc_message(":srv 001 gm_ :Welcome to IRC\r\n").unwrap();
    process_message(&core, welcome, 1, &mut reg);
    let isupport = parse_irc_message(":srv 005 gm_ BOT=B :are supported\r\n").unwrap();
    process_message(&core, isupport, 2, &mut reg);

    // Must not send `MODE` before registration finishes:
    assert!(out_rx.try_recv().is_err());

    // `376 RPL_ENDOFMOTD` completes registration and triggers `MODE gm_ +B` once:
    let endmotd = parse_irc_message(":srv 376 gm_ :End of /MOTD\r\n").unwrap();
    process_message(&core, endmotd, 3, &mut reg);
    assert!(core.is_registered());
    assert_eq!(
        out_rx.try_recv().ok(),
        Some(Command::Raw("MODE".into(), vec!["gm_".into(), "+B".into()]))
    );
    assert!(out_rx.try_recv().is_err());

    // Subsequent `005` lines do not re-emit `MODE`:
    let isupport2 = parse_irc_message(":srv 005 gm_ BOT=B :are supported\r\n").unwrap();
    process_message(&core, isupport2, 4, &mut reg);
    assert!(out_rx.try_recv().is_err());
}

#[tokio::test]
async fn announce_bot_false_skips_bot_mode() {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    let core = test_core_with_announce(out_tx, false);
    let mut reg = RegistrationTracker {
        cap_pending: false,
        motd_done: false,
    };

    let welcome = parse_irc_message(":srv 001 gm :Welcome\r\n").unwrap();
    process_message(&core, welcome, 1, &mut reg);
    let isupport = parse_irc_message(":srv 005 gm BOT=B :are supported\r\n").unwrap();
    process_message(&core, isupport, 2, &mut reg);
    let nomotd = parse_irc_message(":srv 422 gm :No MOTD\r\n").unwrap();
    process_message(&core, nomotd, 3, &mut reg);

    assert!(core.is_registered());
    assert!(out_rx.try_recv().is_err());
}
