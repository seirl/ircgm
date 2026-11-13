//! Core Role-Plugin Trait & Lock-Free Dataflow Contexts (Base + All 4 Expansions!)
//!
//! Supports Base ONUW, Daybreak (Shields, Artifacts, Face-up flips, Aura audit, Multi-peek PI/Witch),
//! Vampire (Dusk phase, 8 Marks, Coven chat), Alien (Dynamic Oracle/App directives, Leader vote rule,
//! Groob/Zerb, Blob, Cow), and Super Villains (Villain pack, Role Retriever, Evilometer, Intern/Mad Scientist)
//! with **zero concrete role names hardcoded in the engine**.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use tokio::sync::watch;
use tokio::time::Instant;

use ircgm::{AskConfig, Bot, CaseMap, Colorize, ExclusiveDm, Result, Text, User};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type ErasedIntent = Box<dyn Any + Send>;

// ============================================================================
// 1. Teams, Capability Tags, Tokens (Marks & Artifacts), & Scheduling Flags
// ============================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Team {
    /// Villagers, Heroes, and village-aligned specialists.
    Village,
    /// Werewolves, Vampires, Aliens, and Super Villains.
    Werewolf,
    /// Solo / custom win condition (`Tanner`, `Assassin`, `Groob`, `Zerb`, `Blob`, `Mortician`, `Mad Scientist`, `Intern`).
    Independent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RoleTag {
    /// Primary evil threat that `Team::Village` must kill at least one of (Werewolves, Vampires, Aliens, Villains).
    WolfThreat,
    /// Fallback evil threat (`Minion`, `Renfield`) when 0 `WolfThreat`s are held by players.
    SoloThreatFallback,

    // Faction-specific identification tags (so cross-expansion games keep factions distinct!):
    /// Waking Werewolves (`Werewolf`, `Alpha Wolf`, `Mystic Wolf`) — seen by `Dream Wolf` and `Minion`.
    WolfPack,
    /// Sleeping Werewolf (`Dream Wolf`) — sticks thumb out for `Minion`, does not wake with `WolfPack`.
    DreamWolfThumb,
    /// Waking Vampires (`Vampire`, `Master`, `Count`) — seen by `Renfield`.
    VampirePack,
    /// Aliens (`Alien`, `Groob`, `Zerb`, plus `Synthetic Alien` who *appears* Alien to `Leader`/`Cow`/`Aliens`!).
    AlienPack,
    /// Super Villains (`Temptress`, `Dr. Peeker`, `Rapscallion`, `Henchman #7`) — wake together & trigger `Evilometer`.
    VillainPack,
    /// Marks the original `Assassin` (tracked by `Apprentice Assassin`).
    OriginalAssassin,
    /// Marks the `Mad Scientist` (tracked by `Intern`).
    MadScientist,
}

/// Vampire expansion Marks (every player starts Dusk with `Mark::Clarity`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mark {
    Clarity,
    Vampire,
    Bat,
    Fear,
    Disease,
    Love,
    Traitor,
    Assassin,
}

impl Mark {
    pub fn name(self) -> &'static str {
        match self {
            Mark::Clarity => "Mark of Clarity",
            Mark::Vampire => "Mark of the Vampire",
            Mark::Bat => "Mark of the Bat",
            Mark::Fear => "Mark of Fear",
            Mark::Disease => "Mark of Disease",
            Mark::Love => "Mark of Love",
            Mark::Traitor => "Mark of the Traitor",
            Mark::Assassin => "Mark of the Assassin",
        }
    }

    pub fn styled(self) -> Text {
        match self {
            Mark::Vampire => Text::from(self.name().bold().red()),
            Mark::Fear | Mark::Disease | Mark::Assassin | Mark::Traitor => {
                Text::from(self.name().bold().orange())
            }
            Mark::Love => Text::from(self.name().bold().yellow()),
            Mark::Bat | Mark::Clarity => Text::from(self.name().bold().light_cyan()),
        }
    }
}

/// Daybreak `Curator` Artifacts placed face-down on a player's card.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Artifact {
    /// Turns the holder into a Werewolf (`Team::Werewolf` + `WolfThreat`).
    ClawOfTheWerewolf,
    /// Turns the holder into a Villager (`Team::Village`, clears threats).
    BrandOfTheVillager,
    /// Turns the holder into a Tanner (`Team::Independent`, wins iff killed).
    CudgelOfTheTanner,
    /// Harmless trinket—does nothing!
    CloakOfNothing,
}

impl Artifact {
    pub fn random() -> Self {
        match fastrand::usize(0..4) {
            0 => Artifact::ClawOfTheWerewolf,
            1 => Artifact::BrandOfTheVillager,
            2 => Artifact::CudgelOfTheTanner,
            _ => Artifact::CloakOfNothing,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Artifact::ClawOfTheWerewolf => "Claw of the Werewolf",
            Artifact::BrandOfTheVillager => "Brand of the Villager",
            Artifact::CudgelOfTheTanner => "Cudgel of the Tanner",
            Artifact::CloakOfNothing => "Shroud of Nothing",
        }
    }
}

/// Alien expansion `Oracle` omen that dynamically alters downstream app-driven Alien roles!
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OracleOmen {
    Normal,
    /// Doubles `Psychic` peeks & grants `Exposer` extra center flips.
    AmplifiedSignals,
    /// Causes `Alien` / `Rascal` swaps to scramble more aggressively.
    QuantumStatic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyBehavior {
    Sleep,
    ActImmediately,
    JoinGroupWake,
    WakeAfterAllMutations,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptDep {
    None,
    AfterCopycats,
    AfterUpstreamBoard,
}

// ============================================================================
// 2. Physical `Card`, `Roster`, & Rich `BoardCards` Snapshot
// ============================================================================

#[derive(Clone, Copy)]
pub enum Card {
    Copycat {
        base: &'static dyn RolePlugin,
        copied: Option<&'static dyn RolePlugin>,
        unawakened_fallback: &'static dyn RolePlugin,
    },
    /// Used when a role dynamically transforms mid-night without swapping cards (e.g. `Paranormal Investigator`).
    Transformed {
        original: &'static dyn RolePlugin,
        became: &'static dyn RolePlugin,
    },
    Standard(&'static dyn RolePlugin),
}

impl fmt::Debug for Card {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Card::Copycat { base, copied, .. } => {
                write!(f, "{}({:?})", base.name(), copied.map(RolePlugin::name))
            }
            Card::Transformed { original, became } => {
                write!(f, "{}->{}", original.name(), became.name())
            }
            Card::Standard(r) => write!(f, "{}", r.name()),
        }
    }
}

impl Card {
    pub fn effective_role(self) -> &'static dyn RolePlugin {
        match self {
            Card::Copycat {
                copied: Some(r), ..
            } => r,
            Card::Copycat {
                copied: None,
                unawakened_fallback,
                ..
            } => unawakened_fallback,
            Card::Transformed { became, .. } => became,
            Card::Standard(r) => r,
        }
    }

    pub fn display_for_peek(self) -> Text {
        match self {
            Card::Copycat { base, .. } => base.styled_name(),
            Card::Transformed { original, .. } => original.styled_name(),
            Card::Standard(r) => r.styled_name(),
        }
    }

    pub fn display_reveal(self) -> Text {
        match self {
            Card::Copycat {
                base,
                copied: Some(r),
                ..
            } => base.styled_name() + " (".light_cyan() + r.styled_name() + ")".light_cyan(),
            Card::Copycat { base, .. } => base.styled_name(),
            Card::Transformed { original, became } => {
                original.styled_name() + " → ".yellow() + became.styled_name()
            }
            Card::Standard(r) => r.styled_name(),
        }
    }
}

pub struct SeatInfo {
    pub user: User,
    pub initial_role: &'static dyn RolePlugin,
}

pub struct Roster {
    pub casemap: CaseMap,
    pub seats: Vec<SeatInfo>,
    pub deck: Vec<&'static dyn RolePlugin>,
}

impl Roster {
    pub fn len(&self) -> usize {
        self.seats.len()
    }

    pub fn initial_holders(&self, plugin: &'static dyn RolePlugin) -> Vec<usize> {
        self.seats
            .iter()
            .enumerate()
            .filter_map(|(i, s)| (s.initial_role.id() == plugin.id()).then_some(i))
            .collect()
    }

    pub fn format_seats(&self) -> String {
        self.seats
            .iter()
            .enumerate()
            .map(|(i, s)| format!("[{}] {}", i + 1, s.user.nick()))
            .collect::<Vec<_>>()
            .join("  ")
    }

    pub fn format_seats_marking_shields(&self, shielded: &HashSet<usize>) -> String {
        self.seats
            .iter()
            .enumerate()
            .map(|(i, s)| {
                if shielded.contains(&i) {
                    format!("[{}] {}🛡", i + 1, s.user.nick())
                } else {
                    format!("[{}] {}", i + 1, s.user.nick())
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
    }

    pub fn format_deck(&self) -> String {
        let mut counts: Vec<(&'static str, usize)> = Vec::new();
        for &r in &self.deck {
            if let Some(entry) = counts.iter_mut().find(|(name, _)| *name == r.name()) {
                entry.1 += 1;
            } else {
                counts.push((r.name(), 1));
            }
        }
        counts
            .into_iter()
            .map(|(name, n)| {
                if n > 1 {
                    format!("{n}x {name}")
                } else {
                    name.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn resolve_token(&self, token: &str) -> Option<usize> {
        let clean = token.trim().trim_end_matches(':').trim_end_matches(',');
        let num_token = clean.strip_prefix('#').unwrap_or(clean);
        if let Ok(seat) = num_token.parse::<usize>() {
            if (1..=self.seats.len()).contains(&seat) {
                return Some(seat - 1);
            }
        }
        self.seats
            .iter()
            .position(|s| self.casemap.eq(&s.user.nick(), clean))
    }

    pub fn neighbors(&self, seat: usize) -> (usize, usize) {
        let n = self.seats.len();
        ((seat + n - 1) % n, (seat + 1) % n)
    }
}

/// Complete tabletop state flowing along lock-free `watch` edges in `dag.rs`:
/// physical cards, Dusk/Vampire `marks`, Daybreak `shielded` seats & `artifacts`,
/// public `face_up_*` cards, `Aura Seer` audit trail, and Alien app state!
#[derive(Clone, Debug)]
pub struct BoardCards {
    pub players: Vec<Card>,
    pub center: [Card; 3],
    /// Extra center wolf/villain card used by `Alpha Wolf` and `Temptress`.
    pub extra_evil_card: Card,
    /// Seats protected by `Sentinel`'s shield token (cannot be viewed, moved, or artifacted).
    pub shielded: HashSet<usize>,
    /// Per-seat `Mark` tokens (Vampire expansion; initialized to `Mark::Clarity`).
    pub marks: Vec<Mark>,
    /// Per-seat `Artifact` tokens placed by `Curator` (Daybreak).
    pub artifacts: HashMap<usize, Artifact>,
    /// Cards flipped face-up during Night (`Revealer`, `Flipper`, `Exposer`) — announced at Dawn!
    pub face_up_players: HashSet<usize>,
    pub face_up_center: HashSet<usize>,
    /// Seats that viewed or moved any card tonight (read by Daybreak's `Aura Seer`).
    pub viewed_or_moved_actors: HashSet<usize>,
    /// Seats absorbed into the `Blob` (Alien expansion).
    pub blob_seats: HashSet<usize>,
    /// Dynamic app modifier chosen by `Oracle` (Alien expansion).
    pub oracle_omen: OracleOmen,
}

impl BoardCards {
    pub fn new(players: Vec<Card>, center: [Card; 3], default_evil_card: Card) -> Self {
        let n = players.len();
        Self {
            players,
            center,
            extra_evil_card: default_evil_card,
            shielded: HashSet::new(),
            marks: vec![Mark::Clarity; n],
            artifacts: HashMap::new(),
            face_up_players: HashSet::new(),
            face_up_center: HashSet::new(),
            viewed_or_moved_actors: HashSet::new(),
            blob_seats: HashSet::new(),
            oracle_omen: OracleOmen::Normal,
        }
    }

    pub fn is_shielded(&self, seat: usize) -> bool {
        self.shielded.contains(&seat)
    }

    pub fn record_card_interaction(&mut self, actor_seat: usize) {
        self.viewed_or_moved_actors.insert(actor_seat);
    }

    /// Computes a seat's final effective `Team` after applying Artifacts and Marks!
    pub fn effective_team(&self, seat: usize) -> Team {
        let mut team = self.players[seat].effective_role().team();

        if let Some(art) = self.artifacts.get(&seat) {
            match art {
                Artifact::ClawOfTheWerewolf => team = Team::Werewolf,
                Artifact::BrandOfTheVillager => team = Team::Village,
                Artifact::CudgelOfTheTanner => team = Team::Independent,
                Artifact::CloakOfNothing => {}
            }
        }

        match self.marks[seat] {
            Mark::Vampire => team = Team::Werewolf,
            Mark::Traitor => {
                team = match team {
                    Team::Village => Team::Werewolf,
                    Team::Werewolf => Team::Village,
                    Team::Independent => Team::Independent,
                };
            }
            _ => {}
        }

        team
    }

    /// True if `seat` counts as a primary Evil threat (`WolfThreat`, `Claw`, or `Mark::Vampire`).
    pub fn is_primary_threat(&self, seat: usize) -> bool {
        if self.marks[seat] == Mark::Vampire {
            return true;
        }
        if let Some(art) = self.artifacts.get(&seat) {
            match art {
                Artifact::ClawOfTheWerewolf => return true,
                Artifact::BrandOfTheVillager | Artifact::CudgelOfTheTanner => return false,
                Artifact::CloakOfNothing => {}
            }
        }
        self.players[seat]
            .effective_role()
            .tags()
            .contains(&RoleTag::WolfThreat)
    }
}

// ============================================================================
// 3. Execution Contexts Passed into Role Plugins
// ============================================================================

#[derive(Clone, Copy)]
pub struct CopiedRecord {
    pub copycat_seat: usize,
    pub copied_role: &'static dyn RolePlugin,
}

pub struct PromptCtx<'a> {
    pub roster: &'a Roster,
    pub ask_cfg: &'a AskConfig,
    pub deadline: Instant,
    /// Shielded seats known from Dusk / `Sentinel` (empty if no shielder is in play!).
    pub shielded: &'a HashSet<usize>,
}

pub struct GroupPromptCtx<'a> {
    pub bot: &'a Bot,
    pub roster: &'a Roster,
    pub ask_cfg: &'a AskConfig,
    pub deadline: Instant,
    pub copied_records: &'a [CopiedRecord],
    pub shielded: &'a HashSet<usize>,
}

impl<'a> GroupPromptCtx<'a> {
    pub fn active_seats_with_tag(&self, tag: RoleTag) -> Vec<usize> {
        let mut out: Vec<usize> = self
            .roster
            .seats
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.initial_role.tags().contains(&tag).then_some(i))
            .collect();
        for rec in self.copied_records {
            if rec.copied_role.copy_behavior() == CopyBehavior::JoinGroupWake
                && rec.copied_role.tags().contains(&tag)
                && !out.contains(&rec.copycat_seat)
            {
                out.push(rec.copycat_seat);
            }
        }
        out
    }
}

pub struct CopycatCtx<'a> {
    pub roster: &'a Roster,
    pub stage1_cfg: &'a AskConfig,
    pub stage1_deadline: Instant,
    pub stage2_min_budget: std::time::Duration,
    pub copied_tx: &'a watch::Sender<Option<Vec<CopiedRecord>>>,
}

pub struct ResolveCtx<'a> {
    pub roster: &'a Roster,
    pub logs: &'a mut Vec<String>,
}

// ============================================================================
// 4. Dawn Death-Trigger & Win-Evaluation Contexts
// ============================================================================

pub struct DeathCtx<'a> {
    pub roster: &'a Roster,
    pub board: &'a BoardCards,
    pub votes: &'a [Option<usize>],
}

pub struct DeathReaction {
    pub kill_seat: usize,
    pub announcement: Text,
}

pub struct VictoryCtx<'a> {
    pub roster: &'a Roster,
    pub board: &'a BoardCards,
    pub votes: &'a [Option<usize>],
    pub dead_seats: &'a HashSet<usize>,
}

impl<'a> VictoryCtx<'a> {
    pub fn is_dead(&self, seat: usize) -> bool {
        self.dead_seats.contains(&seat)
    }

    pub fn find_seats_by_role_id(&self, role_id: &str) -> Vec<usize> {
        (0..self.roster.len())
            .filter(|&i| self.board.players[i].effective_role().id() == role_id)
            .collect()
    }

    pub fn find_seats_with_tag(&self, tag: RoleTag) -> Vec<usize> {
        (0..self.roster.len())
            .filter(|&i| self.board.players[i].effective_role().tags().contains(&tag))
            .collect()
    }
}

// ============================================================================
// 5. The `RolePlugin` Trait
// ============================================================================

/// Wake-order convention:
/// - `1..=99`: **Dusk / Pre-Night Phase** (`Copycat` 5, `Mirror Man` 6, `Doppelgänger` 10, `Sentinel` 15,
///   Vampire Dusk roles `20..=85`). Runs before Night so Shields & Marks (`Mark::Fear`!) take effect!
/// - `100..=999`: **Night Phase** (`Werewolf` 120, `Minion` 130, `Mason` 140, `Seer` 150, `Robber` 160,
///   `Troublemaker` 170, `Drunk` 180, `Insomniac` 190, `Aura Seer` 195, etc.).
pub trait RolePlugin: Send + Sync + 'static {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    fn team(&self) -> Team;
    fn night_summary(&self) -> &'static str;

    fn tags(&self) -> &'static [RoleTag] {
        &[]
    }

    fn styled_name(&self) -> Text {
        match self.team() {
            Team::Werewolf => Text::from(self.name().bold().red()),
            Team::Independent => Text::from(self.name().bold().orange()),
            Team::Village => Text::from(self.name().bold().light_cyan()),
        }
    }

    fn make_card(&'static self) -> Card {
        Card::Standard(self)
    }

    fn requires_pair_in_deck(&self) -> bool {
        false
    }

    /// Optional setup hook (e.g. `Blob` marks initial Blob seats; `Renfield` starts with `Mark::Bat`).
    fn on_deal(&self, _seat: usize, _board: &mut BoardCards, _roster: &Roster) {}

    // --- Dusk / Night DAG Descriptors ---

    fn wake_order(&self) -> Option<u16> {
        None
    }

    fn copy_behavior(&self) -> CopyBehavior {
        CopyBehavior::Sleep
    }

    fn is_copycat(&self) -> bool {
        false
    }

    fn is_group_wake(&self) -> bool {
        false
    }

    fn prompt_dep(&self) -> PromptDep {
        PromptDep::None
    }

    fn mutates_board(&self) -> bool {
        false
    }

    fn needs_dm_prompt(&self) -> bool {
        true
    }

    // --- Dusk / Night Execution Hooks ---

    fn prompt_seat<'a>(
        &'a self,
        _dm: &'a mut ExclusiveDm,
        _seat: usize,
        _ctx: &'a PromptCtx<'a>,
        _upstream_board: Option<&'a BoardCards>,
    ) -> BoxFuture<'a, Result<ErasedIntent>> {
        Box::pin(async { Ok(Box::new(()) as ErasedIntent) })
    }

    fn resolve_seat(
        &self,
        _seat: usize,
        _actor_label: &str,
        _intent: ErasedIntent,
        _board: &mut BoardCards,
        _ctx: &mut ResolveCtx<'_>,
    ) {
    }

    fn resolve_unavailable_seat(
        &self,
        _seat: usize,
        _actor_label: &str,
        _board: &mut BoardCards,
        _ctx: &mut ResolveCtx<'_>,
    ) {
    }

    fn prompt_group<'a>(
        &'a self,
        _active_seats: Vec<usize>,
        _ctx: &'a GroupPromptCtx<'a>,
        _upstream_board: Option<&'a BoardCards>,
    ) -> BoxFuture<'a, Result<ErasedIntent>> {
        Box::pin(async { Ok(Box::new(()) as ErasedIntent) })
    }

    fn resolve_group(
        &self,
        _active_seats: &[usize],
        _intent: ErasedIntent,
        _board: &mut BoardCards,
        _ctx: &mut ResolveCtx<'_>,
    ) {
    }

    fn run_copycat<'a>(
        &'a self,
        _seat: usize,
        initial_board: BoardCards,
        _ctx: &'a CopycatCtx<'a>,
    ) -> BoxFuture<'a, Result<(BoardCards, Vec<String>)>> {
        Box::pin(async move { Ok((initial_board, Vec::new())) })
    }

    // --- Dawn Death & Victory Hooks ---

    fn on_death(&self, _dead_seat: usize, _ctx: &DeathCtx<'_>) -> Option<DeathReaction> {
        None
    }

    fn check_independent_win(&self, _seat: usize, _ctx: &VictoryCtx<'_>) -> bool {
        false
    }

    fn independent_win_blocks(&self, _team: Team) -> bool {
        false
    }

    /// Optional global team override (e.g. Alien `Leader`: if all Aliens vote for `Leader`, Evil wins!).
    fn check_team_win_override(&self, _seat: usize, _ctx: &VictoryCtx<'_>) -> Option<(Team, String)> {
        None
    }
}
