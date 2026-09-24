//! Maps that share a layout (skipped without data/world or the fixtures).

use std::path::Path;

use pokebot_state::PlayerPose;
use pokebot_world::localize::PLAYER_SPRITE;
use pokebot_world::{Localizer, World};

/// Live (Switch): walking into the Viridian Forest gate from Route 2's north
/// end was located in the south gate, which has the same layout.
#[test]
fn the_gate_behind_the_warp_wins_over_its_twin() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let Ok(world) = World::load(root.join("data/world")) else {
        return;
    };
    let Ok(frame) =
        pokebot_video::png::load(root.join("captures/fixtures/switch-forest-north-gate.png"))
    else {
        return;
    };
    let localizer = Localizer::new(&world);
    let hint = PlayerPose {
        map: "Route2".into(),
        x: 6,
        y: 13,
    };
    let found = localizer
        .locate_from(&frame, &hint, &[PLAYER_SPRITE])
        .expect("located");
    assert_eq!(found.pose.map, "Route2_ViridianForest_NorthEntrance");
    assert_eq!((found.pose.x, found.pose.y), (7, 1));
}
