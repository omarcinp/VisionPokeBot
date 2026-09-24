//! Route planner over the real world data (skipped without `data/world`).

use std::path::Path;

use pokebot_state::{Direction, PlayerPose};
use pokebot_world::obstacles::{blockers, Passage};
use pokebot_world::predicate::{MapBelief, Predicate};
use pokebot_world::route::{
    fly_requirement, route, route_to_map, EdgeKind, Place, PlaceGraph, RouteParams, RouteResult,
    UnknownPolicy, MOVE_CUT_NAME, MOVE_FLY, MOVE_SURF,
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

fn small_penalty(_: &Predicate) -> f64 {
    1.0
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

fn flag(name: &str) -> Predicate {
    Predicate::Flag {
        name: name.into(),
        is: true,
    }
}

/// The trainers standing on `map` (ids their scripts battle).
fn trainers_on(world: &World, map: &str) -> Vec<String> {
    blockers(world.map(map).unwrap(), world.events())
        .iter()
        .flat_map(|b| b.passages.iter())
        .filter_map(|p| match p {
            Passage::Trainer { trainer } => Some(trainer.clone()),
            Passage::Hidden { .. } => None,
        })
        .collect()
}

#[test]
fn pewter_center_to_mart_enters_through_the_door() {
    let Some(world) = world() else { return };
    let graph = PlaceGraph::build(&world, RouteParams::default());
    let pc = "PewterCity_PokemonCenter_1F";
    let door = &world.map(pc).unwrap().warps[1];
    let start = pose(pc, door.x, door.y - 1);
    // The Mart's landing: its exit mat.
    let mart = world.map("PewterCity_Mart").unwrap();
    let to = Place::tile("PewterCity_Mart", mart.warps[1].x, mart.warps[1].y);
    let r = route(
        &world,
        &graph,
        &MapBelief::default(),
        &start,
        &to,
        UnknownPolicy::Pessimistic,
    );
    print("Pewter PC -> Pewter Mart", &r);
    assert!(r.found());
    let last = r.legs.last().unwrap();
    assert_eq!(last.kind, EdgeKind::Warp, "{last}");
    assert_eq!(last.to.map, "PewterCity_Mart");
    // Taken from the tile below the Mart's door (28, 18), as the navigator does.
    let mart_door = &world.map("PewterCity").unwrap().warps[3];
    assert_eq!(
        (last.from.map.as_str(), last.from.x, last.from.y),
        ("PewterCity", mart_door.x, mart_door.y + 1)
    );
    let walk = &r.legs[r.legs.len() - 2];
    assert!(matches!(walk.kind, EdgeKind::Walk { .. }), "{walk}");
    assert_eq!(walk.to, last.from);
    assert!(r.legs.iter().all(|l| l.requires.is_empty()));
    assert!(r.blocked.is_empty());

    // Leaving the Mart lands on the door tile; the walk starts from there.
    let back = route(
        &world,
        &graph,
        &MapBelief::default(),
        &pose("PewterCity", mart_door.x, mart_door.y),
        &Place::tile(pc, door.x, door.y),
        UnknownPolicy::Pessimistic,
    );
    print("Pewter Mart door -> Pewter PC", &back);
    assert!(back.found());
    assert!(matches!(back.legs[0].kind, EdgeKind::Walk { .. }));
    assert_eq!(back.legs.last().unwrap().kind, EdgeKind::Warp);
    assert_eq!(back.legs.last().unwrap().to.map, pc);
}

#[test]
fn route4_center_to_cerulean_crosses_mt_moon_past_the_fossils() {
    let Some(world) = world() else { return };
    let graph = PlaceGraph::build(&world, RouteParams::default());
    let from = pose("Route4_PokemonCenter_1F", 7, 7);
    let grunts = trainers_on(&world, "MtMoon_B2F");
    assert!(!grunts.is_empty());
    let fossils = ["FLAG_HIDE_DOME_FOSSIL", "FLAG_HIDE_HELIX_FOSSIL"];
    // On foot: no Fly, no Surf.
    let on_foot = || {
        MapBelief::default()
            .party_move(MOVE_FLY, false)
            .party_move(MOVE_SURF, false)
    };
    let mut nothing_done = on_foot();
    for g in &grunts {
        nothing_done = nothing_done.flag(g, false);
    }
    for f in fossils {
        nothing_done = nothing_done.flag(f, false);
    }

    // B2F's far side is only reached over a fossil's tile: no route until
    // one is picked up, and that is what the blocked alternatives say.
    let pes = route_to_map(
        &world,
        &graph,
        &nothing_done,
        &from,
        "CeruleanCity",
        UnknownPolicy::Pessimistic,
    );
    print(
        "Route 4 PC -> Cerulean, fossils in place, pessimistic",
        &pes,
    );
    assert!(!pes.found());
    // Each fossil is a way (Fly, which the party lacks, is listed too).
    let by_fossil: Vec<&(Vec<Predicate>, f64)> = fossils
        .iter()
        .filter_map(|f| pes.blocked.iter().find(|(req, _)| *req == vec![flag(f)]))
        .collect();
    assert_eq!(by_fossil.len(), 2, "{:?}", pes.blocked);
    let cheapest = by_fossil[0].1;
    assert!(cheapest.is_finite());
    assert!((by_fossil[1].1 - cheapest).abs() < 1e-9);

    // Optimistic about the fossils: the route assumes one, and beats no grunt.
    let mut unknown_fossils = on_foot();
    for g in &grunts {
        unknown_fossils = unknown_fossils.flag(g, false);
    }
    let opt = route_to_map(
        &world,
        &graph,
        &unknown_fossils,
        &from,
        "CeruleanCity",
        UnknownPolicy::Optimistic {
            penalty_of: penalty,
        },
    );
    print("Route 4 PC -> Cerulean, fossils unknown, optimistic", &opt);
    assert!(opt.found());
    assert_eq!(opt.assumes.len(), 1, "{:?}", opt.assumes);
    assert!(fossils.iter().any(|f| opt.assumes[0] == flag(f)));
    assert!((opt.cost_s - (cheapest + 30.0)).abs() < 1e-9);
    for leg in &opt.legs {
        assert!(
            !leg.requires
                .iter()
                .any(|p| grunts.iter().any(|g| *p == flag(g))),
            "{leg}"
        );
    }
    let maps: Vec<&str> = opt.legs.iter().map(|l| l.to.map.as_str()).collect();
    for m in [
        "Route4",
        "MtMoon_1F",
        "MtMoon_B1F",
        "MtMoon_B2F",
        "CeruleanCity",
    ] {
        assert!(maps.contains(&m), "{maps:?}");
    }
    assert_eq!(
        opt.legs[1].kind,
        EdgeKind::Warp,
        "out of the Center: {}",
        opt.legs[1]
    );
    assert_eq!(opt.legs[1].to.map, "Route4");

    // A fossil taken: the same route, open.
    let taken = nothing_done.clone().flag("FLAG_HIDE_DOME_FOSSIL", true);
    let open = route_to_map(
        &world,
        &graph,
        &taken,
        &from,
        "CeruleanCity",
        UnknownPolicy::Pessimistic,
    );
    print("Route 4 PC -> Cerulean, Dome Fossil taken", &open);
    assert!(open.found());
    assert!((open.cost_s - cheapest).abs() < 1e-9);
    assert!(open.assumes.is_empty());
}

#[test]
fn trainer_in_a_corridor_is_passed_by_beating_him() {
    let Some(world) = world() else { return };
    // Route 13: Bird Keeper Perry at (16, 5) stands in the corridor to
    // (10, 5); the way round is through Route 14 (about 18 s). A short
    // battle makes beating him the better way.
    let params = RouteParams {
        battle_s: 5.0,
        ..RouteParams::default()
    };
    let graph = PlaceGraph::build(&world, params);
    let perry = flag("TRAINER_BIRD_KEEPER_PERRY");
    assert!(trainers_on(&world, "Route13").contains(&"TRAINER_BIRD_KEEPER_PERRY".to_string()));
    let from = pose("Route13", 20, 5);
    let to = Place::tile("Route13", 10, 5);
    let past_him = 10.0 * params.tile_s + params.battle_s;

    let undefeated = route(
        &world,
        &graph,
        &MapBelief::default().flag("TRAINER_BIRD_KEEPER_PERRY", false),
        &from,
        &to,
        UnknownPolicy::Pessimistic,
    );
    print("Route 13 past Perry, undefeated", &undefeated);
    assert!(undefeated.found());
    assert!(undefeated.cost_s > past_him, "goes round");
    assert!(undefeated.legs.iter().all(|l| l.requires.is_empty()));
    assert_eq!(undefeated.blocked[0].0, vec![perry.clone()]);
    assert!((undefeated.blocked[0].1 - past_him).abs() < 1e-9);

    let unknown = route(
        &world,
        &graph,
        &MapBelief::default(),
        &from,
        &to,
        UnknownPolicy::Optimistic {
            penalty_of: small_penalty,
        },
    );
    print("Route 13 past Perry, unknown, optimistic", &unknown);
    assert!(unknown.found());
    assert_eq!(unknown.assumes, vec![perry.clone()]);
    assert_eq!(unknown.legs.len(), 1);
    assert_eq!(unknown.legs[0].requires, vec![perry.clone()]);
    assert!((unknown.legs[0].cost_s - past_him).abs() < 1e-9);
    assert!((unknown.cost_s - (past_him + 1.0)).abs() < 1e-9);

    let defeated = route(
        &world,
        &graph,
        &MapBelief::default().flag("TRAINER_BIRD_KEEPER_PERRY", true),
        &from,
        &to,
        UnknownPolicy::Pessimistic,
    );
    print("Route 13 past Perry, defeated", &defeated);
    assert!(defeated.found());
    assert_eq!(defeated.legs.len(), 1);
    assert!(defeated.legs[0].requires.is_empty());
    assert!(
        (defeated.cost_s - 10.0 * params.tile_s).abs() < 1e-9,
        "no battle to pay"
    );
    assert!(defeated.blocked.is_empty());
}

#[test]
fn hidden_blocker_is_passed_once_its_flag_is_set() {
    let Some(world) = world() else { return };
    let params = RouteParams::default();
    let graph = PlaceGraph::build(&world, params);
    // Route 12: Snorlax at (14, 70) seals the way to Route 11.
    let snorlax = flag("FLAG_HIDE_ROUTE_12_SNORLAX");
    let from = pose("Route12", 15, 70);
    let to = Place::tile("Route12", 12, 70);
    let walk_s = 3.0 * params.tile_s;

    let asleep = route(
        &world,
        &graph,
        &MapBelief::default().flag("FLAG_HIDE_ROUTE_12_SNORLAX", false),
        &from,
        &to,
        UnknownPolicy::Pessimistic,
    );
    print("Route 12 past Snorlax, asleep", &asleep);
    assert!(!asleep.found());
    assert_eq!(asleep.blocked[0].0, vec![snorlax.clone()]);
    assert!((asleep.blocked[0].1 - walk_s).abs() < 1e-9);

    let gone = route(
        &world,
        &graph,
        &MapBelief::default().flag("FLAG_HIDE_ROUTE_12_SNORLAX", true),
        &from,
        &to,
        UnknownPolicy::Pessimistic,
    );
    print("Route 12 past Snorlax, gone", &gone);
    assert!(gone.found());
    assert!((gone.cost_s - walk_s).abs() < 1e-9);
    assert!(gone.legs[0].requires.is_empty());
}

#[test]
fn route_to_map_reaches_mt_moon_b2f_from_route3() {
    let Some(world) = world() else { return };
    let graph = PlaceGraph::build(&world, RouteParams::default());
    let route3 = world.map("Route3").unwrap();
    let ((sx, sy), _) = world.crossings(route3, Direction::Up)[0];
    let from = pose("Route3", sx, sy);
    let belief = MapBelief::default()
        .flag("FLAG_HIDE_DOME_FOSSIL", false)
        .flag("FLAG_HIDE_HELIX_FOSSIL", false);

    // B2F's landing at (5, 10) sits in a part of the floor cut off by the
    // fossils: unreachable on its own...
    let cut_off = route(
        &world,
        &graph,
        &belief,
        &from,
        &Place::tile("MtMoon_B2F", 5, 10),
        UnknownPolicy::Pessimistic,
    );
    print("Route 3 -> MtMoon_B2F (5, 10)", &cut_off);
    assert!(!cut_off.found());

    // ...but the floor is: the cheapest landing wins.
    let r = route_to_map(
        &world,
        &graph,
        &belief,
        &from,
        "MtMoon_B2F",
        UnknownPolicy::Pessimistic,
    );
    print("Route 3 -> MtMoon_B2F", &r);
    assert!(r.found());
    assert!(r.assumes.is_empty());
    assert!(r.blocked.is_empty(), "{:?}", r.blocked);
    let last = r.legs.last().unwrap();
    assert_eq!(last.to.map, "MtMoon_B2F");
    assert_eq!(last.kind, EdgeKind::Warp);
    assert!(r.legs.iter().all(|l| l.requires.is_empty()));
    let maps: Vec<&str> = r.legs.iter().map(|l| l.to.map.as_str()).collect();
    assert!(
        maps.contains(&"MtMoon_1F") && maps.contains(&"MtMoon_B1F"),
        "{maps:?}"
    );
    assert!(r.cost_s < cut_off.blocked.first().map_or(f64::INFINITY, |b| b.1));

    // Already there: nothing to do.
    let here = route_to_map(
        &world,
        &graph,
        &belief,
        &pose("MtMoon_B2F", 25, 21),
        "MtMoon_B2F",
        UnknownPolicy::Pessimistic,
    );
    assert!(here.found());
    assert!(here.legs.is_empty());
    assert_eq!(here.cost_s, 0.0);
}
