//! Whole-world localization (skipped without data/world or the fixtures).

use std::path::Path;

use pokebot_core::RgbImage;
use pokebot_world::localize::PLAYER_SPRITE;
use pokebot_world::{Localizer, World};

fn world() -> Option<World> {
    World::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world")).ok()
}

/// A white flash scored 1000 on NavelRock_Fork's flat floor.
#[test]
fn a_flat_frame_is_nowhere() {
    let Some(world) = world() else {
        return;
    };
    let localizer = Localizer::new(&world);
    for colour in [[255, 251, 255], [0, 0, 0], [123, 123, 123]] {
        let frame = RgbImage::filled(240, 160, colour);
        assert_eq!(
            localizer.locate_anywhere(&frame, &[PLAYER_SPRITE]),
            None,
            "{colour:?}"
        );
    }
}

/// Splitting the search over threads finds what one thread finds.
#[test]
fn the_parallel_search_matches_the_sequential_one() {
    let Some(world) = world() else {
        return;
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let localizer = Localizer::new(&world);
    let mut maps: Vec<_> = world.maps().collect();
    maps.sort_by(|a, b| a.name.cmp(&b.name));
    for fixture in ["switch-forest-north-gate", "emu-tools-overworld"] {
        let Ok(frame) =
            pokebot_video::png::load(root.join(format!("captures/fixtures/{fixture}.png")))
        else {
            continue;
        };
        let sequential = maps
            .iter()
            .filter_map(|m| localizer.locate_in(&frame, m, None, 0, &[PLAYER_SPRITE]))
            .max_by_key(|o| o.score);
        let parallel = localizer.locate_anywhere(&frame, &[PLAYER_SPRITE]);
        assert!(parallel.is_some(), "{fixture}");
        assert_eq!(parallel, sequential, "{fixture}");
    }
}

/// Switch: standing in a Pokémon Center, every Center matches alike. The
/// plain global search still names one (as before); the unambiguous one,
/// used past a stale hint, doesn't guess.
#[test]
fn lookalike_maps_are_no_unambiguous_answer() {
    let Some(world) = world() else {
        return;
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let Ok(frame) =
        pokebot_video::png::load(root.join("captures/fixtures/switch-plan-budget-center.png"))
    else {
        return;
    };
    let localizer = Localizer::new(&world);
    let any = localizer
        .locate_anywhere(&frame, &[PLAYER_SPRITE])
        .expect("some Center");
    assert!(any.pose.map.ends_with("PokemonCenter_1F"), "{}", any.pose);
    assert_eq!(
        localizer.locate_anywhere_unambiguous(&frame, &[PLAYER_SPRITE]),
        None
    );
    // The candidates are every Center, on the same tile: what perception
    // reports instead of a guess.
    let candidates = localizer.locate_anywhere_candidates(&frame, &[PLAYER_SPRITE]);
    assert!(candidates.len() >= 2, "{candidates:?}");
    let maps: Vec<&str> = candidates.iter().map(|c| c.pose.map.as_str()).collect();
    assert!(maps.contains(&"PewterCity_PokemonCenter_1F"), "{maps:?}");
    assert!(maps.contains(&"ViridianCity_PokemonCenter_1F"), "{maps:?}");
    assert!(candidates
        .iter()
        .all(|c| (c.pose.x, c.pose.y) == (candidates[0].pose.x, candidates[0].pose.y)));
    // A map of its own is found either way.
    let Ok(gym) = pokebot_video::png::load(root.join("captures/fixtures/switch-pewter-gym.png"))
    else {
        return;
    };
    let found = localizer
        .locate_anywhere_unambiguous(&gym, &[PLAYER_SPRITE])
        .expect("the gym");
    assert_eq!(found.pose.map, "PewterCity_Gym");
}
