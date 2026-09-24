//! Goal planning over the real world data from a Pewter checkpoint
//! (skipped without `data/world`).

use std::path::{Path, PathBuf};

use pokebot_gamedata::GameData;
use pokebot_planner::{
    load_checkpoint, parse_goal, GoalPredicate, Intent, Methods, Obtain, Plan, PlanOptions,
    Planner, ProbeFact,
};
use pokebot_state::inference::InferenceRules;
use pokebot_state::{Pocket, Priors, SavedKnowledge};
use pokebot_world::route::{PlaceGraph, RouteParams};
use pokebot_world::World;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Fixture {
    world: World,
    graph: PlaceGraph,
    data: GameData,
    obtain: Obtain,
    priors: Priors,
    methods: Methods,
}

fn fixture() -> Option<Fixture> {
    let root = root();
    let world = World::load(root.join("data/world")).ok()?;
    world.events()?;
    let graph = PlaceGraph::build(&world, RouteParams::default());
    let data = GameData::load(root.join("data/world/gamedata.json")).ok()?;
    let obtain = Obtain::load(root.join("data/world")).ok()?;
    let priors = Priors::load(root.join("data/rules/priors.json")).ok()?;
    let methods = Methods::load(root.join("data/rules/methods.json")).unwrap();
    Some(Fixture {
        world,
        graph,
        data,
        obtain,
        priors,
        methods,
    })
}

fn pewter() -> (SavedKnowledge, Option<pokebot_state::PlayerPose>) {
    let (mut knowledge, pose) =
        load_checkpoint(root().join("crates/planner/tests/fixtures/pewter_state.json")).unwrap();
    if let Ok(rules) = InferenceRules::load(root().join("data/rules/inference.json")) {
        rules.apply(&mut knowledge.world);
    }
    (knowledge, pose)
}

fn print(plan: &Plan, limit: usize) {
    for (i, step) in plan.intents.iter().enumerate().take(limit) {
        let mut assumes: Vec<String> = step.assumes.iter().map(|p| p.to_string()).collect();
        assumes.extend(step.unless.iter().map(|p| format!("unless {p}")));
        assumes.extend(step.note.iter().cloned());
        println!(
            "{:>3}  {:<70} {:>7.1}s  {}",
            i + 1,
            step.intent.to_string(),
            step.cost_s,
            assumes.join(" & ")
        );
    }
    if plan.intents.len() > limit {
        println!("     … {} more", plan.intents.len() - limit);
    }
    println!("total {:.1}s, {} steps", plan.cost_s, plan.intents.len());
}

#[test]
fn catch_rattata_probes_the_bag_then_goes_to_the_cheapest_area() {
    let Some(f) = fixture() else { return };
    let planner = Planner::new(
        &f.world,
        &f.graph,
        &f.data,
        Some(&f.obtain),
        Some(&f.priors),
        &f.methods,
        PlanOptions::default(),
    );
    let (knowledge, pose) = pewter();
    let goal = parse_goal("catch RATTATA").unwrap();
    let plan = planner.plan(&goal, &knowledge, pose.clone()).unwrap();
    print(&plan, 20);
    let intents: Vec<&Intent> = plan.intents.iter().map(|s| &s.intent).collect();
    assert_eq!(
        intents[0],
        &Intent::Probe {
            fact: ProbeFact::BagPocket(Pocket::PokeBalls)
        },
        "balls are unknown and a wrong guess costs a mart trip: probe first"
    );
    let catch = intents
        .iter()
        .position(|i| matches!(i, Intent::Catch { .. }))
        .expect("a Catch");
    // The Go right before the Catch takes the bot to the area.
    let go = intents[..catch]
        .iter()
        .rposition(|i| matches!(i, Intent::Go { .. }))
        .expect("a Go to the area");
    let (
        Intent::Go { dest },
        Intent::Catch {
            species,
            map,
            balls,
            ..
        },
    ) = (intents[go], intents[catch])
    else {
        unreachable!()
    };
    assert_eq!(dest, map);
    assert_eq!(species, "SPECIES_RATTATA");
    assert!(*balls > 5, "expected throws plus the reserve");
    assert!(
        ["Route2", "Route1", "Route22"].contains(&map.as_str()),
        "cheapest area from Pewter, got {map}"
    );
    assert!(plan.intents.iter().all(|s| s.cost_s.is_finite()));
    assert!(plan.blocked().is_empty(), "{:?}", plan.blocked());
    // The bag is probed; buying is conditional on what it shows.
    let buy = plan
        .intents
        .iter()
        .find(|s| matches!(s.intent, Intent::Buy { .. }))
        .expect("a conditional Buy");
    assert!(buy
        .unless
        .contains(&GoalPredicate::has_item("ITEM_POKE_BALL", *balls)));
    assert!(plan.intents[0].assumes.is_empty());
    // Planning is a pure function of the knowledge.
    let again = planner.plan(&goal, &knowledge, pose).unwrap();
    assert_eq!(plan, again);
}

#[test]
fn game_clear_expands_into_the_eight_gyms_in_order() {
    let Some(f) = fixture() else { return };
    let planner = Planner::new(
        &f.world,
        &f.graph,
        &f.data,
        Some(&f.obtain),
        Some(&f.priors),
        &f.methods,
        PlanOptions::default(),
    );
    let (knowledge, pose) = pewter();
    let goal = parse_goal("flag FLAG_SYS_GAME_CLEAR").unwrap();
    let plan = planner.plan(&goal, &knowledge, pose.clone()).unwrap();
    print(&plan, 60);
    let leaders = [
        "TRAINER_LEADER_MISTY",
        "TRAINER_LEADER_LT_SURGE",
        "TRAINER_LEADER_ERIKA",
        "TRAINER_LEADER_KOGA",
        "TRAINER_LEADER_SABRINA",
        "TRAINER_LEADER_BLAINE",
        "TRAINER_LEADER_GIOVANNI",
    ];
    let scripts: Vec<&str> = plan
        .intents
        .iter()
        .filter_map(|s| match &s.intent {
            Intent::RunScript { script, .. } => Some(script.as_str()),
            _ => None,
        })
        .collect();
    let gym_scripts: Vec<&str> = scripts
        .iter()
        .copied()
        .filter(|s| s.contains("_Gym_EventScript_"))
        .collect();
    println!("gyms: {gym_scripts:?}");
    // Brock is done (badge 1 observed); the other seven follow in order.
    assert!(!gym_scripts.iter().any(|s| s.contains("Brock")));
    let mut last = 0;
    for leader in leaders {
        let name = leader.trim_start_matches("TRAINER_LEADER_");
        let pos = plan
            .intents
            .iter()
            .position(|s| match &s.intent {
                Intent::RunScript { script, .. } => {
                    script.contains("_Gym_EventScript_")
                        && script
                            .to_ascii_uppercase()
                            .replace('_', "")
                            .contains(&name.replace('_', ""))
                }
                _ => false,
            })
            .unwrap_or_else(|| panic!("{leader} missing from the plan"));
        assert!(pos >= last, "{leader} out of order");
        last = pos;
    }
    // HM subgoals appear before the gyms that need them.
    let teach_cut = plan
        .intents
        .iter()
        .position(|s| matches!(&s.intent, Intent::Teach { hm, .. } if hm == "ITEM_HM01"))
        .expect("Cut is taught");
    let surge = plan
        .intents
        .iter()
        .position(
            |s| matches!(&s.intent, Intent::RunScript { script, .. } if script.contains("LtSurge")),
        )
        .unwrap();
    assert!(teach_cut < surge);
    assert!(scripts.contains(&"SSAnne_CaptainsOffice_EventScript_Captain"));
    let again = planner.plan(&goal, &knowledge, pose).unwrap();
    assert_eq!(plan, again);
}

#[test]
fn a_goal_that_already_holds_is_an_empty_plan() {
    let Some(f) = fixture() else { return };
    let planner = Planner::new(
        &f.world,
        &f.graph,
        &f.data,
        Some(&f.obtain),
        Some(&f.priors),
        &f.methods,
        PlanOptions::default(),
    );
    let (knowledge, pose) = pewter();
    let plan = planner
        .plan(&GoalPredicate::badge(1), &knowledge, pose)
        .unwrap();
    assert!(plan.intents.is_empty());
    assert_eq!(plan.cost_s, 0.0);
}
