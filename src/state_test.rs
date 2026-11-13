use super::*;

fn parse_msg(raw: &str) -> Message {
    parse_irc_message(raw).expect("valid IRC message")
}

#[test]
fn isupport_parsing_and_targmax_variants() {
    let mut ws = WorldState::new("Bot");
    let m1 = parse_msg(
        ":srv 005 Bot PREFIX=(qaohv)~&@%+ MODES=6 TARGMAX=PRIVMSG:4,KICK:1 CHANMODES=eIbq,k,flj,CFLMPQScgimnprstuz CASEMAPPING=strict-rfc1459 :are supported\r\n",
    );
    if let Command::Response(Response::RPL_ISUPPORT, args) = &m1.command {
        let changed = ws.apply_isupport(args);
        assert_eq!(changed, Some(CaseMap::StrictRfc1459));
    } else {
        panic!("expected 005");
    }
    assert_eq!(ws.max_modes, 6);
    assert_eq!(ws.targmax_kick, 1);
    assert_eq!(
        ws.status_prefixes,
        vec![('q', '~'), ('a', '&'), ('o', '@'), ('h', '%'), ('v', '+')]
    );
    assert_eq!(ws.chan_modes.param_set_c, "flj");

    let m2 = parse_msg(":srv 005 Bot TARGMAX=PRIVMSG:4,KICK:3 :are supported\r\n");
    if let Command::Response(Response::RPL_ISUPPORT, args) = &m2.command {
        assert_eq!(ws.apply_isupport(args), None);
    }
    assert_eq!(ws.targmax_kick, 3);
}

#[test]
fn multiline_names_multi_prefix_userhost_and_parameterized_modes() {
    let users = UserRegistry::default();
    let mut ws = WorldState::new("Bot");

    let j = parse_msg(":Bot!~bot@initial.host JOIN #c\r\n");
    let bot_track = users.get_or_create("Bot");
    ws.handle_join("#c", "Bot", bot_track, j.prefix.as_ref());
    assert_eq!(ws.self_user_host().as_deref(), Some("~bot@initial.host"));

    let n1 = parse_msg(":srv 353 Bot = #c :@Bot!~bot@initial.host @+Alice!~a@a.host\r\n");
    let n2 = parse_msg(":srv 353 Bot = #c :+Bob!~b@b.host Carol\r\n");
    if let Command::Response(Response::RPL_NAMREPLY, args) = &n1.command {
        ws.handle_namreply(args, &users);
    }
    if let Command::Response(Response::RPL_NAMREPLY, args) = &n2.command {
        ws.handle_namreply(args, &users);
    }
    assert!(!ws.channels[&ws.casemap.fold("#c")].names_synced);

    let end = parse_msg(":srv 366 Bot #c :End of /NAMES list.\r\n");
    if let Command::Response(Response::RPL_ENDOFNAMES, args) = &end.command {
        assert_eq!(ws.handle_endofnames(args).as_deref(), Some("#c"));
    }
    let chan = &ws.channels[&ws.casemap.fold("#c")];
    assert!(chan.names_synced);
    assert_eq!(chan.members.len(), 4);

    let alice = &chan.members[&ws.casemap.fold("Alice")];
    assert_eq!(alice.prefixes, vec!['@', '+']);
    assert!(ws.occupant_is_op(alice));
    assert!(ws.occupant_is_voiced(alice));

    let carol = &chan.members[&ws.casemap.fold("Carol")];
    assert!(!ws.occupant_is_op(carol));

    let mode_msg = parse_msg(":Bot!~bot@initial.host MODE #c +klo secret 50 Carol -v Bob\r\n");
    ws.apply_mode_message(&mode_msg);

    let chan = &ws.channels[&ws.casemap.fold("#c")];
    let carol = &chan.members[&ws.casemap.fold("Carol")];
    assert!(ws.occupant_is_op(carol));
    assert_eq!(carol.prefixes, vec!['@']);

    let bob = &chan.members[&ws.casemap.fold("Bob")];
    assert!(!ws.occupant_is_voiced(bob));
    assert!(bob.prefixes.is_empty());

    let m_iso = parse_msg(":srv 005 Bot CHANMODES=eIbq,k,flj,imnpst :are supported\r\n");
    if let Command::Response(Response::RPL_ISUPPORT, args) = &m_iso.command {
        ws.apply_isupport(args);
    }
    let custom_mode = parse_msg(":Bot!~bot@initial.host MODE #c +fo 5:2 Bob\r\n");
    ws.apply_mode_message(&custom_mode);
    let bob = &ws.channels[&ws.casemap.fold("#c")].members[&ws.casemap.fold("Bob")];
    assert!(ws.occupant_is_op(bob));
}

#[test]
fn topic_cloak_396_self_leave_and_dynamic_casemapping_rekey() {
    let users = UserRegistry::default();
    let mut ws = WorldState::new("Bot");

    let w = parse_msg(":srv 001 Bot :Welcome to IRC Bot!~bot@10.0.0.1\r\n");
    if let Command::Response(Response::RPL_WELCOME, args) = &w.command {
        ws.handle_welcome(args);
    }
    assert_eq!(ws.self_user_host().as_deref(), Some("~bot@10.0.0.1"));

    let vhost = "a".repeat(60);
    let cloak = parse_msg(&format!(":srv 396 Bot {vhost} :is now your displayed host\r\n"));
    if let Command::Response(Response::RPL_HOSTHIDDEN, args) = &cloak.command {
        ws.handle_host_hidden(args);
    }
    assert_eq!(ws.relayed_user_host_len(), "~bot".len() + 1 + 60);

    let bt = users.get_or_create("Bot");
    let at = users.get_or_create("[Alice]");
    ws.handle_join("#[Room]", "Bot", bt.clone(), None);
    ws.handle_join("#[Room]", "[Alice]", at, None);
    ws.handle_topic("#[Room]", Some("Welcome!".into()));
    assert_eq!(
        ws.channels[&ws.casemap.fold("#{room}")].topic.as_deref(),
        Some("Welcome!")
    );
    ws.handle_topic("#{room}", None);
    assert_eq!(ws.channels[&ws.casemap.fold("#{room}")].topic, None);

    let iso = parse_msg(":srv 005 Bot CASEMAPPING=ascii :are supported\r\n");
    if let Command::Response(Response::RPL_ISUPPORT, args) = &iso.command {
        assert_eq!(ws.apply_isupport(args), Some(CaseMap::Ascii));
    }
    assert!(ws.channels.contains_key(&CaseMap::Ascii.fold("#[room]")));
    assert!(!ws.channels.contains_key(&CaseMap::Ascii.fold("#{room}")));
    let ch = &ws.channels[&CaseMap::Ascii.fold("#[room]")];
    assert!(ch.members.contains_key(&CaseMap::Ascii.fold("[alice]")));

    // Self-leave records `lost_channels`:
    assert!(ws.handle_part_or_kick("#[Room]", "Bot", Some("kicked".into())));
    assert!(ws.lost_channels.contains_key(&CaseMap::Ascii.fold("#[room]")));
    // Rejoin clears `lost_channels`:
    ws.handle_join("#[Room]", "Bot", bt, None);
    assert!(!ws.lost_channels.contains_key(&CaseMap::Ascii.fold("#[room]")));
}
