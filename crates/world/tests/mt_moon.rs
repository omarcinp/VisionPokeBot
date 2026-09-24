//! Localization inside Mt. Moon (a cave tileset) on emulator captures
//! (skipped without data/world or the fixtures).

use std::path::Path;

use pokebot_world::localize::PLAYER_SPRITE;
use pokebot_world::{Localizer, World};

#[test]
fn mt_moon_1f_is_located_like_outdoor_maps() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let Ok(world) = World::load(root.join("data/world")) else {
        return;
    };
    let localizer = Localizer::new(&world);
    let map = world.map("MtMoon_1F").expect("MtMoon_1F");
    for (fixture, x, y) in [("mtmoon-1f", 18, 35), ("mtmoon-1f-b", 18, 33)] {
        let Ok(frame) =
            pokebot_video::png::load(root.join(format!("captures/fixtures/{fixture}.png")))
        else {
            return;
        };
        let found = localizer
            .locate_in(&frame, map, None, 0, &[PLAYER_SPRITE])
            .unwrap_or_else(|| panic!("{fixture}: not located"));
        assert_eq!((found.pose.x, found.pose.y), (x, y), "{fixture}");
        // Outdoor frames score 996–1000 when standing still.
        assert!(found.score >= 990, "{fixture}: score {}", found.score);
        // The whole world: no other cave takes it.
        let anywhere = localizer
            .locate_anywhere(&frame, &[PLAYER_SPRITE])
            .unwrap_or_else(|| panic!("{fixture}: not located anywhere"));
        assert_eq!(anywhere.pose, found.pose, "{fixture}");
    }
}
