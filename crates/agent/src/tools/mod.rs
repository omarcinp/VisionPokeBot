//! Tools (spec §7): the things a plan's intents are carried out with. Each
//! tool runs an [`Intent`] to completion or failure through a
//! [`ToolContext`], which issues actions with interrupts handled (§7.3),
//! and reports what it learned as events.
//!
//! Every tool is deterministic given the same observations; none sleeps.

mod battle;
mod beat;
mod buy;
mod catch;
mod context;
pub mod dialogue;
pub mod effects;
pub mod field;
mod go;
mod heal;
mod locate;
pub mod lookup;
mod medicine;
pub mod menu;
mod party_audit;
pub mod probe;
mod save;
pub mod scene;
mod talk;
pub mod teach;
mod unstick;

use pokebot_state::{Direction, GameEvent, PlayerPose, Pocket};
use pokebot_world::World;
use serde::{Deserialize, Serialize};

pub use context::{AsStep, Expects, StepContext, ToolContext, ToolStep, SETTLE_FRAMES};
pub use dialogue::{identify, Conversation, DialogueTool};
pub use unstick::UnstickTool;

use crate::nav::Destination;

/// YES or NO to a question in dialogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Answer {
    Yes,
    No,
}

/// Where a `Go` ends. Mirrors [`Destination`] (which has no `Deserialize`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Dest {
    Tile {
        map: String,
        x: i32,
        y: i32,
    },
    /// Stand next to `(x, y)` facing it.
    Facing {
        map: String,
        x: i32,
        y: i32,
    },
    /// Use warp number `warp` of `map`.
    Warp {
        map: String,
        warp: usize,
    },
    /// Any tile of `map` (the planner's `Go { dest: map }`): the leg is done
    /// once the player stands on it.
    Map {
        map: String,
    },
}

impl Dest {
    pub fn map(&self) -> &str {
        match self {
            Dest::Tile { map, .. }
            | Dest::Facing { map, .. }
            | Dest::Warp { map, .. }
            | Dest::Map { map } => map,
        }
    }
}

impl From<&Dest> for Destination {
    fn from(d: &Dest) -> Destination {
        match d.clone() {
            Dest::Tile { map, x, y } => Destination::Tile { map, x, y },
            Dest::Facing { map, x, y } => Destination::Facing { map, x, y },
            Dest::Warp { map, warp } => Destination::Warp { map, warp },
            // The navigator's cross-map routing only needs some tile of the
            // map; `GoTool` stops on arrival on the map, whichever tile.
            Dest::Map { map } => Destination::Tile { map, x: 0, y: 0 },
        }
    }
}

/// How a battle is played.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BattlePlan {
    /// Wild: the catch policy decides catch, fight or flee; trainer: fight.
    #[default]
    Auto,
    /// Fight it out (no catch attempt).
    Fight,
    /// Run from a wild battle when possible.
    Flee,
}

/// A fact a `Probe` establishes by opening a screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fact", rename_all = "snake_case")]
pub enum ProbeFact {
    Party,
    /// The items of a bag pocket (`PocketObserved`).
    Pocket {
        pocket: Pocket,
    },
    /// The badges on the trainer card.
    TrainerCard,
    /// The lit towns on the Fly map.
    FlyMap,
    /// The caught marks of the Pokédex.
    Pokedex,
}

/// What a tool carries out. The planner produces these; the shape is the
/// minimum the tools need (the planner crate converts to it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "intent", rename_all = "snake_case")]
pub enum Intent {
    Go {
        dest: Dest,
    },
    /// Talk to map object `object` (its decomp local id), answering YES/NO
    /// questions in order.
    Talk {
        map: String,
        object: u32,
        #[serde(default)]
        answers: Vec<Answer>,
    },
    /// Follow the conversation on screen as compiled script `script`
    /// (path `path` when the plan knows which), answering in order.
    RunScript {
        script: String,
        #[serde(default)]
        path: Option<usize>,
        #[serde(default)]
        answers: Vec<Answer>,
    },
    /// Heal at a Pokémon Center (the given map, or the nearest).
    Heal {
        #[serde(default)]
        center: Option<String>,
    },
    /// Fight `trainer` (map object `object` of `map`), healing at the
    /// nearest Center first when the lead isn't ready for it.
    Beat {
        trainer: String,
        map: String,
        object: u32,
    },
    /// Play the battle on screen.
    Battle {
        #[serde(default)]
        policy: BattlePlan,
    },
    /// Walk the encounter tiles of `map` (the current map when `None`)
    /// until `species` is caught.
    Catch {
        species: String,
        #[serde(default)]
        map: Option<String>,
    },
    /// Fight wild battles on `map` until the lead (`species`) reaches
    /// `level`.
    Train {
        map: String,
        species: String,
        level: u8,
    },
    /// Buy `count` of `item` at the nearest mart selling it (0: the ball
    /// stock policy decides).
    Buy {
        item: String,
        count: u16,
    },
    Probe {
        #[serde(flatten)]
        fact: ProbeFact,
    },
    Save,
    /// Get out of an unrecognised screen (§7.4).
    Unstick,
    /// Find out which of several lookalike maps the player is on (the
    /// state's `player.candidates`), by walking out to one that names
    /// itself.
    ConfirmLocation,
    /// Teach the TM or HM `item` (`ITEM_HM01`) to `member`: a species
    /// constant (the first party member of that species), `lead`, or
    /// `slot:N`. With four moves known, the lowest-value move is forgotten
    /// (never the last damaging one, never an HM move).
    Teach {
        item: String,
        member: String,
    },
    /// Use a field move: `mv` (`MOVE_CUT`) on the obstacle `at` faces
    /// (`Dest::Facing`: Cut, Rock Smash, Strength, Surf, Waterfall), to
    /// the map `at` names (Fly), or where the player stands (Flash).
    /// Strength then pushes the boulder along `push`.
    FieldMove {
        mv: String,
        #[serde(default)]
        at: Option<Dest>,
        #[serde(default)]
        push: Vec<Direction>,
    },
}

impl Intent {
    /// Short name for logs and the infeasible set.
    pub fn name(&self) -> &'static str {
        match self {
            Intent::Go { .. } => "Go",
            Intent::Talk { .. } => "Talk",
            Intent::RunScript { .. } => "RunScript",
            Intent::Heal { .. } => "Heal",
            Intent::Beat { .. } => "Beat",
            Intent::Battle { .. } => "Battle",
            Intent::Catch { .. } => "Catch",
            Intent::Train { .. } => "Train",
            Intent::Buy { .. } => "Buy",
            Intent::Probe { .. } => "Probe",
            Intent::Save => "Save",
            Intent::Unstick => "Unstick",
            Intent::ConfirmLocation => "ConfirmLocation",
            Intent::Teach { .. } => "Teach",
            Intent::FieldMove { .. } => "FieldMove",
        }
    }

    /// The tool intent for a planner intent (spec §4.1 → §7.2). `Beat`
    /// becomes a `Talk` to the object on `map` whose script fights the
    /// trainer (found in the compiled events, so it needs the world);
    /// the PC box probe and `Unsupported` have no tool yet and fail as
    /// `Unsupported`.
    pub fn from_planned(
        intent: &pokebot_planner::Intent,
        world: Option<&World>,
    ) -> Result<Intent, ToolError> {
        use pokebot_planner::Intent as P;
        Ok(match intent {
            P::Go { dest } => Intent::Go {
                dest: Dest::Map { map: dest.clone() },
            },
            P::Talk {
                map,
                object,
                answers,
            } => Intent::Talk {
                map: map.clone(),
                object: *object,
                answers: parse_answers(answers),
            },
            P::RunScript {
                script,
                path,
                answers,
                ..
            } => Intent::RunScript {
                script: script.clone(),
                path: Some(*path),
                answers: parse_answers(answers),
            },
            P::Heal { center } => Intent::Heal {
                center: Some(center.clone()),
            },
            P::Battle { policy } => Intent::Battle {
                policy: match policy.as_str() {
                    "fight" => BattlePlan::Fight,
                    "flee" => BattlePlan::Flee,
                    _ => BattlePlan::Auto,
                },
            },
            P::Beat { trainer, map } => {
                let object = world
                    .and_then(|w| trainer_object(w, map, trainer))
                    .ok_or_else(|| {
                        ToolError::Unsupported(format!("no object on {map} fights {trainer}"))
                    })?;
                Intent::Beat {
                    trainer: trainer.clone(),
                    map: map.clone(),
                    object,
                }
            }
            P::Train {
                map,
                species,
                level,
            } => Intent::Train {
                map: map.clone(),
                species: species.clone(),
                level: *level,
            },
            P::Catch { species, map, .. } => Intent::Catch {
                species: species.clone(),
                map: Some(map.clone()),
            },
            P::Buy { item, count, .. } => Intent::Buy {
                item: item.clone(),
                count: u16::try_from(*count).unwrap_or(u16::MAX),
            },
            P::Teach { hm, mon } => Intent::Teach {
                item: hm.clone(),
                member: mon.clone(),
            },
            P::Probe { fact } => Intent::Probe {
                fact: match fact {
                    pokebot_planner::ProbeFact::TrainerCard => ProbeFact::TrainerCard,
                    pokebot_planner::ProbeFact::BagPocket(pocket) => {
                        ProbeFact::Pocket { pocket: *pocket }
                    }
                    pokebot_planner::ProbeFact::FlyMap => ProbeFact::FlyMap,
                    pokebot_planner::ProbeFact::Pokedex => ProbeFact::Pokedex,
                    pokebot_planner::ProbeFact::Party => ProbeFact::Party,
                    pokebot_planner::ProbeFact::PcBoxes => {
                        return Err(ToolError::Unsupported("PC box probe: no tool yet".into()))
                    }
                },
            },
            P::Save => Intent::Save,
            P::Unstick => Intent::Unstick,
            P::Unsupported { reason, .. } => return Err(ToolError::Unsupported(reason.clone())),
        })
    }
}

/// The planner's `YES`/`NO` answers; menu choices (`choice=N`) are not
/// answers to questions and are left out.
fn parse_answers(answers: &[String]) -> Vec<Answer> {
    answers
        .iter()
        .filter_map(|a| match a.to_ascii_uppercase().as_str() {
            "YES" => Some(Answer::Yes),
            "NO" => Some(Answer::No),
            _ => None,
        })
        .collect()
}

/// The object of `map` whose compiled script battles `trainer`.
fn trainer_object(world: &World, map: &str, trainer: &str) -> Option<u32> {
    let events = world.events()?;
    let mut objects: Vec<&pokebot_world::events::ObjectRef> =
        events.objects.iter().filter(|o| o.map == map).collect();
    objects.sort_by_key(|o| o.local_id);
    objects.into_iter().find_map(|o| {
        let script = events.script(o.script.as_deref()?)?;
        script
            .paths
            .iter()
            .flat_map(|p| p.does.iter())
            .any(|e| matches!(e, pokebot_world::events::Effect::Battle { battle, .. } if battle == trainer))
            .then_some(o.local_id)
    })
}

/// Context-free conversion: everything but `Beat` (which needs the world
/// to find the trainer's object).
impl TryFrom<&pokebot_planner::Intent> for Intent {
    type Error = ToolError;

    fn try_from(intent: &pokebot_planner::Intent) -> Result<Intent, ToolError> {
        Intent::from_planned(intent, None)
    }
}

impl std::fmt::Display for Intent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Intent::Go { dest } => write!(f, "Go {dest:?}"),
            Intent::Talk {
                map,
                object,
                answers,
            } => write!(f, "Talk {map}#{object} {answers:?}"),
            Intent::RunScript { script, path, .. } => match path {
                Some(p) => write!(f, "RunScript {script}[{p}]"),
                None => write!(f, "RunScript {script}"),
            },
            Intent::Heal { center } => write!(f, "Heal {}", center.as_deref().unwrap_or("nearest")),
            Intent::Beat {
                trainer,
                map,
                object,
            } => write!(f, "Beat {trainer} ({map}#{object})"),
            Intent::Battle { policy } => write!(f, "Battle {policy:?}"),
            Intent::Catch { species, map } => match map {
                Some(map) => write!(f, "Catch {species} on {map}"),
                None => write!(f, "Catch {species}"),
            },
            Intent::Train {
                map,
                species,
                level,
            } => write!(f, "Train {species} to Lv{level} on {map}"),
            Intent::Buy { item, count } => write!(f, "Buy {item} x{count}"),
            Intent::Probe { fact } => write!(f, "Probe {fact:?}"),
            Intent::Save => write!(f, "Save"),
            Intent::Unstick => write!(f, "Unstick"),
            Intent::ConfirmLocation => write!(f, "ConfirmLocation"),
            Intent::Teach { item, member } => write!(f, "Teach {item} to {member}"),
            Intent::FieldMove { mv, at, push } => {
                write!(f, "FieldMove {mv}")?;
                if let Some(at) = at {
                    write!(f, " at {at:?}")?;
                }
                if !push.is_empty() {
                    write!(f, " push {push:?}")?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("replan: {0}")]
    Replan(String),
    #[error("{0}")]
    Failed(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("no tool serves {0}")]
    NoTool(String),
    /// The tool is already running further up the call chain.
    #[error("{0} is busy")]
    Busy(String),
    #[error("stopped by user")]
    Stopped,
    #[error(transparent)]
    Device(#[from] pokebot_core::Error),
}

impl From<crate::ExecutorError> for ToolError {
    fn from(e: crate::ExecutorError) -> Self {
        match e {
            crate::ExecutorError::Stopped => ToolError::Stopped,
            crate::ExecutorError::Device(e) => ToolError::Device(e),
            crate::ExecutorError::TaskFailed { reason, .. } => ToolError::Failed(reason),
        }
    }
}

/// What a tool run produced.
#[derive(Debug)]
pub struct ToolOutcome {
    pub result: Result<(), ToolError>,
    /// Facts established or refuted while running, with provenance. Events
    /// a tool emits through [`ToolContext::emit`] are collected here by
    /// `invoke`; anything a tool returns here on top is emitted by it.
    pub learned: Vec<GameEvent>,
    /// Where the player ended up, if known.
    pub pose: Option<PlayerPose>,
}

impl ToolOutcome {
    pub fn ok() -> Self {
        Self {
            result: Ok(()),
            learned: Vec::new(),
            pose: None,
        }
    }

    pub fn failed(reason: impl Into<String>) -> Self {
        Self::from(ToolError::Failed(reason.into()))
    }

    pub fn is_ok(&self) -> bool {
        self.result.is_ok()
    }
}

impl From<ToolError> for ToolOutcome {
    fn from(e: ToolError) -> Self {
        Self {
            result: Err(e),
            learned: Vec::new(),
            pose: None,
        }
    }
}

impl From<Result<(), ToolError>> for ToolOutcome {
    fn from(result: Result<(), ToolError>) -> Self {
        Self {
            result,
            learned: Vec::new(),
            pose: None,
        }
    }
}

pub trait Tool {
    fn name(&self) -> &str;
    /// Static: which intents this tool serves.
    fn serves(&self, intent: &Intent) -> bool;
    /// Runs to completion or failure; may call other tools through `ctx`.
    fn run(&mut self, intent: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome;
}

/// The tools a context dispatches to, by [`Tool::serves`] in order. A tool
/// is taken out while it runs, so it can't be invoked from inside itself.
pub struct Toolbox {
    tools: Vec<Option<Box<dyn Tool>>>,
}

impl Default for Toolbox {
    /// The built-in tools.
    fn default() -> Self {
        Toolbox::new(vec![
            Box::new(go::GoTool),
            Box::new(locate::ConfirmLocationTool),
            Box::new(talk::TalkTool),
            Box::new(DialogueTool),
            Box::new(heal::HealTool),
            Box::new(beat::BeatTool),
            Box::new(battle::BattleTool),
            Box::new(catch::CatchTool),
            Box::new(catch::TrainTool),
            Box::new(buy::BuyTool),
            Box::new(probe::ProbeTool),
            Box::new(teach::TeachTool),
            Box::new(field::FieldMoveTool),
            Box::new(save::SaveTool),
            Box::new(UnstickTool),
        ])
    }
}

impl Toolbox {
    pub fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        Self {
            tools: tools.into_iter().map(Some).collect(),
        }
    }

    /// The probe screens the built-in [`probe::ProbeTool`] opens, as the
    /// planner names them (`pokebot_planner::ProbeFact::kind`): the party
    /// and the PC boxes have no tool yet.
    pub fn supported_probes() -> std::collections::BTreeSet<String> {
        ["party", "trainer_card", "bag_pocket", "fly_map", "pokedex"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    pub fn empty() -> Self {
        Self { tools: Vec::new() }
    }

    /// Adds a tool ahead of the existing ones (it wins ties in `serves`).
    pub fn prepend(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(0, Some(tool));
    }

    /// Names of the tools, in dispatch order.
    pub fn names(&self) -> Vec<String> {
        self.tools
            .iter()
            .map(|t| {
                t.as_ref()
                    .map_or("(running)".into(), |t| t.name().to_owned())
            })
            .collect()
    }

    /// Takes the first tool serving `intent` out (`None` if none does, or
    /// it is running).
    fn take(&mut self, intent: &Intent) -> Result<(usize, Box<dyn Tool>), ToolError> {
        let mut busy = None;
        for (i, slot) in self.tools.iter_mut().enumerate() {
            match slot {
                Some(tool) if tool.serves(intent) => {
                    let tool = slot.take().expect("checked above");
                    return Ok((i, tool));
                }
                None => busy = Some(i),
                _ => {}
            }
        }
        match busy {
            Some(_) => Err(ToolError::Busy(intent.name().to_owned())),
            None => Err(ToolError::NoTool(intent.to_string())),
        }
    }

    fn put_back(&mut self, index: usize, tool: Box<dyn Tool>) {
        self.tools[index] = Some(tool);
    }
}

/// The `GoalProgress` events tools log with.
pub(crate) fn progress(phase: &str, detail: impl Into<String>) -> GameEvent {
    GameEvent::GoalProgress {
        goal: "Goal".into(),
        phase: phase.into(),
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intents_round_trip_through_json() {
        let intents = vec![
            Intent::Go {
                dest: Dest::Facing {
                    map: "PewterCity".into(),
                    x: 3,
                    y: 4,
                },
            },
            Intent::Talk {
                map: "PewterCity_PokemonCenter_1F".into(),
                object: 3,
                answers: vec![Answer::Yes],
            },
            Intent::RunScript {
                script: "S".into(),
                path: Some(1),
                answers: vec![],
            },
            Intent::Heal { center: None },
            Intent::Battle {
                policy: BattlePlan::Fight,
            },
            Intent::Catch {
                species: "SPECIES_RATTATA".into(),
                map: Some("Route1".into()),
            },
            Intent::Train {
                map: "Route1".into(),
                species: "SPECIES_IVYSAUR".into(),
                level: 20,
            },
            Intent::Go {
                dest: Dest::Map {
                    map: "Route4".into(),
                },
            },
            Intent::Buy {
                item: "ITEM_POKE_BALL".into(),
                count: 5,
            },
            Intent::Probe {
                fact: ProbeFact::Pocket {
                    pocket: Pocket::PokeBalls,
                },
            },
            Intent::Probe {
                fact: ProbeFact::TrainerCard,
            },
            Intent::Save,
            Intent::Unstick,
            Intent::Teach {
                item: "ITEM_HM01".into(),
                member: "SPECIES_IVYSAUR".into(),
            },
            Intent::FieldMove {
                mv: "MOVE_STRENGTH".into(),
                at: Some(Dest::Facing {
                    map: "VictoryRoad_1F".into(),
                    x: 5,
                    y: 4,
                }),
                push: vec![Direction::Up, Direction::Up],
            },
        ];
        for intent in intents {
            let json = serde_json::to_string(&intent).unwrap();
            let back: Intent = serde_json::from_str(&json).unwrap();
            assert_eq!(back, intent, "{json}");
        }
        let heal: Intent = serde_json::from_str(r#"{"intent":"heal"}"#).unwrap();
        assert_eq!(heal, Intent::Heal { center: None });
        let probe: Intent =
            serde_json::from_str(r#"{"intent":"probe","fact":"pocket","pocket":"PokeBalls"}"#)
                .unwrap();
        assert!(matches!(probe, Intent::Probe { .. }));
    }

    #[test]
    fn the_planners_teach_becomes_the_teach_tool() {
        let planned = pokebot_planner::Intent::Teach {
            hm: "ITEM_HM01".into(),
            mon: "SPECIES_IVYSAUR".into(),
        };
        assert_eq!(
            Intent::try_from(&planned).unwrap(),
            Intent::Teach {
                item: "ITEM_HM01".into(),
                member: "SPECIES_IVYSAUR".into()
            }
        );
        assert!(Toolbox::default()
            .take(&Intent::Teach {
                item: "ITEM_HM01".into(),
                member: "lead".into()
            })
            .is_ok());
        assert!(Toolbox::default()
            .take(&Intent::FieldMove {
                mv: "MOVE_FLASH".into(),
                at: None,
                push: vec![]
            })
            .is_ok());
    }

    #[test]
    fn a_running_tool_cannot_be_invoked_again() {
        let mut toolbox = Toolbox::default();
        let (i, tool) = toolbox.take(&Intent::Unstick).unwrap();
        assert_eq!(tool.name(), "Unstick");
        assert!(matches!(
            toolbox.take(&Intent::Unstick),
            Err(ToolError::Busy(_))
        ));
        toolbox.put_back(i, tool);
        assert!(toolbox.take(&Intent::Unstick).is_ok());
        assert!(matches!(
            Toolbox::empty().take(&Intent::Save),
            Err(ToolError::NoTool(_))
        ));
    }
}
