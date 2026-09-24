//! Planner behaviour on real game data (skipped without data/world/gamedata.json).

use std::path::Path;

use pokebot_gamedata::GameData;
use pokebot_planner::evaluate::faint_probability;
use pokebot_planner::{
    battle_vs_trainer, plan_preparation, Area, Combatant, PartyMember, PlanStep, Request,
};

fn data() -> Option<GameData> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/world/gamedata.json");
    GameData::load(path).ok().or_else(|| {
        eprintln!("skipping: run tools/gamedata/extract_gamedata.py");
        None
    })
}

fn areas() -> Vec<Area> {
    ["Route1", "Route22", "Route2", "ViridianForest"]
        .iter()
        .map(|m| Area {
            map: (*m).into(),
            travel_minutes: 1.0,
            heal_minutes: 2.0,
        })
        .collect()
}

fn request<'a>(data: &'a GameData, species: &str, moves: &[&str]) -> Request<'a> {
    Request {
        party: vec![PartyMember {
            species: species.into(),
            level: 6,
            exp: None,
            moves: moves.iter().map(|m| (*m).to_owned()).collect(),
        }],
        targets: vec!["TRAINER_LEADER_BROCK".into()],
        areas: areas(),
        confidence: 0.9,
        money: 3000,
        data,
    }
}

#[test]
fn super_effective_move_decides_the_gym() {
    let Some(data) = data() else { return };
    let low = Combatant::new(
        &data,
        "SPECIES_BULBASAUR",
        6,
        vec!["MOVE_TACKLE".into(), "MOVE_GROWL".into()],
        10,
    )
    .unwrap();
    let vine = Combatant::new(
        &data,
        "SPECIES_BULBASAUR",
        10,
        data.default_moves("SPECIES_BULBASAUR", 10),
        10,
    )
    .unwrap();
    let before = battle_vs_trainer(&data, &[low], "TRAINER_LEADER_BROCK")
        .unwrap()
        .p_win;
    let after = battle_vs_trainer(&data, &[vine], "TRAINER_LEADER_BROCK")
        .unwrap()
        .p_win;
    assert!(before < 0.05, "{before}");
    assert!(after > 0.9, "{after}");
}

#[test]
fn bulbasaur_trains_for_vine_whip() {
    let Some(data) = data() else { return };
    let plans = plan_preparation(
        &request(&data, "SPECIES_BULBASAUR", &["MOVE_TACKLE", "MOVE_GROWL"]),
        3,
    );
    let best = &plans[0];
    assert!(best.min_confidence() >= 0.9);
    assert!(
        matches!(&best.steps[..], [PlanStep::Train { to: 10, .. }]),
        "{:?}",
        best.steps
    );
}

#[test]
fn charmander_recruits_a_fighting_type() {
    let Some(data) = data() else { return };
    let plans = plan_preparation(
        &request(&data, "SPECIES_CHARMANDER", &["MOVE_SCRATCH", "MOVE_GROWL"]),
        3,
    );
    let best = &plans[0];
    assert!(best.min_confidence() >= 0.9);
    assert!(
        best.steps
            .iter()
            .any(|s| matches!(s, PlanStep::Catch { species, .. } if species == "SPECIES_MANKEY")),
        "{:?}",
        best.steps
    );
}

#[test]
fn plans_are_deterministic() {
    let Some(data) = data() else { return };
    let a = plan_preparation(
        &request(
            &data,
            "SPECIES_SQUIRTLE",
            &["MOVE_TACKLE", "MOVE_TAIL_WHIP"],
        ),
        3,
    );
    let b = plan_preparation(
        &request(
            &data,
            "SPECIES_SQUIRTLE",
            &["MOVE_TACKLE", "MOVE_TAIL_WHIP"],
        ),
        3,
    );
    assert_eq!(
        format!("{:?}", a.iter().map(|p| &p.steps).collect::<Vec<_>>()),
        format!("{:?}", b.iter().map(|p| &p.steps).collect::<Vec<_>>())
    );
}

#[test]
fn faint_probability_grows_with_turns() {
    let Some(data) = data() else { return };
    let foe = Combatant::new(
        &data,
        "SPECIES_GEODUDE",
        9,
        data.default_moves("SPECIES_GEODUDE", 9),
        31,
    )
    .unwrap();
    let mut us = Combatant::new(
        &data,
        "SPECIES_IVYSAUR",
        18,
        vec!["MOVE_VINE_WHIP".into()],
        0,
    )
    .unwrap();
    let one = faint_probability(&data, &foe, &us, 1);
    let ten = faint_probability(&data, &foe, &us, 10);
    assert!(one <= ten && ten <= 1.0);
    us.hp = 1;
    assert!(faint_probability(&data, &foe, &us, 1) > 0.5);
}
