use pokebot_core::ControllerCommand;
use serde::{Deserialize, Serialize};

use crate::{Knowledge, PlayerPose, ScreenState};

/// Whether the bot currently trusts its reconstructed state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SynchronizationState {
    /// Not enough has been observed to know where the game is.
    #[default]
    Unsynchronized,
    Synchronized,
    Desynchronized,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct InputState {
    pub commands_issued: u64,
    pub last_command: Option<ControllerCommand>,
    pub last_command_frame: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Gender {
    Boy,
    Girl,
}

/// Facts about the save file's story progress.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Progression {
    pub gender: Knowledge<Gender>,
    pub player_name: Knowledge<String>,
    pub rival_name: Knowledge<String>,
    /// The player has been seen in control of the character.
    pub in_control: Knowledge<bool>,
    /// Gym badges earned, in the order they were received.
    #[serde(default)]
    pub badges: Knowledge<Vec<String>>,
}

/// The goal the bot is pursuing and where it is in it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GoalStatus {
    pub goal: String,
    pub phase: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PlayerState {
    /// Last confirmed position (Observed when matched on screen).
    pub pose: Knowledge<PlayerPose>,
}

/// Everything the bot believes about the game. Grows phase by phase (world
/// position, party, inventory, battle, ...); every field carries provenance.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GameState {
    pub screen: Knowledge<ScreenState>,
    pub synchronization: SynchronizationState,
    pub input: InputState,
    pub dropped_frames: u64,
    pub progression: Progression,
    pub player: PlayerState,
    pub in_battle: bool,
    pub active_goal: Option<GoalStatus>,
    /// Party in slot order.
    pub party: Knowledge<Vec<crate::PartyMon>>,
    pub bag: crate::Bag,
    pub money: Knowledge<u32>,
    pub pc: crate::PcStorage,
    pub pokedex: crate::Pokedex,
    /// Flags, vars, visited maps, respawn point and NPCs.
    #[serde(default)]
    pub world: crate::WorldBelief,
}

impl GameState {
    /// The part of the state that belongs to the save file.
    pub fn saved_knowledge(&self) -> crate::SavedKnowledge {
        crate::SavedKnowledge {
            party: self.party.clone(),
            bag: self.bag.clone(),
            money: self.money.clone(),
            pc: self.pc.clone(),
            pokedex: self.pokedex.clone(),
            // Infeasible intents are about this session, not the save.
            world: crate::WorldBelief {
                infeasible: Default::default(),
                ..self.world.clone()
            },
        }
    }
}
