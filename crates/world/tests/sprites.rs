//! Sprites found on live frames (skipped without data/world or the
//! fixtures). Each frame is located on its known map first, as the
//! perception's pose hint would.

use std::path::Path;

use pokebot_state::Direction;
use pokebot_world::localize::PLAYER_SPRITE;
use pokebot_world::sprites::{FieldScan, SpriteDetector};
use pokebot_world::{Localizer, World};

/// `(x, y, local id)` of the sprites `fixture` shows with the player on
/// `map`, and the objects seen absent.
type Seen = (Vec<(i32, i32, Option<u32>)>, Vec<u32>);

fn scan(fixture: &str, map: &str, at: (i32, i32)) -> Option<Seen> {
    let FieldScan { sprites, absent } = field(fixture, map, at)?;
    Some((
        sprites.iter().map(|s| (s.x, s.y, s.local_id)).collect(),
        absent,
    ))
}

fn field(fixture: &str, map: &str, at: (i32, i32)) -> Option<FieldScan> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let world = World::load(root.join("data/world")).ok()?;
    let frame =
        pokebot_video::png::load(root.join(format!("captures/fixtures/{fixture}.png"))).ok()?;
    let map = world.map(map).expect("map");
    let found = Localizer::new(&world)
        .locate_in(&frame, map, None, 0, &[PLAYER_SPRITE])
        .expect("located");
    assert_eq!((found.pose.x, found.pose.y), at, "{fixture}");
    Some(SpriteDetector::default().scan(0, &frame, &world, map, &found.pose, &[]))
}

/// Emulator, Oak's lab after the rival arrives: Oak, the rival, the three
/// starter balls on the table and the two Pokédexes on the desk, all named.
#[test]
fn every_object_in_oaks_lab_is_found_and_named() {
    let Some((sprites, absent)) = scan(
        "emu-sprites-oaks-lab",
        "PalletTown_ProfessorOaksLab",
        (6, 4),
    ) else {
        return;
    };
    assert_eq!(
        sprites,
        [
            (4, 1, Some(9)),
            (5, 1, Some(10)),
            (6, 3, Some(4)),
            (5, 4, Some(8)),
            (8, 4, Some(5)),
            (9, 4, Some(6)),
            (10, 4, Some(7)),
        ]
    );
    assert!(absent.is_empty());
}

/// Emulator: once the player has taken the ball at (8, 4) its tile shows
/// the table, in full view with everyone else named: the object is absent.
/// The aide wandering at (2, 9) is named from her area.
#[test]
fn a_taken_ball_is_absent() {
    let Some((sprites, absent)) = scan(
        "emu-sprites-lab-ball-taken",
        "PalletTown_ProfessorOaksLab",
        (8, 5),
    ) else {
        return;
    };
    assert!(!sprites.iter().any(|&(x, y, _)| (x, y) == (8, 4)));
    assert!(sprites.contains(&(2, 9, Some(2))));
    assert_eq!(absent, [5]);
}

/// Emulator, leaving the player's house: the map-name popup covers the top
/// row and the pond animates at the bottom; neither is a sprite. Oak's
/// spawn tile (hidden by a flag until his script) is in view and empty.
#[test]
fn the_name_popup_and_water_are_not_sprites() {
    let Some((sprites, absent)) = scan("emu-sprites-pallet-popup", "PalletTown", (6, 8)) else {
        return;
    };
    assert!(sprites.is_empty(), "{sprites:?}");
    assert_eq!(absent, [3]);
}

/// Emulator: Pallet Town's entry script puts the sign lady at (5, 15), away
/// from her spawn area; Oak, walking the player to his lab by script, is
/// seen but can't be named from the map data.
#[test]
fn a_script_placed_object_is_named_and_a_script_walked_one_is_not() {
    let Some((sprites, _)) = scan("emu-sprites-pallet-sign-lady", "PalletTown", (11, 12)) else {
        return;
    };
    assert_eq!(sprites, [(11, 13, None), (5, 15, Some(1))]);
}

/// Switch (JPEG): Brock stands right above the player (only his face and
/// body clear the player's head), and the camper who spotted the player
/// walked two tiles along his line of sight from (3, 8).
#[test]
fn switch_gym_leader_above_the_player_and_a_trainer_who_walked() {
    let Some((sprites, _)) = scan("switch-sprites-pewter-gym", "PewterCity_Gym", (6, 6)) else {
        return;
    };
    assert_eq!(sprites, [(6, 5, Some(1)), (5, 8, Some(2))]);
}

/// Switch: Route 4's Pokémon Center with six people and a clipboard; the
/// gentleman at (12, 5) stands right above the clipboard.
#[test]
fn switch_pokemon_center_visitors_are_all_named() {
    let Some((sprites, absent)) = scan(
        "switch-sprites-route4-center",
        "Route4_PokemonCenter_1F",
        (7, 4),
    ) else {
        return;
    };
    assert_eq!(
        sprites,
        [
            (7, 2, Some(1)),
            (1, 3, Some(2)),
            (14, 4, Some(5)),
            (4, 5, Some(4)),
            (12, 5, Some(3)),
            (12, 6, Some(6)),
        ]
    );
    assert!(absent.is_empty());
}

/// Switch, the same Pokémon Center: the nurse, the balding man and the
/// gentleman face down, the boy looks left, the youngster shows the back
/// of his cap; the clipboard faces nowhere.
#[test]
fn switch_facings_from_the_faces() {
    let Some(scan) = field(
        "switch-sprites-route4-center",
        "Route4_PokemonCenter_1F",
        (7, 4),
    ) else {
        return;
    };
    let facings: Vec<(u32, Option<Direction>)> = scan
        .sprites
        .iter()
        .map(|s| (s.local_id.unwrap(), s.facing))
        .collect();
    assert_eq!(
        facings,
        [
            (1, Some(Direction::Down)),
            (2, Some(Direction::Down)),
            (5, Some(Direction::Up)),
            (4, Some(Direction::Left)),
            (3, Some(Direction::Down)),
            (6, None),
        ]
    );
    // The camper who walked up to the player faces the way he walked.
    let Some(gym) = field("switch-sprites-pewter-gym", "PewterCity_Gym", (6, 6)) else {
        return;
    };
    let facings: Vec<Option<Direction>> = gym.sprites.iter().map(|s| s.facing).collect();
    assert_eq!(facings, [Some(Direction::Down), Some(Direction::Right)]);
}
