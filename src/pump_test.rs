use std::sync::atomic::Ordering;

use super::*;
use crate::bot::BotConfig;
use crate::outbound::{OutboundSink, RawTransport};
use crate::state::parse_irc_message;

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
fn extract_tags_parses_server_time_account_and_bot_flags() {
    let msg = parse_irc_message(
        "@time=2026-04-06T00:00:01.000Z;account=AliceAcct;draft/bot :a!b@c PRIVMSG #x :hi\r\n",
    )
    .unwrap();
    let tags = extract_tags(&msg);
    assert!(tags.server_time.is_some());
    assert_eq!(tags.raw_account.as_deref(), Some("AliceAcct"));
    assert!(tags.is_bot);

    let human_msg = parse_irc_message(":a!b@c PRIVMSG #x :hi\r\n").unwrap();
    let human_tags = extract_tags(&human_msg);
    assert!(human_tags.server_time.is_none());
    assert!(human_tags.raw_account.is_none());
    assert!(!human_tags.is_bot);
}

#[tokio::test]
async fn multiline_cap_ack_waits_for_final_ack_and_motd_before_marking_registered() {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    let cfg = BotConfig {
        outbound_burst: usize::MAX,
        ..BotConfig::default()
    };
    let (outbound, _task) = OutboundSink::new(RawTransport::Test(out_tx), usize::MAX, cfg.outbound_window);
    let core = BotCore::new("gm".into(), outbound, true, &cfg);

    let mut reg = RegistrationTracker {
        cap_pending: true,
        motd_done: false,
    };

    // Multiline continuation (`CAP gm ACK * :multi-prefix`) must NOT send `CAP END` yet:
    let ack_part1 = parse_irc_message(":srv CAP gm ACK * :multi-prefix\r\n").unwrap();
    process_message(&core, ack_part1, 1, &mut reg);
    assert!(reg.cap_pending);
    assert!(out_rx.try_recv().is_err());
    assert!(!core.registered_flag.load(Ordering::Acquire));

    // Final `CAP gm ACK :server-time` sends `CAP END`, still waiting on MOTD:
    let ack_part2 = parse_irc_message(":srv CAP gm ACK :server-time\r\n").unwrap();
    process_message(&core, ack_part2, 2, &mut reg);
    assert!(!reg.cap_pending);
    assert!(!core.registered_flag.load(Ordering::Acquire));
    assert_eq!(
        out_rx.try_recv().ok(),
        Some(Command::CAP(None, CapSubCommand::END, None, None))
    );

    // `422 ERR_NOMOTD` completes registration:
    let nomotd = parse_irc_message(":srv 422 gm :MOTD File is missing\r\n").unwrap();
    process_message(&core, nomotd, 3, &mut reg);
    assert!(reg.motd_done);
    assert!(core.registered_flag.load(Ordering::Acquire));
}
