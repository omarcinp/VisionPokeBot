//! The compiled event data (`data/world/events.json`, `dialogue.json`,
//! `places.json`, written by `tools/world/compile_events.py`); skipped
//! without `data/world`.

use std::path::{Path, PathBuf};

use pokebot_world::World;

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world")
}

#[test]
fn objects_carry_movement_areas() {
    let Ok(world) = World::load(data_dir()) else {
        return;
    };
    let pallet = world.map("PalletTown").expect("PalletTown");
    let lady = &pallet.objects[0];
    assert_eq!(lady.local_id, 1);
    assert_eq!((lady.range_x, lady.range_y), (1, 4));
    assert_eq!(lady.area(), (2, 6, 4, 14));
    for o in &pallet.objects {
        let (x, y) = (o.x.unwrap(), o.y.unwrap());
        assert_eq!(
            o.area(),
            (x - o.range_x, y - o.range_y, x + o.range_x, y + o.range_y)
        );
    }
}

#[test]
fn brock_paths_are_compiled() {
    use pokebot_world::events::{Condition, Effect};
    let Ok(world) = World::load(data_dir()) else {
        return;
    };
    let Some(events) = world.events() else {
        return; // data built before compile_events.py existed
    };
    assert_eq!(events.rom, "firered_rev1");
    let brock = &events.scripts["PewterCity_Gym_EventScript_Brock"];
    assert_eq!(brock.kind, "object");
    assert_eq!(brock.map.as_deref(), Some("PewterCity_Gym"));
    assert!(brock.paths.len() >= 3, "{} paths", brock.paths.len());
    let fight = brock
        .paths
        .iter()
        .find(|p| {
            p.when.iter().any(|c| matches!(c, Condition::Trainer { trainer, defeated: false } if trainer == "TRAINER_LEADER_BROCK"))
                && p.does.iter().any(|e| matches!(e, Effect::Give { give, .. } if give == "ITEM_TM39"))
        })
        .expect("fight path");
    assert!(fight.does.iter().any(
        |e| matches!(e, Effect::Battle { battle, intro: Some(intro), .. }
        if battle == "TRAINER_LEADER_BROCK" && intro == "PewterCity_Gym_Text_BrockIntro")
    ));
    assert!(fight
        .does
        .iter()
        .any(|e| matches!(e, Effect::Set { set } if set == "FLAG_BADGE01_GET")));
    assert!(fight.does.iter().any(|e| matches!(e, Effect::Var { var, change } if var == "VAR_MAP_SCENE_PEWTER_CITY" && change.eq == Some(1.into()))));
    assert!(fight.opaque.is_empty(), "{:?}", fight.opaque);
    let post = brock
        .paths
        .iter()
        .find(|p| p.when.iter().any(|c| matches!(c, Condition::Flag { flag, is: true } if flag == "FLAG_GOT_TM39_FROM_BROCK")))
        .expect("post-battle path");
    assert!(
        matches!(&post.does[..], [Effect::Say { say }] if say == "PewterCity_Gym_Text_BrockPostBattle")
    );
    // Every script entry parses into a typed condition/effect or `Other`.
    let other = events
        .scripts
        .values()
        .flat_map(|s| s.paths.iter())
        .flat_map(|p| p.does.iter())
        .filter(|e| matches!(e, Effect::Other(_)))
        .count();
    assert_eq!(other, 0, "effects that no variant matched");
    let lady = events
        .objects
        .iter()
        .find(|o| o.map == "PalletTown" && o.local_id == 1)
        .unwrap();
    assert_eq!((lady.range_x, lady.range_y), (1, 4));
    assert_eq!(
        events.map_scripts["PalletTown"].on_transition,
        vec!["PalletTown_OnTransition"]
    );
}

#[test]
fn pokedex_aide_gate_is_typed() {
    use pokebot_world::events::{Condition, DexCount, Effect};
    let Ok(world) = World::load(data_dir()) else {
        return;
    };
    let Some(events) = world.events() else {
        return;
    };
    let aide = &events.scripts["Route2_EastBuilding_EventScript_Aide"];
    let give = aide
        .paths
        .iter()
        .find(|p| {
            p.does
                .iter()
                .any(|e| matches!(e, Effect::Give { give, .. } if give == "ITEM_HM05"))
        })
        .expect("give path");
    assert!(
        give.when.iter().any(|c| matches!(
            c,
            Condition::Pokedex { which: DexCount::Caught, national: false, cmp }
            if cmp.ge == Some(10.into()) && cmp.lt.is_none()
        )),
        "{:?}",
        give.when
    );
    for path in &aide.paths {
        for c in &path.when {
            assert!(
                !matches!(c, Condition::Other(_) | Condition::Var { .. }),
                "{c:?}"
            );
        }
    }
    // The same quantities elsewhere: money, party size, the National Dex.
    let all = || {
        events
            .scripts
            .values()
            .flat_map(|s| s.paths.iter())
            .flat_map(|p| p.when.iter())
    };
    assert!(all().any(|c| matches!(c, Condition::Money { money, cmp } if money == "player" && cmp.ge == Some(500.into()))));
    assert!(all().any(|c| matches!(c, Condition::PartySize { party, cmp } if party == "size" && cmp.eq == Some(6.into()))));
    assert!(all().any(|c| matches!(c, Condition::PokedexComplete { pokedex_complete, is: true } if pokedex_complete == "kanto")));
    assert!(all().any(|c| matches!(c, Condition::InParty { in_party, is: true } if in_party == &pokebot_world::events::Val::Sym("SPECIES_MAGIKARP".into()))));
    assert!(all().any(
        |c| matches!(c, Condition::Flag { flag, is: false } if flag == "FLAG_SYS_NATIONAL_DEX")
    ));
    assert_eq!(
        all().filter(|c| matches!(c, Condition::Other(_))).count(),
        0
    );
}

#[test]
fn dialogue_identifies_lines_with_wildcards() {
    let Ok(world) = World::load(data_dir()) else {
        return;
    };
    let Some(dialogue) = world.dialogue() else {
        return;
    };
    let lines = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let found = dialogue.identify(&lines(&["This tree looks like it can be CUT", "down!"]));
    assert!(found.contains(&"Text_TreeCanBeCutDown"), "{found:?}");
    // OCR doubt (`?`) matches any character; a `*` in the label matches a name.
    let found = dialogue.identify(&lines(&["This tree looks like it can be CU?", "down!"]));
    assert!(found.contains(&"Text_TreeCanBeCutDown"), "{found:?}");
    let found = dialogue.identify(&lines(&["RED's house"]));
    assert!(found.contains(&"PalletTown_Text_PlayersHouse"), "{found:?}");
    // A second line that doesn't continue the text rules the label out.
    let found = dialogue.identify(&lines(&["This tree looks like it can be CUT", "up!"]));
    assert!(!found.contains(&"Text_TreeCanBeCutDown"), "{found:?}");
    // Only labels whose whole first line is a placeholder match nonsense.
    for label in dialogue.identify(&lines(&["no such line at all"])) {
        assert_eq!(dialogue.labels[label][0][0], "*", "{label}");
    }
}

#[test]
fn places_have_heal_fly_and_gates() {
    let Ok(world) = World::load(data_dir()) else {
        return;
    };
    let Some(places) = world.places() else {
        return;
    };
    let pewter = places
        .heal_spots
        .iter()
        .find(|h| h.map == "PewterCity")
        .expect("Pewter heal spot");
    assert_eq!((pewter.x, pewter.y), (17, 26));
    assert_eq!(pewter.respawn_map, "PewterCity_PokemonCenter_1F");
    let pallet = places
        .fly_spot("FLAG_WORLD_MAP_PALLET_TOWN")
        .expect("Pallet fly spot");
    assert_eq!(
        (pallet.map.as_str(), pallet.x, pallet.y),
        ("PalletTown", 6, 8)
    );
    let tree = places
        .gates
        .iter()
        .find(|g| g.map == "Route2" && g.kind == "cut_tree")
        .expect("Route 2 cut tree");
    assert_eq!(tree.requires.r#move, "MOVE_CUT");
    assert_eq!(tree.requires.badge, "FLAG_BADGE02_GET");
    assert!(places.marts["ViridianCity_Mart"].contains(&"ITEM_POKE_BALL".to_string()));
}
