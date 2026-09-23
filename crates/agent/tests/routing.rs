//! Cross-map routing on the real world model (skipped without data/world).

use std::path::Path;

use pokebot_agent::nav::{route_from, Hop};
use pokebot_state::PlayerPose;
use pokebot_world::World;

fn world() -> Option<World> {
    World::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world")).ok()
}

#[test]
fn split_route_2_goes_through_viridian_forest() {
    let Some(world) = world() else { return };
    let pose = PlayerPose {
        map: "Route2".into(),
        x: 9,
        y: 79,
    };
    let hop = route_from(&world, &pose, "PewterCity").expect("route");
    // Not the (unreachable from here) north edge of Route 2: a building warp.
    let Hop::Warp(i) = hop else {
        panic!("expected a warp, got {hop:?}")
    };
    let dest = &world.map("Route2").unwrap().warps[i].dest_map;
    assert!(dest.contains("VIRIDIAN_FOREST"), "{dest}");
}

#[test]
fn pallet_to_viridian_uses_route_1() {
    let Some(world) = world() else { return };
    let pose = PlayerPose {
        map: "PalletTown".into(),
        x: 6,
        y: 8,
    };
    assert!(matches!(
        route_from(&world, &pose, "ViridianCity"),
        Some(Hop::Edge(pokebot_state::Direction::Up))
    ));
}
