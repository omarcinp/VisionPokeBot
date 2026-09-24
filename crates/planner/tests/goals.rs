//! Goal planning over the real world data from the Pewter and Route 4
//! checkpoints (skipped without `data/world`).

use std::path::{Path, PathBuf};
use std::time::Instant;

use pokebot_gamedata::GameData;
use pokebot_planner::{
    load_checkpoint, parse_goal, GoalPredicate, Intent, Methods, Obtain, Plan, PlanOptions,
    Planner, ProbeFact,
};
use pokebot_state::inference::InferenceRules;
use pokebot_state::{PlayerPose, Pocket, Priors, SavedKnowledge};
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

fn checkpoint(name: &str) -> (SavedKnowledge, Option<PlayerPose>) {
    let (mut knowledge, pose) =
        load_checkpoint(root().join("crates/planner/tests/fixtures").join(name)).unwrap();
    if let Ok(rules) = InferenceRules::load(root().join("data/rules/inference.json")) {
        rules.apply(&mut knowledge.world);
    }
    (knowledge, pose)
}

fn pewter() -> (SavedKnowledge, Option<PlayerPose>) {
    checkpoint("pewter_state.json")
}

/// The checkpoint at the Route 4 Pokémon Center's door: four party
/// members, three species caught, seven Poké Balls, Mt. Moon unvisited.
fn route4() -> (SavedKnowledge, Option<PlayerPose>) {
    checkpoint("route4_state.json")
}

impl Fixture {
    fn planner(&self, options: PlanOptions) -> Planner<'_> {
        Planner::new(
            &self.world,
            &self.graph,
            &self.data,
            Some(&self.obtain),
            Some(&self.priors),
            &self.methods,
            options,
        )
    }
}

/// The plan's step at which `pred` first holds, or the plan's length.
fn position(plan: &Plan, pred: impl Fn(&Intent) -> bool) -> usize {
    plan.intents
        .iter()
        .position(|s| pred(&s.intent))
        .unwrap_or(plan.intents.len())
}

/// The maps of a route's legs (from and to of each), in order.
fn leg_maps(step: &pokebot_planner::PlannedIntent) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for leg in &step.route {
        // "Map (x, y) -> Map (x, y) [kind] cost"
        let mut ends = leg.split(" -> ");
        let from = ends.next().unwrap_or("").split(" (").next().unwrap_or("");
        let to = ends.next().unwrap_or("").split(" (").next().unwrap_or("");
        for m in [from, to] {
            if out.last().map(String::as_str) != Some(m) {
                out.push(m.to_string());
            }
        }
    }
    out
}

/// Whether `wanted` appears in `seq` in order (not necessarily adjacent).
fn subsequence(seq: &[String], wanted: &[&str]) -> bool {
    let mut it = seq.iter();
    wanted.iter().all(|w| it.any(|s| s == w))
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

#[test]
fn flash_from_route4_goes_through_mt_moon_the_pokedex_and_cut() {
    let Some(f) = fixture() else { return };
    let options = PlanOptions {
        budget_s: 180.0,
        ..PlanOptions::default()
    };
    let planner = f.planner(options);
    let (knowledge, pose) = route4();
    assert_eq!(pose.as_ref().map(|p| p.map.as_str()), Some("Route4"));
    let goal = parse_goal("item ITEM_HM05 1").unwrap();
    let started = Instant::now();
    let plan = planner.plan(&goal, &knowledge, pose.clone()).unwrap();
    let elapsed = started.elapsed().as_secs_f64();
    print(&plan, 60);
    assert!(elapsed < 180.0, "planned in {elapsed:.1} s");
    assert!(plan.blocked().is_empty(), "{:?}", plan.blocked());
    assert!(plan.intents.iter().all(|s| s.cost_s.is_finite()));
    let intents: Vec<&Intent> = plan.intents.iter().map(|s| &s.intent).collect();

    // The aide is last, reached by the gatehouse's own route. Both ways
    // to the gatehouse cut trees (from Diglett's Cave one, from Pewter
    // two), so once Cut is taught the short way from Pewter is taken.
    let n = intents.len();
    assert!(
        matches!(intents[n - 1], Intent::RunScript { script, answers, .. }
            if script == "Route2_EastBuilding_EventScript_Aide" && answers == &["yes".to_string()]),
        "last step {}",
        intents[n - 1]
    );
    assert_eq!(
        intents[n - 2],
        &Intent::Go {
            dest: "Route2_EastBuilding".into()
        }
    );
    let legs = leg_maps(&plan.intents[n - 2]);
    println!("gatehouse route: {}", legs.join(" > "));
    assert!(
        subsequence(
            &legs,
            &[
                "Route4",
                "Route3",
                "PewterCity",
                "Route2",
                "Route2_EastBuilding"
            ]
        ),
        "{legs:?}"
    );
    // The long way round (Cerulean, the Underground Path, Vermilion,
    // Diglett's Cave) is what the Cut subgoals themselves walk.
    let captain = plan
        .intents
        .iter()
        .find(|s| matches!(&s.intent, Intent::Go { dest } if dest == "SSAnne_CaptainsOffice"))
        .expect("a Go to the captain");
    assert!(
        subsequence(
            &leg_maps(captain),
            &[
                "Route4",
                "MtMoon_1F",
                "MtMoon_B2F",
                "Route4",
                "CeruleanCity",
                "Route5",
                "UndergroundPath_NorthSouthTunnel",
                "Route6",
                "VermilionCity",
                "SSAnne_CaptainsOffice",
            ]
        ),
        "{:?}",
        leg_maps(captain)
    );
    assert!(
        plan.intents[n - 2]
            .route
            .iter()
            .any(|l| l.contains("gate:cut_tree")),
        "the Route 2 tree is cut on the way"
    );

    // Mt. Moon: the Super Nerd is beaten, then one fossil (the Dome, by
    // name) is taken so the way to Route 4's east side opens.
    let miguel = position(
        &plan,
        |i| matches!(i, Intent::Beat { trainer, map } if trainer == "TRAINER_SUPER_NERD_MIGUEL" && map == "MtMoon_B2F"),
    );
    let fossil = position(
        &plan,
        |i| matches!(i, Intent::RunScript { script, .. } if script == "MtMoon_B2F_EventScript_DomeFossil"),
    );
    assert!(
        miguel < fossil && fossil < n,
        "Beat at {miguel}, fossil at {fossil}"
    );
    assert!(!intents
        .iter()
        .any(|i| matches!(i, Intent::RunScript { script, .. } if script.contains("HelixFossil"))));

    // Cut: Bill's ticket, the captain's HM01, Misty's badge, then Teach.
    let bill = position(
        &plan,
        |i| matches!(i, Intent::RunScript { script, .. } if script == "Route25_SeaCottage_EventScript_Bill"),
    );
    let captain = position(
        &plan,
        |i| matches!(i, Intent::RunScript { script, .. } if script == "SSAnne_CaptainsOffice_EventScript_Captain"),
    );
    let misty = position(
        &plan,
        |i| matches!(i, Intent::RunScript { script, .. } if script == "CeruleanCity_Gym_EventScript_Misty"),
    );
    let teach = position(
        &plan,
        |i| matches!(i, Intent::Teach { hm, .. } if hm == "ITEM_HM01"),
    );
    assert!(bill < captain && captain < teach && misty < teach && teach < n - 2);

    // The Pokédex count is unknown (three species observed caught): the
    // Trainer Card is read first and, unless it shows ten, seven new
    // species are caught, each after a Go to its grass.
    let probe = position(&plan, |i| {
        matches!(
            i,
            Intent::Probe {
                fact: ProbeFact::TrainerCard
            }
        )
    });
    let catches: Vec<&pokebot_planner::PlannedIntent> = plan
        .intents
        .iter()
        .filter(|s| {
            matches!(s.intent, Intent::Catch { .. })
                && s.unless.contains(&GoalPredicate::pokedex_caught(10))
        })
        .collect();
    assert_eq!(catches.len(), 10 - 3, "{:?}", catches);
    assert!(probe < position(&plan, |i| matches!(i, Intent::Catch { .. })));
    let mut species: Vec<&str> = catches
        .iter()
        .filter_map(|s| match &s.intent {
            Intent::Catch { species, .. } => Some(species.as_str()),
            _ => None,
        })
        .collect();
    species.sort();
    species.dedup();
    assert_eq!(species.len(), 7, "seven different species");
    assert!(!species
        .iter()
        .any(|s| ["SPECIES_RATTATA", "SPECIES_PIDGEY", "SPECIES_CATERPIE"].contains(s)));
    for (i, s) in plan.intents.iter().enumerate() {
        if let Intent::Catch { map, .. } = &s.intent {
            let go = plan.intents[..i]
                .iter()
                .rev()
                .find(|s| matches!(s.intent, Intent::Go { .. }));
            assert!(
                matches!(go.map(|s| &s.intent), Some(Intent::Go { dest }) if dest == map),
                "catch on {map} without a Go to it"
            );
        }
    }
    // Route 4's grass lies past Mt. Moon: its Go is priced to the tile.
    if let Some(go) = plan.intents.iter().find(|s| {
        matches!(&s.intent, Intent::Go { dest } if dest == "Route4")
            && s.note.as_deref().is_some_and(|n| n.contains("grass"))
    }) {
        assert!(go.cost_s > 60.0, "{:.1}s", go.cost_s);
        assert!(leg_maps(go).contains(&"MtMoon_B2F".to_string()));
    }

    // Planning is a pure function of the knowledge.
    let again = planner.plan(&goal, &knowledge, pose).unwrap();
    assert_eq!(plan, again);
}

/// flash-5: from Viridian Forest (no Cut, one badge) the plan started with
/// Go(DiglettsCave_B1F) through the Route 2 Cut tree, priced with the Cut
/// the plan taught 24 steps later. A trip through a gate comes after the
/// steps that open it.
#[test]
fn a_trip_through_a_cut_tree_comes_after_cut_is_taught() {
    let Some(f) = fixture() else { return };
    let options = PlanOptions {
        budget_s: 180.0,
        ..PlanOptions::default()
    };
    let planner = f.planner(options);
    let (knowledge, pose) = checkpoint("viridian_forest_state.json");
    let goal = parse_goal("item ITEM_HM05 1").unwrap();
    let plan = planner.plan(&goal, &knowledge, pose).unwrap();
    print(&plan, 60);
    let cut = position(&plan, |i| matches!(i, Intent::Teach { .. }));
    for (i, step) in plan.intents.iter().enumerate() {
        let through_tree = step.route.iter().any(|leg| leg.contains("gate:cut_tree"));
        assert!(
            !through_tree || i > cut,
            "step {} {} cuts a tree before Teach (step {})",
            i + 1,
            step.intent,
            cut + 1
        );
    }
}

#[test]
fn cerulean_from_route2_returns_within_the_budget_with_the_fossil() {
    let Some(f) = fixture() else { return };
    let options = PlanOptions {
        budget_s: 60.0,
        ..PlanOptions::default()
    };
    let planner = f.planner(options);
    let (knowledge, _) = pewter();
    let pose = Some(PlayerPose {
        map: "Route2".into(),
        x: 8,
        y: 10,
    });
    let goal = parse_goal("at CeruleanCity").unwrap();
    let started = Instant::now();
    let plan = planner.plan(&goal, &knowledge, pose.clone()).unwrap();
    let elapsed = started.elapsed().as_secs_f64();
    print(&plan, 20);
    assert!(elapsed < 60.0, "planned in {elapsed:.1} s");
    assert!(plan.blocked().is_empty(), "{:?}", plan.blocked());
    let n = plan.intents.len();
    assert_eq!(
        plan.intents[n - 1].intent,
        Intent::Go {
            dest: "CeruleanCity".into()
        }
    );
    let fossil = position(
        &plan,
        |i| matches!(i, Intent::RunScript { script, .. } if script == "MtMoon_B2F_EventScript_DomeFossil"),
    );
    let miguel = position(
        &plan,
        |i| matches!(i, Intent::Beat { trainer, .. } if trainer == "TRAINER_SUPER_NERD_MIGUEL"),
    );
    assert!(miguel < fossil && fossil < n - 1);
    // Both are skipped when the fossil turns out taken.
    assert!(plan.intents[fossil]
        .unless
        .contains(&GoalPredicate::flag("FLAG_HIDE_DOME_FOSSIL", true)));
    let legs = leg_maps(&plan.intents[n - 1]);
    assert!(
        subsequence(
            &legs,
            &[
                "Route2",
                "PewterCity",
                "Route3",
                "Route4",
                "MtMoon_1F",
                "MtMoon_B2F",
                "Route4",
                "CeruleanCity"
            ]
        ),
        "{legs:?}"
    );
    let again = planner.plan(&goal, &knowledge, pose).unwrap();
    assert_eq!(plan, again);
}

#[test]
fn a_budget_of_nothing_returns_the_partial_plan() {
    let Some(f) = fixture() else { return };
    let options = PlanOptions {
        budget_s: 0.0,
        ..PlanOptions::default()
    };
    let planner = f.planner(options);
    let (knowledge, pose) = route4();
    let goal = parse_goal("item ITEM_HM05 1").unwrap();
    match planner.plan(&goal, &knowledge, pose) {
        Err(pokebot_planner::PlanError::Budget {
            best_partial,
            nodes,
            ..
        }) => {
            assert!(nodes <= 1);
            let partial = best_partial.expect("a partial plan");
            assert!(partial
                .intents
                .iter()
                .any(|s| matches!(&s.intent, Intent::Unsupported { establishes, .. } if *establishes == goal)));
        }
        other => panic!("expected a budget error, got {other:?}"),
    }
    // A stop request ends planning the same way.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let options = PlanOptions {
        stop: Some(stop),
        ..PlanOptions::default()
    };
    let planner = f.planner(options);
    let (knowledge, pose) = route4();
    assert!(matches!(
        planner.plan(&goal, &knowledge, pose),
        Err(pokebot_planner::PlanError::Budget { .. })
    ));
}
