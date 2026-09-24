//! Cross-map routing on the real world model (skipped without data/world).

use std::path::Path;

use pokebot_agent::nav::{
    goal_tiles, object_obstacles, plan_hop, route_from, Destination, Gone, Hop,
};
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

#[test]
fn lab_exit_uses_the_mat_not_the_plain_warp_beside_it() {
    let Some(world) = world() else { return };
    // After the rival battle: (7, 12) is a warp event on plain floor that
    // never fires; the arrow mat is (6, 12).
    let pose = PlayerPose {
        map: "PalletTown_ProfessorOaksLab".into(),
        x: 7,
        y: 8,
    };
    let Some(Hop::Warp(i)) = route_from(&world, &pose, "PalletTown") else {
        panic!("expected a warp");
    };
    let warp = &world.map("PalletTown_ProfessorOaksLab").unwrap().warps[i];
    assert_eq!((warp.x, warp.y), (6, 12));
}

/// Follows the navigator's plan hop by hop: walk (A*, as the navigator does)
/// to the hop's warp tile or map edge, take it, and plan again on the map it
/// lands on, until a goal tile of `dest` is walkable. Returns the maps
/// visited with their entry tiles, or where it got stuck.
fn follow(
    world: &World,
    mut pose: PlayerPose,
    dest: &Destination,
    gone: &Gone,
) -> Result<Vec<String>, String> {
    use pokebot_world::path::{find_path, path_to};
    let goals = goal_tiles(world, dest);
    let mut visited = vec![format!("{} ({}, {})", pose.map, pose.x, pose.y)];
    for _ in 0..20 {
        let map = world.map(&pose.map).ok_or("unknown map")?;
        let obstacles = object_obstacles(map, gone);
        let Some(hop) = plan_hop(world, &pose, dest, gone) else {
            if pose.map != dest.map() {
                return Err(format!("no route from {visited:?}"));
            }
            let path = find_path(
                map,
                (pose.x, pose.y),
                &obstacles,
                |p| goals.contains(&p),
                |_| 0,
            );
            return path
                .map(|_| visited.clone())
                .ok_or_else(|| format!("no path to {goals:?} on {}: {visited:?}", pose.map));
        };
        let (dest_map, x, y) = match hop {
            Hop::Warp(i) => {
                let w = &map.warps[i];
                path_to(map, (pose.x, pose.y), (w.x, w.y), &obstacles).ok_or_else(|| {
                    format!(
                        "no path to warp ({}, {}) on {}: {visited:?}",
                        w.x, w.y, pose.map
                    )
                })?;
                let (m, x, y) = world.warp_destination(w).ok_or("bad warp")?;
                (m.name.clone(), x, y)
            }
            Hop::Edge(dir) => {
                let (_, b) = world
                    .crossings(map, dir)
                    .into_iter()
                    .find(|(a, _)| path_to(map, (pose.x, pose.y), *a, &obstacles).is_some())
                    .ok_or_else(|| format!("no reachable {dir:?} edge: {visited:?}"))?;
                let conn = map
                    .connections
                    .iter()
                    .find(|c| c.direction() == Some(dir))
                    .unwrap();
                (world.name_of(&conn.map).unwrap().to_owned(), b.0, b.1)
            }
        };
        pose = PlayerPose {
            map: dest_map,
            x,
            y,
        };
        visited.push(format!("{} ({x}, {y})", pose.map));
    }
    Err(format!("too many hops: {visited:?}"))
}

#[test]
fn mt_moon_entrance_reaches_miguels_trigger_on_b2f() {
    let Some(world) = world() else { return };
    // Just inside, above the entrance from Route 4. B2F's first ladder from
    // the entrance side lands in a part without (14, 11).
    let pose = PlayerPose {
        map: "MtMoon_1F".into(),
        x: 18,
        y: 36,
    };
    let dest = Destination::Tile {
        map: "MtMoon_B2F".into(),
        x: 14,
        y: 11,
    };
    let visited = follow(&world, pose, &dest, &Gone::new()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        visited.last().map(|v| v.starts_with("MtMoon_B2F")),
        Some(true),
        "{visited:?}"
    );
}

#[test]
fn mt_moon_fossils_leave_through_the_b1f_part_with_the_exit() {
    let Some(world) = world() else { return };
    // Below the Helix Fossil at (14, 7), after taking it: the fossils stood
    // in the gap north toward the ladder to B1F's exit part.
    let pose = PlayerPose {
        map: "MtMoon_B2F".into(),
        x: 14,
        y: 8,
    };
    let dest = Destination::Warp {
        map: "MtMoon_B1F".into(),
        warp: 7,
    };
    let gone = Gone::from([("MtMoon_B2F".to_owned(), 2)]);
    let visited = follow(&world, pose, &dest, &gone).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        visited,
        ["MtMoon_B2F (14, 8)", "MtMoon_B1F (39, 4)"],
        "B2F's ladder at (5, 10) leads to the part of B1F with the exit"
    );
    // Warp 7 leads out to Route 4.
    let exit = &world.map("MtMoon_B1F").unwrap().warps[7];
    assert_eq!(world.name_of(&exit.dest_map), Some("Route4"));
    // With the fossil still there, no ladder leads to the exit's part: the
    // plan never picks a ladder into another part of B1F.
    let still = follow(
        &world,
        PlayerPose {
            map: "MtMoon_B2F".into(),
            x: 14,
            y: 8,
        },
        &dest,
        &Gone::new(),
    );
    assert!(still.is_err(), "{still:?}");
}
