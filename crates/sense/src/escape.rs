//! Where Dig and the Escape Rope lead, followed as the player moves: each
//! change of the committed pose's map is checked against the game's rule
//! ([`pokebot_world::escape::after_map_change`]); the belief keeps the
//! outdoor tile outside the entrance last taken, so an escape planned in a
//! cave goes back out the mouth it came in by.

use pokebot_state::{GameEvent, GameState, PlayerPose};
use pokebot_world::escape::{after_map_change, EscapeChange};
use pokebot_world::World;

#[derive(Debug, Default)]
pub(crate) struct EscapeTracker {
    /// The last committed pose: the tile the player left a map from.
    last: Option<PlayerPose>,
}

impl EscapeTracker {
    pub(crate) fn observe(&mut self, world: Option<&World>, state: &GameState) -> Vec<GameEvent> {
        let Some(pose) = state.player.pose.value.as_ref() else {
            return Vec::new();
        };
        let last = self.last.replace(pose.clone());
        let (Some(world), Some(last)) = (world, last) else {
            return Vec::new();
        };
        let known = state.world.escape.value.as_ref();
        match after_map_change(world, &last, pose) {
            EscapeChange::Set(e) if known != Some(&e) => {
                vec![GameEvent::EscapeWarpSet { escape: Some(e) }]
            }
            EscapeChange::Lost if known.is_some() => {
                vec![GameEvent::EscapeWarpSet { escape: None }]
            }
            _ => Vec::new(),
        }
    }
}
