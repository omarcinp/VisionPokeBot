//! Structural checks of the Route 3 → Cascade Badge milestones against the
//! world model and game data (skipped without data/world).

use std::path::Path;

use pokebot_agent::nav::{route_from, Destination};
use pokebot_agent::story::{all_milestones, to_cerulean, to_mt_moon, Starter, StoryStep};
use pokebot_gamedata::GameData;
use pokebot_state::PlayerPose;
use pokebot_world::World;

fn root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn load() -> Option<(World, GameData)> {
    let world = World::load(root().join("data/world")).ok()?;
    let data = GameData::load(root().join("data/world/gamedata.json")).ok()?;
    Some((world, data))
}

fn has_graphics(world: &World, map: &str, graphics: &str) -> bool {
    world.map(map).is_some_and(|m| {
        m.objects
            .iter()
            .any(|o| o.graphics.as_deref() == Some(graphics))
    })
}

fn check_destination(world: &World, dest: &Destination) {
    match dest {
        Destination::Tile { map, x, y } => {
            let m = world.map(map).unwrap_or_else(|| panic!("no map {map}"));
            let t = m
                .tile(*x, *y)
                .unwrap_or_else(|| panic!("{map} ({x}, {y}) off the map"));
            assert_eq!(t.collision, 0, "{map} ({x}, {y}) is not walkable");
        }
        Destination::Warp { map, warp } => {
            let m = world.map(map).unwrap_or_else(|| panic!("no map {map}"));
            assert!(*warp < m.warps.len(), "{map} has no warp {warp}");
        }
        Destination::Facing { map, .. } => assert!(world.map(map).is_some(), "no map {map}"),
    }
}

#[test]
fn milestone_names_and_order() {
    let names: Vec<String> = to_cerulean().into_iter().map(|m| m.name).collect();
    assert_eq!(
        names,
        [
            "StockUpPewter",
            "CrossRoute3",
            "PrepareForMtMoon",
            "CrossMtMoon",
            "ReachCerulean",
            "PrepareForMisty",
            "BeatMisty"
        ]
    );
    let all: Vec<String> = all_milestones(Starter::Bulbasaur)
        .into_iter()
        .map(|m| m.name)
        .collect();
    let route3 = all.iter().position(|n| n == "PrepareForRoute3").unwrap();
    let pewter = all.iter().position(|n| n == "StockUpPewter").unwrap();
    assert_eq!(pewter, route3 + to_mt_moon().len());
    assert_eq!(all.last().map(String::as_str), Some("BeatMisty"));
}

#[test]
fn cerulean_steps_match_the_world() {
    let Some((world, data)) = load() else { return };
    for milestone in to_cerulean() {
        for step in &milestone.steps {
            let at = format!("{}: {step:?}", milestone.name);
            match step {
                StoryStep::Talk { map, object, .. } | StoryStep::Challenge { map, object } => {
                    let m = world.map(map).unwrap_or_else(|| panic!("{at}: no map"));
                    assert!(
                        m.objects.iter().any(|o| o.local_id == *object),
                        "{at}: no object {object}"
                    );
                }
                StoryStep::Go(dest) => check_destination(&world, dest),
                StoryStep::Battle { trigger, .. } => check_destination(&world, trigger),
                StoryStep::Heal { center } => {
                    let center = center.as_deref().expect("a named center");
                    assert!(
                        has_graphics(&world, center, "OBJ_EVENT_GFX_NURSE"),
                        "{at}: no nurse"
                    );
                }
                StoryStep::StockUp { mart } => {
                    let mart = mart.as_deref().expect("a named mart");
                    assert!(
                        data.marts
                            .get(mart)
                            .is_some_and(|items| items.iter().any(|i| i == "ITEM_POKE_BALL")),
                        "{at}: no Poké Balls"
                    );
                    assert!(
                        has_graphics(&world, mart, "OBJ_EVENT_GFX_CLERK"),
                        "{at}: no clerk"
                    );
                }
                StoryStep::Prepare { targets, areas, .. } => {
                    for t in targets {
                        assert!(data.trainers.contains_key(t), "{at}: unknown trainer {t}");
                    }
                    for a in areas {
                        assert!(world.map(a).is_some(), "{at}: unknown area {a}");
                        assert!(
                            data.wild
                                .get(a.as_str())
                                .is_some_and(|w| w.contains_key("land")),
                            "{at}: {a} has no land encounters"
                        );
                    }
                }
                StoryStep::Settle { .. } => {}
                other => panic!("{at}: unexpected step {other:?}"),
            }
        }
    }
}

#[test]
fn cerulean_routes_exist() {
    let Some((world, _)) = load() else { return };
    let pose = |map: &str, x, y| PlayerPose {
        map: map.into(),
        x,
        y,
    };
    let routes = [
        (
            pose("PewterCity_PokemonCenter_1F", 7, 4),
            "Route4_PokemonCenter_1F",
        ),
        (pose("Route4_PokemonCenter_1F", 7, 4), "MtMoon_B2F"),
        (pose("MtMoon_B2F", 14, 7), "CeruleanCity"),
    ];
    for (from, to) in routes {
        assert!(
            route_from(&world, &from, to).is_some(),
            "no route from {from:?} to {to}"
        );
    }
}
