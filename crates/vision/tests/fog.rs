//! Switch, Pokémon Tower 3F under fog: perception tracks the player up the
//! stairs from 2F onto the fogged floor (the run stood "stuck waiting:
//! locating the player"). Skipped without data/world or the fixtures.
use pokebot_core::NormalizedFrame;
use pokebot_state::PlayerPose;
use pokebot_vision::{FireRedPerception, PerceptionSystem};
use pokebot_world::World;
use std::{path::Path, sync::Arc, time::Instant};

#[test]
fn the_player_is_tracked_onto_a_fogged_floor() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let Ok(world) = World::load(root.join("data/world")) else {
        return;
    };
    let mut vision = FireRedPerception::with_world(Arc::new(world));
    vision.set_pose_hint(PlayerPose {
        map: "PokemonTower_2F".into(),
        x: 4,
        y: 10,
    });
    let mut frame_id = 0;
    for name in [
        "switch-tower-3f-fog-arrival.png",
        "switch-tower-3f-fog.png",
        "switch-tower-3f-fog-later.png",
    ] {
        let Ok(image) = pokebot_video::png::load(root.join("captures/fixtures").join(name)) else {
            continue;
        };
        frame_id += 1;
        let frame = NormalizedFrame::new(frame_id, Instant::now(), image).unwrap();
        let o = vision.observe(&frame);
        let player = o.player.unwrap_or_else(|| panic!("{name}: not located"));
        assert_eq!(
            (player.pose.map.as_str(), player.pose.x, player.pose.y),
            ("PokemonTower_3F", 4, 10),
            "{name}"
        );
    }
}
