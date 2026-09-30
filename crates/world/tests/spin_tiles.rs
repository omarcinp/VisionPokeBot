//! The Rocket Hideout's spin tiles (skipped without data/world).

use std::path::Path;

use pokebot_world::path::{find_path_with, spin_dir, step_with, Obstacles, Walk};
use pokebot_world::World;

fn world() -> Option<World> {
    World::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world")).ok()
}

/// The Switch looped on B3F (10, 14): the arrows were walked as plain
/// floor and the slides took the player elsewhere. A step onto an arrow is
/// the whole slide; B3F is crossed from the stairs from B2F (18, 2) to the
/// stairs to B4F (15, 18), and every step of the path is a legal press.
#[test]
fn the_arrow_floor_is_crossed_by_its_slides() {
    let Some(world) = world() else { return };
    let map = world.map("RocketHideout_B3F").unwrap();
    let none = Obstacles::new();
    let walk = Walk {
        obstacles: &none,
        surf: false,
        opened: None,
    };
    let path =
        find_path_with(map, (18, 2), &walk, |_| 0, |t| t == (15, 18), |_| 0).expect("B3F crossed");
    let mut at = (18, 2);
    for s in &path {
        assert_eq!(step_with(map, at, s.dir, &walk), Some(*s), "from {at:?}");
        at = s.to;
    }
    assert_eq!(at, (15, 18));
    // Nothing ends on an arrow: a slide never stops on one.
    assert!(path.iter().all(|s| {
        map.tile(s.to.0, s.to.1)
            .is_none_or(|t| spin_dir(t.behavior).is_none())
    }));
    // A slide, not a walk: (6, 13) → onto the arrow at (6, 14)… the step
    // from (4, 14) Right lands where the arrows send it.
    let slide = step_with(map, (4, 14), pokebot_state::Direction::Right, &walk).unwrap();
    assert_ne!(slide.to, (5, 14));
}
