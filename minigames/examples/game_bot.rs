//! Playable multi-minigame single-lobby IRC bot (`cargo run -p minigames --example game_bot`).
//!
//! Demonstrates **external supervision**: none of the minigames in [`minigames`] know about
//! admins, `!stop`, `!abort`, or [`Bot::shutdown_requested`]. Instead, [`run_lobby_bot`] wraps
//! [`minigames::play_by_name`] from the outside with `.until(...)` / `.or_until(...)`, relying on
//! RAII `Drop` (`Breakout`, `Huddle`, `ExclusiveDm`, `Inbox`) to clean up any cancelled game.
//!
//! Run against Libera.Chat (or any IRC server):
//! ```sh
//! cargo run -p minigames --example game_bot -- --server irc.libera.chat:6667 --lobby '#ircgm-minigames'
//! ```
//!
//! Anyone in the lobby can list and start any of the 8 [`minigames`]:
//! - `!games` / `!help` — list all minigames and lobby commands
//! - `!play <game>` / `!start <game>` / `!<game>` — start a specific minigame
//! - `!random` / `!play random` — roll and start a random minigame
//! - `!abort` — cancel the active game (by its starter in lobby/DM, or anyone if the starter left)

#![forbid(unsafe_code)]

use std::process::ExitCode;

use ircgm::irc::client::data::Config as IrcConfig;
use ircgm::prelude::*;
use minigames::{play_by_name, PlayOutcome, GAMES};

const DEFAULT_SERVER: &str = "127.0.0.1:6667";
const DEFAULT_LOBBY: &str = "#ircgm-minigames";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BotCli {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) use_tls: bool,
    pub(crate) lobby: String,
    pub(crate) lobby_key: Option<String>,
    pub(crate) nick: Option<String>,
    pub(crate) password: Option<String>,
    pub(crate) admins: Vec<String>,
    pub(crate) verbose: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ParseOutcome {
    Run(BotCli),
    Help(String),
}

fn usage_text(bin: &str) -> String {
    let mut games_lines = String::new();
    for (slug, desc) in GAMES {
        games_lines.push_str(&format!("  {slug:<12} {desc}\n"));
    }
    format!(
        "\
Usage: {bin} [OPTIONS]

Single-lobby IRC test bot that lets anyone in the channel start all {count} ircgm minigames.

Options:
  -s, --server <HOST[:PORT]>   IRC server address (default: {DEFAULT_SERVER}; supports irc:// and ircs://)
  -p, --port <PORT>            Override IRC server port (default: 6697 with TLS, 6667 without TLS)
  -l, --lobby <CHANNEL>        Lobby channel to join (default: {DEFAULT_LOBBY})
      --lobby-key <KEY>        Optional channel key (+k) for joining --lobby
  -n, --nick <NICK>            Bot nickname (default: ircgm-<random4>)
      --password <PASS>        Optional server PASS (on Libera.Chat: <account>:<password> for NickServ)
      --tls / --no-tls         Force TLS on or off (auto-enabled for ircs:// and ports 6697/7000/7070)
      --admin <ACCOUNT>        Services account allowed to force `!stop` / `!quit` (repeatable)
  -v, --verbose                Enable debug-level tracing output
  -h, --help                   Show this help message

Lobby commands (anyone can use):
  !games, !help                Show all playable minigames
  !play <game>, !<game>        Start a minigame in the lobby
  !random                      Roll and start a random minigame
  !abort                       Cancel the running game (by starter, or anyone if starter left)

Available minigames:
{games_lines}\
Example:
  cargo run -p minigames --example game_bot -- --server irc.libera.chat:6667 --lobby '#ircgm-minigames'",
        count = GAMES.len(),
    )
}

fn parse_server_spec(spec: &str) -> std::result::Result<(String, Option<u16>, Option<bool>), String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err("--server cannot be empty".into());
    }

    let (scheme_tls, rest) = if let Some(r) = trimmed.strip_prefix("ircs://") {
        (Some(true), r)
    } else if let Some(r) = trimmed.strip_prefix("irc://") {
        (Some(false), r)
    } else {
        (None, trimmed)
    };
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() {
        return Err(format!("invalid --server `{spec}`: missing host"));
    }

    // Bracketed IPv6: `[::1]:6667` or `[::1]`
    if let Some(after_open) = rest.strip_prefix('[') {
        let Some((ipv6_host, tail)) = after_open.split_once(']') else {
            return Err(format!("invalid IPv6 address in --server `{spec}`"));
        };
        if ipv6_host.is_empty() {
            return Err(format!("empty IPv6 host in --server `{spec}`"));
        }
        let port = if tail.is_empty() {
            None
        } else if let Some(port_str) = tail.strip_prefix(':') {
            let p = port_str
                .parse::<u16>()
                .map_err(|_| format!("invalid port `{port_str}` in --server `{spec}`"))?;
            Some(p)
        } else {
            return Err(format!("unexpected trailing `{tail}` in --server `{spec}`"));
        };
        return Ok((ipv6_host.to_owned(), port, scheme_tls));
    }

    // Unbracketed `host:port` (single colon) vs bare host / raw IPv6:
    if let Some((host_part, port_part)) = rest.rsplit_once(':') {
        if !host_part.contains(':') {
            if host_part.is_empty() {
                return Err(format!("missing host before `:` in --server `{spec}`"));
            }
            let p = port_part
                .parse::<u16>()
                .map_err(|_| format!("invalid port `{port_part}` in --server `{spec}`"))?;
            return Ok((host_part.to_owned(), Some(p), scheme_tls));
        }
    }

    Ok((rest.to_owned(), None, scheme_tls))
}

fn normalize_channel(raw: &str) -> std::result::Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("--lobby channel cannot be empty".into());
    }
    if trimmed.chars().any(char::is_whitespace) || trimmed.contains(',') {
        return Err(format!(
            "invalid --lobby `{trimmed}`: must be a single channel name without spaces or commas"
        ));
    }
    if trimmed.starts_with(['#', '&', '+', '!']) {
        Ok(trimmed.to_owned())
    } else {
        Ok(format!("#{trimmed}"))
    }
}

pub(crate) fn parse_cli<I, S>(args: I) -> std::result::Result<ParseOutcome, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut iter = args.into_iter().map(Into::into);
    let bin = iter.next().unwrap_or_else(|| "game_bot".to_owned());

    let mut raw_server: Option<String> = None;
    let mut port_override: Option<u16> = None;
    let mut tls_override: Option<bool> = None;
    let mut lobby: Option<String> = None;
    let mut lobby_key: Option<String> = None;
    let mut nick: Option<String> = None;
    let mut password: Option<String> = None;
    let mut admins: Vec<String> = Vec::new();
    let mut verbose = false;

    let next_val = |flag: &str, it: &mut dyn Iterator<Item = String>| -> std::result::Result<String, String> {
        it.next()
            .ok_or_else(|| format!("missing value for `{flag}`"))
    };

    while let Some(arg) = iter.next() {
        if arg == "-h" || arg == "--help" {
            return Ok(ParseOutcome::Help(usage_text(&bin)));
        } else if arg == "-v" || arg == "--verbose" {
            verbose = true;
        } else if arg == "--tls" {
            tls_override = Some(true);
        } else if arg == "--no-tls" {
            tls_override = Some(false);
        } else if arg == "-s" || arg == "--server" {
            raw_server = Some(next_val(&arg, &mut iter)?);
        } else if let Some(v) = arg.strip_prefix("--server=") {
            raw_server = Some(v.to_owned());
        } else if arg == "-p" || arg == "--port" {
            let v = next_val(&arg, &mut iter)?;
            let p = v
                .parse::<u16>()
                .map_err(|_| format!("invalid port `{v}` for `{arg}`"))?;
            port_override = Some(p);
        } else if let Some(v) = arg.strip_prefix("--port=") {
            let p = v
                .parse::<u16>()
                .map_err(|_| format!("invalid port `{v}` for `--port`"))?;
            port_override = Some(p);
        } else if arg == "-l" || arg == "--lobby" || arg == "--channel" {
            lobby = Some(next_val(&arg, &mut iter)?);
        } else if let Some(v) = arg
            .strip_prefix("--lobby=")
            .or_else(|| arg.strip_prefix("--channel="))
        {
            lobby = Some(v.to_owned());
        } else if arg == "--lobby-key" {
            lobby_key = Some(next_val(&arg, &mut iter)?);
        } else if let Some(v) = arg.strip_prefix("--lobby-key=") {
            lobby_key = Some(v.to_owned());
        } else if arg == "-n" || arg == "--nick" {
            let v = next_val(&arg, &mut iter)?;
            if v.trim().is_empty() {
                return Err("--nick cannot be empty".into());
            }
            nick = Some(v.trim().to_owned());
        } else if let Some(v) = arg.strip_prefix("--nick=") {
            if v.trim().is_empty() {
                return Err("--nick cannot be empty".into());
            }
            nick = Some(v.trim().to_owned());
        } else if arg == "--password" || arg == "--pass" {
            password = Some(next_val(&arg, &mut iter)?);
        } else if let Some(v) = arg
            .strip_prefix("--password=")
            .or_else(|| arg.strip_prefix("--pass="))
        {
            password = Some(v.to_owned());
        } else if arg == "--admin" {
            let v = next_val(&arg, &mut iter)?;
            if !v.trim().is_empty() {
                admins.push(v.trim().to_owned());
            }
        } else if let Some(v) = arg.strip_prefix("--admin=") {
            if !v.trim().is_empty() {
                admins.push(v.trim().to_owned());
            }
        } else {
            return Err(format!("unknown argument `{arg}` (pass --help for usage)"));
        }
    }

    let server_str = raw_server.as_deref().unwrap_or(DEFAULT_SERVER);
    let (host, parsed_port, scheme_tls) = parse_server_spec(server_str)?;

    let is_tls_port = |p: u16| matches!(p, 6697 | 7000 | 7070);
    let port = match (port_override, parsed_port) {
        (Some(p), _) => p,
        (None, Some(p)) => p,
        (None, None) => {
            if tls_override.or(scheme_tls).unwrap_or(false) {
                6697
            } else {
                6667
            }
        }
    };
    let use_tls = tls_override
        .or(scheme_tls)
        .unwrap_or_else(|| is_tls_port(port));

    let lobby_chan = normalize_channel(lobby.as_deref().unwrap_or(DEFAULT_LOBBY))?;

    Ok(ParseOutcome::Run(BotCli {
        host,
        port,
        use_tls,
        lobby: lobby_chan,
        lobby_key: lobby_key.filter(|k| !k.is_empty()),
        nick,
        password: password.filter(|p| !p.is_empty()),
        admins,
        verbose,
    }))
}

fn random_suffix_4() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(4);
    for _ in 0..4 {
        out.push(ALPHABET[fastrand::usize(..ALPHABET.len())] as char);
    }
    out
}

/// Resolves a user-supplied game name or alias to a canonical slug in [`GAMES`].
pub(crate) fn resolve_game_slug(raw: &str) -> Option<&'static str> {
    let token = raw.split_whitespace().next()?;
    let norm = token
        .trim_start_matches('!')
        .to_ascii_lowercase()
        .replace('-', "_");
    match norm.as_str() {
        "bj" => Some("blackjack"),
        "coldwar" => Some("cold_war"),
        "werewolf" | "wolves" => Some("mafia"),
        "simon" | "simonsays" => Some("simon_says"),
        other => GAMES
            .iter()
            .find(|(slug, _)| *slug == other)
            .map(|(slug, _)| *slug),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LobbyAction {
    ListGames,
    PlayUsage,
    StartGame(&'static str),
    StartRandom,
    UnknownGame(String),
}

/// Classifies an incoming lobby chat message into an optional [`LobbyAction`].
pub(crate) fn classify_lobby_command(msg: &ChatMessage) -> Option<LobbyAction> {
    if msg.is_cmd("!games") || msg.is_cmd("!help") {
        return Some(LobbyAction::ListGames);
    }
    if msg.is_cmd("!random") {
        return Some(LobbyAction::StartRandom);
    }
    if let Some(args) = msg.cmd_args("!play").or_else(|| msg.cmd_args("!start")) {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            return Some(LobbyAction::PlayUsage);
        }
        let first = trimmed.split_whitespace().next().unwrap_or(trimmed);
        if first.eq_ignore_ascii_case("random") {
            return Some(LobbyAction::StartRandom);
        }
        return match resolve_game_slug(first) {
            Some(slug) => Some(LobbyAction::StartGame(slug)),
            None => Some(LobbyAction::UnknownGame(first.to_owned())),
        };
    }

    // Direct `!<slug>` shortcut for known games (e.g. `!blackjack`, `!cold-war`, `!simon_says`):
    let trimmed = msg.text.trim_start();
    if let Some(after_bang) = trimmed.strip_prefix('!') {
        let cmd_word = after_bang.split_whitespace().next().unwrap_or("");
        if !cmd_word.is_empty() {
            if let Some(slug) = resolve_game_slug(cmd_word) {
                if msg.is_cmd(&format!("!{cmd_word}")) {
                    return Some(LobbyAction::StartGame(slug));
                }
            }
        }
    }

    None
}

fn send_games_catalog(lobby: &Room) -> Result<()> {
    let entries: Vec<String> = GAMES
        .iter()
        .map(|(slug, desc)| format!("{slug} ({desc})"))
        .collect();
    let mid = entries.len().div_ceil(2);
    lobby.say(
        "🎮 ".bold()
            + "Minigames".bold().cyan()
            + " — start any game with "
            + "!play <name>".bold().green()
            + ", "
            + "!<name>".bold().green()
            + ", or "
            + "!random".bold().green()
            + " (`!abort` cancels):",
    )?;
    lobby.say(format!("  • {}", entries[..mid].join(" | ")))?;
    lobby.say(format!("  • {}", entries[mid..].join(" | ")))?;
    Ok(())
}

fn game_slug_list() -> String {
    GAMES
        .iter()
        .map(|(slug, _)| *slug)
        .collect::<Vec<_>>()
        .join(", ")
}

fn is_admin_msg(msg: &ChatMessage, admins: &[String]) -> bool {
    let Some(acct) = msg.account.as_deref() else {
        return false;
    };
    admins.iter().any(|a| a.eq_ignore_ascii_case(acct))
}

enum StopReason {
    Shutdown,
    Admin(Box<ChatMessage>),
    Aborted(User),
}

async fn busy_lobby_responder(lobby: &Room, active_slug: &str, starter: &User) -> Result<()> {
    let mut inbox = lobby.inbox().lossy();
    loop {
        let msg = inbox.next_chat().await?;
        match classify_lobby_command(&msg) {
            Some(LobbyAction::ListGames) => {
                send_games_catalog(lobby)?;
                lobby.say(
                    format!("⏳ Currently running `{active_slug}` (started by ")
                        + starter.nick().bold()
                        + ").",
                )?;
            }
            Some(
                LobbyAction::PlayUsage
                | LobbyAction::StartGame(_)
                | LobbyAction::StartRandom
                | LobbyAction::UnknownGame(_),
            ) => {
                let abort_hint = if lobby.has_user(starter) && starter.is_online() {
                    format!("{} can type `!abort`", starter.nick())
                } else {
                    "type `!abort`".to_owned()
                };
                lobby.say(
                    format!("⏳ `{active_slug}` is already running ({abort_hint} to cancel)!")
                        .yellow(),
                )?;
            }
            None => {}
        }
    }
}

/// Runs the single-lobby minigames loop until `bot.shutdown_requested()` or fatal disconnect.
pub(crate) async fn run_lobby_bot(bot: &Bot, lobby: &Room, admins: &[String]) -> Result<()> {
    lobby.say(
        "🎮 ".bold()
            + "Minigames bot ready!".bold().green()
            + " Anyone can start: "
            + "!play <game>".bold().cyan()
            + ", "
            + "!random".bold().cyan()
            + ", or "
            + "!games".bold().cyan()
            + " ("
            + game_slug_list()
            + ").",
    )?;

    loop {
        // Open a fresh lobby inbox only while idle so messages sent during an active game
        // never backlog or replay after the game ends.
        let idle_msg = {
            let mut inbox = lobby.inbox();
            match inbox.next_chat().until(bot.shutdown_requested()).await {
                Outcome::Completed(res) => res?,
                Outcome::Stopped(()) => break,
            }
        };

        let slug: &'static str = match classify_lobby_command(&idle_msg) {
            None => continue,
            Some(LobbyAction::ListGames) => {
                send_games_catalog(lobby)?;
                continue;
            }
            Some(LobbyAction::PlayUsage) => {
                lobby.say(
                    "Usage: ".bold()
                        + "`!play <game>` or `!random` — games: "
                        + game_slug_list(),
                )?;
                continue;
            }
            Some(LobbyAction::UnknownGame(bad)) => {
                lobby.say(
                    format!("Unknown game `{bad}` — available: {}.", game_slug_list()).yellow(),
                )?;
                continue;
            }
            Some(LobbyAction::StartRandom) => {
                let (picked, _) = GAMES[fastrand::usize(..GAMES.len())];
                lobby.say(
                    "🎲 ".bold()
                        + idle_msg.sender.nick().bold()
                        + " rolled "
                        + picked.bold().cyan()
                        + "!",
                )?;
                picked
            }
            Some(LobbyAction::StartGame(picked)) => picked,
        };

        let starter = idle_msg.sender.clone();
        tracing::info!(
            game = slug,
            starter = %starter.nick(),
            lobby = %lobby.name(),
            "starting minigame"
        );

        let admin_list = admins.to_vec();
        let lobby_for_admin = lobby.clone();
        let admin_stop = bot
            .inbox_all()
            .on_for(move |m| {
                is_admin_msg(m, &admin_list)
                    && m.is_cmd("!stop")
                    && (m.is_dm() || m.is_in(&lobby_for_admin))
            })
            .map_output(|m| StopReason::Admin(Box::new(m)));

        let abort_scope = InboxScope::new().room(lobby).dm_from(&starter);
        let starter_for_abort = starter.clone();
        let lobby_for_abort = lobby.clone();
        let abort_stop = bot.inbox_scope(abort_scope).on_map(move |m| {
            if !(m.is_cmd("!abort") || m.is_cmd("!stop")) {
                return None;
            }
            let is_starter = m.sender == starter_for_abort;
            let starter_gone =
                !starter_for_abort.is_online() || !lobby_for_abort.has_user(&starter_for_abort);
            if is_starter || (starter_gone && m.is_in(&lobby_for_abort)) {
                Some(StopReason::Aborted(m.sender.clone()))
            } else {
                None
            }
        });

        let game_with_busy_hint = async {
            tokio::select! {
                biased;
                res = play_by_name(slug, bot, lobby) => res,
                res = busy_lobby_responder(lobby, slug, &starter) => {
                    res.map(|()| Some(PlayOutcome::Completed))
                }
            }
        };

        let outcome = game_with_busy_hint
            .until(bot.shutdown_requested().map_output(|()| StopReason::Shutdown))
            .or_until(admin_stop)
            .or_until(abort_stop)
            .await;

        match outcome {
            Outcome::Completed(Ok(Some(PlayOutcome::Completed))) => {
                tracing::info!(game = slug, "minigame completed");
                lobby.say(
                    "🏁 ".bold()
                        + slug.bold().cyan()
                        + " finished — lobby is open! Type `!play <game>`, `!random`, or `!games`.",
                )?;
            }
            Outcome::Completed(Ok(Some(PlayOutcome::NotEnoughPlayers))) => {
                tracing::info!(game = slug, "minigame ended: not enough players");
            }
            Outcome::Completed(Ok(None)) => {
                lobby.say(format!("Unknown game `{slug}` — try `!games`.").yellow())?;
            }
            Outcome::Completed(Err(err)) => {
                let lost_lobby = matches!(
                    &err,
                    Error::BotLeftRoom { channel, .. } if lobby.casemap().str_eq(channel, lobby.name())
                );
                if err.is_fatal() || lost_lobby {
                    return Err(err);
                }
                tracing::warn!(game = slug, error = %err, "minigame ended with error");
                let auth_hint = if matches!(
                    err,
                    Error::JoinFailed { code: 477, .. } | Error::NotChannelOp { .. }
                ) {
                    " (tip: pass `--password <account>:<password>` if the network requires NickServ auth for temporary breakout channels)"
                } else {
                    ""
                };
                lobby.say(
                    format!("⚠️ `{slug}` ended early: {err}{auth_hint} — lobby is open again!")
                        .yellow(),
                )?;
            }
            Outcome::Stopped(StopReason::Shutdown) => break,
            Outcome::Stopped(StopReason::Admin(admin_msg)) => {
                tracing::info!(game = slug, admin = %admin_msg.nick, "minigame stopped by admin");
                admin_msg.reply(format!("🛑 Stopped `{slug}`.").yellow())?;
                if admin_msg.is_dm() {
                    lobby.say(format!("🛑 `{slug}` was stopped by an admin.").yellow())?;
                }
            }
            Outcome::Stopped(StopReason::Aborted(who)) => {
                tracing::info!(game = slug, by = %who.nick(), "minigame aborted");
                lobby.say(
                    "🛑 ".yellow()
                        + who.nick().bold()
                        + format!(" aborted `{slug}` — lobby is open!").yellow(),
                )?;
            }
        }
    }

    Ok(())
}

async fn admin_quit_loop(bot: &Bot, lobby: &Room, admins: &[String]) -> Result<()> {
    if admins.is_empty() {
        bot.shutdown_requested().await;
        return Ok(());
    }

    let mut inbox = bot.inbox_all().lossy();
    while let Outcome::Completed(msg) = inbox
        .wait_cmd("!quit")
        .until(bot.shutdown_requested())
        .await
    {
        let msg = msg?;
        if (msg.is_dm() || msg.is_in(lobby)) && is_admin_msg(&msg, admins) {
            msg.reply("👋 Shutting down minigames bot!".yellow())?;
            bot.quit(Some("Requested by admin")).await?;
            break;
        }
    }
    Ok(())
}

async fn run_live(cli: BotCli) -> Result<()> {
    let primary_nick = cli
        .nick
        .clone()
        .unwrap_or_else(|| format!("ircgm-{}", random_suffix_4()));
    let alt_nicks = vec![
        format!("{primary_nick}_"),
        format!("{primary_nick}__"),
        format!("ircgm-{}", random_suffix_4()),
    ];

    let irc_cfg = IrcConfig {
        nickname: Some(primary_nick.clone()),
        alt_nicks,
        username: Some("ircgm".into()),
        realname: Some("ircgm minigames test bot".into()),
        server: Some(cli.host.clone()),
        port: Some(cli.port),
        password: cli.password.clone(),
        use_tls: Some(cli.use_tls),
        ..Default::default()
    };

    tracing::info!(
        server = %format!("{}:{}", cli.host, cli.port),
        tls = cli.use_tls,
        nick = %primary_nick,
        lobby = %cli.lobby,
        "connecting to IRC server"
    );

    let bot = Bot::connect(irc_cfg).await?;
    tracing::info!(nick = %bot.nick(), "registered with IRC server");

    let lobby = match &cli.lobby_key {
        Some(key) => bot.join_with_key(&cli.lobby, key).await?,
        None => bot.join(&cli.lobby).await?,
    };
    tracing::info!(lobby = %lobby.name(), "joined lobby channel — ready for `!play`");

    tokio::try_join!(
        run_lobby_bot(&bot, &lobby, &cli.admins),
        admin_quit_loop(&bot, &lobby, &cli.admins),
    )?;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = match parse_cli(std::env::args()) {
        Ok(ParseOutcome::Help(help)) => {
            println!("{help}");
            return ExitCode::SUCCESS;
        }
        Ok(ParseOutcome::Run(cli)) => cli,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::from(2);
        }
    };

    let max_level = if cli.verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };
    tracing_subscriber::fmt()
        .with_max_level(max_level)
        .with_target(false)
        .init();

    if let Err(err) = run_live(cli).await {
        tracing::error!(error = %err, "game_bot exited with error");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use irc::proto::Command;
    use ircgm::testing::test_harness;

    use super::{parse_cli, resolve_game_slug, run_lobby_bot, ParseOutcome};

    #[test]
    fn parse_cli_handles_libera_server_and_lobby_flags() {
        let out = parse_cli([
            "game_bot",
            "--server",
            "irc.libera.chat:6667",
            "--lobby",
            "#ircgm-minigames",
        ])
        .expect("valid CLI");

        let ParseOutcome::Run(cli) = out else {
            panic!("expected ParseOutcome::Run");
        };
        assert_eq!(cli.host, "irc.libera.chat");
        assert_eq!(cli.port, 6667);
        assert!(!cli.use_tls);
        assert_eq!(cli.lobby, "#ircgm-minigames");
        assert!(cli.nick.is_none());
    }

    #[test]
    fn parse_cli_supports_tls_schemes_ports_and_unprefixed_lobby() {
        let ParseOutcome::Run(tls_port) = parse_cli([
            "game_bot",
            "--server=irc.libera.chat:6697",
            "--lobby=ircgm-minigames",
            "--nick",
            "TestGM",
            "--admin",
            "seirl",
        ])
        .unwrap()
        else {
            panic!("expected Run");
        };
        assert_eq!(tls_port.host, "irc.libera.chat");
        assert_eq!(tls_port.port, 6697);
        assert!(tls_port.use_tls);
        assert_eq!(tls_port.lobby, "#ircgm-minigames");
        assert_eq!(tls_port.nick.as_deref(), Some("TestGM"));
        assert_eq!(tls_port.admins, vec!["seirl".to_owned()]);

        let ParseOutcome::Run(scheme_tls) =
            parse_cli(["game_bot", "--server", "ircs://irc.libera.chat"]).unwrap()
        else {
            panic!("expected Run");
        };
        assert_eq!(scheme_tls.host, "irc.libera.chat");
        assert_eq!(scheme_tls.port, 6697);
        assert!(scheme_tls.use_tls);

        let ParseOutcome::Run(forced_no_tls) = parse_cli([
            "game_bot",
            "--server",
            "irc.libera.chat:6697",
            "--no-tls",
        ])
        .unwrap()
        else {
            panic!("expected Run");
        };
        assert!(!forced_no_tls.use_tls);

        assert!(matches!(
            parse_cli(["game_bot", "--help"]).unwrap(),
            ParseOutcome::Help(_)
        ));
        assert!(parse_cli(["game_bot", "--server", "bad:port"]).is_err());
        assert!(parse_cli(["game_bot", "--unknown"]).is_err());
    }

    #[test]
    fn resolve_game_slug_normalizes_hyphens_case_and_aliases() {
        assert_eq!(resolve_game_slug("Blackjack"), Some("blackjack"));
        assert_eq!(resolve_game_slug("bj"), Some("blackjack"));
        assert_eq!(resolve_game_slug("cold-war"), Some("cold_war"));
        assert_eq!(resolve_game_slug("coldwar"), Some("cold_war"));
        assert_eq!(resolve_game_slug("!simon-says"), Some("simon_says"));
        assert_eq!(resolve_game_slug("werewolf"), Some("mafia"));
        assert_eq!(resolve_game_slug("nonexistent"), None);
    }

    #[tokio::test(start_paused = true)]
    async fn single_lobby_allows_anyone_to_list_play_abort_and_chain_games() {
        let (bot, mut srv) = test_harness("GM");
        srv.complete_join("#ircgm-minigames", &["Alice", "Bob", "Carol"]);
        srv.settle().await;

        let lobby = bot.room("#ircgm-minigames").unwrap();
        let bot_task = tokio::spawn({
            let bot = bot.clone();
            let lobby = lobby.clone();
            async move { run_lobby_bot(&bot, &lobby, &[]).await }
        });
        srv.settle().await;

        // Initial greeting in `#ircgm-minigames`:
        let opening = srv.take_messages_to("#ircgm-minigames");
        assert!(
            opening.iter().any(|m| m.contains("Minigames bot ready!")),
            "expected greeting, got: {opening:?}"
        );

        // 1. Anyone (`Carol`) lists games with uppercase `!GAMES` (case-insensitive commands!):
        srv.chat("Carol", "#ircgm-minigames", "!GAMES");
        srv.settle().await;
        let catalog = srv.take_messages_to("#ircgm-minigames");
        assert!(catalog.iter().any(|m| m.contains("blackjack")));
        assert!(catalog.iter().any(|m| m.contains("simon_says")));

        // 2. `Alice` starts `blackjack` via `!PLAY Blackjack`; `Bob` tries starting `heist` mid-game
        //    and gets a busy hint without queueing a stale `!play heist` for later!
        srv.chat("Alice", "#ircgm-minigames", "!PLAY Blackjack");
        srv.settle().await;
        let bj_open = srv.take_messages_to("#ircgm-minigames");
        assert!(
            bj_open.iter().any(|m| m.contains("Blackjack")),
            "expected Blackjack recruitment prompt, got: {bj_open:?}"
        );

        srv.chat("Bob", "#ircgm-minigames", "!play heist");
        srv.settle().await;
        let busy = srv.take_messages_to("#ircgm-minigames");
        assert!(
            busy.iter().any(|m| m.contains("already running")),
            "expected busy message while blackjack is active, got: {busy:?}"
        );

        // Let Blackjack's 20s seat window expire with 0 players: it announces `Not enough players`
        // without a misleading `blackjack finished` banner!
        tokio::time::advance(Duration::from_secs(20)).await;
        srv.settle().await;
        let bj_done = srv.take_messages_to("#ircgm-minigames");
        assert!(
            bj_done
                .iter()
                .any(|m| m.contains("Not enough players joined for Blackjack.")),
            "expected not-enough-players notice, got: {bj_done:?}"
        );
        assert!(
            !bj_done.iter().any(|m| m.contains("blackjack finished")),
            "0-player expiry must not emit completion banner: {bj_done:?}"
        );
        assert!(
            !bj_done.iter().any(|m| m.contains("Vault Heist")),
            "mid-game `!play heist` must not replay after blackjack ends: {bj_done:?}"
        );

        // 3. `Bob` starts `heist` via shorthand `!heist`, then aborts directly in `#ircgm-minigames`:
        srv.chat("Bob", "#ircgm-minigames", "!heist");
        srv.settle().await;
        let heist_open = srv.take_messages_to("#ircgm-minigames");
        assert!(heist_open.iter().any(|m| m.contains("Co-op Vault Heist")));

        // Non-starter `Carol` cannot abort while `Bob` is still in the lobby:
        srv.chat("Carol", "#ircgm-minigames", "!abort");
        srv.settle().await;
        assert!(srv.take_messages_to("#ircgm-minigames").is_empty());

        // Starter `Bob` aborts right in the lobby channel:
        srv.chat("Bob", "#ircgm-minigames", "!abort");
        srv.settle().await;
        let aborted = srv.take_messages_to("#ircgm-minigames");
        assert!(
            aborted.iter().any(|m| m.contains("Bob aborted `heist`")),
            "expected Bob abort message, got: {aborted:?}"
        );

        // 4. Orphaned game recovery: `Alice` starts `cold-war`, then quits IRC —
        //    now `Carol` in `#ircgm-minigames` can `!abort`!
        srv.chat("Alice", "#ircgm-minigames", "!play cold-war");
        srv.settle().await;
        let _ = srv.take_messages_to("#ircgm-minigames");

        srv.quit_user("Alice", Some("Client Quit"));
        srv.settle().await;

        srv.chat("Carol", "#ircgm-minigames", "!abort");
        srv.settle().await;
        let orphan_abort = srv.take_messages_to("#ircgm-minigames");
        assert!(
            orphan_abort
                .iter()
                .any(|m| m.contains("Carol aborted `cold_war`")),
            "expected orphan abort by Carol, got: {orphan_abort:?}"
        );

        // Clean shutdown:
        bot.request_shutdown();
        bot_task.await.unwrap().unwrap();

        while let Some(cmd) = srv.try_recv_sent() {
            let _ = Command::from(cmd);
        }
    }
}
