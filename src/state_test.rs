use super::*;

fn parse_msg(raw: &str) -> Message {
    parse_irc_message(raw).expect("valid IRC message")
}

#[test]
fn isupport_parsing_and_targmax_variants() {
    let mut ws = WorldState::new("Bot", AtomicCaseMap::default());
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
    let casemap = AtomicCaseMap::default();
    let users = UserRegistry::new(casemap.clone());
    let mut ws = WorldState::new("Bot", casemap);

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
    assert!(!ws.channel("#c").unwrap().members().is_some());

    let end = parse_msg(":srv 366 Bot #c :End of /NAMES list.\r\n");
    if let Command::Response(Response::RPL_ENDOFNAMES, args) = &end.command {
        assert_eq!(ws.handle_endofnames(args).as_deref(), Some("#c"));
    }
    let chan = ws.channel("#c").unwrap();
    assert!(chan.members().is_some());
    assert_eq!(chan.members().unwrap().len(), 4);

    let alice = chan.member_by_nick("Alice").unwrap();
    assert_eq!(alice.prefixes, vec!['@', '+']);
    assert!(ws.occupant_is_op(alice));
    assert!(ws.occupant_is_voiced(alice));

    let carol = chan.member_by_nick("Carol").unwrap();
    assert!(!ws.occupant_is_op(carol));

    // Direct interleaved multi-word mode-block parsing in `apply_channel_mode_tokens`:
    ws.apply_channel_mode_tokens("#c", &["+klo", "secret", "50", "Carol", "-v", "Bob"], &users);

    let chan = ws.channel("#c").unwrap();
    let carol = chan.member_by_nick("Carol").unwrap();
    assert!(ws.occupant_is_op(carol));
    assert_eq!(carol.prefixes, vec!['@']);

    let bob = chan.member_by_nick("Bob").unwrap();
    assert!(!ws.occupant_is_voiced(bob));
    assert!(bob.prefixes.is_empty());

    // Live/test `Command::ChannelMODE` parsing handles combined `+`/`-` flags (`MODE #c -o+v Carol Bob`):
    let mode_msg = parse_msg(":Bot!~bot@initial.host MODE #c -o+v Carol Bob\r\n");
    assert!(matches!(mode_msg.command, Command::ChannelMODE(..)));
    ws.apply_mode_message(&mode_msg, &users);
    let chan = ws.channel("#c").unwrap();
    assert!(!ws.occupant_is_op(chan.member_by_nick("Carol").unwrap()));
    assert!(ws.occupant_is_voiced(chan.member_by_nick("Bob").unwrap()));

    let m_iso = parse_msg(":srv 005 Bot CHANMODES=eIbq,k,flj,imnpst :are supported\r\n");
    if let Command::Response(Response::RPL_ISUPPORT, args) = &m_iso.command {
        ws.apply_isupport(args);
    }
    let custom_mode_324 = parse_msg(":srv 324 Bot #c +fo 5:2 Bob\r\n");
    ws.apply_mode_message(&custom_mode_324, &users);
    let bob = ws.channel("#c").unwrap()
        .member_by_nick("Bob")
        .unwrap();
    assert!(ws.occupant_is_op(bob));

    // `Command::ChannelMODE` re-tokenization fixes `irc-proto`'s `-l` arg consumption (`MODE #c -lo Bob`):
    let live_parsed: Message = parse_msg(":Bot!~bot@initial.host MODE #c -lo Bob\r\n");
    assert!(matches!(live_parsed.command, Command::ChannelMODE(..)));
    ws.apply_mode_message(&live_parsed, &users);
    let bob = ws.channel("#c").unwrap()
        .member_by_nick("Bob")
        .unwrap();
    assert!(!ws.occupant_is_op(bob));
}

#[test]
fn topic_cloak_396_self_leave_and_dynamic_casemapping_rekey() {
    let casemap = AtomicCaseMap::default();
    let users = UserRegistry::new(casemap.clone());
    let mut ws = WorldState::new("Bot", casemap);

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
    ws.handle_join("#[Room]", "[Alice]", at.clone(), None);
    ws.handle_topic("#[Room]", Some("Welcome!".into()));
    assert_eq!(
        ws.channel("#{room}").unwrap().topic.as_deref(),
        Some("Welcome!")
    );
    ws.handle_topic("#{room}", None);
    assert_eq!(ws.channel("#{room}").unwrap().topic, None);

    let iso = parse_msg(":srv 005 Bot CASEMAPPING=ascii :are supported\r\n");
    if let Command::Response(Response::RPL_ISUPPORT, args) = &iso.command {
        assert_eq!(ws.apply_isupport(args), Some(CaseMap::Ascii));
        users.rekey_casemap(CaseMap::Ascii);
    }
    assert!(ws.channel("#[room]").is_some());
    assert!(ws.channel("#{room}").is_none());
    let ch = ws.channel("#[room]").unwrap();
    assert!(ch.roster().unwrap().contains_key(&at.id));
    assert!(ch.member_by_nick("[alice]").is_some());
    assert!(ch.member_by_nick("{alice}").is_none());

    // Self-leave transitions the channel to `Left`:
    assert!(ws.handle_part_or_kick("#[Room]", "Bot", bt.id, Some("kicked".into()), 10, false));
    assert_eq!(
        ws.channel("#[room]").and_then(|c| c.left_at_seq()),
        Some(10)
    );
    assert!(ws.active_channel("#[room]").is_none());
    // Rejoin restarts `Joining`:
    ws.handle_join("#[Room]", "Bot", bt, None);
    assert!(ws.active_channel("#[room]").is_some());
}

#[test]
fn names_staging_tracks_interleaved_nick_join_part_quit_and_modes() {
    let casemap = AtomicCaseMap::default();
    let users = UserRegistry::new(casemap.clone());
    let mut ws = WorldState::new("Bot", casemap);

    let bot_track = users.get_or_create("Bot");
    let j = parse_msg(":Bot!~bot@h JOIN #c\r\n");
    ws.handle_join("#c", "Bot", Arc::clone(&bot_track), j.prefix.as_ref());

    // First 353 batch stages Bot (@), Alice, Bob, Carol:
    let n1 = parse_msg(":srv 353 Bot = #c :@Bot Alice Bob Carol\r\n");
    if let Command::Response(Response::RPL_NAMREPLY, args) = &n1.command {
        ws.handle_namreply(args, &users);
    }
    let bob_track = users.get_or_create("Bob");
    let carol_track = users.get_or_create("Carol");

    // Before 366 arrives:
    // - Alice renames to Alicia
    // - Bot itself renames to Bot2
    // - Bob parts #c, Carol quits IRC
    // - Eve joins #c
    // - Alicia receives +v
    let alice_track = users.rename("Alice", "Alicia");
    assert_eq!(
        ws.handle_nick(alice_track.id, "Alice", "Alicia", None),
        vec!["#c".to_owned()]
    );
    users.rename("Bot", "Bot2");
    ws.handle_nick(bot_track.id, "Bot", "Bot2", None);
    assert!(!ws.handle_part_or_kick("#c", "Bob", bob_track.id, None, 20, false));
    assert_eq!(ws.handle_quit(carol_track.id), vec!["#c".to_owned()]);
    let eve_track = users.get_or_create("Eve");
    ws.handle_join("#c", "Eve", Arc::clone(&eve_track), None);
    let mode_msg = parse_msg(":Bot2!~bot@h MODE #c +v Alicia\r\n");
    ws.apply_mode_message(&mode_msg, &users);

    // Commit on 366:
    let end = parse_msg(":srv 366 Bot2 #c :End of /NAMES list.\r\n");
    if let Command::Response(Response::RPL_ENDOFNAMES, args) = &end.command {
        assert_eq!(ws.handle_endofnames(args).as_deref(), Some("#c"));
    }

    let chan = ws.channel("#c").unwrap();
    assert!(chan.members().is_some());
    assert!(chan.member_by_nick("Alice").is_none());
    assert!(!chan.members().unwrap().contains_key(&bob_track.id));
    assert!(!chan.members().unwrap().contains_key(&carol_track.id));

    let bot_occ = &chan.members().unwrap()[&bot_track.id];
    assert_eq!(bot_occ.track.nick(), "Bot2");
    assert!(ws.occupant_is_op(bot_occ));

    let alicia_occ = &chan.members().unwrap()[&alice_track.id];
    assert_eq!(alicia_occ.track.nick(), "Alicia");
    assert!(ws.occupant_is_voiced(alicia_occ));

    let eve_occ = &chan.members().unwrap()[&eve_track.id];
    assert_eq!(eve_occ.track.id, eve_track.id);
}

#[test]
fn joining_phase_preserves_join_arrivals_before_first_353() {
    let casemap = AtomicCaseMap::default();
    let users = UserRegistry::new(casemap.clone());
    let mut ws = WorldState::new("Bot", casemap);

    let bot_track = users.get_or_create("Bot");
    ws.handle_join("#c", "Bot", Arc::clone(&bot_track), None);
    assert!(ws.active_channel("#c").unwrap().members().is_none());

    // Eve's `JOIN` races ahead of the server's `353` (which was generated before she joined):
    let eve_track = users.get_or_create("Eve");
    ws.handle_join("#c", "Eve", Arc::clone(&eve_track), None);
    let n = parse_msg(":srv 353 Bot = #c :@Bot Alice\r\n");
    if let Command::Response(Response::RPL_NAMREPLY, args) = &n.command {
        ws.handle_namreply(args, &users);
    }
    let end = parse_msg(":srv 366 Bot #c :End of /NAMES list.\r\n");
    if let Command::Response(Response::RPL_ENDOFNAMES, args) = &end.command {
        assert_eq!(ws.handle_endofnames(args).as_deref(), Some("#c"));
    }

    let members = ws.channel("#c").unwrap().members().unwrap();
    assert_eq!(members.len(), 3);
    assert!(members.contains_key(&bot_track.id));
    assert!(members.contains_key(&eve_track.id));
    assert!(members.contains_key(&users.get_or_create("Alice").id));
}
