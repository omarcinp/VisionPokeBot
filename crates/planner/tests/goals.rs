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
    // No methods: the story is derived from the compiled events alone.
    // `data/rules/methods.json` must still load (it may order the search).
    Methods::load(root.join("data/rules/methods.json")).unwrap();
    let methods = Methods::default();
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
        if !step.expected.is_empty() {
            let e: Vec<String> = step
                .expected
                .iter()
                .map(|(p, prob)| format!("{p} {prob:.2}"))
                .collect();
            assumes.push(format!("expects {}", e.join(", ")));
        }
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
        // Bounded by nodes: the wall clock depends on the machine's load.
        PlanOptions {
            budget_s: 1200.0,
            ..PlanOptions::default()
        },
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

/// Spec §4.6: the story (Mt. Moon, Bill, the S.S. Anne, Misty, Cut) comes
/// first; the walk through its encounter areas is expected to catch new
/// species on its own, with balls stocked ahead of it; explicit catches
/// cover only the remainder and come after Cut, just before the aide;
/// the PC boxes are audited first, the Center being next door.
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
    // The story is not held up by the Pokédex: it starts at Mt. Moon.
    assert!(miguel < bill);

    // The Pokédex count is unknown (three species observed caught): the
    // Trainer Card is read before anything conditional on it. Ten are
    // needed: the walk through Mt. Moon, the Nugget Bridge, Route 25 and
    // the Underground Path is expected to catch some (§4.6.2), and only
    // the remainder is hunted explicitly, after Cut and just before the
    // aide, each after a Go to its grass.
    let probe = position(&plan, |i| {
        matches!(
            i,
            Intent::Probe {
                fact: ProbeFact::TrainerCard
            }
        )
    });
    let catches: Vec<(usize, &pokebot_planner::PlannedIntent)> = plan
        .intents
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            matches!(s.intent, Intent::Catch { .. })
                && s.unless.contains(&GoalPredicate::pokedex_caught(10))
        })
        .collect();
    let expected = plan.expected_new_species();
    println!(
        "expected on the way {expected:.2}, explicit catches {}",
        catches.len()
    );
    assert!(expected > 0.0, "the walk yields nothing?");
    assert!(
        expected + catches.len() as f64 >= 7.0,
        "expected {expected:.2} + {} explicit < 7",
        catches.len()
    );
    assert!(catches.len() < 7, "the walk should spare some catches");
    assert!(probe < catches.first().map_or(n, |(i, _)| *i));
    for (i, s) in &catches {
        assert!(
            *i > teach && *i < n - 2,
            "step {} {} is not between Cut and the aide",
            i + 1,
            s.intent
        );
    }
    let mut species: Vec<&str> = catches
        .iter()
        .filter_map(|(_, s)| match &s.intent {
            Intent::Catch { species, .. } => Some(species.as_str()),
            _ => None,
        })
        .collect();
    species.sort();
    species.dedup();
    assert_eq!(species.len(), catches.len(), "different species each");
    assert!(!species
        .iter()
        .any(|s| ["SPECIES_RATTATA", "SPECIES_PIDGEY", "SPECIES_CATERPIE"].contains(s)));
    // The story's own walks carry the expectations (Mt. Moon is the
    // first), and the balls for them are bought ahead of the story.
    let mt_moon = position(
        &plan,
        |i| matches!(i, Intent::Go { dest } if dest == "MtMoon_B2F"),
    );
    assert!(plan.intents[mt_moon].expected_new_species() > 0.5);
    let buy = position(
        &plan,
        |i| matches!(i, Intent::Buy { item, .. } if item == "ITEM_POKE_BALL"),
    );
    assert!(buy < mt_moon, "balls at {buy}, Mt. Moon at {mt_moon}");
    // Every explicit catch for the Pokédex lies after the story; none
    // before Cut is taught. (A catch the readiness planner adds for a
    // battle on the way, the rival's at Cerulean whose team follows the
    // starter IVYSAUR evolved from, goes before that battle.)
    assert!(
        !plan.intents[..teach].iter().any(|s| {
            matches!(s.intent, Intent::Catch { .. })
                && s.unless.contains(&GoalPredicate::pokedex_caught(10))
        }),
        "a Pokédex catch before Cut"
    );
    // Bootstrap audit (§4.6.3): the Route 4 Center is next door and the
    // boxes unknown, so the plan opens with the PC.
    let pc = position(&plan, |i| {
        matches!(
            i,
            Intent::Probe {
                fact: ProbeFact::PcBoxes
            }
        )
    });
    assert!(pc <= 1, "PC audit at {pc}");
    assert!(
        matches!(&plan.intents[0].intent, Intent::Go { dest } if dest == "Route4_PokemonCenter_1F")
    );
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

/// The first Switch goal run: a Lv6 Bulbasaur at 10/22 HP in Oak's lab
/// after DeliverParcel, boxes unknown, nothing else observed.
fn fresh_game() -> (SavedKnowledge, Option<PlayerPose>) {
    checkpoint("fresh_game_state.json")
}

/// The first Switch goal run sent the 10/22 HP starter onto Route 1 and it
/// whited out: a walk through wild encounters, a hunt or a training session
/// needs the lead at half its HP or more, so the plan heals first, at Mom's
/// (the respawn heal spot two doors away), by least commitment right ahead
/// of the walk; with the boxes unknown the PC audit rides on that heal.
#[test]
fn a_low_lead_heals_before_the_first_walk_through_grass() {
    let Some(f) = fixture() else { return };
    let planner = f.planner(PlanOptions::default());
    let (knowledge, pose) = fresh_game();
    let plan = planner
        .plan(&GoalPredicate::badge(1), &knowledge, pose.clone())
        .unwrap();
    print(&plan, 20);
    assert!(plan.blocked().is_empty(), "{:?}", plan.blocked());
    let heal = position(
        &plan,
        |i| matches!(i, Intent::Heal { center } if center == "PalletTown_PlayersHouse_1F"),
    );
    let first_grass = plan
        .intents
        .iter()
        .position(
            // Route 2 too: a Lv5 lead now trains only where it wins its
            // first battles (not Route 1's or Route 22's middle levels).
            |s| matches!(&s.intent, Intent::Go { dest } if ["Route22", "Route1", "Route2"].contains(&dest.as_str())),
        )
        .expect("a walk onto a route");
    let train = position(&plan, |i| matches!(i, Intent::Train { .. }));
    assert!(heal < first_grass && heal < train, "heal at {heal}");
    assert!(
        matches!(&plan.intents[heal - 1].intent, Intent::Go { dest } if dest == "PalletTown_PlayersHouse_1F"),
        "the trip home precedes the heal"
    );
    let pc = position(&plan, |i| {
        matches!(
            i,
            Intent::Probe {
                fact: ProbeFact::PcBoxes
            }
        )
    });
    assert_eq!(pc, heal + 1, "the box audit rides on the heal");
    let brock = position(
        &plan,
        |i| matches!(i, Intent::RunScript { script, .. } if script.contains("Brock")),
    );
    assert_eq!(brock, plan.intents.len() - 1);
    let again = planner
        .plan(&GoalPredicate::badge(1), &knowledge, pose)
        .unwrap();
    assert_eq!(plan, again);
}

/// The League from a fresh game: readiness that falls short of the
/// confidence target is a `Train` to the best level the window finds,
/// marked for re-evaluation, never an `Unsupported` placeholder that fails
/// on execution; the lead's level carries over from one training to the
/// next, so the levels climb instead of restarting from Lv6 at every gym.
#[test]
fn readiness_that_falls_short_trains_as_far_as_it_can_and_is_judged_again() {
    let Some(f) = fixture() else { return };
    let options = PlanOptions {
        budget_s: 1200.0,
        ..PlanOptions::default()
    };
    let planner = f.planner(options);
    let (knowledge, pose) = fresh_game();
    let goal = parse_goal("flag FLAG_SYS_GAME_CLEAR").unwrap();
    let started = Instant::now();
    let plan = planner.plan(&goal, &knowledge, pose).unwrap();
    println!("planned in {:.1} s", started.elapsed().as_secs_f64());
    print(&plan, 90);
    for reason in plan.blocked() {
        assert!(
            !reason.contains("training") && !reason.contains("readies"),
            "readiness placeholder: {reason}"
        );
    }
    let trains: Vec<(u8, &pokebot_planner::PlannedIntent)> = plan
        .intents
        .iter()
        .filter_map(|s| match &s.intent {
            Intent::Train { level, .. } => Some((*level, s)),
            _ => None,
        })
        .collect();
    assert!(trains.len() >= 3, "{} Train steps", trains.len());
    for w in trains.windows(2) {
        assert!(w[0].0 <= w[1].0, "levels fall: {} then {}", w[0].0, w[1].0);
    }
    // A shortfall is marked on the training that leaves it, with the
    // confidence it reaches, and never priced as a placeholder.
    let short: Vec<_> = trains
        .iter()
        .filter(|(_, s)| {
            s.note
                .as_deref()
                .is_some_and(|n| n.contains("reaches only"))
        })
        .collect();
    assert!(!short.is_empty(), "no shortfall marked from a Lv6 starter");
    for (_, s) in &short {
        assert!(s
            .expected
            .iter()
            .any(|(p, prob)| matches!(p, GoalPredicate::CanBeat { .. }) && *prob < 0.9));
    }
    assert!(!plan.intents.iter().any(
        |s| matches!(&s.intent, Intent::Unsupported { reason, .. } if reason.contains("training"))
    ),);
    // The heal comes first here too.
    let heal = position(&plan, |i| matches!(i, Intent::Heal { .. }));
    let train = position(&plan, |i| matches!(i, Intent::Train { .. }));
    assert!(heal < train);
}

/// The first Switch run under §4.6: the heal at Mom's failed twice (a
/// tool bug) and the PC probe twice (no tool), and the planner then found
/// no plan at all, its only heal spot being the nearest. An infeasible
/// instance never takes the predicate's other establishers with it: the
/// heal moves to Viridian's Center (whose own trip, through Route 1's
/// grass, does not wait for a fit lead), and probes the toolbox can't
/// open are not planned.
#[test]
fn an_infeasible_heal_moves_to_the_next_heal_spot_and_unsupported_probes_stay_out() {
    let Some(f) = fixture() else { return };
    let (mut knowledge, pose) = fresh_game();
    knowledge
        .world
        .infeasible
        .insert("Heal(PalletTown_PlayersHouse_1F)".into());
    knowledge.world.infeasible.insert("Probe(pc boxes)".into());
    let planner = f.planner(PlanOptions::default());
    let plan = planner
        .plan(&GoalPredicate::badge(1), &knowledge, pose.clone())
        .unwrap();
    print(&plan, 20);
    assert!(plan.blocked().is_empty(), "{:?}", plan.blocked());
    let heal = position(
        &plan,
        |i| matches!(i, Intent::Heal { center } if center == "ViridianCity_PokemonCenter_1F"),
    );
    let train = position(&plan, |i| matches!(i, Intent::Train { .. }));
    assert!(heal < train, "heal at {heal}, train at {train}");
    assert!(!plan.intents.iter().any(
        |s| matches!(&s.intent, Intent::Heal { center } if center == "PalletTown_PlayersHouse_1F")
    ));
    assert!(!plan.intents.iter().any(|s| matches!(
        s.intent,
        Intent::Probe {
            fact: ProbeFact::PcBoxes
        }
    )));
    // The same with the toolbox's word instead of two failures.
    let (knowledge, pose) = fresh_game();
    let options = PlanOptions {
        supported_probes: Some(
            ["trainer_card", "bag_pocket", "fly_map", "pokedex"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        ),
        ..PlanOptions::default()
    };
    let planner = f.planner(options);
    let plan = planner
        .plan(&GoalPredicate::badge(1), &knowledge, pose)
        .unwrap();
    print(&plan, 20);
    assert!(!plan.intents.iter().any(|s| matches!(
        s.intent,
        Intent::Probe {
            fact: ProbeFact::PcBoxes
        }
    )));
    assert!(plan.intents.iter().any(
        |s| matches!(&s.intent, Intent::Heal { center } if center == "PalletTown_PlayersHouse_1F")
    ));
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

/// The story is derived, not scripted: with no methods at all, the League
/// is planned from a fresh game (just after the parcel) through the compiled
/// events alone. The gates the story puts in the way (the Route 23 guards,
/// the thirsty Saffron guards, the ghost of Pokémon Tower, Snorlax, the
/// locked Cinnabar gym, the Elite Four doors) come from the decomp's
/// scripts, and so does every subgoal that opens them: nothing is hinted.
/// Bounded by nodes, not the wall clock (the machine may be busy).
#[test]
fn game_clear_is_derived_without_methods() {
    let Some(f) = fixture() else { return };
    assert!(f.methods.methods.is_empty());
    let options = PlanOptions {
        budget_s: 1200.0,
        node_budget: 20_000,
        ..PlanOptions::default()
    };
    let planner = f.planner(options);
    let (mut knowledge, pose) = fresh_game();
    // The game starts with ₽3000: Bulbasaur can't learn Fly, so a Pidgey
    // is caught for it, with balls bought (earning money isn't planned).
    knowledge.money = pokebot_state::Knowledge::observed(3000, 1);
    let goal = parse_goal("flag FLAG_SYS_GAME_CLEAR").unwrap();
    let started = Instant::now();
    let plan = match planner.plan(&goal, &knowledge, pose.clone()) {
        Ok(p) => p,
        Err(pokebot_planner::PlanError::Budget {
            best_partial,
            nodes,
            ..
        }) => {
            if let Some(p) = best_partial {
                print(&p, 200);
            }
            panic!("no plan within {nodes} nodes");
        }
        Err(e) => panic!("{e}"),
    };
    println!("planned in {:.1} s", started.elapsed().as_secs_f64());
    print(&plan, 300);
    assert!(plan.blocked().is_empty(), "{:?}", plan.blocked());
    assert!(plan.intents.iter().all(|s| s.cost_s.is_finite()));

    let script = |name: &str| {
        let name = name.to_string();
        move |i: &Intent| matches!(i, Intent::RunScript { script, .. } if script.contains(&name))
    };
    let at = |name: &str| position(&plan, script(name));
    let n = plan.intents.len();
    // The eight badges, from the eight leaders.
    let gyms = [
        "PewterCity_Gym_EventScript_Brock",
        "CeruleanCity_Gym_EventScript_Misty",
        "VermilionCity_Gym_EventScript_LtSurge",
        "CeladonCity_Gym_EventScript_Erika",
        "FuchsiaCity_Gym_EventScript_Koga",
        "SaffronCity_Gym_EventScript_Sabrina",
        "CinnabarIsland_Gym_EventScript_Blaine",
        "ViridianCity_Gym_EventScript_Giovanni",
    ];
    let gym_at: Vec<usize> = gyms.iter().map(|g| at(g)).collect();
    for (g, i) in gyms.iter().zip(&gym_at) {
        assert!(*i < n, "{g} missing");
    }
    // The subgoals the gates need, discovered from the scripts.
    let teach = |hm: &str| {
        let hm = hm.to_string();
        position(
            &plan,
            move |i| matches!(i, Intent::Teach { hm: h, .. } if *h == hm),
        )
    };
    let cut = teach("ITEM_HM01");
    let surf = teach("ITEM_HM03");
    let strength = teach("ITEM_HM04");
    let tea = at("CeladonCity_Condominiums_1F_EventScript_TeaWoman");
    let scope = at("RocketHideout_B4F_EventScript_SilphScope");
    let fuji = at("PokemonTower_7F_EventScript_MrFuji");
    let flute = at("LavenderTown_VolunteerPokemonHouse_EventScript_MrFuji");
    let snorlax = position(
        &plan,
        |i| matches!(i, Intent::RunScript { script, .. } if script.starts_with("Route1") && script.ends_with("_EventScript_Snorlax")),
    );
    let key = at("PokemonMansion_B1F_EventScript_ItemSecretKey");
    let lorelei = at("PokemonLeague_LoreleisRoom_EventScript_Lorelei");
    let champion = at("PokemonLeague_ChampionsRoom_EventScript_EnterRoom");
    let hall = at("PokemonLeague_HallOfFame_EventScript_EnterRoom");
    for (what, i) in [
        ("Cut", cut),
        ("Surf", surf),
        ("Strength", strength),
        ("Tea", tea),
        ("Silph Scope", scope),
        ("Mr. Fuji", fuji),
        ("Poké Flute", flute),
        ("Snorlax", snorlax),
        ("Secret Key", key),
        ("Lorelei", lorelei),
        ("Champion", champion),
    ] {
        assert!(i < n, "{what} missing from the plan");
    }
    // Each before what needs it.
    assert!(cut < gym_at[2], "Cut before the tree to Lt. Surge");
    assert!(scope < fuji && fuji < flute && flute < snorlax);
    assert!(snorlax < gym_at[4], "Snorlax moved before Koga");
    assert!(tea < gym_at[5], "the guards' tea before Sabrina");
    assert!(
        surf < gym_at[6] && key < gym_at[6],
        "Surf and the key before Blaine"
    );
    assert!(
        gym_at.iter().all(|g| *g < lorelei),
        "all badges before the League"
    );
    assert!(strength < lorelei, "Strength for Victory Road");
    assert!(lorelei < champion && champion < hall && hall == n - 1);
    // Planning is a pure function of the knowledge.
    let again = planner.plan(&goal, &knowledge, pose).unwrap();
    assert_eq!(plan, again);
}

/// Right after the naming screens: no Pokémon, an empty bag, ¥3000, in
/// the bedroom, nothing of the story known beyond a new game's start.
fn new_game() -> (SavedKnowledge, Option<PlayerPose>) {
    checkpoint("new_game_state.json")
}

/// In Oak's lab after his starter scene (the scene vars tracked as the
/// goal loop's opening leaves them): the exit is closed until a ball is
/// taken.
fn starter_scene() -> (SavedKnowledge, Option<PlayerPose>) {
    checkpoint("starter_scene_state.json")
}

/// The opening is planned from the compiled events, not scripted: Oak's
/// trigger at the edge of Pallet Town carries the player off (a gate on
/// the way north while its scene is armed), so it runs before any walk to
/// Route 1; the parcel is the Viridian Mart's entry scene; the Pokédex is
/// Oak's parcel path.
#[test]
fn a_new_game_runs_oaks_trigger_before_leaving_pallet() {
    let Some(f) = fixture() else { return };
    let planner = f.planner(PlanOptions::default());
    let (knowledge, pose) = new_game();
    let goal = parse_goal("flag FLAG_SYS_POKEDEX_GET").unwrap();
    let plan = planner.plan(&goal, &knowledge, pose).expect("a plan");
    print(&plan, 20);
    assert!(plan.blocked().is_empty(), "{:?}", plan.blocked());
    let script = |name: &'static str| move |i: &Intent| matches!(i, Intent::RunScript { script, .. } if script.contains(name));
    let oak = position(&plan, script("PalletTown_EventScript_OakTrigger"));
    let mart = position(
        &plan,
        |i| matches!(i, Intent::Go { dest } if dest == "ViridianCity_Mart"),
    );
    let parcel = position(&plan, script("ViridianCity_Mart_EventScript_ParcelScene"));
    let dex = position(
        &plan,
        script("PalletTown_ProfessorOaksLab_EventScript_ProfOak"),
    );
    let n = plan.intents.len();
    assert!(oak < mart && mart < parcel && parcel < dex && dex == n - 1);
}

/// In the lab after Oak's scene: the exit trigger turns the player back
/// until a ball is taken, so the plan starts with a ball script path with
/// YES to the Pokémon and NO to the nickname (the naming screen is a
/// command the compiler doesn't model: dearer). Which ball comes from the
/// preference, not from the script's name.
#[test]
fn the_starter_comes_from_a_ball_on_the_table_by_preference() {
    let Some(f) = fixture() else { return };
    let (knowledge, pose) = starter_scene();
    let goal = parse_goal("flag FLAG_SYS_POKEDEX_GET").unwrap();
    for (prefer, ball) in [
        ("SPECIES_BULBASAUR", "BulbasaurBall"),
        ("SPECIES_SQUIRTLE", "SquirtleBall"),
        ("SPECIES_CHARMANDER", "CharmanderBall"),
    ] {
        let planner = f.planner(PlanOptions {
            prefer_species: vec![prefer.into()],
            ..PlanOptions::default()
        });
        let plan = planner
            .plan(&goal, &knowledge, pose.clone())
            .expect("a plan");
        print(&plan, 10);
        match &plan.intents[0].intent {
            Intent::RunScript {
                script, answers, ..
            } => {
                assert!(script.ends_with(ball), "{script} for {prefer}");
                assert_eq!(answers, &vec!["yes".to_string(), "no".to_string()]);
            }
            other => panic!("{other}"),
        }
    }
}

/// In Bill's Sea Cottage before helping him (the Switch checkpoint): the
/// PC's Cell Separator path needs `FLAG_TEMP_2`, which only talking to
/// Bill sets, and only for this visit. The plan talks to Bill, runs the PC
/// right after, then takes the S.S. Ticket.
#[test]
fn bill_is_asked_for_help_before_his_pc_runs() {
    let Some(f) = fixture() else { return };
    let planner = f.planner(PlanOptions {
        budget_s: 60.0,
        ..PlanOptions::default()
    });
    let (knowledge, pose) = checkpoint("sea_cottage_state.json");
    let goal = parse_goal("flag FLAG_GOT_SS_TICKET").unwrap();
    let plan = planner.plan(&goal, &knowledge, pose).unwrap();
    print(&plan, 20);
    let events = f.world.events().unwrap();
    let sets = |script: &str, path: usize, flag: &str| {
        events.script(script).unwrap().paths[path]
            .does
            .iter()
            .any(|e| matches!(e, pokebot_world::events::Effect::Set { set } if set == flag))
    };
    let scripts: Vec<(&str, usize)> = plan
        .intents
        .iter()
        .filter_map(|s| match &s.intent {
            Intent::RunScript { script, path, .. } => Some((script.as_str(), *path)),
            _ => None,
        })
        .collect();
    let bill = "Route25_SeaCottage_EventScript_Bill";
    let pc = "Route25_SeaCottage_EventScript_Computer";
    let separator = scripts
        .iter()
        .position(|&(s, p)| s == pc && sets(s, p, "FLAG_HELPED_BILL_IN_SEA_COTTAGE"))
        .expect("the Cell Separator runs");
    assert!(separator > 0, "{scripts:?}");
    let (before, p) = scripts[separator - 1];
    assert!(
        before == bill && sets(before, p, "FLAG_TEMP_2"),
        "{scripts:?}"
    );
    // Nothing between asking Bill and the PC: the flag lasts the visit.
    let at = |pred: &dyn Fn(&Intent) -> bool| position(&plan, pred);
    let ask = at(
        &|i| matches!(i, Intent::RunScript { script, path, .. } if script == bill && *path == p),
    );
    let run = at(&|i| matches!(i, Intent::RunScript { script, .. } if script == pc));
    assert_eq!(ask + 1, run);
    assert!(scripts[separator + 1..]
        .iter()
        .any(|&(s, p)| s == bill && sets(s, p, "FLAG_GOT_SS_TICKET")));
}

/// The Route 4 checkpoint with HM01 in the TM CASE, two badges and the
/// party replaced by `party` (species, level), each a copy of the lead.
fn with_hm01(party: &[(&str, u8)]) -> (SavedKnowledge, Option<PlayerPose>) {
    use pokebot_state::Knowledge;
    let (mut knowledge, pose) = route4();
    let lead = knowledge.party.value.as_ref().unwrap()[0].clone();
    let members = party
        .iter()
        .map(|(species, level)| {
            let mut m = lead.clone();
            m.species = Knowledge::observed((*species).to_owned(), 1);
            m.level = Knowledge::observed(*level, 1);
            m
        })
        .collect();
    knowledge.party = Knowledge::observed(members, 1);
    knowledge.bag.pockets.insert(
        Pocket::TmCase,
        Knowledge::observed(vec![("ITEM_HM01".to_owned(), 1)], 1),
    );
    for flag in ["FLAG_BADGE01_GET", "FLAG_BADGE02_GET"] {
        knowledge
            .world
            .flags
            .insert(flag.to_owned(), Knowledge::observed(true, 1));
    }
    (knowledge, pose)
}

/// Blastoise can't learn Cut ("NOT ABLE"): a species that can is caught
/// first and taught, never Blastoise.
#[test]
fn cut_for_a_party_that_cannot_learn_it_is_caught_first() {
    let Some(f) = fixture() else { return };
    let planner = f.planner(PlanOptions::default());
    let (knowledge, pose) = with_hm01(&[("SPECIES_BLASTOISE", 40)]);
    let goal = GoalPredicate::party_has_move("MOVE_CUT");
    let plan = planner.plan(&goal, &knowledge, pose).unwrap();
    print(&plan, 20);
    let teach = plan
        .intents
        .iter()
        .position(|s| matches!(&s.intent, Intent::Teach { hm, .. } if hm == "ITEM_HM01"))
        .expect("Cut is taught");
    let Intent::Teach { mon, .. } = &plan.intents[teach].intent else {
        unreachable!()
    };
    assert!(f.data.can_learn(mon, "ITEM_HM01"), "taught to {mon}");
    let caught = plan.intents[..teach]
        .iter()
        .any(|s| matches!(&s.intent, Intent::Catch { species, .. } if species == mon));
    assert!(caught, "{mon} is caught before it is taught");
    assert!(!plan
        .intents
        .iter()
        .any(|s| matches!(&s.intent, Intent::Teach { mon, .. } if mon == "SPECIES_BLASTOISE")));
}

/// Switch, HM03 from the Safari Zone: a full party where no member can
/// learn the HM. One that can is taken in at the PC for the weakest
/// member but the lead (one caught already; else caught first, which goes
/// to the box), then taught; never "the party is full".
#[test]
fn a_full_party_that_cannot_learn_an_hm_swaps_one_in_at_the_pc() {
    let Some(f) = fixture() else { return };
    let planner = f.planner(PlanOptions::default());
    let party = [
        ("SPECIES_BLASTOISE", 40),
        ("SPECIES_MAGIKARP", 7),
        ("SPECIES_ZUBAT", 12),
        ("SPECIES_PIDGEY", 9),
        ("SPECIES_GEODUDE", 10),
        ("SPECIES_SPEAROW", 11),
    ];
    for (species, _) in party {
        assert!(!f.data.can_learn(species, "ITEM_HM01"), "{species}");
    }
    let goal = GoalPredicate::party_has_move("MOVE_CUT");
    let swaps = |plan: &Plan| -> Vec<(String, String)> {
        plan.intents
            .iter()
            .filter_map(|s| match &s.intent {
                Intent::Swap {
                    deposit, withdraw, ..
                } => Some((deposit.clone(), withdraw.clone())),
                _ => None,
            })
            .collect()
    };
    // Caught already: swapped in for MAGIKARP (Lv7), then taught.
    let (mut knowledge, pose) = with_hm01(&party);
    knowledge.pokedex.caught.insert(
        "SPECIES_RATTATA".to_owned(),
        pokebot_state::Knowledge::observed(true, 1),
    );
    let plan = planner.plan(&goal, &knowledge, pose.clone()).unwrap();
    print(&plan, 20);
    assert_eq!(
        swaps(&plan),
        [("SPECIES_MAGIKARP".to_owned(), "SPECIES_RATTATA".to_owned())]
    );
    let swap = position(&plan, |i| matches!(i, Intent::Swap { .. }));
    let teach = position(
        &plan,
        |i| matches!(i, Intent::Teach { mon, .. } if mon == "SPECIES_RATTATA"),
    );
    assert!(swap < teach);
    // None caught (known): one is caught for it first.
    let (mut knowledge, pose) = with_hm01(&party);
    for species in f.data.species.keys() {
        if f.data.can_learn(species, "ITEM_HM01") {
            knowledge.pokedex.caught.insert(
                species.clone(),
                pokebot_state::Knowledge::observed(false, 1),
            );
        }
    }
    let plan = planner.plan(&goal, &knowledge, pose).unwrap();
    print(&plan, 20);
    let all = swaps(&plan);
    let [(deposit, withdraw)] = &all[..] else {
        panic!("one swap: {all:?}");
    };
    assert_eq!(deposit, "SPECIES_MAGIKARP");
    assert!(f.data.can_learn(withdraw, "ITEM_HM01"), "{withdraw}");
    let caught = position(
        &plan,
        |i| matches!(i, Intent::Catch { species, .. } if species == withdraw),
    );
    let swap = position(&plan, |i| matches!(i, Intent::Swap { .. }));
    assert!(caught < swap);
}

/// The catch tool doesn't throw Safari Balls: a SURF learner for the
/// Switch's party (HM03 in hand) is never planned as a catch in the Safari
/// Zone (it planned KANGASKHAN on SafariZone_East).
#[test]
fn a_safari_zone_species_is_not_planned_as_a_catch() {
    let Some(f) = fixture() else { return };
    let planner = f.planner(PlanOptions::default());
    let (mut knowledge, pose) = checkpoint("switch_silph_state.json");
    knowledge.bag.pockets.insert(
        Pocket::TmCase,
        pokebot_state::Knowledge::observed(vec![("ITEM_HM03".to_owned(), 1)], 1),
    );
    let goal = GoalPredicate::party_has_move("MOVE_SURF");
    let plan = planner.plan(&goal, &knowledge, pose).expect("a plan");
    print(&plan, 20);
    assert!(!plan
        .intents
        .iter()
        .any(|s| matches!(&s.intent, Intent::Catch { map, .. } if map.starts_with("SafariZone"))));
}

/// Switch, HM03 in hand: a full party none of which can learn SURF, no
/// grass catch in reach that can, but SILPH CO. 7F's LAPRAS still to be
/// had: the gift is taken (to the box, the party being full), swapped in
/// and taught, instead of "Unsupported".
#[test]
fn a_full_party_takes_a_gift_that_can_learn_the_hm() {
    use pokebot_state::Knowledge;
    let Some(f) = fixture() else { return };
    let planner = f.planner(PlanOptions::default());
    let (mut knowledge, pose) = checkpoint("switch_silph_state.json");
    let lead = knowledge.party.value.as_ref().unwrap()[0].clone();
    let party = [
        ("SPECIES_VENUSAUR", 59),
        ("SPECIES_PIDGEY", 7),
        ("SPECIES_PARAS", 8),
        ("SPECIES_DUGTRIO", 29),
        ("SPECIES_VENONAT", 24),
        ("SPECIES_DODRIO", 47),
    ];
    let members = party
        .iter()
        .map(|(species, level)| {
            assert!(!f.data.can_learn(species, "ITEM_HM03"), "{species}");
            let mut m = lead.clone();
            m.species = Knowledge::observed((*species).to_owned(), 1);
            m.level = Knowledge::observed(*level, 1);
            m
        })
        .collect();
    knowledge.party = Knowledge::observed(members, 1);
    knowledge.bag.pockets.insert(
        Pocket::TmCase,
        Knowledge::observed(vec![("ITEM_HM03".to_owned(), 1)], 1),
    );
    knowledge.world.flags.insert(
        "FLAG_GOT_LAPRAS_FROM_SILPH".to_owned(),
        Knowledge::observed(false, 1),
    );
    // The Pokédex audited: no SURF learner caught.
    for species in f.data.species.keys() {
        if f.data.can_learn(species, "ITEM_HM03") {
            knowledge
                .pokedex
                .caught
                .insert(species.clone(), Knowledge::observed(false, 1));
        }
    }
    let goal = GoalPredicate::party_has_move("MOVE_SURF");
    let plan = planner.plan(&goal, &knowledge, pose).expect("a plan");
    print(&plan, 20);
    assert!(
        !plan
            .intents
            .iter()
            .any(|s| matches!(&s.intent, Intent::Unsupported { .. })),
        "no Unsupported step"
    );
    let teach = position(
        &plan,
        |i| matches!(i, Intent::Teach { hm, .. } if hm == "ITEM_HM03"),
    );
    let Intent::Teach { mon, .. } = &plan.intents[teach].intent else {
        unreachable!()
    };
    assert!(f.data.can_learn(mon, "ITEM_HM03"), "taught to {mon}");
    let swap = position(
        &plan,
        |i| matches!(i, Intent::Swap { withdraw, .. } if withdraw == mon),
    );
    assert!(swap < teach);
    // Obtained first, not assumed: the gift's script runs before the swap.
    assert!(plan.intents[..swap].iter().any(
        |s| matches!(&s.intent, Intent::RunScript { script, .. } if script.contains("Lapras"))
    ));
}

/// With a member that can learn Cut, it is taught to that one directly.
#[test]
fn cut_goes_to_the_member_that_can_learn_it() {
    let Some(f) = fixture() else { return };
    let planner = f.planner(PlanOptions::default());
    let (knowledge, pose) = with_hm01(&[("SPECIES_BLASTOISE", 40), ("SPECIES_RATTATA", 6)]);
    let goal = GoalPredicate::party_has_move("MOVE_CUT");
    let plan = planner.plan(&goal, &knowledge, pose).unwrap();
    print(&plan, 20);
    let teaches: Vec<&Intent> = plan
        .intents
        .iter()
        .map(|s| &s.intent)
        .filter(|i| matches!(i, Intent::Teach { .. }))
        .collect();
    assert_eq!(
        teaches,
        [&Intent::Teach {
            hm: "ITEM_HM01".into(),
            mon: "SPECIES_RATTATA".into()
        }]
    );
    assert!(!plan
        .intents
        .iter()
        .any(|s| matches!(s.intent, Intent::Catch { .. })));
}

/// Fleet workers walked a weak lead into trainers whose sight spans the
/// way (Route 3, Viridian Forest, Mt. Moon's grunts) and whited out: a
/// walk through an unbeaten trainer's sight is its battle, so it needs
/// `CanBeat`. From Pewter with a weak IVYSAUR the way to Mt. Moon
/// prepares for Route 3's trainers; with them beaten it doesn't. (Known
/// gap: readiness may pick a training area behind the same sight lines,
/// here Mt. Moon; the areas it considers are readiness's to restrict.)
#[test]
fn a_walk_through_a_trainers_sight_prepares_for_its_battle() {
    use pokebot_state::Knowledge;
    let Some(f) = fixture() else { return };
    let planner = f.planner(PlanOptions::default());
    let (mut knowledge, pose) = pewter();
    if let Some(party) = knowledge.party.value.as_mut() {
        let lead = &mut party[0];
        lead.level = Knowledge::observed(8, 1);
        lead.hp = Knowledge::observed((27, 27), 1);
        for slot in lead.moves.iter_mut().skip(1) {
            *slot = None;
        }
    }
    let route3: Vec<String> = f.data.map_trainers["Route3"]
        .iter()
        .map(|t| t.trainer.clone())
        .collect();
    let with = |beaten: bool| {
        let mut k = knowledge.clone();
        for t in &route3 {
            k.world
                .flags
                .insert(t.clone(), Knowledge::observed(beaten, 1));
        }
        planner
            .plan(&parse_goal("at MtMoon_1F").unwrap(), &k, pose.clone())
            .unwrap()
    };
    let unbeaten = with(false);
    print(&unbeaten, 20);
    let beaten = with(true);
    print(&beaten, 20);
    let prepares = |p: &Plan| {
        p.intents
            .iter()
            .any(|s| matches!(s.intent, Intent::Train { .. } | Intent::Catch { .. }))
    };
    assert!(prepares(&unbeaten), "prepares for Route 3's trainers");
    assert!(!prepares(&beaten), "nothing to prepare for");
    // Where it prepares is short of them: Mt. Moon's grass lies past
    // Route 3's sight lines, so training there walks the weak lead into
    // the battles it trains for.
    for s in &unbeaten.intents {
        if let Intent::Train { map, .. } | Intent::Catch { map, .. } = &s.intent {
            assert!(
                !["MtMoon_1F", "Route3", "Route4"].contains(&map.as_str()),
                "prepares on {map}, past the trainers it prepares for"
            );
        }
    }
}

/// Fleet workers 2 and 5 had no money for the balls a catch needed and no
/// plan earned it. From Pewter with ₽100, having ₽1000 is planned as
/// prize battles (an Earn) against trainers known not to be beaten; Brock
/// (badge held, his flag unknown here) is no prize. With the money on hand
/// nothing is planned. (A purchase weighs this against other ways: for
/// Poké Balls, Oak's gift after the parcel is cheaper.)
#[test]
fn money_short_is_earned_from_trainers_not_yet_beaten() {
    use pokebot_state::Knowledge;
    let Some(f) = fixture() else { return };
    if f.data.trainer_class_money.classes.is_empty() {
        return;
    }
    let planner = f.planner(PlanOptions::default());
    let (mut knowledge, pose) = pewter();
    // Route 3's trainers are known not beaten (a new game knows them all).
    for t in &f.data.map_trainers["Route3"] {
        knowledge
            .world
            .flags
            .insert(t.trainer.clone(), Knowledge::observed(false, 1));
    }
    let goal = GoalPredicate::Money { money: 1000 };
    knowledge.money = Knowledge::observed(100, 1);
    let poor = planner.plan(&goal, &knowledge, pose.clone()).unwrap();
    print(&poor, 10);
    let Intent::Earn { money, trainers } = &poor.intents[0].intent else {
        panic!("{:?}", poor.intents[0].intent);
    };
    assert_eq!(*money, 1000);
    assert!(trainers.iter().all(|(t, _)| t != "TRAINER_LEADER_BROCK"));
    let prizes: u32 = trainers.iter().filter_map(|(t, _)| f.data.prize(t)).sum();
    assert!(prizes >= 900, "{prizes}");
    knowledge.money = Knowledge::observed(20_000, 1);
    let rich = planner.plan(&goal, &knowledge, pose).unwrap();
    assert!(rich.intents.is_empty(), "{:?}", rich.intents);
}

/// Fleet workers 2 and 5 looped between a mart with no money for balls and
/// the MANKEY that needed them. Readiness weighs earning first: a SPEAROW
/// (PECK, GROWL: walled by Brock's ROCK) facing Brock with ₽1800 (after
/// the Potions kept and the shiny reserve, one ball) beats Viridian
/// Forest's Bug Catchers for the second ball, since a recruit gets further
/// than training alone. (CHARMANDER once, while ROCK counted ×4 against
/// pure FIRE; it trains through METAL CLAW now.)
#[test]
fn readiness_short_of_ball_money_earns_it_first() {
    use pokebot_state::{Knowledge, MoveSlot};
    let Some(f) = fixture() else { return };
    if f.data.trainer_class_money.classes.is_empty() {
        return;
    }
    let planner = f.planner(PlanOptions::default());
    let (mut knowledge, pose) = pewter();
    let mut lead = knowledge.party.value.as_ref().unwrap()[0].clone();
    lead.species = Knowledge::observed("SPECIES_SPEAROW".to_owned(), 1);
    lead.level = Knowledge::observed(8, 1);
    lead.hp = Knowledge::observed((26, 26), 1);
    let slot = |mv: &str| {
        Some(MoveSlot {
            mv: Knowledge::observed(mv.to_owned(), 1),
            pp: Knowledge::observed((35, 35), 1),
        })
    };
    lead.moves = [slot("MOVE_PECK"), slot("MOVE_GROWL"), None, None];
    knowledge.party = Knowledge::observed(vec![lead], 1);
    for flag in ["FLAG_BADGE01_GET", "TRAINER_LEADER_BROCK"] {
        knowledge
            .world
            .flags
            .insert(flag.to_owned(), Knowledge::observed(false, 1));
    }
    for map in ["ViridianForest", "Route22", "Route2", "PewterCity_Gym"] {
        for t in f.data.map_trainers.get(map).into_iter().flatten() {
            knowledge
                .world
                .flags
                .insert(t.trainer.clone(), Knowledge::observed(false, 1));
        }
    }
    knowledge.money = Knowledge::observed(1800, 1);
    let plan = planner
        .plan(&parse_goal("badge 1").unwrap(), &knowledge, pose)
        .unwrap();
    print(&plan, 20);
    let earn = position(&plan, |i| matches!(i, Intent::Earn { .. }));
    let catch = position(&plan, |i| matches!(i, Intent::Catch { .. }));
    assert!(
        earn < catch && catch < plan.intents.len(),
        "earns, then catches"
    );
    let Intent::Earn { money, trainers } = &plan.intents[earn].intent else {
        unreachable!()
    };
    assert_eq!(*money, 2000);
    assert!(trainers.iter().all(|(t, _)| t.contains("BUG_CATCHER")));
}

/// The Switch in Rocket Hideout B4F (its checkpoint), Giovanni's room
/// behind the barrier that stays shut until TEAM_ROCKET_GRUNT_16 and _17
/// are beaten (`RocketHideout_B4F_OnLoad`). The plan beat Giovanni first:
/// "no path next to (19, 4)", 13 times, then no plan at all. The grunts
/// are on the lift's side of B4F, so the Lift Key comes first.
#[test]
fn giovanni_in_the_hideout_waits_for_the_barrier_grunts() {
    let Some(f) = fixture() else { return };
    let planner = f.planner(PlanOptions {
        budget_s: 60.0,
        ..PlanOptions::default()
    });
    let (knowledge, _) = checkpoint("rocket_hideout_state.json");
    let pose = Some(PlayerPose {
        map: "RocketHideout_B4F".into(),
        x: 11,
        y: 15,
    });
    // Giovanni the goal, and Giovanni what the Silph Scope's appearing
    // needs (the Switch's plan: Beat him first, the grunts after).
    for goal in [
        "flag TRAINER_BOSS_GIOVANNI",
        "item SILPH_SCOPE",
        "flag FLAG_SYS_GAME_CLEAR",
    ] {
        let plan = planner
            .plan(&parse_goal(goal).unwrap(), &knowledge, pose.clone())
            .unwrap();
        print(&plan, 30);
        barrier_grunts_first(&plan);
    }
}

/// The trainers (a `Beat`, or their object's script) in the plan's order:
/// the Lift Key, the barrier's grunts, then Giovanni.
fn barrier_grunts_first(plan: &Plan) {
    let beats = |who: &str, script_name: &str| {
        let who = who.to_string();
        let script_name = format!("RocketHideout_B4F_EventScript_{script_name}");
        move |i: &Intent| match i {
            Intent::Beat { trainer, .. } => *trainer == who,
            Intent::RunScript { script, .. } => *script == script_name,
            _ => false,
        }
    };
    let giovanni = position(plan, beats("TRAINER_BOSS_GIOVANNI", "Giovanni"));
    assert!(giovanni < plan.intents.len(), "Giovanni missing");
    // B4F's stairs land west of a wall: the grunts' side is the lift's.
    let key = position(
        plan,
        |i| matches!(i, Intent::RunScript { script, .. } if script == "RocketHideout_B4F_EventScript_LiftKey"),
    );
    for (grunt, script) in [
        ("TRAINER_TEAM_ROCKET_GRUNT_16", "Grunt2"),
        ("TRAINER_TEAM_ROCKET_GRUNT_17", "Grunt3"),
    ] {
        let at = position(plan, beats(grunt, script));
        assert!(
            key < at && at < giovanni,
            "Lift Key at {key}, {grunt} at {at}, Giovanni at {giovanni}"
        );
    }
}

/// Fleet worker 2's nugget farm stored CHARIZARD in the PC: the starter
/// var, inferred from the party only, went unknown, the Cerulean rival's
/// trigger was taken for another starter's, and no plan left Cerulean.
/// The starter's evolution implies what its base form does.
#[test]
fn an_evolved_starter_implies_the_starter_choice() {
    let Some(f) = fixture() else { return };
    let of = |species: &str| {
        pokebot_planner::goals::choices_of(&f.world, &f.data, &[species.to_string()])
    };
    let chose = of("SPECIES_CHARMANDER");
    assert!(chose.contains_key("VAR_STARTER_MON"), "{chose:?}");
    assert_eq!(of("SPECIES_CHARIZARD"), chose);
    assert_ne!(of("SPECIES_SQUIRTLE"), chose);
    assert!(of("SPECIES_PIDGEY").is_empty());
}

/// The Switch (four badges, FLY, Fuchsia visited) trained its Lv35
/// DODRIO for KOGA on Route 7, Lv17–22, because the areas offered were
/// the nearest by maps crossed, each healing at the Center nearest the
/// player. Areas are now offered by what training there takes: one with
/// stronger wild Pokémon, flown to, wins over Route 7, and each heals at
/// its own nearest Center.
#[test]
fn training_goes_where_it_pays_and_heals_at_the_areas_own_center() {
    let Some(f) = fixture() else { return };
    let planner = f.planner(PlanOptions {
        budget_s: 1200.0,
        ..PlanOptions::default()
    });
    let (knowledge, pose) = checkpoint("switch_route7_state.json");
    let areas = planner.training_areas(&knowledge, pose.clone());
    for a in &areas {
        println!(
            "{}: travel {:.1} min, heal {:.1} min",
            a.map, a.travel_minutes, a.heal_minutes
        );
    }
    let top = |map: &str| {
        f.data.wild[map]["land"]
            .slots
            .iter()
            .map(|s| s.max_level)
            .max()
            .unwrap()
    };
    let route7 = areas
        .iter()
        .find(|a| a.map == "Route7")
        .expect("Route 7, next door");
    // Each area's heal is its own Center's round trip: Route 7's (Celadon
    // next door) is the cheapest, not the one every area shares.
    assert!(areas
        .iter()
        .any(|a| a.heal_minutes > route7.heal_minutes + 0.1));
    assert!(areas.iter().all(|a| a.heal_minutes >= route7.heal_minutes));
    let goal = parse_goal("badge 5").unwrap();
    let plan = planner.plan(&goal, &knowledge, pose.clone()).unwrap();
    print(&plan, 10);
    let (map, trained) = plan
        .intents
        .iter()
        .find_map(|s| match &s.intent {
            Intent::Train { map, species, .. } if species == "SPECIES_DODRIO" => {
                Some((map.clone(), s.cost_s))
            }
            _ => None,
        })
        .expect("DODRIO trains for KOGA");
    assert_ne!(map, "Route7");
    assert!(top(&map) > top("Route7"), "{map}");
    // The same training on Route 7 alone takes longer.
    let belief = pokebot_planner::StateBelief::new(&knowledge, &f.data, pose);
    let request = pokebot_planner::Request {
        party: belief.party_members().unwrap(),
        targets: vec!["TRAINER_LEADER_KOGA".to_string()],
        areas: vec![route7.clone()],
        confidence: PlanOptions::default().confidence,
        money: 0,
        data: &f.data,
        handicap: 0,
    };
    let on_route7: f64 = pokebot_planner::plan_training(&request, 1)[0]
        .steps
        .iter()
        .map(|s| match s {
            pokebot_planner::PlanStep::Train { minutes, .. } => *minutes,
            _ => 0.0,
        })
        .sum();
    println!("{map}: {trained:.0} s, Route 7: {:.0} s", on_route7 * 60.0);
    assert!(trained < on_route7 * 60.0);
}

/// The user's "do some parts of the story to train at the same time":
/// readiness counts the experience of the battles fought before the
/// target, the trainers whose sight the walk there can't go round too.
/// From Pewter with a Lv10 IVYSAUR, Mt. Moon's grunt asks for training to
/// Lv14 when planned from here, and none once the walk to Mt. Moon has
/// fought Route 3's trainers; Misty asks for less of it.
#[test]
fn readiness_counts_the_trainers_fought_on_the_way() {
    use pokebot_state::{Knowledge, MoveSlot};
    let Some(f) = fixture() else { return };
    let planner = f.planner(PlanOptions::default());
    let (mut knowledge, pose) = pewter();
    if let Some(party) = knowledge.party.value.as_mut() {
        let lead = &mut party[0];
        assert_eq!(lead.species.value.as_deref(), Some("SPECIES_IVYSAUR"));
        lead.level = Knowledge::observed(10, 1);
        lead.hp = Knowledge::observed((33, 33), 1);
        let moves = f.data.default_moves("SPECIES_IVYSAUR", 10);
        for (i, slot) in lead.moves.iter_mut().enumerate() {
            *slot = moves.get(i).map(|mv| MoveSlot {
                mv: Knowledge::observed(mv.clone(), 1),
                pp: Knowledge::observed((20, 20), 1),
            });
        }
    }
    for t in &f.data.map_trainers["Route3"] {
        knowledge
            .world
            .flags
            .insert(t.trainer.clone(), Knowledge::observed(false, 1));
    }
    let to_mt_moon = pokebot_planner::PlannedIntent {
        intent: Intent::Go {
            dest: "MtMoon_1F".into(),
        },
        cost_s: 0.0,
        assumes: Vec::new(),
        unless: Vec::new(),
        note: None,
        route: Vec::new(),
        expected: Vec::new(),
    };
    let readiness = |trainer: &str, ahead: Vec<pokebot_planner::PlannedIntent>| {
        let n = ahead.len();
        let (steps, cost) = planner
            .readiness_after(&knowledge, pose.clone(), trainer, ahead)
            .expect("readiness");
        let trained: Option<u8> = steps
            .iter()
            .filter_map(|s| match &s.intent {
                Intent::Train { species, level, .. } if species == "SPECIES_IVYSAUR" => {
                    Some(*level)
                }
                _ => None,
            })
            .max();
        println!(
            "{trainer} after {n} steps: {cost:.0} s, {}",
            steps
                .iter()
                .map(|s| s.intent.to_string())
                .collect::<Vec<_>>()
                .join(" ; ")
        );
        (trained, cost)
    };
    let grunt = "TRAINER_TEAM_ROCKET_GRUNT";
    let (here, here_cost) = readiness(grunt, Vec::new());
    assert!(here.is_some(), "trains for the grunt from here");
    let (on, on_cost) = readiness(grunt, vec![to_mt_moon.clone()]);
    assert!(
        on.is_none() && on_cost < here_cost,
        "Route 3 trains it on the way"
    );
    let misty = "TRAINER_LEADER_MISTY";
    let (_, here_cost) = readiness(misty, Vec::new());
    let (_, on_cost) = readiness(misty, vec![to_mt_moon]);
    assert!(on_cost < here_cost);
}

/// Switch, Silph Co. 11F by the elevator, no Card Key: Giovanni's
/// trigger was planned as the next step, and "no path to (5, 15)". The
/// trigger's tile counts shut while armed, so the walk's needs come from
/// the tiles beside it: the Card Key and the doors it opens come first.
#[test]
fn silph_cos_giovanni_waits_for_the_card_key() {
    let Some(f) = fixture() else { return };
    let planner = f.planner(PlanOptions {
        budget_s: 120.0,
        ..PlanOptions::default()
    });
    let (knowledge, _) = checkpoint("switch_silph_state.json");
    let pose = Some(PlayerPose {
        map: "SilphCo_11F".into(),
        x: 13,
        y: 3,
    });
    let plan = planner
        .plan(
            &parse_goal("flag TRAINER_BOSS_GIOVANNI_2").unwrap(),
            &knowledge,
            pose,
        )
        .unwrap();
    print(&plan, 12);
    let at = |name: &str| {
        let name = name.to_string();
        position(
            &plan,
            move |i| matches!(i, Intent::RunScript { script, .. } if *script == name),
        )
    };
    let giovanni = at("SilphCo_11F_EventScript_GiovanniTriggerLeft")
        .min(at("SilphCo_11F_EventScript_GiovanniTriggerRight"));
    assert!(giovanni < plan.intents.len(), "Giovanni missing");
    let key = at("SilphCo_5F_EventScript_ItemCardKey");
    assert!(key < giovanni, "Card Key at {key}, Giovanni at {giovanni}");
}
