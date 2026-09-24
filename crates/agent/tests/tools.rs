//! Tools (spec §7): the interrupt wrapper on scripted frames, dialogue
//! recognition against the compiled game text, effect → event translation,
//! and, ignored, a `Go` + `Heal` smoke on the virtual console.
//!
//! Frames come from `captures/fixtures/emu-tools-*.png` (normalized
//! 240×160, not tracked); the data from `data/world`. Tests skip when
//! either is missing.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pokebot_agent::tools::effects::{path_events, translate, LabelIndex, Skipped};
use pokebot_agent::tools::{
    dialogue, identify, Answer, BattlePlan, Dest, Expects, Intent, StepContext, Tool, ToolContext,
    ToolOutcome, ToolStep, Toolbox,
};
use pokebot_agent::{Action, Decision, Executor, Expectation, InputKind, Outcome, Syncer};
use pokebot_core::{
    CapturedFrame, Controller, ControllerCommand, ControllerReceipt, RgbImage, VideoSource,
};
use pokebot_gamedata::GameData;
use pokebot_runtime::{Devices, Runtime};
use pokebot_state::{
    GameEvent, GameState, Knowledge, Observation, Observed, PlayerPose, Pocket, ScreenState,
};
use pokebot_video::{Normalizer, ViewportLocator};
use pokebot_vision::FireRedPerception;
use pokebot_world::events::{Effect, VarChange};
use pokebot_world::World;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture(name: &str) -> Option<RgbImage> {
    let path = root().join("captures/fixtures").join(name);
    match pokebot_video::png::load(&path) {
        Ok(image) => Some(image),
        Err(_) => {
            eprintln!("skipping: no fixture {}", path.display());
            None
        }
    }
}

struct Data {
    world: Arc<World>,
    data: Arc<GameData>,
    font: Arc<pokebot_vision::text::Font>,
}

fn data() -> Option<Data> {
    let dir = root().join("data/world");
    let (Ok(world), Ok(data), Ok(font)) = (
        World::load(&dir),
        GameData::load(dir.join("gamedata.json")),
        pokebot_vision::text::Font::load(dir.join("font_normal.json")),
    ) else {
        eprintln!("skipping: no world data (run tools/world/build.sh)");
        return None;
    };
    if world.events().is_none() || world.dialogue().is_none() {
        eprintln!("skipping: no compiled events/dialogue in data/world");
        return None;
    }
    Some(Data {
        world: Arc::new(world),
        data: Arc::new(data),
        font: Arc::new(font),
    })
}

/// Plays a list of frames, then repeats the last one forever.
struct ScriptedVideo {
    frames: Vec<RgbImage>,
    next: usize,
}

impl VideoSource for ScriptedVideo {
    fn next_frame(&mut self) -> pokebot_core::Result<CapturedFrame> {
        let i = self.next.min(self.frames.len() - 1);
        let frame = CapturedFrame {
            frame_id: self.next as u64,
            captured_at: Instant::now(),
            image: self.frames[i].clone(),
        };
        self.next += 1;
        Ok(frame)
    }
}

/// Records commands; busy for a few polls after each (so an interruptible
/// hold can be cut short like on a real device).
#[derive(Default)]
struct FakeController {
    commands: Arc<Mutex<Vec<ControllerCommand>>>,
    busy_polls: std::cell::Cell<u32>,
}

impl Controller for FakeController {
    fn execute(&mut self, command: ControllerCommand) -> pokebot_core::Result<ControllerReceipt> {
        let busy = matches!(
            command,
            ControllerCommand::Hold { .. } | ControllerCommand::Sequence(_)
        );
        self.busy_polls.set(if busy { 3 } else { 0 });
        let id = {
            let mut c = self.commands.lock().unwrap();
            c.push(command);
            c.len() as u64
        };
        Ok(ControllerReceipt {
            command_id: id,
            issued_at: Instant::now(),
            input_duration: Duration::from_millis(0),
        })
    }

    fn is_idle(&self) -> pokebot_core::Result<bool> {
        let left = self.busy_polls.get();
        if left > 0 {
            self.busy_polls.set(left - 1);
            return Ok(false);
        }
        Ok(true)
    }
}

/// A tool that records the intents it was invoked with and succeeds.
struct Recorder {
    seen: Arc<Mutex<Vec<Intent>>>,
}

impl Tool for Recorder {
    fn name(&self) -> &str {
        "Recorder"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Battle { .. } | Intent::Unstick)
    }

    fn run(&mut self, intent: &Intent, _ctx: &mut ToolContext<'_>) -> ToolOutcome {
        self.seen.lock().unwrap().push(intent.clone());
        ToolOutcome::ok()
    }
}

fn runtime(d: &Data, frames: Vec<RgbImage>) -> (Runtime, Arc<Mutex<Vec<ControllerCommand>>>) {
    let controller = FakeController::default();
    let commands = Arc::clone(&controller.commands);
    let perception =
        FireRedPerception::with_world(Arc::clone(&d.world)).with_font(Arc::clone(&d.font));
    let runtime = Runtime::with_perception(
        Devices {
            video: Box::new(ScriptedVideo { frames, next: 0 }),
            controller: Box::new(controller),
            normalizer: Normalizer::new(ViewportLocator::FullFrame),
            video_name: "scripted".into(),
            controller_name: "fake".into(),
            persist_save: None,
        },
        perception,
    );
    (runtime, commands)
}

fn pose(map: &str, x: i32, y: i32) -> PlayerPose {
    PlayerPose {
        map: map.into(),
        x,
        y,
    }
}

/// Where the overworld fixture was taken.
const FIXTURE_POSE: (&str, i32, i32) = ("ViridianCity_PokemonCenter_1F", 7, 4);

#[test]
fn a_battle_mid_walk_interrupts_the_action_and_runs_the_battle_tool() {
    let Some(d) = data() else { return };
    let (Some(overworld), Some(battle)) = (
        fixture("emu-tools-overworld.png"),
        fixture("emu-tools-battle-command.png"),
    ) else {
        return;
    };
    let frames = vec![overworld, battle.clone(), battle.clone(), battle];
    let (mut runtime, commands) = runtime(&d, frames);
    runtime.set_pose_hint(pose(FIXTURE_POSE.0, FIXTURE_POSE.1, FIXTURE_POSE.2));
    let executor = Executor {
        syncer: Some(Arc::new(Mutex::new(Syncer::new("emulator")))),
        ..Executor::default()
    };
    let stop = AtomicBool::new(false);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut ctx = ToolContext::new(
        &mut runtime,
        &executor,
        Arc::clone(&d.world),
        Arc::clone(&d.data),
        &stop,
    )
    .with_toolbox(Toolbox::new(vec![Box::new(Recorder {
        seen: Arc::clone(&seen),
    })]));
    // The first frame locates the player: a map visited.
    let o = ctx.observe().expect("observe");
    let at = o.player.as_ref().map(|p| p.pose.clone());
    assert_eq!(
        at,
        Some(pose(FIXTURE_POSE.0, FIXTURE_POSE.1, FIXTURE_POSE.2))
    );
    let walk = Action::new(
        "walk right 3",
        vec![ControllerCommand::Hold {
            buttons: [pokebot_core::Button::Right].into_iter().collect(),
            duration: Duration::from_millis(700),
        }],
        Expectation::PlayerAt(pose(FIXTURE_POSE.0, FIXTURE_POSE.1 + 3, FIXTURE_POSE.2)),
        30,
    )
    .interruptible()
    .timed(InputKind::WalkTile, 3);
    let outcome = ctx.act(walk).expect("act");
    assert_eq!(outcome, Outcome::Interrupted);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![Intent::Battle {
            policy: BattlePlan::Auto
        }]
    );
    // The hold was released when the battle box showed.
    let issued = commands.lock().unwrap().clone();
    assert!(
        issued
            .iter()
            .any(|c| matches!(c, ControllerCommand::Neutral)),
        "{issued:?}"
    );
    // Map changes are learned by the context.
    let visited = ctx.state().world.visited(FIXTURE_POSE.0).value;
    assert_eq!(visited, Some(true));
}

#[test]
fn unexpected_dialogue_after_an_action_runs_unstick() {
    let Some(d) = data() else { return };
    let Some(page) = fixture("emu-tools-dialogue.png") else {
        return;
    };
    let (mut runtime, _) = runtime(&d, vec![page]);
    let executor = Executor::default();
    let stop = AtomicBool::new(false);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut ctx = ToolContext::new(
        &mut runtime,
        &executor,
        Arc::clone(&d.world),
        Arc::clone(&d.data),
        &stop,
    )
    .with_toolbox(Toolbox::new(vec![Box::new(Recorder {
        seen: Arc::clone(&seen),
    })]));
    let press = Action::new(
        "press A",
        vec![ControllerCommand::Press(pokebot_core::Button::A)],
        Expectation::InputsDone,
        1,
    );
    let outcome = ctx.act(press).expect("act");
    assert_eq!(outcome, Outcome::Interrupted);
    assert_eq!(*seen.lock().unwrap(), vec![Intent::Unstick]);
}

/// A tool that records the intents it was invoked with and fails, so a
/// step driven on a static frame ends instead of looping.
struct Refuser {
    seen: Arc<Mutex<Vec<Intent>>>,
}

impl Tool for Refuser {
    fn name(&self) -> &str {
        "Refuser"
    }

    fn serves(&self, intent: &Intent) -> bool {
        matches!(intent, Intent::Battle { .. } | Intent::Unstick)
    }

    fn run(&mut self, intent: &Intent, _ctx: &mut ToolContext<'_>) -> ToolOutcome {
        self.seen.lock().unwrap().push(intent.clone());
        ToolOutcome::failed("refused")
    }
}

/// flash-1 on Route 3: a trainer spotted the player on the way to the
/// grass ("Excuse me! You looked at me, didn't you?"). The hunt claimed
/// dialogue as its own (it plays its battles) and waited for the scene to
/// settle until the stuck rule pressed B; the trainer's PIDGEY was then
/// hunted as a wild one. Outside a battle, the hunt leaves dialogue to
/// the interrupt wrapper.
#[test]
fn a_trainers_challenge_during_a_catch_hunt_runs_unstick() {
    let Some(d) = data() else { return };
    let Some(spotted) = fixture("emu-trainer-spotted-route3.png") else {
        return;
    };
    let (mut runtime, _) = runtime(&d, vec![spotted]);
    runtime.set_pose_hint(pose("Route3", 15, 9));
    let executor = Executor::default();
    let stop = AtomicBool::new(false);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut toolbox = Toolbox::default();
    toolbox.prepend(Box::new(Refuser {
        seen: Arc::clone(&seen),
    }));
    let mut ctx = ToolContext::new(
        &mut runtime,
        &executor,
        Arc::clone(&d.world),
        Arc::clone(&d.data),
        &stop,
    )
    .with_toolbox(toolbox);
    let outcome = ctx.invoke(&Intent::Catch {
        species: "SPECIES_SPEAROW".into(),
        map: Some("Route3".into()),
    });
    let err = outcome
        .result
        .expect_err("the refused Unstick fails the hunt");
    assert!(err.to_string().contains("refused"), "{err}");
    assert_eq!(*seen.lock().unwrap(), vec![Intent::Unstick]);
}

/// A step that waits `waits` frames, then is done.
struct Waiter {
    waits: usize,
}

impl ToolStep for Waiter {
    fn next(&mut self, _ctx: &mut StepContext<'_>) -> Decision {
        if self.waits == 0 {
            return Decision::Done("waited".into());
        }
        self.waits -= 1;
        Decision::Wait("conversation ending".into())
    }

    fn on_outcome(&mut self, _action: &Action, _outcome: Outcome, _ctx: &mut StepContext<'_>) {}

    fn expects(&self) -> Expects {
        Expects::NONE
    }
}

/// flash-2: Unstick waited for a trainer's intro to end, the battle ran as
/// an interrupt for 95 s, and the frame after it the wait counted the
/// whole battle ("stuck waiting: conversation ending"). A handled
/// interrupt starts the step's wait over.
#[test]
fn a_handled_interrupt_restarts_the_steps_wait() {
    let Some(d) = data() else { return };
    let (Some(overworld), Some(battle)) = (
        fixture("emu-tools-overworld.png"),
        fixture("emu-tools-battle-command.png"),
    ) else {
        return;
    };
    let frames = vec![overworld.clone(), battle, overworld.clone(), overworld];
    let (mut runtime, _) = runtime(&d, frames);
    runtime.set_pose_hint(pose(FIXTURE_POSE.0, FIXTURE_POSE.1, FIXTURE_POSE.2));
    let executor = Executor {
        max_wait_frames: 2,
        ..Executor::default()
    };
    let stop = AtomicBool::new(false);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut ctx = ToolContext::new(
        &mut runtime,
        &executor,
        Arc::clone(&d.world),
        Arc::clone(&d.data),
        &stop,
    )
    .with_toolbox(Toolbox::new(vec![Box::new(Recorder {
        seen: Arc::clone(&seen),
    })]));
    // Frame 1: wait. Frame 2: the battle interrupt. Frames 3 and 4: wait
    // again, 3 frames after the first (over the limit of 2 unless the wait
    // restarted at frame 3). Frame 5: done.
    let mut step = Waiter { waits: 3 };
    let result = ctx.drive(&mut step);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![Intent::Battle {
            policy: BattlePlan::Auto
        }]
    );
    assert_eq!(
        result.expect("the wait restarted after the battle"),
        "waited"
    );
}

#[test]
fn the_nurses_welcome_is_identified_in_the_game_text() {
    let Some(d) = data() else { return };
    let dialogue = d.world.dialogue().unwrap();
    let events = d.world.events().unwrap();
    let mut o = Observation::bare(
        1,
        Observed {
            value: ScreenState::Dialogue,
            detector: "test".into(),
        },
        Default::default(),
    );
    o.dialogue = Some(pokebot_state::DialogueObservation {
        kind: pokebot_state::DialogueKind::MessageBox,
        region: Default::default(),
        waiting_for_input: true,
        arrow: None,
        stable_frames: 10,
        text_cells: vec![],
        lines: vec!["Welcome to our POKéMON CENTER!".into()],
        help: false,
    });
    let labels = identify(&o, dialogue);
    assert_eq!(labels, vec!["Text_WelcomeWantToHealPkmn".to_string()]);
    // OCR doubt and a name filled in still match.
    o.dialogue.as_mut().unwrap().lines = vec!["Welcome to our POK?MON CENTER!".into()];
    assert_eq!(identify(&o, dialogue), labels);
    // The label leads to every Pokémon Center nurse: not one script.
    let index = LabelIndex::build(events);
    let scripts = index.scripts_for(&labels);
    assert!(
        scripts.contains(&"PewterCity_PokemonCenter_1F_EventScript_Nurse".to_string()),
        "{scripts:?}"
    );
    assert!(scripts.len() > 1);
    // The nurse's YES branch heals: the policy answers YES.
    let nurse = events
        .script("PewterCity_PokemonCenter_1F_EventScript_Nurse")
        .unwrap();
    assert_eq!(dialogue::policy_answer(Some(nurse), &[]), Answer::Yes);
    // With the script known, the path that ran is the healing one.
    let path = dialogue::resolve_path(
        nurse,
        &[
            "Text_WelcomeWantToHealPkmn".into(),
            "Text_RestoredPkmnToFullHealth".into(),
        ],
        &[true],
    )
    .expect("a path");
    assert!(nurse.paths[path]
        .does
        .iter()
        .any(|e| matches!(e, Effect::Heal { heal: true })));
    // Brock's gift text pins the TM path.
    let brock = events.script("PewterCity_Gym_EventScript_Brock").unwrap();
    let path = dialogue::resolve_path(
        brock,
        &["PewterCity_Gym_Text_ReceivedTM39FromBrock".into()],
        &[],
    )
    .expect("a path");
    assert!(brock.paths[path]
        .does
        .iter()
        .any(|e| matches!(e, Effect::Give { give, .. } if give == "ITEM_TM39")));
}

#[test]
fn script_effects_become_tracked_events() {
    let Some(d) = data() else { return };
    let mut state = GameState::default();
    state
        .world
        .vars
        .insert("VAR_X".into(), Knowledge::tracked(4, None));
    let world = Arc::clone(&d.world);
    let map_name = |id: &str| world.name_of(id).map(str::to_owned);
    let t = |e: &Effect| translate(e, &state, &d.data, d.world.places(), &map_name);
    assert_eq!(
        t(&Effect::Set {
            set: "FLAG_BADGE01_GET".into()
        }),
        Ok(vec![GameEvent::FlagTracked {
            flag: "FLAG_BADGE01_GET".into(),
            value: true
        }])
    );
    assert_eq!(
        t(&Effect::Clear {
            clear: "FLAG_HIDE_X".into()
        }),
        Ok(vec![GameEvent::FlagTracked {
            flag: "FLAG_HIDE_X".into(),
            value: false
        }])
    );
    assert_eq!(
        t(&Effect::Defeated {
            defeated: "TRAINER_LEADER_BROCK".into()
        }),
        Ok(vec![GameEvent::FlagTracked {
            flag: "TRAINER_LEADER_BROCK".into(),
            value: true
        }])
    );
    assert_eq!(
        t(&Effect::Var {
            var: "VAR_MAP_SCENE_PEWTER_CITY".into(),
            change: VarChange {
                eq: Some(1.into()),
                ..VarChange::default()
            }
        }),
        Ok(vec![GameEvent::VarTracked {
            var: "VAR_MAP_SCENE_PEWTER_CITY".into(),
            value: 1
        }])
    );
    assert_eq!(
        t(&Effect::Var {
            var: "VAR_X".into(),
            change: VarChange {
                add: Some(2.into()),
                ..VarChange::default()
            }
        }),
        Ok(vec![GameEvent::VarTracked {
            var: "VAR_X".into(),
            value: 6
        }])
    );
    assert!(matches!(
        t(&Effect::Var {
            var: "VAR_UNKNOWN".into(),
            change: VarChange {
                add: Some(1.into()),
                ..VarChange::default()
            }
        }),
        Err(Skipped(_))
    ));
    assert_eq!(
        t(&Effect::Give {
            give: "ITEM_POKE_BALL".into(),
            count: 5,
            text: None,
            find: false
        }),
        Ok(vec![GameEvent::ItemsChanged {
            pocket: Pocket::PokeBalls,
            item: "ITEM_POKE_BALL".into(),
            delta: 5,
            reason: "script gave it".into()
        }])
    );
    assert_eq!(
        t(&Effect::Take {
            take: "ITEM_POKE_BALL".into(),
            count: 1
        }),
        Ok(vec![GameEvent::ItemsChanged {
            pocket: Pocket::PokeBalls,
            item: "ITEM_POKE_BALL".into(),
            delta: -1,
            reason: "script took it".into()
        }])
    );
    assert_eq!(
        t(&Effect::Warp {
            warp: "MAP_PEWTER_CITY".into(),
            warp_id: None,
            x: None,
            y: None
        }),
        Ok(vec![GameEvent::MapVisited {
            map: "PewterCity".into()
        }])
    );
    assert_eq!(t(&Effect::Heal { heal: true }), Ok(vec![GameEvent::Healed]));
    assert_eq!(
        t(&Effect::Money { money: 500.into() }),
        Ok(vec![GameEvent::MoneyChanged {
            delta: 500,
            reason: "script".into()
        }])
    );
    assert!(matches!(
        t(&Effect::Say { say: "X".into() }),
        Err(Skipped(_))
    ));
    if let Some(places) = d.world.places() {
        if let Some(spot) = places.heal_spots.first() {
            assert_eq!(
                t(&Effect::Respawn {
                    respawn: spot.id.clone()
                }),
                Ok(vec![GameEvent::RespawnSet {
                    map: spot.map.clone(),
                    x: spot.x,
                    y: spot.y
                }])
            );
        }
    }
    // A whole path: Brock's TM path records the run, the badge and the TM.
    let (events, log) = path_events(
        d.world.events().unwrap(),
        "PewterCity_Gym_EventScript_Brock",
        1,
        &state,
        &d.data,
        d.world.places(),
        &map_name,
    );
    assert_eq!(
        events[0],
        GameEvent::ScriptPathRun {
            script: "PewterCity_Gym_EventScript_Brock".into(),
            path: 1
        }
    );
    assert!(events.contains(&GameEvent::FlagTracked {
        flag: "FLAG_BADGE01_GET".into(),
        value: true
    }));
    assert!(events.iter().any(|e| matches!(
        e,
        GameEvent::ItemsChanged { item, delta: 1, .. } if item == "ITEM_TM39"
    )));
    // The battle and the text lines are skipped, and said so.
    assert!(log.iter().any(|l| l.contains("battle")), "{log:?}");
}

/// `Go` one tile and `Heal` at the nearest Center on the virtual console
/// (`pokebot emulator serve`: `/dev/video10`, `/tmp/pokebot-esp32`), from
/// whatever the cartridge save shows. Run with:
/// `cargo test -p pokebot-agent --release --test tools -- --ignored --nocapture`
#[test]
#[ignore]
fn console_go_and_heal_through_the_tools() {
    use pokebot_capture_card::{CaptureCardConfig, CaptureCardVideoSource};
    use pokebot_pabotbase::{PabotBaseConfig, PabotBaseController};

    let Some(d) = data() else { return };
    let small_font = pokebot_vision::text::Font::load(root().join("data/world/font_small.json"))
        .expect("font_small.json");
    let video = CaptureCardVideoSource::open(CaptureCardConfig {
        device: PathBuf::from("/dev/video10"),
        size: None,
        controls: Vec::new(),
    })
    .expect("virtual console camera /dev/video10");
    let controller =
        PabotBaseController::open(PabotBaseConfig::new("/tmp/pokebot-esp32")).expect("pabotbase");
    let perception = FireRedPerception::with_world(Arc::clone(&d.world))
        .with_font(Arc::clone(&d.font))
        .with_small_font(Arc::new(small_font))
        .with_global_search(true);
    let mut runtime = Runtime::with_perception(
        Devices {
            video: Box::new(video),
            controller: Box::new(controller),
            normalizer: Normalizer::new(ViewportLocator::Fixed(pokebot_video::Rect {
                x: 0,
                y: 0,
                width: 720,
                height: 480,
            })),
            video_name: "capture-card:/dev/video10".into(),
            controller_name: "pabotbase:/tmp/pokebot-esp32".into(),
            persist_save: None,
        },
        perception,
    );
    runtime.echo_events(true);
    let mut syncer = Syncer::new("emulator");
    syncer.set_base_latency_frames(30);
    let executor = Executor {
        latency_frames: 30,
        syncer: Some(Arc::new(Mutex::new(syncer))),
        ..Executor::default()
    };
    let stop = AtomicBool::new(false);
    let mut ctx = ToolContext::new(
        &mut runtime,
        &executor,
        Arc::clone(&d.world),
        Arc::clone(&d.data),
        &stop,
    );
    // Locate the player.
    let mut at = None;
    for _ in 0..600 {
        let o = ctx.observe().expect("observe");
        if let Some(p) = &o.player {
            at = Some(p.pose.clone());
            break;
        }
    }
    let at = at.expect("player located on the console");
    eprintln!("located at {at}");
    // A free tile next to the player.
    let map = d.world.map(&at.map).expect("known map");
    let occupied: std::collections::HashSet<(i32, i32)> = map
        .objects
        .iter()
        .filter_map(|o| Some((o.x?, o.y?)))
        .collect();
    let target = [(0, 1), (1, 0), (-1, 0), (0, -1)]
        .iter()
        .map(|(dx, dy)| (at.x + dx, at.y + dy))
        .find(|(x, y)| {
            map.tile(*x, *y).is_some_and(|t| t.collision == 0) && !occupied.contains(&(*x, *y))
        })
        .expect("a walkable neighbour");
    let go = ctx.invoke(&Intent::Go {
        dest: Dest::Tile {
            map: at.map.clone(),
            x: target.0,
            y: target.1,
        },
    });
    eprintln!(
        "Go -> {:?} pose {:?} learned {:?}",
        go.result, go.pose, go.learned
    );
    assert!(go.is_ok(), "{:?}", go.result);
    assert_eq!(
        go.pose.as_ref().map(|p| (p.x, p.y)),
        Some(target),
        "arrived at the tile"
    );
    let heal = ctx.invoke(&Intent::Heal { center: None });
    eprintln!("Heal -> {:?} learned {:?}", heal.result, heal.learned);
    assert!(heal.is_ok(), "{:?}", heal.result);
    assert!(
        heal.learned.iter().any(|e| matches!(e, GameEvent::Healed)),
        "{:?}",
        heal.learned
    );
    let ran: Vec<_> = heal
        .learned
        .iter()
        .filter(|e| matches!(e, GameEvent::ScriptPathRun { .. }))
        .collect();
    eprintln!("script paths run: {ran:?}");
    let _ = ctx.runtime.execute(ControllerCommand::Neutral);
}
