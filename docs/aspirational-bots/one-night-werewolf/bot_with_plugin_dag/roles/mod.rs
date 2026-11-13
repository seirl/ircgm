pub mod alien;
pub mod daybreak;
pub mod doppelganger;
pub mod drunk;
pub mod hunter;
pub mod insomniac;
pub mod mason;
pub mod minion;
pub mod robber;
pub mod rogue;
pub mod seer;
pub mod super_villains;
pub mod tanner;
pub mod troublemaker;
pub mod vampire;
pub mod villager;
pub mod werewolf;

use super::plugin::RolePlugin;

use alien::{
    alien::{ALIEN, SYNTHETIC_ALIEN},
    blob::BLOB,
    cow::COW,
    groob_zerb::{GROOB, ZERB},
    leader::LEADER,
    oracle::ORACLE,
    psychic::PSYCHIC,
};
use daybreak::{
    alpha_wolf::ALPHA_WOLF, aura_seer::AURA_SEER, curator::CURATOR, dream_wolf::DREAM_WOLF,
    paranormal_investigator::PARANORMAL_INVESTIGATOR, revealer::REVEALER, sentinel::SENTINEL,
    village_idiot::VILLAGE_IDIOT, witch::WITCH,
};
use doppelganger::DOPPELGANGER;
use drunk::DRUNK;
use hunter::HUNTER;
use insomniac::INSOMNIAC;
use mason::MASON;
use minion::MINION;
use robber::ROBBER;
use rogue::ROGUE;
use seer::SEER;
use super_villains::{
    annoying_lad::ANNOYING_LAD,
    mad_scientist_intern::{INTERN, MAD_SCIENTIST},
    role_retriever::ROLE_RETRIEVER,
};
use tanner::TANNER;
use troublemaker::TROUBLEMAKER;
use vampire::{
    assassin::ASSASSIN, copycat::COPYCAT, count::COUNT, cupid::CUPID, diseased::DISEASED,
    marksman::MARKSMAN, pickpocket::PICKPOCKET, priest::PRIEST, vampire::VAMPIRE,
};
use villager::VILLAGER;
use werewolf::WEREWOLF;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DeckPreset {
    #[default]
    Base,
    Daybreak,
    Vampire,
    Alien,
    SuperVillains,
    Chaos,
}

impl DeckPreset {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "base" | "classic" => Some(Self::Base),
            "daybreak" | "db" => Some(Self::Daybreak),
            "vampire" | "vamp" => Some(Self::Vampire),
            "alien" | "aliens" => Some(Self::Alien),
            "villains" | "supervillains" | "sv" => Some(Self::SuperVillains),
            "chaos" | "all" | "epic" => Some(Self::Chaos),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Base => "Base ONUW",
            Self::Daybreak => "Daybreak",
            Self::Vampire => "Vampire (Dusk + Marks)",
            Self::Alien => "Alien (Dynamic Omen)",
            Self::SuperVillains => "Super Villains",
            Self::Chaos => "Cross-Expansion Chaos",
        }
    }
}

/// Complete registry of all 43 implemented `RolePlugin`s across Base + 4 Expansions!
pub static ALL_ROLE_PLUGINS: &[&'static dyn RolePlugin] = &[
    // Base + Rogue (13)
    &DOPPELGANGER,
    &WEREWOLF,
    &MINION,
    &MASON,
    &SEER,
    &ROBBER,
    &TROUBLEMAKER,
    &ROGUE,
    &DRUNK,
    &INSOMNIAC,
    &TANNER,
    &HUNTER,
    &VILLAGER,
    // Daybreak (9)
    &SENTINEL,
    &ALPHA_WOLF,
    &DREAM_WOLF,
    &PARANORMAL_INVESTIGATOR,
    &WITCH,
    &VILLAGE_IDIOT,
    &CURATOR,
    &REVEALER,
    &AURA_SEER,
    // Vampire (9)
    &COPYCAT,
    &VAMPIRE,
    &COUNT,
    &CUPID,
    &DISEASED,
    &ASSASSIN,
    &PRIEST,
    &PICKPOCKET,
    &MARKSMAN,
    // Alien (8)
    &ALIEN,
    &SYNTHETIC_ALIEN,
    &GROOB,
    &ZERB,
    &LEADER,
    &ORACLE,
    &PSYCHIC,
    &BLOB,
    &COW,
    // Super Villains (4)
    &ROLE_RETRIEVER,
    &ANNOYING_LAD,
    &MAD_SCIENTIST,
    &INTERN,
];

pub fn build_deck(preset: DeckPreset, player_count: usize) -> Vec<&'static dyn RolePlugin> {
    let target = player_count + 3;
    let pool: &[&'static dyn RolePlugin] = match preset {
        DeckPreset::Base => return build_base_deck(player_count),
        DeckPreset::Daybreak => &[
            &SENTINEL,
            &ALPHA_WOLF,
            &DREAM_WOLF,
            &PARANORMAL_INVESTIGATOR,
            &WITCH,
            &VILLAGE_IDIOT,
            &REVEALER,
            &CURATOR,
            &AURA_SEER,
            &SEER,
            &ROBBER,
            &TANNER,
            &HUNTER,
        ],
        DeckPreset::Vampire => &[
            &COPYCAT,
            &VAMPIRE,
            &COUNT,
            &PRIEST,
            &PICKPOCKET,
            &CUPID,
            &ASSASSIN,
            &MARKSMAN,
            &DISEASED,
            &TROUBLEMAKER,
            &INSOMNIAC,
            &HUNTER,
            &VILLAGER,
        ],
        DeckPreset::Alien => &[
            &ALIEN,
            &ALIEN,
            &LEADER,
            &ORACLE,
            &PSYCHIC,
            &COW,
            &SYNTHETIC_ALIEN,
            &GROOB,
            &ZERB,
            &BLOB,
            &ROBBER,
            &TROUBLEMAKER,
            &INSOMNIAC,
        ],
        DeckPreset::SuperVillains => &[
            &ALPHA_WOLF,
            &WEREWOLF,
            &ROLE_RETRIEVER,
            &ANNOYING_LAD,
            &MAD_SCIENTIST,
            &INTERN,
            &COPYCAT,
            &WITCH,
            &REVEALER,
            &COW,
            &TROUBLEMAKER,
            &INSOMNIAC,
            &VILLAGER,
        ],
        DeckPreset::Chaos => &[
            &COPYCAT,
            &SENTINEL,
            &VAMPIRE,
            &ALPHA_WOLF,
            &PARANORMAL_INVESTIGATOR,
            &WITCH,
            &ROLE_RETRIEVER,
            &VILLAGE_IDIOT,
            &CURATOR,
            &REVEALER,
            &AURA_SEER,
            &ORACLE,
            &PSYCHIC,
        ],
    };

    let mut deck: Vec<&'static dyn RolePlugin> = pool.iter().copied().take(target).collect();
    while deck.len() < target {
        deck.push(&VILLAGER);
    }
    deck
}

pub fn build_base_deck(player_count: usize) -> Vec<&'static dyn RolePlugin> {
    let mut deck: Vec<&'static dyn RolePlugin> = match player_count {
        3 => vec![&WEREWOLF, &WEREWOLF, &SEER, &ROBBER, &TROUBLEMAKER, &VILLAGER],
        4 => vec![
            &DOPPELGANGER,
            &WEREWOLF,
            &WEREWOLF,
            &SEER,
            &ROBBER,
            &TROUBLEMAKER,
            &INSOMNIAC,
        ],
        5 => vec![
            &DOPPELGANGER,
            &WEREWOLF,
            &WEREWOLF,
            &MINION,
            &SEER,
            &ROBBER,
            &TROUBLEMAKER,
            &INSOMNIAC,
        ],
        6 => vec![
            &DOPPELGANGER,
            &WEREWOLF,
            &WEREWOLF,
            &MINION,
            &SEER,
            &ROBBER,
            &TROUBLEMAKER,
            &DRUNK,
            &INSOMNIAC,
        ],
        7 => vec![
            &DOPPELGANGER,
            &WEREWOLF,
            &WEREWOLF,
            &MINION,
            &MASON,
            &MASON,
            &SEER,
            &ROBBER,
            &TROUBLEMAKER,
            &TANNER,
        ],
        8 => vec![
            &DOPPELGANGER,
            &WEREWOLF,
            &WEREWOLF,
            &MINION,
            &MASON,
            &MASON,
            &SEER,
            &ROBBER,
            &TROUBLEMAKER,
            &INSOMNIAC,
            &TANNER,
        ],
        9 => vec![
            &DOPPELGANGER,
            &WEREWOLF,
            &WEREWOLF,
            &MINION,
            &MASON,
            &MASON,
            &SEER,
            &ROBBER,
            &TROUBLEMAKER,
            &DRUNK,
            &INSOMNIAC,
            &HUNTER,
        ],
        _ => vec![
            &DOPPELGANGER,
            &WEREWOLF,
            &WEREWOLF,
            &MINION,
            &MASON,
            &MASON,
            &SEER,
            &ROBBER,
            &TROUBLEMAKER,
            &DRUNK,
            &INSOMNIAC,
            &TANNER,
            &HUNTER,
        ],
    };

    while deck.len() < player_count + 3 {
        deck.push(&VILLAGER);
    }

    for &plugin in ALL_ROLE_PLUGINS {
        if plugin.requires_pair_in_deck() {
            let count = deck.iter().filter(|&&r| r.id() == plugin.id()).count();
            assert!(
                count == 0 || count % 2 == 0,
                "role `{}` must be included in pairs",
                plugin.name()
            );
        }
    }

    deck
}
