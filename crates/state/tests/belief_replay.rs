//! The world belief is rebuilt from the event log alone: replaying the
//! world events gives the same belief whether applied at once or one by one,
//! and a checkpoint restore replaces it.

use pokebot_state::{
    DefaultReducer, Direction, EventRecord, Fact, GameEvent, GameState, HealSpot, InferenceRules,
    Knowledge, KnowledgeSource, Priors, StateReducer, RULES_DIR,
};

fn records(events: Vec<(u64, GameEvent)>) -> Vec<EventRecord> {
    events
        .into_iter()
        .map(|(frame_id, event)| EventRecord { frame_id, event })
        .collect()
}

fn flag_observed(flag: &str, value: bool) -> GameEvent {
    GameEvent::FlagObserved {
        flag: flag.into(),
        value,
    }
}

fn flag_tracked(flag: &str, value: bool) -> GameEvent {
    GameEvent::FlagTracked {
        flag: flag.into(),
        value,
    }
}

#[test]
fn replaying_world_events_rebuilds_the_belief() {
    let events = records(vec![
        (
            1,
            GameEvent::MapVisited {
                map: "PalletTown".into(),
            },
        ),
        (2, flag_tracked("FLAG_BADGE01_GET", true)),
        (
            3,
            GameEvent::VarTracked {
                var: "VAR_MAP_SCENE_PALLET_TOWN_SIGN_LADY".into(),
                value: 1,
            },
        ),
        (
            4,
            GameEvent::VarObserved {
                var: "VAR_MAP_SCENE_PALLET_TOWN_SIGN_LADY".into(),
                value: 2,
            },
        ),
        (
            5,
            GameEvent::RespawnSet {
                map: "PewterCity_PokemonCenter_1F".into(),
                x: 7,
                y: 4,
            },
        ),
        (
            6,
            GameEvent::NpcSeen {
                map: "PalletTown".into(),
                local_id: 1,
                x: 4,
                y: 8,
                facing: Some(Direction::Down),
            },
        ),
        (
            7,
            GameEvent::NpcAbsent {
                map: "PalletTown".into(),
                local_id: 2,
            },
        ),
        (
            8,
            GameEvent::ScriptPathRun {
                script: "PewterCity_Gym_EventScript_Brock".into(),
                path: 0,
            },
        ),
        (9, flag_observed("FLAG_BADGE01_GET", true)),
        // Tracking what is already observed leaves the observation.
        (10, flag_tracked("FLAG_BADGE01_GET", true)),
        (
            11,
            GameEvent::IntentInfeasible {
                intent: "Fly".into(),
            },
        ),
        // Tracking a different value than observed makes it stale, keeping
        // the observation's frame.
        (12, flag_observed("FLAG_DEFEATED_BROCK", false)),
        (13, flag_tracked("FLAG_DEFEATED_BROCK", true)),
    ]);
    let reducer = DefaultReducer;
    let at_once = reducer.reduce(&GameState::default(), &events);
    let one_by_one = events.iter().fold(GameState::default(), |s, e| {
        reducer.reduce(&s, std::slice::from_ref(e))
    });
    assert_eq!(at_once, one_by_one);
    let w = &at_once.world;

    assert_eq!(w.visited("PalletTown"), Knowledge::observed(true, 1));
    assert_eq!(w.flag("FLAG_BADGE01_GET"), Knowledge::observed(true, 9));
    assert_eq!(
        w.var("VAR_MAP_SCENE_PALLET_TOWN_SIGN_LADY"),
        Knowledge::observed(2, 4)
    );
    assert_eq!(
        w.respawn,
        Knowledge::observed(
            HealSpot {
                map: "PewterCity_PokemonCenter_1F".into(),
                x: 7,
                y: 4
            },
            5
        )
    );
    let npc = w.npc("PalletTown", 1).unwrap();
    assert_eq!(npc.pos, Knowledge::observed((4, 8), 6));
    assert_eq!(npc.facing, Knowledge::observed(Direction::Down, 6));
    assert_eq!(npc.present, Knowledge::observed(true, 6));
    let gone = w.npc("PalletTown", 2).unwrap();
    assert_eq!(gone.present, Knowledge::observed(false, 7));
    assert_eq!(gone.pos, Knowledge::unknown());
    assert_eq!(
        w.paths_run,
        vec![("PewterCity_Gym_EventScript_Brock".to_owned(), 0)]
    );
    assert!(w.infeasible.contains("Fly"));
    assert_eq!(
        w.flag("FLAG_DEFEATED_BROCK"),
        Knowledge::tracked(true, Some(12))
    );
    assert_eq!(
        w.needs(&[
            Fact::flag("FLAG_BADGE02_GET"),
            Fact::flag("FLAG_BADGE01_GET")
        ]),
        vec![Fact::flag("FLAG_BADGE02_GET")]
    );

    // The intermediate tracked state, before the observation at frame 9.
    let early = reducer.reduce(&GameState::default(), &events[..3]);
    assert_eq!(
        early.world.flag("FLAG_BADGE01_GET"),
        Knowledge::tracked(true, None)
    );
    assert_eq!(
        early.world.var("VAR_MAP_SCENE_PALLET_TOWN_SIGN_LADY"),
        Knowledge::tracked(1, None)
    );

    // The state, belief included, survives JSON.
    let json = serde_json::to_string(&at_once).unwrap();
    let back: GameState = serde_json::from_str(&json).unwrap();
    assert_eq!(back, at_once);
}

#[test]
fn checkpoint_restore_replaces_the_belief_and_forgets_infeasible_intents() {
    let reducer = DefaultReducer;
    let saved = reducer
        .reduce(
            &GameState::default(),
            &records(vec![(1, flag_observed("FLAG_BADGE01_GET", true))]),
        )
        .saved_knowledge();
    let s = reducer.reduce(
        &GameState::default(),
        &records(vec![
            (
                2,
                GameEvent::MapVisited {
                    map: "Route4".into(),
                },
            ),
            (
                3,
                GameEvent::IntentInfeasible {
                    intent: "Fly".into(),
                },
            ),
            (
                4,
                GameEvent::CheckpointRestored {
                    knowledge: Box::new(saved.clone()),
                },
            ),
        ]),
    );
    assert_eq!(s.world, saved.world);
    assert_eq!(s.world.visited("Route4"), Knowledge::unknown());
    assert!(s.world.infeasible.is_empty());
}

#[test]
fn inference_and_priors_read_the_belief_built_from_events() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let rules = InferenceRules::load(root.join(RULES_DIR).join("inference.json")).unwrap();
    let priors = Priors::load(root.join(RULES_DIR).join("priors.json")).unwrap();
    let mut s = DefaultReducer.reduce(
        &GameState::default(),
        &records(vec![
            (1, flag_tracked("FLAG_BADGE02_GET", true)),
            (2, flag_observed("FLAG_BADGE01_GET", true)),
        ]),
    );
    rules.apply(&mut s.world);
    assert_eq!(
        s.world.flag("FLAG_DEFEATED_BROCK"),
        Knowledge::derived(true, 2)
    );
    assert_eq!(
        s.world.flag("FLAG_DEFEATED_MISTY").source,
        KnowledgeSource::Unknown,
        "tracked premises do not fire"
    );
    assert_eq!(
        priors.probability(&s.world, &Fact::visited("PewterCity")),
        1.0
    );
    assert_eq!(
        priors.probability(&s.world, &Fact::visited("CeruleanCity")),
        0.5
    );
}
