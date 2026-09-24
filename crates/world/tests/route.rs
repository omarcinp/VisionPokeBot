//! Route planner over the real world data (skipped without `data/world`).

use std::path::Path;

use pokebot_state::{Direction, PlayerPose};
use pokebot_world::predicate::{MapBelief, Predicate};
use pokebot_world::route::{
    fly_requirement, route, EdgeKind, Place, PlaceGraph, RouteParams, RouteResult, UnknownPolicy,
    MOVE_CUT_NAME, MOVE_FLY, MOVE_SURF,
};
use pokebot_world::World;

fn world() -> Option<World> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    World::load(root.join("data/world")).ok()
}

fn pose(map: &str, x: i32, y: i32) -> PlayerPose {
    PlayerPose {
        map: map.to_string(),
        x,
        y,
    }
}

fn print(name: &str, r: &RouteResult) {
    println!("== {name}: {:.1}s, assumes {:?}", r.cost_s, r.assumes);
    for leg in &r.legs {
        println!("   {leg}");
    }
    for (req, cost) in &r.blocked {
        let req: Vec<String> = req.iter().map(|p| p.to_string()).collect();
        println!("   blocked: {:.1}s needs {}", cost, req.join(" & "));
    }
}

fn penalty(_: &Predicate) -> f64 {
    30.0
}

/// Fly and Surf usable; nothing known about visited towns.
fn traveller() -> MapBelief {
    MapBelief::default()
        .party_move(MOVE_FLY, true)
        .party_move(MOVE_SURF, true)
        .badge(3, true)
        .badge(5, true)
}

#[test]
fn pewter_center_to_route2_south_is_walk_warp_connection() {
    let Some(world) = world() else { return };
    let graph = PlaceGraph::build(&world, RouteParams::default());
    // Inside the Center, one tile above the middle door.
    let pc = world
        .places()
        .unwrap()
        .heal_spot("HEAL_LOCATION_PEWTER_CITY")
        .unwrap()
        .respawn_map
        .clone();
    assert_eq!(pc, "PewterCity_PokemonCenter_1F");
    let door = &world.map(&pc).unwrap().warps[1];
    let start = pose(&pc, door.x, door.y - 1);
    assert_eq!(
        world
            .map(&pc)
            .unwrap()
            .tile(start.x, start.y)
            .unwrap()
            .collision,
        0
    );
    let route2 = world.map("Route2").unwrap();
    let ((sx, sy), _) = world.crossings(route2, Direction::Down)[0];
    let to = Place::tile("Route2", sx, sy);
    let belief = MapBelief::default(); // nothing known: no Cut, no Fly
    let r = route(
        &world,
        &graph,
        &belief,
        &start,
        &to,
        UnknownPolicy::Pessimistic,
    );
    print("Pewter PC -> Route 2 south", &r);
    assert!(r.found());
    assert!(!r.legs.is_empty());
    assert_eq!(r.legs[0].from.map, pc);
    let end = &r.legs.last().unwrap().to;
    assert_eq!((end.map.as_str(), end.x, end.y), ("Route2", to.x, to.y));
    for leg in &r.legs {
        assert!(
            matches!(
                leg.kind,
                EdgeKind::Walk { surf: false, .. } | EdgeKind::Warp | EdgeKind::Connection
            ),
            "{leg}"
        );
        assert!(leg.requires.is_empty(), "{leg}");
    }
    let maps: Vec<&str> = r.legs.iter().map(|l| l.to.map.as_str()).collect();
    assert!(maps.contains(&"PewterCity"), "{maps:?}");
    assert!(maps.contains(&"Route2"), "{maps:?}");
    assert!(r.assumes.is_empty());
    let total: f64 = r.legs.iter().map(|l| l.cost_s).sum();
    assert!((total - r.cost_s).abs() < 1e-9);
    // Same input, same output.
    let again = route(
        &world,
        &graph,
        &belief,
        &start,
        &to,
        UnknownPolicy::Pessimistic,
    );
    assert_eq!(format!("{:?}", again.legs), format!("{:?}", r.legs));
}

#[test]
fn cinnabar_to_pallet_flies_when_pallet_is_visited() {
    let Some(world) = world() else { return };
    let graph = PlaceGraph::build(&world, RouteParams::default());
    let belief = traveller().visited("PalletTown", true);
    let r = route(
        &world,
        &graph,
        &belief,
        &pose("CinnabarIsland", 14, 12),
        &Place::tile("PalletTown", 6, 8),
        UnknownPolicy::Pessimistic,
    );
    print("Cinnabar -> Pallet, visited", &r);
    assert!(r.found());
    assert_eq!(r.legs[0].kind, EdgeKind::Fly);
    assert_eq!(r.legs[0].requires, fly_requirement("PalletTown"));
    assert_eq!(r.legs.len(), 1, "the fly spot is the target");
    assert!((r.cost_s - 12.0).abs() < 1e-9);
    assert!(r.assumes.is_empty());
    assert!(r.blocked.is_empty(), "{:?}", r.blocked);
}

#[test]
fn cinnabar_to_pallet_with_unknown_visit_assumes_or_surfs() {
    let Some(world) = world() else { return };
    let graph = PlaceGraph::build(&world, RouteParams::default());
    let belief = traveller();
    let from = pose("CinnabarIsland", 14, 12);
    let to = Place::tile("PalletTown", 6, 8);
    let visited = Predicate::Visited {
        map: "PalletTown".into(),
    };

    let opt = route(
        &world,
        &graph,
        &belief,
        &from,
        &to,
        UnknownPolicy::Optimistic {
            penalty_of: penalty,
        },
    );
    print("Cinnabar -> Pallet, visited unknown, optimistic", &opt);
    assert_eq!(opt.legs[0].kind, EdgeKind::Fly);
    assert_eq!(opt.assumes, vec![visited.clone()]);
    assert!((opt.cost_s - 42.0).abs() < 1e-9, "12 s + 30 s penalty");

    let pes = route(
        &world,
        &graph,
        &belief,
        &from,
        &to,
        UnknownPolicy::Pessimistic,
    );
    print("Cinnabar -> Pallet, visited unknown, pessimistic", &pes);
    assert!(pes.found());
    assert!(pes.legs.iter().all(|l| l.kind != EdgeKind::Fly));
    assert!(
        pes.legs
            .iter()
            .any(|l| matches!(l.kind, EdgeKind::Walk { surf: true, .. })),
        "Surfs Route 21"
    );
    let maps: Vec<&str> = pes.legs.iter().map(|l| l.to.map.as_str()).collect();
    assert!(
        maps.contains(&"Route21_South") && maps.contains(&"Route21_North"),
        "{maps:?}"
    );
    // One Go per map: walks through intermediate places are merged.
    for w in pes.legs.windows(2) {
        assert!(
            !(matches!(w[0].kind, EdgeKind::Walk { .. })
                && matches!(w[1].kind, EdgeKind::Walk { .. })
                && w[0].to.map == w[1].from.map),
            "{} then {}",
            w[0],
            w[1]
        );
    }
    assert!(pes.assumes.is_empty());
    // Cheapest blocked alternative first: Fly straight to Pallet.
    assert_eq!(pes.blocked[0].0, vec![visited]);
    assert!((pes.blocked[0].1 - 12.0).abs() < 1e-9);
    assert!(pes.blocked.len() <= 8);
    for w in pes.blocked.windows(2) {
        assert!(w[0].1 <= w[1].1);
    }
    assert!(pes.cost_s > 12.0);
}

#[test]
fn cut_tree_needs_badge_and_move() {
    let Some(world) = world() else { return };
    let graph = PlaceGraph::build(&world, RouteParams::default());
    // Route 2's tree at (16, 62): the corridor along y = 62 is open on both
    // sides of it.
    let from = pose("Route2", 15, 62);
    let to = Place::tile("Route2", 17, 62);
    let need = vec![
        Predicate::PartyHasMove {
            mv: MOVE_CUT_NAME.into(),
        },
        Predicate::Badge { n: 2 },
    ];

    let without = route(
        &world,
        &graph,
        &MapBelief::default()
            .badge(2, false)
            .party_move(MOVE_CUT_NAME, false),
        &from,
        &to,
        UnknownPolicy::Pessimistic,
    );
    print("Route 2 across the tree, no Cut", &without);
    assert!(without
        .legs
        .iter()
        .all(|l| !matches!(l.kind, EdgeKind::Gate { .. })));
    let mut sorted = need.clone();
    sorted.sort();
    assert!(
        without.blocked.iter().any(|(req, _)| *req == sorted),
        "{:?}",
        without.blocked
    );
    let gate_cost = without
        .blocked
        .iter()
        .find(|(req, _)| *req == sorted)
        .unwrap()
        .1;
    assert!(gate_cost < without.cost_s);

    let with = route(
        &world,
        &graph,
        &MapBelief::default()
            .badge(2, true)
            .party_move(MOVE_CUT_NAME, true),
        &from,
        &to,
        UnknownPolicy::Pessimistic,
    );
    print("Route 2 across the tree, with Cut", &with);
    assert!(with.found());
    assert!(
        with.legs
            .iter()
            .any(|l| matches!(l.kind, EdgeKind::Gate { .. })),
        "{:?}",
        with.legs
    );
    assert!((with.cost_s - gate_cost).abs() < 1e-9);
    assert!(with.blocked.is_empty());
}
