//! The screen probes (spec §3.2, §4.3): the Trainer Card and the Pokédex
//! list driven through the Start menu against a small simulated game that
//! answers the controller like the real one (menu cursor, the card, the
//! TABLE OF CONTENTS, the scrolling list), and, ignored, both probes on
//! the virtual console.
//!
//! The data comes from `data/world`; tests skip when it is missing.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pokebot_agent::tools::{Intent, ProbeFact, ToolContext, ToolError};
use pokebot_agent::{Executor, Syncer};
use pokebot_core::{
    Button, CapturedFrame, Controller, ControllerCommand, ControllerReceipt, NormalizedFrame,
    RgbImage, VideoSource,
};
use pokebot_gamedata::GameData;
use pokebot_runtime::{Devices, Runtime};
use pokebot_state::{
    GameEvent, MenuObservation, Observation, Observed, PlayerPose, PokedexListObservation,
    PoseObservation, Region, ScreenState, TrainerCardObservation,
};
use pokebot_video::{Normalizer, ViewportLocator};
use pokebot_vision::PerceptionSystem;
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
    Some(Data {
        world: Arc::new(world),
        data: Arc::new(data),
    })
}

/// A blank frame forever; the scripted perception decides what is seen.
struct BlankVideo {
    next: u64,
}

impl VideoSource for BlankVideo {
    fn next_frame(&mut self) -> pokebot_core::Result<CapturedFrame> {
        let frame = CapturedFrame {
            frame_id: self.next,
            delivered: self.next,
            captured_at: Instant::now(),
            image: RgbImage::filled(240, 160, [0, 0, 0]),
        };
        self.next += 1;
        Ok(frame)
    }
}

/// The Start menu rows of a player named RED with the Pokédex.
const START_MENU: [&str; 7] = ["POKéDEX", "POKéMON", "BAG", "RED", "SAVE", "OPTION", "EXIT"];
const START_MENU_WINDOW: Region = Region {
    x: 168,
    y: 8,
    width: 64,
    height: 105,
};
/// Start menu rows are 15 px apart; the ▶ sits 4 px below its row's top.
const PITCH: u32 = 15;
/// Rows the Pokédex list shows at once, and the row the ▶ parks on while
/// the list scrolls (measured on the console: No.048 shown as row 5 of
/// 043–051).
const LIST_ROWS: usize = 9;
const LIST_PARK_ROW: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Overworld,
    StartMenu {
        cursor: usize,
    },
    Card,
    /// The Pokédex TABLE OF CONTENTS (nothing recognises it).
    Contents,
    List {
        cursor: usize,
        scroll: usize,
    },
}

/// What the simulated game shows, and how it answers inputs.
struct Game {
    screen: Screen,
    /// Frames still to show a black transition after the last input.
    fade: u32,
    badges: Vec<u8>,
    pokedex_count: Option<u16>,
    /// The numerical list: (name, caught mark).
    dex: Vec<(&'static str, bool)>,
    /// The name row opens nothing (a game without a Trainer Card).
    card_missing: bool,
    /// Frames since the list was last redrawn: the caught balls are
    /// blitted a few frames after the names, like the game does.
    list_drawn: u32,
    /// Frames observed so far, and presses waiting to take effect: the
    /// console answers a press [`PRESS_LATENCY`] frames later.
    now: u64,
    pending: Vec<(u64, Button)>,
}

/// Frames after a list redraw before its caught marks show.
const MARK_DELAY: u32 = 4;
/// Frames between a press and its effect on screen.
const PRESS_LATENCY: u64 = 8;

impl Game {
    fn press(&mut self, button: Button) {
        self.pending.push((self.now + PRESS_LATENCY, button));
    }

    fn apply(&mut self, button: Button) {
        use Screen::*;
        self.screen = match (self.screen, button) {
            (Overworld, Button::Start) => StartMenu { cursor: 0 },
            (StartMenu { cursor }, Button::Down) => StartMenu {
                cursor: (cursor + 1) % START_MENU.len(),
            },
            (StartMenu { cursor }, Button::Up) => StartMenu {
                cursor: (cursor + START_MENU.len() - 1) % START_MENU.len(),
            },
            (StartMenu { cursor }, Button::A) => match START_MENU[cursor] {
                "RED" if self.card_missing => Overworld,
                "RED" => Card,
                "POKéDEX" => Contents,
                "EXIT" => Overworld,
                _ => StartMenu { cursor },
            },
            (StartMenu { .. }, Button::B) => Overworld,
            (Card, Button::B) => StartMenu { cursor: 3 },
            (Contents, Button::A) => List {
                cursor: 0,
                scroll: 0,
            },
            (Contents, Button::B) => StartMenu { cursor: 0 },
            // Like the game's list: the ▶ walks down to the middle row,
            // the list scrolls under it, and the ▶ walks on to the last
            // entry once the list can't scroll.
            (List { cursor, scroll }, Button::Down) => {
                let len = self.dex.len();
                let shown = LIST_ROWS.min(len - scroll);
                if cursor < LIST_PARK_ROW && cursor + 1 < shown {
                    List {
                        cursor: cursor + 1,
                        scroll,
                    }
                } else if scroll + LIST_ROWS < len {
                    List {
                        cursor,
                        scroll: scroll + 1,
                    }
                } else if cursor + 1 < shown {
                    List {
                        cursor: cursor + 1,
                        scroll,
                    }
                } else {
                    List { cursor, scroll }
                }
            }
            (List { cursor, scroll }, Button::Up) => {
                if cursor > 0 {
                    List {
                        cursor: cursor - 1,
                        scroll,
                    }
                } else {
                    List {
                        cursor,
                        scroll: scroll.saturating_sub(1),
                    }
                }
            }
            (List { .. }, Button::B) => Contents,
            (screen, _) => screen,
        };
        // Screens change behind a short fade, like the game's.
        self.fade = 3;
        self.list_drawn = 0;
    }

    fn observe(&mut self, frame_id: u64) -> Observation {
        self.now = frame_id;
        let due: Vec<Button> = self
            .pending
            .iter()
            .filter(|(at, _)| *at <= frame_id)
            .map(|(_, b)| *b)
            .collect();
        self.pending.retain(|(at, _)| *at > frame_id);
        for button in due {
            self.apply(button);
        }
        let screen = |value, detector: &str| Observed {
            value,
            detector: detector.to_owned(),
        };
        if self.fade > 0 {
            self.fade -= 1;
            return Observation::bare(
                frame_id,
                screen(ScreenState::Transition, "sim-fade"),
                Default::default(),
            );
        }
        match self.screen {
            Screen::Overworld => {
                let mut o = Observation::bare(
                    frame_id,
                    screen(ScreenState::Unknown, "sim"),
                    Default::default(),
                );
                o.player = Some(PoseObservation {
                    pose: PlayerPose {
                        map: "Route4".into(),
                        x: 10,
                        y: 6,
                    },
                    score: 990,
                });
                o
            }
            Screen::StartMenu { cursor } => {
                let mut o = Observation::bare(
                    frame_id,
                    screen(ScreenState::Menu, "sim"),
                    Default::default(),
                );
                o.menu = Some(MenuObservation {
                    window: START_MENU_WINDOW,
                    rows: START_MENU.len() as u8,
                    cursor_row: cursor as u8,
                    cursor_y: START_MENU_WINDOW.y + PITCH * cursor as u32 + 4,
                });
                o.menu_lines = START_MENU.iter().map(|s| (*s).to_owned()).collect();
                o
            }
            Screen::Card => {
                let mut o = Observation::bare(
                    frame_id,
                    screen(ScreenState::Unknown, "sim-card"),
                    Default::default(),
                );
                o.trainer_card = Some(TrainerCardObservation {
                    badges: self.badges.clone(),
                    pokedex_count: self.pokedex_count,
                });
                o
            }
            Screen::Contents => Observation::bare(
                frame_id,
                screen(ScreenState::Unknown, "sim-contents"),
                Default::default(),
            ),
            Screen::List { cursor, scroll } => {
                let mut o = Observation::bare(
                    frame_id,
                    screen(ScreenState::Unknown, "sim-list"),
                    Default::default(),
                );
                let marks_drawn = self.list_drawn >= MARK_DELAY;
                self.list_drawn += 1;
                let rows = self
                    .dex
                    .iter()
                    .skip(scroll)
                    .take(LIST_ROWS)
                    .map(|(name, mark)| ((*name).to_owned(), *mark && marks_drawn))
                    .chain(std::iter::repeat((String::new(), false)))
                    .take(LIST_ROWS)
                    .collect();
                o.pokedex_list = Some(PokedexListObservation {
                    rows,
                    cursor: Some(cursor as u8),
                });
                o
            }
        }
    }
}

/// Feeds presses to the game.
struct SimController {
    game: Arc<Mutex<Game>>,
    presses: Arc<Mutex<Vec<Button>>>,
}

impl Controller for SimController {
    fn execute(&mut self, command: ControllerCommand) -> pokebot_core::Result<ControllerReceipt> {
        if let ControllerCommand::Press(button) = &command {
            self.game.lock().unwrap().press(*button);
            self.presses.lock().unwrap().push(*button);
        }
        let id = self.presses.lock().unwrap().len() as u64;
        Ok(ControllerReceipt {
            command_id: id,
            issued_at: Instant::now(),
            input_duration: Duration::from_millis(0),
        })
    }

    fn is_idle(&self) -> pokebot_core::Result<bool> {
        Ok(true)
    }
}

/// Shows what the game shows.
struct SimPerception {
    game: Arc<Mutex<Game>>,
}

impl PerceptionSystem for SimPerception {
    fn observe(&mut self, frame: &NormalizedFrame) -> Observation {
        self.game.lock().unwrap().observe(frame.frame_id)
    }
}

struct Sim {
    game: Arc<Mutex<Game>>,
    presses: Arc<Mutex<Vec<Button>>>,
    runtime: Runtime,
}

fn sim(game: Game) -> Sim {
    let game = Arc::new(Mutex::new(game));
    let presses = Arc::new(Mutex::new(Vec::new()));
    let runtime = Runtime::with_perception_system(
        Devices {
            video: Box::new(BlankVideo { next: 0 }),
            controller: Box::new(SimController {
                game: Arc::clone(&game),
                presses: Arc::clone(&presses),
            }),
            normalizer: Normalizer::new(ViewportLocator::FullFrame),
            video_name: "sim".into(),
            controller_name: "sim".into(),
            persist_save: None,
        },
        Box::new(SimPerception {
            game: Arc::clone(&game),
        }),
    );
    Sim {
        game,
        presses,
        runtime,
    }
}

fn route4_game() -> Game {
    Game {
        screen: Screen::Overworld,
        fade: 0,
        badges: vec![1],
        pokedex_count: Some(3),
        dex: vec![
            ("BULBASAUR", true),
            ("IVYSAUR", true),
            ("-----", false),
            ("CHARMANDER", false),
            ("-----", false),
            ("-----", false),
            ("-----", false),
            ("-----", false),
            ("-----", false),
            ("-----", false),
            ("-----", false),
            ("-----", false),
            ("-----", false),
            ("-----", false),
            ("-----", false),
            ("PIDGEY", true),
            ("-----", false),
            ("-----", false),
            ("RATTATA", false),
        ],
        card_missing: false,
        list_drawn: 0,
        now: 0,
        pending: Vec::new(),
    }
}

fn flag(events: &[GameEvent], name: &str) -> Option<bool> {
    events.iter().find_map(|e| match e {
        GameEvent::FlagObserved { flag, value } if flag == name => Some(*value),
        _ => None,
    })
}

#[test]
fn the_trainer_card_probe_reads_the_badges_and_closes_the_menus() {
    let Some(d) = data() else { return };
    let mut s = sim(route4_game());
    let executor = Executor::default();
    let stop = AtomicBool::new(false);
    let mut ctx = ToolContext::new(
        &mut s.runtime,
        &executor,
        Arc::clone(&d.world),
        Arc::clone(&d.data),
        &stop,
    );
    let outcome = ctx.invoke(&Intent::Probe {
        fact: ProbeFact::TrainerCard,
    });
    assert!(outcome.is_ok(), "{:?}", outcome.result);
    let learned = &outcome.learned;
    assert_eq!(flag(learned, "FLAG_BADGE01_GET"), Some(true));
    for n in 2..=8 {
        assert_eq!(
            flag(learned, &format!("FLAG_BADGE0{n}_GET")),
            Some(false),
            "badge {n}"
        );
    }
    // The count event is C2b's; when it exists it carries the card's 3.
    let count = pokebot_agent::tools::probe::pokedex_count_event(None, Some(3));
    if let Some(count) = &count {
        assert!(learned.contains(count), "{learned:?}");
    }
    // Back in the overworld, the belief updated.
    assert_eq!(s.game.lock().unwrap().screen, Screen::Overworld);
    assert_eq!(ctx.state().world.flag("FLAG_BADGE01_GET").value, Some(true));
    assert_eq!(
        ctx.state().world.flag("FLAG_BADGE05_GET").value,
        Some(false)
    );
    // Start, Down ×3 to RED, A, B (card), B (menu).
    let presses = s.presses.lock().unwrap().clone();
    assert_eq!(
        presses,
        vec![
            Button::Start,
            Button::Down,
            Button::Down,
            Button::Down,
            Button::A,
            Button::B,
            Button::B
        ],
        "{presses:?}"
    );
}

#[test]
fn a_card_that_never_opens_fails_the_probe_cleanly() {
    let Some(d) = data() else { return };
    let mut game = route4_game();
    game.card_missing = true;
    let mut s = sim(game);
    let executor = Executor::default();
    let stop = AtomicBool::new(false);
    let mut ctx = ToolContext::new(
        &mut s.runtime,
        &executor,
        Arc::clone(&d.world),
        Arc::clone(&d.data),
        &stop,
    );
    let outcome = ctx.invoke(&Intent::Probe {
        fact: ProbeFact::TrainerCard,
    });
    assert!(
        matches!(&outcome.result, Err(ToolError::Failed(reason)) if reason.contains("did not open")),
        "{:?}",
        outcome.result
    );
    assert!(
        !outcome
            .learned
            .iter()
            .any(|e| matches!(e, GameEvent::FlagObserved { .. })),
        "nothing was seen: {:?}",
        outcome.learned
    );
    assert_eq!(s.game.lock().unwrap().screen, Screen::Overworld);
}

#[test]
fn the_pokedex_probe_reads_every_page_of_the_list() {
    let Some(d) = data() else { return };
    let mut s = sim(route4_game());
    let executor = Executor::default();
    let stop = AtomicBool::new(false);
    let mut ctx = ToolContext::new(
        &mut s.runtime,
        &executor,
        Arc::clone(&d.world),
        Arc::clone(&d.data),
        &stop,
    );
    let outcome = ctx.invoke(&Intent::Probe {
        fact: ProbeFact::Pokedex,
    });
    assert!(outcome.is_ok(), "{:?}", outcome.result);
    let learned = &outcome.learned;
    let caught: Vec<&str> = learned
        .iter()
        .filter_map(|e| match e {
            GameEvent::SpeciesCaught { species } => Some(species.as_str()),
            _ => None,
        })
        .collect();
    let seen: Vec<&str> = learned
        .iter()
        .filter_map(|e| match e {
            GameEvent::SpeciesSeen { species } => Some(species.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        caught,
        vec!["SPECIES_BULBASAUR", "SPECIES_IVYSAUR", "SPECIES_PIDGEY"]
    );
    assert_eq!(seen, vec!["SPECIES_CHARMANDER", "SPECIES_RATTATA"]);
    let count = pokebot_agent::tools::probe::pokedex_count_event(Some(5), Some(3));
    if let Some(count) = &count {
        assert!(learned.contains(count), "{learned:?}");
    }
    assert_eq!(s.game.lock().unwrap().screen, Screen::Overworld);
    assert_eq!(
        ctx.state()
            .pokedex
            .caught
            .get("SPECIES_PIDGEY")
            .and_then(|k| k.value),
        Some(true)
    );
    let presses = s.presses.lock().unwrap().clone();
    // Start, A on POKéDEX, A on NUMERICAL MODE, Downs through 19 rows
    // (5 to the parking row, 10 scrolls, 3 to the last entry, 2 that show
    // the end), B ×3.
    assert_eq!(&presses[..3], &[Button::Start, Button::A, Button::A]);
    let downs = presses.iter().filter(|b| **b == Button::Down).count();
    assert_eq!(downs, 5 + 10 + 3 + 2, "{presses:?}");
    assert_eq!(
        &presses[presses.len() - 3..],
        &[Button::B, Button::B, Button::B]
    );
}

/// Both probes on the virtual console (`pokebot emulator serve`:
/// `/dev/video10`, `/tmp/pokebot-esp32`), from whatever the cartridge save
/// shows (Route 4, Boulder Badge, a few species caught). Run with:
/// `cargo test -p pokebot-agent --release --test probes -- --ignored --nocapture`
#[test]
#[ignore]
fn console_trainer_card_and_pokedex_probes() {
    use pokebot_capture_card::{CaptureCardConfig, CaptureCardVideoSource};
    use pokebot_pabotbase::{PabotBaseConfig, PabotBaseController};
    use pokebot_vision::FireRedPerception;

    let Some(d) = data() else { return };
    let font = pokebot_vision::text::Font::load(root().join("data/world/font_normal.json"))
        .expect("font_normal.json");
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
        .with_font(Arc::new(font))
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
    // Locate the player; a screen left open by an earlier run is closed
    // with B first (nothing the game shows in the field reacts to B).
    let mut at = None;
    'locate: for round in 0..6 {
        for _ in 0..240 {
            let o = ctx.observe().expect("observe");
            if let Some(p) = &o.player {
                at = Some(p.pose.clone());
                break 'locate;
            }
        }
        eprintln!("not located (round {round}): pressing B");
        ctx.runtime
            .execute(ControllerCommand::Press(Button::B))
            .expect("press B");
    }
    eprintln!("located at {}", at.expect("player located on the console"));
    let started = Instant::now();
    let card = ctx.invoke(&Intent::Probe {
        fact: ProbeFact::TrainerCard,
    });
    eprintln!(
        "TrainerCard -> {:?} in {:.1}s learned {:?}",
        card.result,
        started.elapsed().as_secs_f32(),
        card.learned
    );
    assert!(card.is_ok(), "{:?}", card.result);
    assert_eq!(flag(&card.learned, "FLAG_BADGE01_GET"), Some(true));
    let started = Instant::now();
    let dex = ctx.invoke(&Intent::Probe {
        fact: ProbeFact::Pokedex,
    });
    eprintln!(
        "Pokedex -> {:?} in {:.1}s learned {:?}",
        dex.result,
        started.elapsed().as_secs_f32(),
        dex.learned
    );
    assert!(dex.is_ok(), "{:?}", dex.result);
    assert!(
        dex.learned
            .iter()
            .any(|e| matches!(e, GameEvent::SpeciesCaught { .. })),
        "{:?}",
        dex.learned
    );
    let _ = ctx.runtime.execute(ControllerCommand::Neutral);
}
