//! Compiled event scripts (`data/world/events.json`, written by
//! `tools/world/compile_events.py`, spec §2.1): every object, sign, trigger
//! and map script as the paths of conditions and effects through it.
//!
//! Conditions and effects are open shapes in the JSON (one key names the
//! kind); unknown shapes land in the `Other` variants so newer data still
//! loads.

use std::collections::BTreeMap;
use std::path::Path;

use pokebot_core::{Error, Result};
use serde::Deserialize;

/// A script value: a number, or a name the compiler could not resolve
/// (`SPECIES_ABRA`, `VAR_LAST_TALKED`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Val {
    Int(i64),
    Sym(String),
}

impl From<i64> for Val {
    fn from(v: i64) -> Self {
        Val::Int(v)
    }
}

impl Val {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Val::Int(v) => Some(*v),
            Val::Sym(_) => None,
        }
    }
}

/// Which Pokédex count a [`Condition::Pokedex`] compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DexCount {
    Seen,
    Caught,
}

/// A comparison against a value; exactly one operator is set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Cmp {
    pub eq: Option<Val>,
    pub ne: Option<Val>,
    pub lt: Option<Val>,
    pub gt: Option<Val>,
    pub le: Option<Val>,
    pub ge: Option<Val>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Condition {
    Flag {
        flag: String,
        is: bool,
    },
    Trainer {
        trainer: String,
        defeated: bool,
    },
    Item {
        item: String,
        count: i64,
        has: bool,
    },
    BagSpace {
        bagspace: String,
        count: i64,
        has: bool,
    },
    Move {
        r#move: String,
        known: bool,
    },
    /// The answer to the YES/NO box just shown: `"yes"` or `"no"`.
    Answer {
        answer: String,
    },
    /// The option picked in a multichoice menu.
    Choice {
        choice: String,
        #[serde(flatten)]
        cmp: Cmp,
    },
    Coins {
        coins: String,
        #[serde(flatten)]
        cmp: Cmp,
    },
    /// The player's money (`checkmoney N` → `{"money": "player", "ge": N}`).
    Money {
        money: String,
        #[serde(flatten)]
        cmp: Cmp,
    },
    /// The Pokédex count (`GetPokedexCount`): species seen or caught, Kanto
    /// unless `national`.
    Pokedex {
        #[serde(rename = "pokedex")]
        which: DexCount,
        #[serde(default)]
        national: bool,
        #[serde(flatten)]
        cmp: Cmp,
    },
    /// `HasAllKantoMons` / `HasAllMons`: `"kanto"` or `"national"`.
    PokedexComplete {
        pokedex_complete: String,
        is: bool,
    },
    /// The party count (`getpartysize`, `CalculatePlayerPartyCount`):
    /// `"size"` counts eggs, `"non_egg"` doesn't.
    PartySize {
        party: String,
        #[serde(flatten)]
        cmp: Cmp,
    },
    /// A species (or egg) is in the party (`DoesPlayerPartyContainSpecies`).
    InParty {
        in_party: Val,
        is: bool,
    },
    /// `VAR_RESULT` as written by a `special`.
    Special {
        special: String,
        #[serde(flatten)]
        cmp: Cmp,
    },
    /// `VAR_RESULT` as written by another command (`givemon`, `random`...).
    ResultOf {
        result: String,
        #[serde(flatten)]
        cmp: Cmp,
    },
    Var {
        var: String,
        #[serde(flatten)]
        cmp: Cmp,
    },
    Other(serde_json::Map<String, serde_json::Value>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct VarChange {
    pub eq: Option<Val>,
    pub add: Option<Val>,
    pub sub: Option<Val>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Effect {
    Battle {
        battle: String,
        intro: Option<String>,
        defeat: Option<String>,
        victory: Option<String>,
        #[serde(default)]
        double: bool,
        #[serde(default)]
        rematch: bool,
    },
    Defeated {
        defeated: String,
    },
    Undefeated {
        undefeated: String,
    },
    Set {
        set: String,
    },
    Clear {
        clear: String,
    },
    Var {
        var: String,
        #[serde(flatten)]
        change: VarChange,
    },
    Give {
        give: String,
        count: i64,
        /// The "received X" text label when the script names one.
        text: Option<String>,
        /// An item ball on the ground rather than a gift.
        #[serde(default)]
        find: bool,
    },
    Take {
        take: String,
        count: i64,
    },
    GiveMon {
        givemon: Val,
        level: Val,
    },
    GiveEgg {
        giveegg: Val,
    },
    /// A scripted wild battle (static encounter).
    Wild {
        wild: Val,
        level: Val,
    },
    Warp {
        warp: String,
        warp_id: Option<Val>,
        x: Option<Val>,
        y: Option<Val>,
    },
    SetWarp {
        set_warp: String,
        warp_id: Option<Val>,
        x: Option<Val>,
        y: Option<Val>,
    },
    EscapeWarp {
        escape_warp: String,
        warp_id: Option<Val>,
        x: Option<Val>,
        y: Option<Val>,
    },
    Heal {
        heal: bool,
    },
    Respawn {
        respawn: String,
    },
    MoveObject {
        move_object: Val,
        x: Val,
        y: Val,
    },
    AddObject {
        add_object: Val,
        map: Option<String>,
    },
    RemoveObject {
        remove_object: Val,
        map: Option<String>,
    },
    Mart {
        mart: Vec<String>,
    },
    Say {
        say: String,
    },
    Money {
        money: Val,
    },
    Coins {
        coins: Val,
    },
    Other(serde_json::Map<String, serde_json::Value>),
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScriptPath {
    pub when: Vec<Condition>,
    pub does: Vec<Effect>,
    /// Commands on this path the compiler doesn't model (names).
    #[serde(default)]
    pub opaque: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Script {
    /// `object`, `sign`, `trigger` or `map`.
    pub kind: String,
    /// The map the script belongs to; `None` for scripts shared by several
    /// maps (e.g. `EventScript_CutTree`).
    pub map: Option<String>,
    pub local_id: Option<u32>,
    pub paths: Vec<ScriptPath>,
    /// The path cap was hit: some branches are missing.
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FrameScript {
    pub var: String,
    pub value: Val,
    pub script: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MapScripts {
    #[serde(default)]
    pub on_load: Vec<String>,
    #[serde(default)]
    pub on_transition: Vec<String>,
    #[serde(default)]
    pub on_resume: Vec<String>,
    #[serde(default)]
    pub on_return_to_field: Vec<String>,
    #[serde(default)]
    pub on_frame: Vec<FrameScript>,
    #[serde(default)]
    pub on_warp: Vec<FrameScript>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TriggerEvent {
    pub map: String,
    pub x: i32,
    pub y: i32,
    pub when: Vec<Condition>,
    pub script: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObjectRef {
    pub map: String,
    pub local_id: u32,
    pub graphics: Option<String>,
    pub x: Option<i32>,
    pub y: Option<i32>,
    #[serde(default)]
    pub range_x: i32,
    #[serde(default)]
    pub range_y: i32,
    /// Movement type without the `MOVEMENT_TYPE_` prefix.
    pub moves: Option<String>,
    /// The flag that hides the object when set.
    pub hidden_by: Option<String>,
    pub trainer_type: Option<String>,
    #[serde(default)]
    pub sight: i32,
    pub script: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Events {
    pub rom: String,
    #[serde(default)]
    pub sha1: String,
    pub scripts: BTreeMap<String, Script>,
    pub map_scripts: BTreeMap<String, MapScripts>,
    pub triggers: Vec<TriggerEvent>,
    pub objects: Vec<ObjectRef>,
}

impl Events {
    /// Loads `dir/events.json`.
    pub fn load(dir: impl AsRef<Path>) -> Result<Events> {
        load_json(&dir.as_ref().join("events.json"))
    }

    pub fn script(&self, label: &str) -> Option<&Script> {
        self.scripts.get(label)
    }
}

/// Parses a JSON data file, naming the file in errors.
pub(crate) fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    serde_json::from_str(&text).map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))
}

/// `None` when the file doesn't exist (data built before it was compiled),
/// an error when it exists but doesn't parse.
pub(crate) fn load_optional<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    load_json(path).map(Some)
}
