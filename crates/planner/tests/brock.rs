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
        handicap: 0,
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

/// Fleet workers (CHARMANDER starts): the relay estimate counted on
/// CHARMANDER fainting to ONIX and MANKEY finishing, but no Pokémon may
/// faint. The best single fighter is who leads, and its chance is the
/// party's.
#[test]
fn the_member_who_wins_alone_is_the_fighter() {
    let Some(data) = data() else { return };
    let charmander = Combatant::new(
        &data,
        "SPECIES_CHARMANDER",
        14,
        data.default_moves("SPECIES_CHARMANDER", 14),
        10,
    )
    .unwrap();
    let mankey = Combatant::new(
        &data,
        "SPECIES_MANKEY",
        14,
        data.default_moves("SPECIES_MANKEY", 14),
        10,
    )
    .unwrap();
    let party = [charmander.clone(), mankey.clone()];
    let (slot, p) = pokebot_planner::best_fighter(&data, &party, "TRAINER_LEADER_BROCK").unwrap();
    let alone = |c: &Combatant| {
        battle_vs_trainer(&data, std::slice::from_ref(c), "TRAINER_LEADER_BROCK")
            .unwrap()
            .p_win
    };
    assert_eq!(slot, 1, "MANKEY (Fighting) over CHARMANDER against Rock");
    assert!((p - alone(&mankey)).abs() < 1e-9);
    assert!(alone(&mankey) > alone(&charmander));
}

/// Switch, S.S. Anne: the rival's PIDGEOTTO (SAND-ATTACK) and CHARMELEON
/// (SMOKESCREEN) cut VENUSAUR's accuracy twice, its TACKLEs missed and
/// EMBER fainted it; the evaluator had counted every hit. The game scales
/// a move's accuracy by the attacker's accuracy stage net of the target's
/// evasion (`sAccuracyStageRatios`).
#[test]
fn accuracy_drops_lower_the_chance_to_win() {
    use pokebot_planner::evaluate::{matchup, staged_accuracy};
    let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
    assert!(close(staged_accuracy(1.0, 0), 1.0));
    assert!(close(staged_accuracy(0.95, -1), 0.95 * 0.75));
    assert!(close(staged_accuracy(0.95, -2), 0.95 * 0.6));
    assert!(close(staged_accuracy(0.95, -9), 0.95 * 0.33));
    assert!(close(staged_accuracy(0.95, 6), 1.0));
    let Some(data) = data() else { return };
    let moves = |m: &[&str]| m.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let mut us = Combatant::new(
        &data,
        "SPECIES_VENUSAUR",
        32,
        moves(&[
            "MOVE_TACKLE",
            "MOVE_SLEEP_POWDER",
            "MOVE_RAZOR_LEAF",
            "MOVE_VINE_WHIP",
        ]),
        10,
    )
    .unwrap();
    us.hp = 78;
    let them = Combatant::new(
        &data,
        "SPECIES_CHARMELEON",
        20,
        data.default_moves("SPECIES_CHARMELEON", 20),
        31,
    )
    .unwrap();
    let fresh = matchup(&data, &us, &them).p_win;
    us.acc_stage = -2;
    let blinded = matchup(&data, &us, &them).p_win;
    assert!(blinded < fresh, "{blinded} vs {fresh}");
}

/// Fleet worker 6: MISTY's SUPER POTION healed STARYU mid-fight (six
/// turns instead of three) and WARTORTLE met STARMIE worn and whited out.
/// A trainer's healing items count in the estimate: the same team is less
/// likely to win against her with the potion than without it.
#[test]
fn a_trainers_healing_item_lowers_the_chance_to_win() {
    use pokebot_planner::evaluate::battle_vs_trainer;
    let (Some(data), Some(mut without_items)) = (data(), data()) else {
        return;
    };
    assert_eq!(
        data.trainers["TRAINER_LEADER_MISTY"].items,
        vec!["ITEM_SUPER_POTION".to_owned()]
    );
    let wartortle = Combatant::new(
        &data,
        "SPECIES_WARTORTLE",
        20,
        ["MOVE_TACKLE", "MOVE_TAIL_WHIP", "MOVE_BUBBLE", "MOVE_BITE"]
            .map(String::from)
            .to_vec(),
        10,
    )
    .unwrap();
    let with = battle_vs_trainer(
        &data,
        std::slice::from_ref(&wartortle),
        "TRAINER_LEADER_MISTY",
    )
    .unwrap()
    .p_win;
    without_items
        .trainers
        .get_mut("TRAINER_LEADER_MISTY")
        .unwrap()
        .items
        .clear();
    let without = battle_vs_trainer(
        &without_items,
        std::slice::from_ref(&wartortle),
        "TRAINER_LEADER_MISTY",
    )
    .unwrap()
    .p_win;
    assert!(with < without, "with the potion {with}, without {without}");
}

/// Fleet workers 2 and 5 (CHARMANDER vs Brock, ₽680, no balls): the plan
/// caught a MANKEY, but keeping the Potion money and the shiny reserve the
/// mart sold no ball, and the same catch came back fourteen cycles. With
/// the tools' ball budget no catch is planned; the preparation trains
/// instead. With money for balls, recruiting stays an option.
#[test]
fn a_catch_the_money_cant_buy_balls_for_is_not_planned() {
    use pokebot_planner::stock::ball_budget;
    let Some(data) = data() else { return };
    let mut poor = request(&data, "SPECIES_CHARMANDER", &["MOVE_SCRATCH", "MOVE_GROWL"]);
    poor.money = ball_budget(&data, 680, 0);
    assert_eq!(poor.money, 0);
    let plans = plan_preparation(&poor, 1);
    let plan = plans.first().expect("a plan without catching");
    assert!(
        !plan
            .steps
            .iter()
            .any(|s| matches!(s, PlanStep::Catch { .. })),
        "{:?}",
        plan.steps
    );
    let mut rich = request(&data, "SPECIES_CHARMANDER", &["MOVE_SCRATCH", "MOVE_GROWL"]);
    rich.money = ball_budget(&data, 5000, 0);
    let any_catch = plan_preparation(&rich, 3)
        .iter()
        .any(|p| p.steps.iter().any(|s| matches!(s, PlanStep::Catch { .. })));
    assert!(any_catch, "with money a recruit is still an option");
}
