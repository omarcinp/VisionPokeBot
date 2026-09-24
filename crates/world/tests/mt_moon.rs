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

/// Live: dark fade frames and battle wipes (black sweeping over the view)
/// were located in MtMoon_B1F's black void at score 850–1000, and the
/// navigator then planned from a map the player wasn't on. Black on the
/// frame matching black on the render is no evidence.
#[test]
fn black_frames_and_battle_wipes_are_not_located_in_b1f_void() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let Ok(world) = World::load(root.join("data/world")) else {
        return;
    };
    let localizer = Localizer::new(&world);
    let b1f = world.map("MtMoon_B1F").expect("MtMoon_B1F");
    for grey in [0, 8, 16] {
        let frame = pokebot_core::RgbImage::filled(240, 160, [grey; 3]);
        let found = localizer.locate_in(&frame, b1f, None, 0, &[PLAYER_SPRITE]);
        assert!(found.is_none(), "black {grey}: {found:?}");
    }
    // Battle wipes recorded on MtMoon_1F (19, 27): the part of the view still
    // showing is 1F, the rest black.
    for fixture in [
        "mtmoon-battle-wipe",
        "mtmoon-battle-wipe-b",
        "mtmoon-intro-fade",
    ] {
        let Ok(frame) =
            pokebot_video::png::load(root.join(format!("captures/fixtures/{fixture}.png")))
        else {
            continue;
        };
        let found = localizer.locate_in(&frame, b1f, None, 0, &[PLAYER_SPRITE]);
        assert!(found.is_none(), "{fixture}: {found:?}");
    }
}
