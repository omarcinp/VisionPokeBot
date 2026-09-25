//! The goal loop (spec §8) on a scripted planner and scripted tools:
//! replan on failure, `unless` skipping, the same failure twice marking
//! the intent infeasible, contradiction of an assumption, re-localisation
//! probes, stopping when the goal holds, giving up after `max_replans`;
//! the planner→tool intent conversion; and the planner honouring the
//! session's infeasible intents.
//!
//! Frames come from `captures/fixtures/emu-tools-overworld.png` (not
//! tracked) and the data from `data/world`; tests skip when either is
//! missing.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pokebot_agent::goal::{self, GoalOptions, GoalPlanner, GoalReport};
use pokebot_agent::tools::{
    Answer, BattlePlan, Dest, Intent, ProbeFact, Tool, ToolContext, ToolError, ToolOutcome, Toolbox,
};
use pokebot_agent::Executor;
use pokebot_core::{
    CapturedFrame, Controller, ControllerCommand, ControllerReceipt, RgbImage, VideoSource,
};
use pokebot_gamedata::GameData;
use pokebot_planner::{
    GoalPredicate, Intent as Planned, Plan, PlanError, PlanOptions, PlannedIntent, Planner,
    ProbeFact as PlannedFact,
};
use pokebot_runtime::{Devices, Runtime};
use pokebot_state::{GameEvent, PlayerPose, Pocket, SavedKnowledge};
use pokebot_video::{Normalizer, ViewportLocator};
use pokebot_vision::FireRedPerception;
use pokebot_world::route::{PlaceGraph, RouteParams};
use pokebot_world::World;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Data {
    world: Arc<World>,
    data: Arc<GameData>,
}

fn data() -> Option<Data> {
    let dir = root().join("data/world");
    let (Ok(world), Ok(data)) = (World::load(&dir), GameData::load(dir.join("gamedata.json")))
    else {
        eprintln!("skipping: no world data (run tools/world/build.sh)");
        return None;
    };
    if world.events().is_none() {
        eprintln!("skipping: no compiled events in data/world");
        return None;
    }
    Some(Data {
        world: Arc::new(world),
        data: Arc::new(data),
    })
}

fn overworld() -> Option<RgbImage> {
    let path = root().join("captures/fixtures/emu-tools-overworld.png");
    match pokebot_video::png::load(&path) {
        Ok(image) => Some(image),
        Err(_) => {
            eprintln!("skipping: no fixture {}", path.display());
            None
        }
    }
}

/// Repeats one frame forever.
struct StillVideo {
    frame: RgbImage,
    next: u64,
}

impl VideoSource for StillVideo {
    fn next_frame(&mut self) -> pokebot_core::Result<CapturedFrame> {
        self.next += 1;
        Ok(CapturedFrame {
            frame_id: self.next,
            delivered: self.next,
            captured_at: Instant::now(),
            image: self.frame.clone(),
        })
    }
}

#[derive(Default)]
struct NullController;

impl Controller for NullController {
    fn execute(&mut self, _command: ControllerCommand) -> pokebot_core::Result<ControllerReceipt> {
        Ok(ControllerReceipt {
            command_id: 1,
            issued_at: Instant::now(),
            input_duration: Duration::from_millis(0),
        })
    }

    fn is_idle(&self) -> pokebot_core::Result<bool> {
        Ok(true)
    }
}

/// Where the overworld fixture was taken.
const FIXTURE_POSE: (&str, i32, i32) = ("ViridianCity_PokemonCenter_1F", 7, 4);

fn runtime(d: &Data, frame: RgbImage) -> Runtime {
    let perception = FireRedPerception::with_world(Arc::clone(&d.world));
    let mut runtime = Runtime::with_perception(
        Devices {
            video: Box::new(StillVideo { frame, next: 0 }),
            controller: Box::new(NullController),
            normalizer: Normalizer::new(ViewportLocator::FullFrame),
            video_name: "scripted".into(),
            controller_name: "null".into(),
            persist_save: None,
        },
        perception,
    );
    runtime.set_pose_hint(PlayerPose {
        map: FIXTURE_POSE.0.into(),
        x: FIXTURE_POSE.1,
        y: FIXTURE_POSE.2,
    });
    runtime
}

/// What a scripted tool does for one invocation.
type Scripted = Result<Vec<GameEvent>, String>;

/// Serves every intent; answers from a queue per intent name (empty: ok
/// with nothing learned) and records what it was asked.
struct FakeTools {
    script: Arc<Mutex<HashMap<&'static str, VecDeque<Scripted>>>>,
    seen: Arc<Mutex<Vec<Intent>>>,
}

#[test]
fn urgent_event_suspends_and_resumes_the_same_running_tool() {
    use pokebot_agent::tools::{StepContext, ToolStep};
    use pokebot_agent::Decision;
    use pokebot_state::{Knowledge, Status};
    let (Some(d), Some(frame)) = (data(), overworld()) else {
        return;
    };
    let mut mon = pokebot_agent::party::starter_mon(&d.data, "SPECIES_BULBASAUR", 20);
    mon.hp = Knowledge::observed((100, 100), 0);
    mon.status = Knowledge::observed(Status::Healthy, 0);
    struct Work {
        mon: pokebot_state::PartyMon,
        wounded: bool,
    }
    impl ToolStep for Work {
        fn next(&mut self, ctx: &mut StepContext<'_>) -> Decision {
            if !self.wounded {
                self.wounded = true;
                let mut mon = self.mon.clone();
                mon.hp = Knowledge::observed((10, 100), ctx.observation.frame_id);
                ctx.events
                    .push(GameEvent::PartyAudited { members: vec![mon] });
                return Decision::Wait("health changed during work".into());
            }
            if ctx.state.party.value.as_ref().unwrap()[0].hp.value == Some((100, 100)) {
                Decision::Done("resumed original work".into())
            } else {
                Decision::Wait("work suspended until safe".into())
            }
        }
    }
    struct WorkTool {
        mon: pokebot_state::PartyMon,
        calls: Arc<Mutex<u32>>,
    }
    impl Tool for WorkTool {
        fn name(&self) -> &str {
            "Work"
        }
        fn serves(&self, i: &Intent) -> bool {
            matches!(i, Intent::Go { .. })
        }
        fn run(&mut self, _: &Intent, ctx: &mut ToolContext<'_>) -> ToolOutcome {
            *self.calls.lock().unwrap() += 1;
            ctx.drive(&mut Work {
                mon: self.mon.clone(),
                wounded: false,
            })
            .map(|_| ())
            .into()
        }
    }
    struct Recover {
        mon: pokebot_state::PartyMon,
        calls: Arc<Mutex<u32>>,
    }
    impl Tool for Recover {
        fn name(&self) -> &str {
            "Recover"
        }
        fn serves(&self, i: &Intent) -> bool {
            matches!(i, Intent::Heal { .. })
        }
        fn run(&mut self, _: &Intent, _: &mut ToolContext<'_>) -> ToolOutcome {
            *self.calls.lock().unwrap() += 1;
            ToolOutcome {
                result: Ok(()),
                learned: vec![GameEvent::PartyAudited {
                    members: vec![self.mon.clone()],
                }],
                pose: None,
            }
        }
    }
    let work_calls = Arc::new(Mutex::new(0));
    let recover_calls = Arc::new(Mutex::new(0));
    let toolbox = Toolbox::new(vec![
        Box::new(WorkTool {
            mon: mon.clone(),
            calls: Arc::clone(&work_calls),
        }),
        Box::new(Recover {
            mon: mon.clone(),
            calls: Arc::clone(&recover_calls),
        }),
    ]);
    let mut rt = runtime(&d, frame);
    let executor = Executor::default();
    let stop = AtomicBool::new(false);
    let mut ctx = ToolContext::new(
        &mut rt,
        &executor,
        Arc::clone(&d.world),
        Arc::clone(&d.data),
        &stop,
    )
    .with_toolbox(toolbox);
    ctx.scheduler.enabled = true;
    ctx.emit(GameEvent::PartyAudited { members: vec![mon] })
        .unwrap();
    let result = ctx.invoke(&Intent::Go {
        dest: pokebot_agent::tools::Dest::Map {
            map: FIXTURE_POSE.0.into(),
        },
    });
    assert!(result.is_ok(), "{:?}", result.result);
    assert_eq!(*work_calls.lock().unwrap(), 1, "original tool was retained");
    assert_eq!(
        *recover_calls.lock().unwrap(),
        1,
        "urgent recovery ran once"
    );
    assert!(ctx.scheduler.queue.is_empty());
}

impl Tool for FakeTools {
    fn name(&self) -> &str {
        "Fake"
    }

    fn serves(&self, _intent: &Intent) -> bool {
        true
    }

    fn run(&mut self, intent: &Intent, _ctx: &mut ToolContext<'_>) -> ToolOutcome {
        self.seen.lock().unwrap().push(intent.clone());
        let next = self
            .script
            .lock()
            .unwrap()
            .get_mut(intent.name())
            .and_then(VecDeque::pop_front);
        match next {
            None => ToolOutcome::ok(),
            Some(Ok(learned)) => ToolOutcome {
                result: Ok(()),
                learned,
                pose: None,
            },
            Some(Err(reason)) => ToolOutcome::failed(reason),
        }
    }
}

/// Returns its plans in order (the last one again when they run out),
/// or the alternative once `infeasible` names an intent.
struct FakePlanner {
    plans: Vec<Plan>,
    alternative: Option<Plan>,
    calls: Mutex<usize>,
    infeasible_seen: Mutex<Vec<BTreeSet<String>>>,
}

impl FakePlanner {
    fn new(plans: Vec<Plan>) -> Self {
        FakePlanner {
            plans,
            alternative: None,
            calls: Mutex::new(0),
            infeasible_seen: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

impl GoalPlanner for FakePlanner {
    fn plan(
        &self,
        _goal: &GoalPredicate,
        knowledge: &SavedKnowledge,
        _pose: Option<PlayerPose>,
    ) -> Result<Plan, PlanError> {
        let mut calls = self.calls.lock().unwrap();
        self.infeasible_seen
            .lock()
            .unwrap()
            .push(knowledge.world.infeasible.clone());
        let i = *calls;
        *calls += 1;
        if let Some(alt) = &self.alternative {
            if !knowledge.world.infeasible.is_empty() {
                return Ok(alt.clone());
            }
        }
        Ok(self.plans[i.min(self.plans.len() - 1)].clone())
    }
}

fn step(intent: Planned) -> PlannedIntent {
    PlannedIntent {
        intent,
        cost_s: 10.0,
        assumes: Vec::new(),
        unless: Vec::new(),
        note: None,
        route: Vec::new(),
        expected: Vec::new(),
    }
}

fn plan(steps: Vec<PlannedIntent>) -> Plan {
    Plan {
        assumes: steps.iter().flat_map(|s| s.assumes.clone()).collect(),
        intents: steps,
        cost_s: 10.0,
        belief_snapshot: 0,
    }
}

fn go(map: &str) -> Planned {
    Planned::Go {
        dest: map.to_string(),
    }
}

fn catch() -> Planned {
    Planned::Catch {
        species: "SPECIES_RATTATA".into(),
        map: "Route4".into(),
        slot: "land".into(),
        balls: 7,
    }
}

fn buy() -> Planned {
    Planned::Buy {
        item: "ITEM_POKE_BALL".into(),
        count: 5,
        map: "PewterCity_Mart".into(),
    }
}

fn probe_balls() -> Planned {
    Planned::Probe {
        fact: PlannedFact::BagPocket(Pocket::PokeBalls),
    }
}

fn caught() -> GameEvent {
    GameEvent::SpeciesCaught {
        species: "SPECIES_RATTATA".into(),
    }
}

fn goal() -> GoalPredicate {
    GoalPredicate::caught("SPECIES_RATTATA")
}

struct Harness {
    seen: Arc<Mutex<Vec<Intent>>>,
    script: Arc<Mutex<HashMap<&'static str, VecDeque<Scripted>>>>,
}

impl Harness {
    fn new(script: Vec<(&'static str, Vec<Scripted>)>) -> Self {
        let script: HashMap<&'static str, VecDeque<Scripted>> = script
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().collect()))
            .collect();
        Harness {
            seen: Arc::new(Mutex::new(Vec::new())),
            script: Arc::new(Mutex::new(script)),
        }
    }

    fn seen_names(&self) -> Vec<&'static str> {
        self.seen.lock().unwrap().iter().map(Intent::name).collect()
    }

    /// Runs the loop with the fake tools; the report and the state's
    /// infeasible set.
    fn run(
        &self,
        d: &Data,
        frame: RgbImage,
        planner: &dyn GoalPlanner,
        mut opts: GoalOptions,
        before: Vec<GameEvent>,
    ) -> (GoalReport, BTreeSet<String>) {
        let mut runtime = runtime(d, frame);
        // These tests exercise the goal executor, not startup auditing.
        // Give the scripted run a known healthy lead so its safety gate
        // leaves the fake plan under test in control.
        runtime
            .emit(GameEvent::PartyMonDerived {
                slot: 0,
                mon: Box::new(pokebot_state::PartyMon {
                    species: pokebot_state::Knowledge::derived("SPECIES_BULBASAUR".into(), 0),
                    level: pokebot_state::Knowledge::derived(6, 0),
                    hp: pokebot_state::Knowledge::derived((22, 22), 0),
                    ..Default::default()
                }),
            })
            .unwrap();
        for e in before {
            runtime.emit(e).unwrap();
        }
        let executor = Executor::default();
        let stop = AtomicBool::new(false);
        let mut ctx = ToolContext::new(
            &mut runtime,
            &executor,
            Arc::clone(&d.world),
            Arc::clone(&d.data),
            &stop,
        )
        .with_toolbox(Toolbox::new(vec![Box::new(FakeTools {
            script: Arc::clone(&self.script),
            seen: Arc::clone(&self.seen),
        })]));
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&statuses);
        opts.on_status = Some(Box::new(move |s| sink.lock().unwrap().push(s.clone())));
        let report = goal::run(&goal(), &mut ctx, planner, opts).expect("no device error");
        assert!(
            !statuses.lock().unwrap().is_empty(),
            "the status sink is told about the run"
        );
        let infeasible = ctx.state().world.infeasible.clone();
        (report, infeasible)
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pokebot-goal-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn urgent_health_is_healed_before_the_campaign_plan() {
    let (Some(d), Some(frame)) = (data(), overworld()) else {
        return;
    };
    let planner = FakePlanner::new(vec![
        plan(vec![step(Planned::Heal {
            center: "PalletTown_PlayersHouse_1F".into(),
        })]),
        plan(vec![step(catch())]),
    ]);
    let h = Harness::new(vec![
        ("Heal", vec![Ok(vec![GameEvent::Healed])]),
        ("Catch", vec![Ok(vec![caught()])]),
    ]);
    let low_hp = GameEvent::PartyObserved {
        slot: 0,
        species: None,
        nickname: None,
        level: None,
        hp: Some((10, 22)),
        status: None,
        held_item: None,
    };
    let (report, _) = h.run(&d, frame, &planner, GoalOptions::default(), vec![low_hp]);
    assert!(report.satisfied, "{report:?}");
    assert_eq!(h.seen_names(), vec!["Heal", "Catch"]);
    assert_eq!(report.plans.len(), 2);
    assert!(report.plans[0].reason.starts_with("urgent health"));
}

#[test]
fn a_failed_step_replans_and_the_loop_stops_when_the_goal_holds() {
    let (Some(d), Some(frame)) = (data(), overworld()) else {
        return;
    };
    let planner = FakePlanner::new(vec![
        plan(vec![step(go("Route4")), step(catch())]),
        plan(vec![step(catch())]),
    ]);
    let h = Harness::new(vec![
        ("Go", vec![Err("blocked by a trainer".into())]),
        ("Catch", vec![Ok(vec![caught()])]),
    ]);
    let session = temp_dir("replan");
    let opts = GoalOptions {
        session_dir: Some(session.clone()),
        ..GoalOptions::default()
    };
    let (report, _) = h.run(&d, frame, &planner, opts, vec![]);
    assert!(report.satisfied, "{report:?}");
    assert_eq!(report.outcome, "goal satisfied");
    assert_eq!(planner.calls(), 2);
    assert_eq!(report.plans.len(), 2);
    assert_eq!(report.plans[0].reason, "start");
    assert!(
        report.plans[1]
            .reason
            .contains("Go(Route4) failed: blocked by a trainer"),
        "{}",
        report.plans[1].reason
    );
    assert_eq!(report.steps_run, 1);
    assert_eq!(report.failures.len(), 1);
    assert_eq!(h.seen_names(), vec!["Go", "Catch"]);
    assert!(report.learned.contains(&caught()));
    // plan.jsonl: one line per plan with its reason.
    let lines: Vec<String> = std::fs::read_to_string(session.join("plan.jsonl"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(lines.len(), 2);
    let second: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(second["plan_no"], 2);
    assert!(second["reason"].as_str().unwrap().contains("failed"));
    assert_eq!(second["plan"]["intents"].as_array().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&session);
}

#[test]
fn a_step_whose_unless_facts_hold_is_skipped() {
    let (Some(d), Some(frame)) = (data(), overworld()) else {
        return;
    };
    let mut buy = step(buy());
    buy.unless = vec![GoalPredicate::has_item("ITEM_POKE_BALL", 5)];
    let planner = FakePlanner::new(vec![plan(vec![step(probe_balls()), buy, step(catch())])]);
    let h = Harness::new(vec![
        (
            "Probe",
            vec![Ok(vec![GameEvent::PocketObserved {
                pocket: Pocket::PokeBalls,
                items: vec![("ITEM_POKE_BALL".into(), 12)],
            }])],
        ),
        ("Catch", vec![Ok(vec![caught()])]),
    ]);
    let (report, _) = h.run(&d, frame, &planner, GoalOptions::default(), vec![]);
    assert!(report.satisfied, "{report:?}");
    assert_eq!(h.seen_names(), vec!["Probe", "Catch"], "Buy is skipped");
    assert_eq!(report.steps_skipped, 1);
    assert_eq!(report.steps_run, 2);
    assert_eq!(planner.calls(), 1);
}

#[test]
fn the_same_failure_twice_marks_the_intent_infeasible_and_the_planner_avoids_it() {
    let (Some(d), Some(frame)) = (data(), overworld()) else {
        return;
    };
    let mut planner = FakePlanner::new(vec![plan(vec![step(buy()), step(catch())])]);
    planner.alternative = Some(plan(vec![step(catch())]));
    let h = Harness::new(vec![
        (
            "Buy",
            vec![
                Err("no mart selling ITEM_POKE_BALL found".into()),
                Err("no mart selling ITEM_POKE_BALL found".into()),
            ],
        ),
        ("Catch", vec![Ok(vec![caught()])]),
    ]);
    let (report, infeasible) = h.run(&d, frame, &planner, GoalOptions::default(), vec![]);
    assert!(report.satisfied, "{report:?}");
    let key = buy().to_string();
    assert_eq!(report.infeasible, vec![key.clone()]);
    assert!(infeasible.contains(&key), "{infeasible:?}");
    assert_eq!(planner.calls(), 3);
    // The third plan was made with the infeasible intent in the knowledge.
    let seen = planner.infeasible_seen.lock().unwrap();
    assert!(seen[0].is_empty() && seen[1].is_empty());
    assert!(seen[2].contains(&key));
    assert_eq!(h.seen_names(), vec!["Buy", "Buy", "Catch"]);
}

#[test]
fn a_contradicted_assumption_replans_before_the_step_runs() {
    let (Some(d), Some(frame)) = (data(), overworld()) else {
        return;
    };
    let mut walk = step(go("Route4"));
    walk.assumes = vec![GoalPredicate::flag("FLAG_HIDE_ROUTE4_GUARD", true)];
    let planner = FakePlanner::new(vec![
        plan(vec![step(probe_balls()), walk, step(catch())]),
        plan(vec![step(catch())]),
    ]);
    let h = Harness::new(vec![
        (
            "Probe",
            vec![Ok(vec![GameEvent::FlagObserved {
                flag: "FLAG_HIDE_ROUTE4_GUARD".into(),
                value: false,
            }])],
        ),
        ("Catch", vec![Ok(vec![caught()])]),
    ]);
    let (report, _) = h.run(&d, frame, &planner, GoalOptions::default(), vec![]);
    assert!(report.satisfied, "{report:?}");
    assert_eq!(report.plans.len(), 2);
    assert!(
        report.plans[1].reason.starts_with("belief_contradiction"),
        "{}",
        report.plans[1].reason
    );
    assert_eq!(h.seen_names(), vec!["Probe", "Catch"], "Go never ran");
    assert!(report.failures.is_empty());
}

#[test]
fn three_different_failures_at_one_pose_probe_the_plans_assumptions() {
    let (Some(d), Some(frame)) = (data(), overworld()) else {
        return;
    };
    let assumes = vec![GoalPredicate::has_item("ITEM_POKE_BALL", 1)];
    let with = |intent: Planned| {
        let mut s = step(intent);
        s.assumes = assumes.clone();
        s
    };
    let planner = FakePlanner::new(vec![
        plan(vec![with(go("Route4"))]),
        plan(vec![with(buy())]),
        plan(vec![with(Planned::Talk {
            map: "PewterCity".into(),
            object: 1,
            answers: vec![],
        })]),
        plan(vec![step(catch())]),
    ]);
    let h = Harness::new(vec![
        ("Go", vec![Err("a".into())]),
        ("Buy", vec![Err("b".into())]),
        ("Talk", vec![Err("c".into())]),
        ("Catch", vec![Ok(vec![caught()])]),
    ]);
    let (report, infeasible) = h.run(&d, frame, &planner, GoalOptions::default(), vec![]);
    assert!(report.satisfied, "{report:?}");
    assert!(infeasible.is_empty(), "different failures: none infeasible");
    let seen = h.seen.lock().unwrap().clone();
    let names: Vec<&str> = seen.iter().map(Intent::name).collect();
    assert_eq!(
        names,
        vec!["Go", "Buy", "Talk", "Probe", "Catch"],
        "{seen:?}"
    );
    assert_eq!(
        seen[3],
        Intent::Probe {
            fact: ProbeFact::Pocket {
                pocket: Pocket::PokeBalls
            }
        }
    );
}

#[test]
fn a_goal_that_already_holds_needs_no_plan() {
    let (Some(d), Some(frame)) = (data(), overworld()) else {
        return;
    };
    let planner = FakePlanner::new(vec![plan(vec![step(catch())])]);
    let h = Harness::new(vec![]);
    let (report, _) = h.run(&d, frame, &planner, GoalOptions::default(), vec![caught()]);
    assert!(report.satisfied);
    assert_eq!(planner.calls(), 0);
    assert!(report.plans.is_empty());
    assert!(h.seen_names().is_empty());
}

#[test]
fn the_loop_gives_up_after_max_replans_with_the_last_plan() {
    let (Some(d), Some(frame)) = (data(), overworld()) else {
        return;
    };
    let planner = FakePlanner::new(vec![plan(vec![step(go("Route4")), step(catch())])]);
    let h = Harness::new(vec![(
        "Go",
        vec![
            Err("a".into()),
            Err("b".into()),
            Err("c".into()),
            Err("d".into()),
        ],
    )]);
    let opts = GoalOptions {
        max_replans: 2,
        ..GoalOptions::default()
    };
    let (report, _) = h.run(&d, frame, &planner, opts, vec![]);
    assert!(!report.satisfied);
    assert!(
        report.outcome.starts_with("out of replans"),
        "{}",
        report.outcome
    );
    assert_eq!(report.plans.len(), 3, "the first plan and two replans");
    assert_eq!(planner.calls(), 3);
    assert_eq!(report.failures.len(), 3);
    assert_eq!(report.last_plan().unwrap().intents.len(), 2);
}

#[test]
fn planner_intents_convert_to_tool_intents() {
    let Some(d) = data() else { return };
    let world = Some(&*d.world);
    let conv = |p: &Planned| Intent::from_planned(p, world);
    assert_eq!(
        conv(&go("Route4")).unwrap(),
        Intent::Go {
            dest: Dest::Map {
                map: "Route4".into()
            }
        }
    );
    assert_eq!(
        conv(&catch()).unwrap(),
        Intent::Catch {
            species: "SPECIES_RATTATA".into(),
            map: Some("Route4".into()),
        }
    );
    assert_eq!(
        conv(&Planned::Train {
            map: "Route3".into(),
            species: "SPECIES_IVYSAUR".into(),
            level: 22,
        })
        .unwrap(),
        Intent::Train {
            map: "Route3".into(),
            species: "SPECIES_IVYSAUR".into(),
            level: 22,
        }
    );
    assert_eq!(
        conv(&buy()).unwrap(),
        Intent::Buy {
            item: "ITEM_POKE_BALL".into(),
            count: 5
        }
    );
    assert_eq!(
        conv(&probe_balls()).unwrap(),
        Intent::Probe {
            fact: ProbeFact::Pocket {
                pocket: Pocket::PokeBalls
            }
        }
    );
    assert_eq!(
        conv(&Planned::Probe {
            fact: PlannedFact::TrainerCard
        })
        .unwrap(),
        Intent::Probe {
            fact: ProbeFact::TrainerCard
        }
    );
    assert_eq!(
        conv(&Planned::Heal {
            center: "PewterCity_PokemonCenter_1F".into()
        })
        .unwrap(),
        Intent::Heal {
            center: Some("PewterCity_PokemonCenter_1F".into())
        }
    );
    assert_eq!(
        conv(&Planned::Battle {
            policy: "fight".into()
        })
        .unwrap(),
        Intent::Battle {
            policy: BattlePlan::Fight
        }
    );
    assert_eq!(
        conv(&Planned::RunScript {
            script: "S".into(),
            path: 2,
            answers: vec!["YES".into(), "choice=1".into(), "no".into()],
            map: "M".into(),
        })
        .unwrap(),
        Intent::RunScript {
            script: "S".into(),
            path: Some(2),
            answers: vec![Answer::Yes, Answer::No],
        }
    );
    assert_eq!(conv(&Planned::Save).unwrap(), Intent::Save);
    assert_eq!(conv(&Planned::Unstick).unwrap(), Intent::Unstick);
    assert_eq!(
        conv(&Planned::Teach {
            hm: "ITEM_HM01".into(),
            mon: "SPECIES_IVYSAUR".into()
        })
        .unwrap(),
        Intent::Teach {
            item: "ITEM_HM01".into(),
            member: "SPECIES_IVYSAUR".into()
        }
    );
    assert_eq!(
        conv(&Planned::Probe {
            fact: PlannedFact::Party
        })
        .unwrap(),
        Intent::Probe {
            fact: ProbeFact::Party
        }
    );
    assert!(matches!(
        conv(&Planned::Unsupported {
            reason: "link trade".into(),
            establishes: goal()
        }),
        Err(ToolError::Unsupported(_))
    ));
    // Beat: the object on the map whose script fights the trainer.
    let events = d.world.events().unwrap();
    let (object, trainer) = events
        .objects
        .iter()
        .filter(|o| o.map == "Route3")
        .find_map(|o| {
            let script = events.script(o.script.as_deref()?)?;
            script
                .paths
                .iter()
                .flat_map(|p| p.does.iter())
                .find_map(|e| match e {
                    pokebot_world::events::Effect::Battle { battle, .. } => {
                        Some((o.local_id, battle.clone()))
                    }
                    _ => None,
                })
        })
        .expect("a trainer on Route 3");
    let beat = Planned::Beat {
        trainer: trainer.clone(),
        map: "Route3".into(),
    };
    assert_eq!(
        conv(&beat).unwrap(),
        Intent::Beat {
            trainer: trainer.clone(),
            map: "Route3".into(),
            object,
        }
    );
    assert!(matches!(
        Intent::try_from(&beat),
        Err(ToolError::Unsupported(_))
    ));
    assert!(matches!(
        conv(&Planned::Beat {
            trainer: "TRAINER_NOBODY".into(),
            map: "Route3".into()
        }),
        Err(ToolError::Unsupported(_))
    ));
}

#[test]
fn the_planner_leaves_infeasible_intents_to_other_branches() {
    let Some(d) = data() else { return };
    let graph = PlaceGraph::build(&d.world, RouteParams::default());
    let methods = pokebot_planner::Methods::default();
    let obtain = pokebot_planner::Obtain::load(root().join("data/world")).ok();
    let planner = Planner::new(
        &d.world,
        &graph,
        &d.data,
        obtain.as_ref(),
        None,
        &methods,
        PlanOptions::default(),
    );
    let pose = Some(PlayerPose {
        map: "Route4_PokemonCenter_1F".into(),
        x: 7,
        y: 4,
    });
    let mut knowledge = SavedKnowledge::default();
    knowledge.bag.pockets.insert(
        Pocket::PokeBalls,
        pokebot_state::Knowledge::observed(vec![("ITEM_POKE_BALL".into(), 15)], 1),
    );
    let first = GoalPlanner::plan(&planner, &goal(), &knowledge, pose.clone()).expect("a plan");
    let catch = first
        .intents
        .iter()
        .find(|s| matches!(s.intent, Planned::Catch { .. }))
        .expect("a catch")
        .intent
        .clone();
    knowledge.world.infeasible.insert(catch.to_string());
    let second = GoalPlanner::plan(&planner, &goal(), &knowledge, pose).expect("another plan");
    assert!(
        second.intents.iter().all(|s| s.intent != catch),
        "{second:?}"
    );
    assert!(second
        .intents
        .iter()
        .any(|s| matches!(s.intent, Planned::Catch { .. })));
}

/// Flash-6: `Go(MtMoon_B2F)` failed twice with the same "looping" reason
/// (Youngster Josh had walked up to the player), was marked infeasible,
/// and no plan could reach HM05 any more. A leg's failure is about one
/// tile the navigator learns; the destination stays plannable.
#[test]
fn a_go_failing_twice_stays_plannable() {
    let (Some(d), Some(frame)) = (data(), overworld()) else {
        return;
    };
    let mut planner = FakePlanner::new(vec![plan(vec![step(go("Route4")), step(catch())])]);
    planner.alternative = Some(plan(vec![step(catch())]));
    let looping = "looping at MtMoon_1F (16, 17): 4 acts from the same tile";
    let h = Harness::new(vec![
        (
            "Go",
            vec![Err(looping.into()), Err(looping.into()), Ok(vec![])],
        ),
        ("Catch", vec![Ok(vec![caught()])]),
    ]);
    let (report, infeasible) = h.run(&d, frame, &planner, GoalOptions::default(), vec![]);
    assert!(report.satisfied, "{report:?}");
    assert!(report.infeasible.is_empty(), "{:?}", report.infeasible);
    assert!(infeasible.is_empty(), "{infeasible:?}");
    assert_eq!(h.seen_names(), vec!["Go", "Go", "Go", "Catch"]);
    assert_eq!(planner.calls(), 3);
}

/// A Pokémon fainting ends the run with a `fainted` outcome instead of a
/// replan (the session reloads the last save), and marks nothing
/// infeasible.
#[test]
fn a_faint_ends_the_run_for_the_session_to_reload() {
    let (Some(d), Some(frame)) = (data(), overworld()) else {
        return;
    };
    let planner = FakePlanner::new(vec![plan(vec![step(go("Route4")), step(catch())])]);
    let h = Harness::new(vec![
        ("Go", vec![Err("our Pokémon fainted (BULBASAUR)".into())]),
        ("Catch", vec![Ok(vec![caught()])]),
    ]);
    let (report, infeasible) = h.run(&d, frame, &planner, GoalOptions::default(), vec![]);
    assert!(!report.satisfied);
    assert!(
        report.outcome.starts_with("fainted: "),
        "{}",
        report.outcome
    );
    assert!(infeasible.is_empty());
    assert_eq!(h.seen_names(), vec!["Go"]);
    assert_eq!(planner.calls(), 1);
}
