//! Dig and the Escape Rope over the real world data (skipped without
//! `data/world`): where the escape warp is set, and the routes out.

use std::path::Path;

use pokebot_state::{EscapeWarp, PlayerPose};
use pokebot_world::escape::{after_map_change, EscapeChange};
use pokebot_world::predicate::MapBelief;
use pokebot_world::route::{
    route, route_to_map, EdgeKind, Place, PlaceGraph, RouteParams, UnknownPolicy, ITEM_ESCAPE_ROPE,
    MOVE_DIG,
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

fn set(map: &str, x: i32, y: i32, entered: &str) -> EscapeChange {
    EscapeChange::Set(EscapeWarp {
        map: map.into(),
        x,
        y,
        entered: entered.into(),
    })
}

/// Entered from Route 4's west mouth: Mt. Moon escapes back out of it.
fn in_mt_moon() -> MapBelief {
    MapBelief::default().escape("Route4", (19, 6), "MtMoon_1F")
}

#[test]
fn an_entrance_from_outdoors_sets_the_escape_outside_it() {
    let Some(world) = world() else { return };
    // Walked north into the cave mouth at (19, 5).
    assert_eq!(
        after_map_change(&world, &pose("Route4", 19, 6), &pose("MtMoon_1F", 18, 37)),
        set("Route4", 19, 6, "MtMoon_1F")
    );
    // Deeper in, and back out: nothing changes.
    assert_eq!(
        after_map_change(&world, &pose("MtMoon_1F", 5, 6), &pose("MtMoon_B1F", 5, 5)),
        EscapeChange::Keep
    );
    assert_eq!(
        after_map_change(&world, &pose("MtMoon_1F", 18, 37), &pose("Route4", 19, 5)),
        EscapeChange::Keep
    );
    // A building's door: the tile below it.
    assert_eq!(
        after_map_change(
            &world,
            &pose("Route4", 12, 6),
            &pose("Route4_PokemonCenter_1F", 7, 8)
        ),
        set("Route4", 12, 6, "Route4_PokemonCenter_1F")
    );
}

#[test]
fn viridian_forest_keeps_the_escape_of_route_2() {
    let Some(world) = world() else { return };
    assert_eq!(
        after_map_change(
            &world,
            &pose("Route2", 5, 52),
            &pose("Route2_ViridianForest_SouthEntrance", 5, 1)
        ),
        set("Route2", 5, 52, "Route2_ViridianForest_SouthEntrance")
    );
    // Out of the forest into its north gate: kept (`UpdateEscapeWarp`).
    assert_eq!(
        after_map_change(
            &world,
            &pose("ViridianForest", 1, 1),
            &pose("Route2_ViridianForest_NorthEntrance", 5, 8)
        ),
        EscapeChange::Keep
    );
}

#[test]
fn a_map_that_sets_its_escape_on_arrival_is_followed() {
    let Some(world) = world() else { return };
    assert_eq!(
        after_map_change(
            &world,
            &pose("ThreeIsland_BondBridge", 12, 5),
            &pose("ThreeIsland_BerryForest", 13, 60)
        ),
        set("ThreeIsland_BondBridge", 12, 6, "ThreeIsland_BerryForest")
    );
    // Pattern Bush escapes by the side it was entered from.
    let bush = |x| {
        after_map_change(
            &world,
            &pose("SixIsland_GreenPath", 0, 0),
            &pose("SixIsland_PatternBush", x, 10),
        )
    };
    assert_eq!(
        bush(60),
        set("SixIsland_GreenPath", 64, 10, "SixIsland_PatternBush")
    );
    assert_eq!(
        bush(3),
        set("SixIsland_GreenPath", 45, 10, "SixIsland_PatternBush")
    );
}

#[test]
fn a_setting_warp_not_seen_taken_leaves_the_escape_unknown() {
    let Some(world) = world() else { return };
    assert_eq!(
        after_map_change(&world, &pose("Route4", 30, 12), &pose("MtMoon_1F", 18, 37)),
        EscapeChange::Lost
    );
}

#[test]
fn dig_leaves_a_cave_by_the_mouth_it_was_entered_from() {
    let Some(world) = world() else { return };
    let graph = PlaceGraph::build(&world, RouteParams::default());
    let from = pose("MtMoon_B2F", 17, 30);
    let out = |b: &MapBelief| {
        route_to_map(
            &world,
            &graph,
            b,
            &from,
            "Route4",
            UnknownPolicy::Pessimistic,
        )
    };
    let dig = out(&in_mt_moon().party_move(MOVE_DIG, true));
    assert!(dig.found());
    let first = &dig.legs[0];
    assert_eq!(first.kind, EdgeKind::Dig, "{:?}", dig.legs);
    assert_eq!(
        (first.from.map.as_str(), first.from.x, first.from.y),
        ("MtMoon_B2F", 17, 30)
    );
    assert_eq!(
        (first.to.map.as_str(), first.to.x, first.to.y),
        ("Route4", 19, 6)
    );
    assert!(dig.cost_s <= RouteParams::default().dig_s + 1e-9);

    let rope = out(&in_mt_moon()
        .party_move(MOVE_DIG, false)
        .item(ITEM_ESCAPE_ROPE, 1));
    let walk = out(&in_mt_moon()
        .party_move(MOVE_DIG, false)
        .item(ITEM_ESCAPE_ROPE, 0));
    assert!(rope.cost_s > dig.cost_s);
    assert!(!walk
        .legs
        .iter()
        .any(|l| matches!(l.kind, EdgeKind::Dig | EdgeKind::EscapeRope)));
    // Both at hand: Dig costs nothing.
    let both = out(&in_mt_moon()
        .party_move(MOVE_DIG, true)
        .item(ITEM_ESCAPE_ROPE, 3));
    assert_eq!(both.legs[0].kind, EdgeKind::Dig);
    // Not knowing the move is no route that needs it.
    let unknown = out(&in_mt_moon());
    assert!(!unknown.legs.iter().any(|l| l.kind == EdgeKind::Dig));
    assert!(unknown
        .blocked
        .iter()
        .all(|(needs, _)| !needs.iter().any(|p| p.to_string().contains(MOVE_DIG))));
    assert!(walk.found());
    // The rope, when it is cheaper than the walk out.
    if rope.legs[0].kind == EdgeKind::EscapeRope {
        assert!(rope.cost_s < walk.cost_s);
    } else {
        assert!(walk.cost_s <= rope.cost_s);
    }
}

#[test]
fn no_escape_where_the_map_forbids_it_or_the_warp_is_another_caves() {
    let Some(world) = world() else { return };
    let graph = PlaceGraph::build(&world, RouteParams::default());
    let digger = |b: MapBelief| b.party_move(MOVE_DIG, true);
    let dug = |b: &MapBelief, from: &PlayerPose, to: &str| {
        let r = route_to_map(&world, &graph, b, from, to, UnknownPolicy::Pessimistic);
        assert!(r.found(), "{from} to {to}");
        r.legs.iter().any(|l| l.kind == EdgeKind::Dig)
    };
    // A Pokémon Center doesn't allow escaping.
    let center = digger(MapBelief::default().escape("Route4", (12, 6), "Route4_PokemonCenter_1F"));
    assert!(!dug(
        &center,
        &pose("Route4_PokemonCenter_1F", 7, 4),
        "Route3"
    ));
    // The escape of Mt. Moon doesn't hold in Rock Tunnel.
    let tunnel = pose("RockTunnel_1F", 15, 5);
    assert!(!dug(&digger(in_mt_moon()), &tunnel, "Route10"));
    // Unknown: nothing to dig to.
    assert!(!dug(
        &digger(MapBelief::default()),
        &pose("MtMoon_B2F", 17, 30),
        "Route4"
    ));
}

#[test]
fn viridian_forest_digs_back_to_the_south_gate() {
    let Some(world) = world() else { return };
    let graph = PlaceGraph::build(&world, RouteParams::default());
    let b = MapBelief::default()
        .escape("Route2", (5, 52), "Route2_ViridianForest_SouthEntrance")
        .party_move(MOVE_DIG, true);
    let region = graph.escape_region("Route2_ViridianForest_SouthEntrance");
    assert!(region.contains("ViridianForest"));
    assert!(!region.contains("Route2"));
    let r = route_to_map(
        &world,
        &graph,
        &b,
        &pose("ViridianForest", 2, 5),
        "ViridianCity",
        UnknownPolicy::Pessimistic,
    );
    assert!(r.found());
    assert_eq!(r.legs[0].kind, EdgeKind::Dig, "{:?}", r.legs);
    assert_eq!(r.legs[0].to.map, "Route2");
}

#[test]
fn a_rope_is_used_when_the_walk_back_costs_more() {
    let Some(world) = world() else { return };
    let graph = PlaceGraph::build(&world, RouteParams::default());
    // Entered Rock Tunnel from Route 10's north mouth (8, 19); by its
    // south exit, the way back north is the whole tunnel.
    let b = MapBelief::default()
        .escape("Route10", (8, 20), "RockTunnel_1F")
        .party_move(MOVE_DIG, false)
        .item(ITEM_ESCAPE_ROPE, 1);
    let from = pose("RockTunnel_1F", 18, 35);
    let to = Place::tile("Route10", 8, 22);
    let r = route(&world, &graph, &b, &from, &to, UnknownPolicy::Pessimistic);
    let without = b.clone().item(ITEM_ESCAPE_ROPE, 0);
    let walk = route(
        &world,
        &graph,
        &without,
        &from,
        &to,
        UnknownPolicy::Pessimistic,
    );
    assert!(
        walk.cost_s > r.cost_s,
        "walk {:.1}s, rope {:.1}s",
        walk.cost_s,
        r.cost_s
    );
    assert_eq!(r.legs[0].kind, EdgeKind::EscapeRope);
    assert_eq!((r.legs[0].to.x, r.legs[0].to.y), (8, 20));
}
