//! Tools (spec §7): the things a plan's intents are carried out with. Each
//! tool runs an [`Intent`] to completion or failure through a
//! [`ToolContext`], which issues actions with interrupts handled (§7.3),
//! and reports what it learned as events.
//!
//! Every tool is deterministic given the same observations; none sleeps.

mod battle;
mod buy;
mod catch;
mod context;
pub mod dialogue;
pub mod effects;
mod go;
mod heal;
pub mod lookup;
mod probe;
mod save;
mod talk;
mod unstick;

use pokebot_state::{GameEvent, PlayerPose, Pocket};
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
}

impl Dest {
    pub fn map(&self) -> &str {
        match self {
            Dest::Tile { map, .. } | Dest::Facing { map, .. } | Dest::Warp { map, .. } => map,
        }
    }
}

impl From<&Dest> for Destination {
    fn from(d: &Dest) -> Destination {
        match d.clone() {
            Dest::Tile { map, x, y } => Destination::Tile { map, x, y },
            Dest::Facing { map, x, y } => Destination::Facing { map, x, y },
            Dest::Warp { map, warp } => Destination::Warp { map, warp },
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
    /// The items of a bag pocket (`PocketObserved`).
    Pocket { pocket: Pocket },
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
    /// Play the battle on screen.
    Battle {
        #[serde(default)]
        policy: BattlePlan,
    },
    /// Walk the encounter tiles of the current map until `species` is
    /// caught.
    Catch {
        species: String,
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
}

impl Intent {
    /// Short name for logs and the infeasible set.
    pub fn name(&self) -> &'static str {
        match self {
            Intent::Go { .. } => "Go",
            Intent::Talk { .. } => "Talk",
            Intent::RunScript { .. } => "RunScript",
            Intent::Heal { .. } => "Heal",
            Intent::Battle { .. } => "Battle",
            Intent::Catch { .. } => "Catch",
            Intent::Buy { .. } => "Buy",
            Intent::Probe { .. } => "Probe",
            Intent::Save => "Save",
            Intent::Unstick => "Unstick",
        }
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
            Intent::Battle { policy } => write!(f, "Battle {policy:?}"),
            Intent::Catch { species } => write!(f, "Catch {species}"),
            Intent::Buy { item, count } => write!(f, "Buy {item} x{count}"),
            Intent::Probe { fact } => write!(f, "Probe {fact:?}"),
            Intent::Save => write!(f, "Save"),
            Intent::Unstick => write!(f, "Unstick"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
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
            Box::new(talk::TalkTool),
            Box::new(DialogueTool),
            Box::new(heal::HealTool),
            Box::new(battle::BattleTool),
            Box::new(catch::CatchTool),
            Box::new(buy::BuyTool),
            Box::new(probe::ProbeTool),
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
