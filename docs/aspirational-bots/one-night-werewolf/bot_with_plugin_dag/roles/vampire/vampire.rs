//! Vampire Expansion **Vampire** — Multi-Player Coven DM Coordination!
//!
//! Pressure-tests `Bot::exclusive_many` + `Bot::users_inbox` from `DESIGN.md` §4.2/§3.3:
//! - All waking Vampires (`Vampire`, `Count`, `Master`, Copycat-Vampire) see each other in Dusk.
//! - Active `Vampire` card holders enter a private bot-relayed coven session where any message is
//!   echoed to fellow Vampires until one locks in `!mark <seat|nick>` on a non-Vampire player,
//!   bestowing `Mark::Vampire` (turning that victim into a Vampire)!

use ircgm::{Colorize, Error, ExclusiveDm, User, UserEvent};

use crate::bot_with_plugin_dag::plugin::{
    BoardCards, BoxFuture, CopyBehavior, ErasedIntent, GroupPromptCtx, Mark, PromptDep, ResolveCtx,
    RolePlugin, RoleTag, Team,
};

pub struct Vampire;
pub static VAMPIRE: Vampire = Vampire;

impl RolePlugin for Vampire {
    fn id(&self) -> &'static str {
        "vampire"
    }

    fn name(&self) -> &'static str {
        "Vampire"
    }

    fn team(&self) -> Team {
        Team::Werewolf
    }

    fn tags(&self) -> &'static [RoleTag] {
        &[RoleTag::WolfThreat, RoleTag::VampirePack]
    }

    fn night_summary(&self) -> &'static str {
        "Wake in Dusk with the Vampire coven and bestow the Mark of the Vampire on a non-Vampire player!"
    }

    fn wake_order(&self) -> Option<u16> {
        Some(25)
    }

    fn copy_behavior(&self) -> CopyBehavior {
        CopyBehavior::JoinGroupWake
    }

    fn is_group_wake(&self) -> bool {
        true
    }

    fn prompt_dep(&self) -> PromptDep {
        PromptDep::AfterCopycats
    }

    fn mutates_board(&self) -> bool {
        true
    }

    fn prompt_group<'a>(
        &'a self,
        active_seats: Vec<usize>,
        ctx: &'a GroupPromptCtx<'a>,
        _upstream_board: Option<&'a BoardCards>,
    ) -> BoxFuture<'a, ircgm::Result<ErasedIntent>> {
        Box::pin(async move {
            let full_coven = ctx.active_seats_with_tag(RoleTag::VampirePack);
            let non_vampires: Vec<usize> = (0..ctx.roster.len())
                .filter(|s| !full_coven.contains(s))
                .collect();

            if non_vampires.is_empty() {
                return Ok(Box::new(None::<usize>) as ErasedIntent);
            }

            // NOTE (API Friction surfaced in `FINDINGS.md` §1.7):
            // `bot.exclusive_many(&users)` fails the ENTIRE batch with `Err(Error::UserLeft)`
            // if even ONE Vampire quit mid-Dusk, so we must manually filter online users in a retry loop!
            let mut online_vamps: Vec<User> = active_seats
                .iter()
                .map(|&s| ctx.roster.seats[s].user.clone())
                .filter(User::is_online)
                .collect();

            let guards: Vec<ExclusiveDm> = loop {
                if online_vamps.is_empty() {
                    break Vec::new();
                }
                let refs: Vec<&User> = online_vamps.iter().collect();
                match ctx.bot.exclusive_many(&refs).await {
                    Ok(g) => break g,
                    Err(Error::UserLeft { .. }) => {
                        online_vamps.retain(User::is_online);
                    }
                    Err(fatal) => return Err(fatal),
                }
            };

            if guards.is_empty() {
                let auto = non_vampires[fastrand::usize(..non_vampires.len())];
                return Ok(Box::new(Some(auto)) as ErasedIntent);
            }

            let coven_names: Vec<String> = full_coven
                .iter()
                .map(|&s| format!("[{}] {}", s + 1, ctx.roster.seats[s].user.nick()))
                .collect();
            let seats_str = ctx.roster.format_seats();

            for g in &guards {
                let _ = g.say(
                    "🧛 Vampire Coven: ".bold().red()
                        + coven_names.join(", ").bold()
                        + format!(
                            " — Whisper here to coordinate, or type `!mark <seat|nick>` to turn a non-Vampire! Seats: {seats_str}"
                        ),
                );
            }

            let mut coven_inbox = ctx.bot.users_inbox(&online_vamps);
            let chosen = loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(ctx.deadline) => {
                        let auto = non_vampires[fastrand::usize(..non_vampires.len())];
                        for g in &guards {
                            let _ = g.say(format!(
                                "⏳ Time expired — randomly marked [{}] {}!",
                                auto + 1,
                                ctx.roster.seats[auto].user.nick()
                            ));
                        }
                        break auto;
                    }
                    ev = coven_inbox.next_event() => {
                        let UserEvent::Chat(msg) = ev? else { continue };
                        if !msg.is_dm() {
                            continue;
                        }
                        if let Some(arg) = msg.cmd_args("!mark") {
                            match ctx.roster.resolve_token(arg) {
                                Some(idx) if non_vampires.contains(&idx) => {
                                    for g in &guards {
                                        let _ = g.say(
                                            "🧛 ".bold()
                                                + msg.sender.nick().bold()
                                                + " bestowed the "
                                                + Mark::Vampire.styled()
                                                + " upon "
                                                + ctx.roster.seats[idx].user.nick().bold()
                                                + "!",
                                        );
                                    }
                                    break idx;
                                }
                                Some(_) => {
                                    let _ = msg.sender.say("Pick a NON-Vampire player!".yellow());
                                }
                                None => {
                                    let _ = msg.sender.say("Unknown seat or nickname.".yellow());
                                }
                            }
                        } else if online_vamps.len() > 1 {
                            // Relay coven whisper to fellow Vampires!
                            for peer in &online_vamps {
                                if *peer != msg.sender {
                                    let _ = peer.say(
                                        "[Coven] <".red()
                                            + msg.sender.nick().bold()
                                            + "> ".red()
                                            + msg.text.as_str(),
                                    );
                                }
                            }
                        }
                    }
                }
            };

            drop(guards);
            Ok(Box::new(Some(chosen)) as ErasedIntent)
        })
    }

    fn resolve_group(
        &self,
        active_seats: &[usize],
        intent: ErasedIntent,
        board: &mut BoardCards,
        ctx: &mut ResolveCtx<'_>,
    ) {
        let Ok(chosen_opt) = intent.downcast::<Option<usize>>() else {
            return;
        };
        if let Some(target) = *chosen_opt {
            board.marks[target] = Mark::Vampire;
            let actors: Vec<String> = active_seats
                .iter()
                .map(|&s| ctx.roster.seats[s].user.nick())
                .collect();
            ctx.logs.push(format!(
                "Vampire Coven ({}) bestowed Mark of the Vampire on {}.",
                actors.join(", "),
                ctx.roster.seats[target].user.nick()
            ));
        }
    }
}
