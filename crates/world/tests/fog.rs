//! Maps under fog (`WEATHER_FOG_HORIZONTAL`: Pokémon Tower 3F–7F, the Lost
//! Cave): the Switch run arriving on Pokémon Tower 3F matched nowhere and
//! stood "stuck waiting: locating the player", then on 7F again (skipped
//! without data/world or the fixtures).

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

/// A Switch frame, the map and tile the player stands on, and the map and
/// tile they came from (tracked from there).
type Fixture = (
    &'static str,
    &'static str,
    (i32, i32),
    (&'static str, (i32, i32)),
);

const FIXTURES: [Fixture; 8] = [
    // Up the stairs from 2F (its warp at (4, 10) leads to 3F's at
    // (4, 10)): on the arrival tile, the stairs drawn to the left, the
    // channelers of (9, 9) and (10, 14) in view. Just arrived (the
    // map-name popup up), then 15 s on and later, not moved.
    (
        "switch-tower-3f-fog-arrival.png",
        "PokemonTower_3F",
        (4, 10),
        ("PokemonTower_2F", (4, 10)),
    ),
    (
        "switch-tower-3f-fog.png",
        "PokemonTower_3F",
        (4, 10),
        ("PokemonTower_2F", (4, 10)),
    ),
    (
        "switch-tower-3f-fog-later.png",
        "PokemonTower_3F",
        (4, 10),
        ("PokemonTower_2F", (4, 10)),
    ),
    // Below the channeler of (10, 14), up and to the left; 5F's corner
    // matched too (861) while its void over 3F's floor was left out.
    (
        "switch-tower-3f-fog-channeler.png",
        "PokemonTower_3F",
        (11, 15),
        ("PokemonTower_3F", (11, 16)),
    ),
    // Beside the stairs down at (18, 10), the channeler of (17, 7) three
    // tiles up, the item balls of (12, 11) and the channeler of (15, 13)
    // to the lower left.
    (
        "switch-tower-4f-fog.png",
        "PokemonTower_4F",
        (17, 10),
        ("PokemonTower_3F", (18, 10)),
    ),
    // Two tiles left of the stairs down at (18, 10), the channeler of
    // (13, 10) three tiles left.
    (
        "switch-tower-6f-fog.png",
        "PokemonTower_6F",
        (16, 10),
        ("PokemonTower_5F", (18, 10)),
    ),
    // Up the stairs from 6F onto 7F's arrival tile, the map's bottom edge
    // two tiles down: half the view is the fog over the void. The orange
    // tombstones under the fog saturate the blend ((255, 253, 250) over
    // render (247, 235, 165)).
    (
        "switch-tower-7f-fog.png",
        "PokemonTower_7F",
        (11, 16),
        ("PokemonTower_6F", (11, 16)),
    ),
    (
        "switch-tower-7f-fog-00220500.png",
        "PokemonTower_7F",
        (11, 16),
        ("PokemonTower_6F", (11, 16)),
    ),
];

#[test]
fn fogged_frames_are_located_on_their_map() {
    let Some(world) = world() else {
        return;
    };
    let localizer = Localizer::new(&world);
    for (name, map, (x, y), (from, (fx, fy))) in FIXTURES {
        let Some(frame) = fixture(name) else {
            continue;
        };
        let tower = world.map(map).unwrap();
        assert!(tower.is_foggy());
        for near in [Some((x, y)), Some((x + 2, y - 2)), None] {
            let found = localizer
                .locate_in(&frame, tower, near, 3, &[PLAYER_SPRITE])
                .unwrap_or_else(|| panic!("{name}: not located (near {near:?})"));
            assert_eq!((found.pose.x, found.pose.y), (x, y), "{name} near {near:?}");
        }
        let hint = PlayerPose {
            map: from.into(),
            x: fx,
            y: fy,
        };
        let found = localizer
            .locate_from(&frame, &hint, &[PLAYER_SPRITE])
            .unwrap_or_else(|| panic!("{name}: not tracked from {from}"));
        assert_eq!(
            (found.pose.map.as_str(), found.pose.x, found.pose.y),
            (map, x, y),
            "{name}"
        );
    }
}

/// The fog model is no looser match anywhere else: the fogged frames match
/// no other map (the tower's floors share a tileset), and on the fog maps
/// no unfogged frame matches (Oak's pale mint intro screen, less half
/// Pokémon Tower 5F's green floor, is grey: it matched 5F's corner at 917
/// before the fog's streaks were required).
#[test]
fn fog_matches_only_fogged_frames_of_the_right_map() {
    let Some(world) = world() else {
        return;
    };
    let localizer = Localizer::new(&world);
    for (name, map, (x, y), _) in FIXTURES {
        let Some(frame) = fixture(name) else {
            continue;
        };
        for other in world.maps().filter(|m| m.is_foggy() && m.name != map) {
            assert_eq!(
                localizer.locate_in(&frame, other, None, 0, &[PLAYER_SPRITE]),
                None,
                "{name} on {}",
                other.name
            );
        }
        let found = localizer.locate_anywhere_candidates(&frame, &[PLAYER_SPRITE]);
        let poses: Vec<_> = found
            .iter()
            .map(|o| (o.pose.map.as_str(), o.pose.x, o.pose.y))
            .collect();
        assert_eq!(poses, [(map, x, y)], "{name}");
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
