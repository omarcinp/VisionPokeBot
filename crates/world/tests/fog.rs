//! Maps under fog (`WEATHER_FOG_HORIZONTAL`: Pokémon Tower 3F–7F, the Lost
//! Cave): the Switch run arriving on Pokémon Tower 3F matched nowhere and
//! stood "stuck waiting: locating the player" (skipped without data/world
//! or the fixtures).

use std::path::Path;

use pokebot_core::RgbImage;
use pokebot_state::PlayerPose;
use pokebot_world::localize::PLAYER_SPRITE;
use pokebot_world::{Localizer, World};

fn root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn world() -> Option<World> {
    World::load(root().join("data/world")).ok()
}

fn fixture(name: &str) -> Option<RgbImage> {
    pokebot_video::png::load(root().join("captures/fixtures").join(name)).ok()
}

/// Up the stairs from 2F (its warp at (4, 10) leads to 3F's at (4, 10)):
/// the player stands on the arrival tile, the stairs drawn on the tiles to
/// the left, the channelers of (9, 9) and (10, 14) in view. Just arrived
/// (the map-name popup up), then 15 s on and later, not moved.
const FIXTURES: [&str; 3] = [
    "switch-tower-3f-fog-arrival.png",
    "switch-tower-3f-fog.png",
    "switch-tower-3f-fog-later.png",
];

#[test]
fn fogged_frames_are_located_on_their_map() {
    let Some(world) = world() else {
        return;
    };
    let localizer = Localizer::new(&world);
    let tower = world.map("PokemonTower_3F").unwrap();
    assert!(tower.is_foggy());
    for name in FIXTURES {
        let Some(frame) = fixture(name) else {
            continue;
        };
        for near in [Some((4, 10)), Some((6, 12)), None] {
            let found = localizer
                .locate_in(&frame, tower, near, 3, &[PLAYER_SPRITE])
                .unwrap_or_else(|| panic!("{name}: not located (near {near:?})"));
            assert_eq!(
                (found.pose.x, found.pose.y),
                (4, 10),
                "{name} near {near:?}"
            );
        }
        // Tracked up the stairs from 2F.
        let hint = PlayerPose {
            map: "PokemonTower_2F".into(),
            x: 4,
            y: 10,
        };
        let found = localizer
            .locate_from(&frame, &hint, &[PLAYER_SPRITE])
            .unwrap_or_else(|| panic!("{name}: not tracked from 2F"));
        assert_eq!(
            (found.pose.map.as_str(), found.pose.x, found.pose.y),
            ("PokemonTower_3F", 4, 10),
            "{name}"
        );
    }
}

/// The fog model is no looser match anywhere else: the fogged frames match
/// no other map (4F–7F share 3F's tileset), and on the fog maps no unfogged
/// frame matches (Oak's pale mint intro screen, less half Pokémon Tower
/// 5F's green floor, is grey: it matched 5F's corner at 917 before the
/// fog's streaks were required).
#[test]
fn fog_matches_only_fogged_frames_of_the_right_map() {
    let Some(world) = world() else {
        return;
    };
    let localizer = Localizer::new(&world);
    for name in FIXTURES {
        let Some(frame) = fixture(name) else {
            continue;
        };
        let found = localizer.locate_anywhere_candidates(&frame, &[PLAYER_SPRITE]);
        let poses: Vec<_> = found
            .iter()
            .map(|o| (o.pose.map.as_str(), o.pose.x, o.pose.y))
            .collect();
        assert_eq!(poses, [("PokemonTower_3F", 4, 10)], "{name}");
    }
    let Some(oak) = fixture("switch-oak-arrow.png") else {
        return;
    };
    for map in world.maps().filter(|m| m.is_foggy()) {
        assert_eq!(
            localizer.locate_in(&oak, map, None, 0, &[PLAYER_SPRITE]),
            None,
            "{}",
            map.name
        );
    }
}
