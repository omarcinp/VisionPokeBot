//! Tracking from a pose hint: views of a step in progress, map edges and
//! repeating views (skipped without data/world or the fixtures).

use std::path::Path;

use pokebot_core::RgbImage;
use pokebot_state::{PlayerPose, Region};
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

fn pose(map: &str, x: i32, y: i32) -> PlayerPose {
    PlayerPose {
        map: map.into(),
        x,
        y,
    }
}

/// The map-name popup's area when fully down (see the vision crate's
/// `detect::map_popup`).
const POPUP: Region = Region {
    x: 0,
    y: 0,
    width: 128,
    height: 23,
};

/// Live audits (Switch Route 3, Pewter City and its Pokémon Center;
/// emulator Mt. Moon 1F): walking frames are 3–13 px off the tile grid,
/// scored 350–570 on it and were never located. Tracked from a hint they
/// are, on the step's end nearer the hint: walking on from the last located
/// tile, the pose changes once the step has played out.
#[test]
fn views_mid_step_are_located_on_the_step_end_nearer_the_hint() {
    let Some(world) = world() else {
        return;
    };
    let localizer = Localizer::new(&world);
    // The fixture, its map and the two tiles the view is between.
    for (name, map, a, b) in [
        // 13 px right of (4, 9).
        ("switch-route3-mid-step.png", "Route3", (4, 9), (5, 9)),
        // 5 px below (7, 7), walking to the door mat.
        (
            "switch-pokecenter-mid-step.png",
            "PewterCity_PokemonCenter_1F",
            (7, 7),
            (7, 8),
        ),
        // 13 px right of (23, 26).
        (
            "switch-pewter-mid-step.png",
            "PewterCity",
            (23, 26),
            (24, 26),
        ),
        // 12 px below (18, 34).
        (
            "emu-mtmoon-1f-mid-step.png",
            "MtMoon_1F",
            (18, 34),
            (18, 35),
        ),
    ] {
        let Some(frame) = fixture(name) else {
            continue;
        };
        let data = world.map(map).expect(map);
        // On the tile grid alone the view matches nowhere.
        assert_eq!(
            localizer.locate_in(&frame, data, None, 0, &[PLAYER_SPRITE]),
            None,
            "{name}: grid"
        );
        let beyond = |t: (i32, i32), o: (i32, i32)| (2 * t.0 - o.0, 2 * t.1 - o.1);
        for (hint, expected) in [(a, a), (beyond(a, b), a), (b, b), (beyond(b, a), b)] {
            let hint = pose(map, hint.0, hint.1);
            let found = localizer
                .locate_from(&frame, &hint, &[PLAYER_SPRITE])
                .unwrap_or_else(|| panic!("{name}: not located from {hint}"));
            assert_eq!(
                found.pose,
                pose(map, expected.0, expected.1),
                "{name} from {hint}"
            );
            assert!(found.score >= 960, "{name}: score {}", found.score);
        }
    }
}

/// Switch: stepping from Pewter City onto Route 3 (popup "ROUTE 3" down,
/// mid-step). Tracked from Pewter's east edge, the view past the edge is
/// Route 3's first column, drawn in Pewter's render padding.
#[test]
fn a_view_past_a_map_edge_is_on_the_connected_map() {
    let Some(world) = world() else {
        return;
    };
    let Some(frame) = fixture("switch-map-popup-route3.png") else {
        return;
    };
    let localizer = Localizer::new(&world);
    let exclude = [PLAYER_SPRITE, POPUP];
    // The view is between Route 3's (0, 9) and (1, 9).
    for (hint, expected) in [
        (pose("PewterCity", 47, 19), pose("Route3", 0, 9)),
        (pose("Route3", 1, 9), pose("Route3", 1, 9)),
    ] {
        let found = localizer
            .locate_from(&frame, &hint, &exclude)
            .unwrap_or_else(|| panic!("not located from {hint}"));
        assert_eq!(found.pose, expected, "from {hint}");
    }
    // The popup hides a tenth of the view: left in, it costs the score
    // what JPEG noise and NPC sprites may not leave to spare.
    let with = localizer.locate_from(&frame, &pose("Route3", 1, 9), &exclude);
    let without = localizer.locate_from(&frame, &pose("Route3", 1, 9), &[PLAYER_SPRITE]);
    assert!(with.expect("with").score >= 990);
    assert!(without.expect("without").score <= 940);
}

/// Emulator, Mt. Moon B2F: a view of a long straight corridor matches
/// every tile along it at 1000. Without a hint that is no position; from
/// a hint the nearest matching tile is kept (tracking continuity).
#[test]
fn a_repeating_view_is_not_located_without_a_hint() {
    let Some(world) = world() else {
        return;
    };
    let Some(frame) = fixture("emu-mtmoon-b2f-corridor.png") else {
        return;
    };
    let localizer = Localizer::new(&world);
    let b2f = world.map("MtMoon_B2F").expect("MtMoon_B2F");
    assert_eq!(
        localizer.locate_in(&frame, b2f, None, 0, &[PLAYER_SPRITE]),
        None
    );
    assert_eq!(localizer.locate_anywhere(&frame, &[PLAYER_SPRITE]), None);
    let found = localizer
        .locate_from(&frame, &pose("MtMoon_B2F", 26, 38), &[PLAYER_SPRITE])
        .expect("tracked");
    assert_eq!(found.pose, pose("MtMoon_B2F", 26, 38));
}

/// The same corridor tracked from a hint seven tiles along it, past
/// where it stops repeating: the window's best (899, three tiles off) sits
/// on its edge, so the window moves on until the corridor's 1000s, and
/// keeps the one nearest the hint.
#[test]
fn the_search_window_follows_a_better_score_past_its_edge() {
    let Some(world) = world() else {
        return;
    };
    let Some(frame) = fixture("emu-mtmoon-b2f-corridor.png") else {
        return;
    };
    let localizer = Localizer::new(&world);
    let map = world.map("MtMoon_B2F").expect("MtMoon_B2F");
    let found = localizer
        .locate_in(&frame, map, Some((34, 38)), 3, &[PLAYER_SPRITE])
        .expect("located");
    // 6 px right of (27, 38): reported on the end nearer the hint.
    assert_eq!((found.pose.x, found.pose.y, found.score), (28, 38, 1000));
}

/// Tracking searches many more views than the grid: the dark-frame
/// guarantees must hold for it too (black, dark greys, battle wipes and the
/// intro fade tracked from the Mt. Moon poses they were recorded at).
#[test]
fn dark_frames_are_not_tracked_anywhere() {
    let Some(world) = world() else {
        return;
    };
    let localizer = Localizer::new(&world);
    let hints = [
        pose("MtMoon_B1F", 20, 10),
        pose("MtMoon_1F", 19, 27),
        pose("MtMoon_B2F", 26, 38),
    ];
    for grey in [0, 8, 16, 24] {
        let frame = RgbImage::filled(240, 160, [grey; 3]);
        for hint in &hints {
            let found = localizer.locate_from(&frame, hint, &[PLAYER_SPRITE]);
            assert!(found.is_none(), "grey {grey} from {hint}: {found:?}");
        }
    }
    for name in [
        "mtmoon-battle-wipe.png",
        "mtmoon-battle-wipe-b.png",
        "mtmoon-intro-fade.png",
    ] {
        let Some(frame) = fixture(name) else {
            continue;
        };
        // Recorded on MtMoon_1F (19, 27): nowhere else, from either floor.
        for hint in &hints[..2] {
            let found = localizer.locate_from(&frame, hint, &[PLAYER_SPRITE]);
            assert!(
                found
                    .as_ref()
                    .is_none_or(|f| f.pose == pose("MtMoon_1F", 19, 27)),
                "{name} from {hint}: {found:?}"
            );
        }
    }
}

/// New game (emulator via capture card): walking into Oak's lab, the first
/// frames after the fade are already mid-step (5 px below (6, 11)). The
/// hint is still Pallet Town's door; the lab is searched from where that
/// door's warp arrives.
#[test]
fn a_walk_through_a_door_is_tracked_from_the_warp_arrival() {
    let Some(world) = world() else {
        return;
    };
    let Some(frame) = fixture("emu-oaks-lab-mid-step.png") else {
        return;
    };
    let localizer = Localizer::new(&world);
    let lab = world
        .map("PalletTown_ProfessorOaksLab")
        .expect("PalletTown_ProfessorOaksLab");
    assert_eq!(
        localizer.locate_in(&frame, lab, None, 0, &[PLAYER_SPRITE]),
        None
    );
    let found = localizer
        .locate_from(&frame, &pose("PalletTown", 16, 13), &[PLAYER_SPRITE])
        .expect("located");
    // Leaving the door mat (6, 12) the warp put him on.
    assert_eq!(found.pose, pose("PalletTown_ProfessorOaksLab", 6, 12));
}

/// Emulator audit (every 40th frame, running along Mt. Moon B2F's
/// corridor): the frame matched 850 at (24..26, 37) +11 px alike, the
/// true pose some ten tiles on (the next frame: (15, 37) at 1000). The
/// tracker kept the tile nearest its hint, (27, 37): a weak match that
/// repeats is no answer.
#[test]
fn a_weak_repeating_match_is_not_tracked() {
    let Some(world) = world() else {
        return;
    };
    let Some(frame) = fixture("emu-mtmoon-b2f-corridor-weak.png") else {
        return;
    };
    let localizer = Localizer::new(&world);
    let map = world.map("MtMoon_B2F").expect("MtMoon_B2F");
    assert_eq!(
        localizer.locate_in(&frame, map, Some((27, 37)), 3, &[PLAYER_SPRITE]),
        None
    );
}
