//! Two emulators linked through gpSP's link port complete a real trade.
//!
//! Both load `saves/cascade.sav` (Cerulean City, after Misty), walk to the
//! Pokémon Center's Wireless Club, meet in the Direct Corner's Trade Center
//! (host "Become leader", guest "Join group") and trade: the host gives
//! GEODUDE (party slot 2) for the guest's ZUBAT (slot 3). Only video,
//! joypad and the link port are used; the result is checked on screen.
//!
//! Slow (~16k frames per emulator), so ignored by default:
//!   cargo test -p pokebot-emulator-libretro --test link_trade -- --ignored
//! Needs `emulator/gpsp_libretro.so` (tools/fetch-emulator.sh) and a FireRed
//! ROM (`$VPB_ROM` or the first `*.gba` in `roms/`). `VPB_LINK_TRACE=<n>`
//! saves both screens every n frames to the test's temp directory;
//! `VPB_LINK_CLOCK=realtime` runs both at console speed, as
//! `pokebot emulator serve` does (about five minutes).

use std::path::{Path, PathBuf};

use pokebot_controller::FrameRate;
use pokebot_core::{ButtonSet, Controller, ControllerCommand, RgbImage, VideoSource};
use pokebot_emulator_libretro::{
    launch, ClockMode, EmulatorConfig, EmulatorController, EmulatorVideoSource, LinkRole,
};

/// Button script, run on both emulators in lockstep. Lines are
/// `<who> <BUTTON>[*repeat] [hold frames] [frames after]` with who = a
/// (host), b (guest) or ab (both), `wait <frames>`, and `mark <name>`.
const SCRIPT: &str = "
wait 300
ab START 4 60           # skip the intro
ab A 4 90
ab A 4 90
ab A 4 240
ab A 4 240              # CONTINUE
ab B 4 150              # skip the recap
ab DOWN 16 4            # out of the Cerulean Gym
ab RIGHT 48 4
ab DOWN 48 4
ab LEFT 48 4
ab DOWN 250 10
wait 90
ab LEFT 16 4
ab DOWN 32 120
ab DOWN 16 4            # to the Pokémon Center
ab LEFT 128 10
ab LEFT 16 4
ab UP 96 120
ab UP 32 4              # up the escalator
ab LEFT 128 150
wait 60
ab A*4 4 40             # first-visit tutorial
ab A*12 4 40
ab A*24 4 40
ab B*40 4 40
ab RIGHT 64 10          # past the Union Room desk...
ab UP 4 10
ab A 4 60
ab B*25 4 40
ab RIGHT 64 10          # ...to the Direct Corner
ab UP 4 10
ab A 4 60
ab A*3 4 90
ab A*4 4 90             # TRADE CENTER, trade: yes
wait 60
ab A 4 90               # save before linking
ab A 4 90
ab A 4 120
ab A 4 90
wait 60
ab A 4 10
wait 1000
ab A 4 90
wait 60
ab A 4 90
wait 60
a DOWN 4 20             # host: BECOME LEADER
a A 4 60
wait 30
b A 4 90                # guest: JOIN GROUP
wait 240
b A 4 30                # guest: pick the host's group
wait 240
a A 4 30                # host: accept
wait 360
ab A 4 60
wait 300
ab A 4 60
wait 300
a UP 32 10              # to the seats
b UP 32 10
a LEFT 16 10
b RIGHT 16 10
a UP 16 10
b UP 16 10
a RIGHT 4 10
b LEFT 4 10
ab A 4 120
wait 800
mark before
a RIGHT 4 20            # host: GEODUDE
b DOWN 4 20             # guest: ZUBAT
ab A 4 40
ab DOWN 4 20            # TRADE
ab A 4 120
wait 120
ab A 4 60               # Is this trade okay? YES
wait 3300
mark after
";

struct Console {
    name: &'static str,
    video: EmulatorVideoSource,
    controller: EmulatorController,
    frame: Option<RgbImage>,
    /// Frames seen (the next frame's id) and frames the script has asked for.
    frames: u64,
    target: u64,
    trace: Option<(PathBuf, u64)>,
    marks: Vec<(String, RgbImage)>,
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The gpSP core (`$VPB_LINK_CORE` overrides, e.g. a debug build).
fn gpsp() -> PathBuf {
    std::env::var_os("VPB_LINK_CORE")
        .map(PathBuf::from)
        .unwrap_or_else(|| root().join("emulator/gpsp_libretro.so"))
}

fn rom() -> Option<PathBuf> {
    std::env::var_os("VPB_ROM").map(PathBuf::from).or_else(|| {
        let mut roms: Vec<PathBuf> = std::fs::read_dir(root().join("roms"))
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("gba")))
            .collect();
        roms.sort();
        roms.into_iter().next()
    })
}

fn start(dir: &Path, name: &'static str, rom: &Path, link: LinkRole) -> (Console, Option<String>) {
    // One core file per emulator: a libretro core is process-global.
    let core = dir.join(format!("{name}_libretro.so"));
    std::fs::copy(gpsp(), &core).unwrap();
    let save = dir.join(format!("{name}.sav"));
    std::fs::copy(root().join("saves/cascade.sav"), &save).unwrap();
    let mut config = EmulatorConfig::new(core, rom);
    config.battery_save = Some(save);
    config.clock = match std::env::var("VPB_LINK_CLOCK").as_deref() {
        Ok("realtime") => ClockMode::RealTime,
        _ => ClockMode::Stepped,
    };
    config.link = Some(link);
    let (video, controller, info) = launch(config).expect("emulator starts");
    let trace = std::env::var("VPB_LINK_TRACE")
        .ok()
        .and_then(|n| n.parse().ok())
        .map(|every| (dir.to_path_buf(), every));
    let console = Console {
        name,
        video,
        controller,
        frame: None,
        frames: 0,
        target: 0,
        trace,
        marks: Vec::new(),
    };
    (console, info.link_addr.map(|a| a.to_string()))
}

fn step(consoles: &mut [&mut Console], frames: u64) {
    for _ in 0..frames {
        for console in consoles.iter_mut() {
            console.advance();
        }
    }
}

impl Console {
    /// Waits for the next frame. In real time, frames the reader missed
    /// count as passed.
    fn advance(&mut self) {
        self.target += 1;
        while self.frames < self.target {
            let frame = self.video.next_frame().expect("frame");
            self.frames = frame.frame_id + 1;
            if let Some((dir, every)) = &self.trace {
                if frame.frame_id % every == 0 {
                    let path = dir.join(format!("{}-{:05}.png", self.name, frame.frame_id));
                    pokebot_video::png::save(&frame.image, path).unwrap();
                }
            }
            self.frame = Some(frame.image);
        }
    }
}

fn run_script(host: &mut Console, guest: &mut Console) {
    for line in SCRIPT.lines() {
        let words: Vec<&str> = line.split('#').next().unwrap().split_whitespace().collect();
        match words.as_slice() {
            [] => {}
            ["wait", frames] => step(&mut [host, guest], frames.parse().unwrap()),
            ["mark", name] => {
                for console in [&mut *host, &mut *guest] {
                    let frame = console.frame.clone().unwrap();
                    console.marks.push((name.to_string(), frame));
                }
            }
            [who, button, timing @ ..] => {
                let (button, repeat) = button.split_once('*').unwrap_or((button, "1"));
                let buttons: ButtonSet = button_named(button).into();
                let hold: u64 = timing.first().map_or(4, |t| t.parse().unwrap());
                let after: u64 = timing.get(1).map_or(20, |t| t.parse().unwrap());
                for _ in 0..repeat.parse::<u32>().unwrap() {
                    let mut pressing: Vec<&mut Console> = match *who {
                        "a" => vec![&mut *host],
                        "b" => vec![&mut *guest],
                        "ab" => vec![&mut *host, &mut *guest],
                        _ => panic!("bad target in {line:?}"),
                    };
                    for console in &mut pressing {
                        console
                            .controller
                            .execute(ControllerCommand::Hold {
                                buttons,
                                duration: FrameRate::GBA.duration_of(hold),
                            })
                            .unwrap();
                    }
                    step(&mut [host, guest], hold + after);
                }
            }
            [_] => panic!("bad script line {line:?}"),
        }
    }
}

fn button_named(name: &str) -> pokebot_core::Button {
    use pokebot_core::Button::*;
    match name {
        "A" => A,
        "B" => B,
        "START" => Start,
        "UP" => Up,
        "DOWN" => Down,
        "LEFT" => Left,
        "RIGHT" => Right,
        _ => panic!("unknown button {name}"),
    }
}

/// The name label of a party slot on the trade screen (own party, left).
fn label(image: &RgbImage, slot: u32) -> Vec<[u8; 3]> {
    let (x0, y0) = match slot {
        2 => (64, 46),
        3 => (8, 86),
        _ => unreachable!(),
    };
    (y0..y0 + 12)
        .flat_map(|y| (x0..x0 + 48).map(move |x| (x, y)))
        .map(|(x, y)| image.pixel(x, y))
        .collect()
}

fn mark<'a>(console: &'a Console, name: &str) -> &'a RgbImage {
    &console.marks.iter().find(|(n, _)| n == name).unwrap().1
}

#[test]
#[ignore = "slow: runs two emulators through a full trade"]
fn linked_emulators_trade_pokemon() {
    let Some(rom) = rom() else {
        eprintln!("skipping: no ROM");
        return;
    };
    if !gpsp().exists() {
        eprintln!("skipping: no gpSP core (run tools/fetch-emulator.sh)");
        return;
    }
    let dir = std::env::temp_dir().join(format!("pokebot-link-trade-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let listen = "127.0.0.1:0".parse().unwrap();
    let (mut host, addr) = start(&dir, "host", &rom, LinkRole::Host { listen });
    let host_addr = addr.expect("host reports its address");
    let (mut guest, _) = start(&dir, "guest", &rom, LinkRole::Join { host: host_addr });
    run_script(&mut host, &mut guest);

    for console in [&host, &guest] {
        for (mark, image) in &console.marks {
            let path = dir.join(format!("{}-{mark}.png", console.name));
            pokebot_video::png::save(image, path).unwrap();
        }
    }
    let host_before = mark(&host, "before");
    let geodude = label(host_before, 2);
    let zubat = label(host_before, 3);
    let see = dir.display();
    assert!(geodude != zubat, "not on the trade screen; see {see}");
    assert!(
        label(mark(&guest, "before"), 2) == geodude,
        "guest party differs; see {see}"
    );

    // Traded Pokémon take the offered slot: the host's GEODUDE slot now
    // holds ZUBAT and the guest's ZUBAT slot holds GEODUDE.
    let host_after = mark(&host, "after");
    let guest_after = mark(&guest, "after");
    assert!(
        label(host_after, 2) == zubat,
        "host did not receive ZUBAT; see {see}"
    );
    assert!(
        label(guest_after, 3) == geodude,
        "guest did not receive GEODUDE; see {see}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
