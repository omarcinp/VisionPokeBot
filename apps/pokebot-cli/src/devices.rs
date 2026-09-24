//! Chooses concrete video/controller devices from command-line flags. This
//! is the only place that knows adapters exist; everything downstream sees
//! `dyn VideoSource` and `dyn Controller`.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use pokebot_capture_card::{CaptureCardConfig, CaptureCardVideoSource};
use pokebot_controller::NullController;
use pokebot_core::{Controller, GbaOnSwitch, PressProfile, VideoSource};
use pokebot_emulator_libretro::{launch, ClockMode, EmulatorConfig};
use pokebot_esp32_wifi::{Esp32WifiConfig, Esp32WifiController};
use pokebot_pabotbase::{PabotBaseConfig, PabotBaseController};
use pokebot_replay::Session;
use pokebot_runtime::Devices;
use pokebot_video::{ImageSequenceSource, Normalizer, Rect, ViewportLocator};

#[derive(Debug, Clone)]
pub enum VideoSpec {
    Emulator,
    CaptureCard(PathBuf),
    Replay(PathBuf),
    Images(PathBuf),
}

impl FromStr for VideoSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.split_once(':') {
            None if s == "emulator" => Ok(Self::Emulator),
            Some(("capture-card", dev)) => Ok(Self::CaptureCard(dev.into())),
            Some(("replay", dir)) => Ok(Self::Replay(dir.into())),
            Some(("images", dir)) => Ok(Self::Images(dir.into())),
            _ => Err(format!(
                "expected emulator | capture-card:<device> | replay:<session> | images:<dir>, got {s:?}"
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub enum ControllerSpec {
    Emulator,
    Pabotbase(PathBuf),
    /// ESP32-S3 WiFi controller firmware (or its simulator) at `host[:port]`.
    Esp32Wifi(String),
    Null,
}

impl FromStr for ControllerSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.split_once(':') {
            None if s == "emulator" => Ok(Self::Emulator),
            None if s == "null" => Ok(Self::Null),
            Some(("pabotbase", port)) => Ok(Self::Pabotbase(port.into())),
            Some(("esp32", address)) => Ok(Self::Esp32Wifi(address.into())),
            _ => Err(format!(
                "expected emulator | pabotbase:<serial port> | esp32:<host[:port]> | null, got {s:?}"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ViewportArg(pub Rect);

impl FromStr for ViewportArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let parts: Vec<u32> = s
            .split(',')
            .map(|p| {
                p.trim()
                    .parse()
                    .map_err(|_| format!("bad viewport {s:?}, expected x,y,w,h"))
            })
            .collect::<Result<_, _>>()?;
        let [x, y, width, height] = parts[..] else {
            return Err(format!("bad viewport {s:?}, expected x,y,w,h"));
        };
        Ok(Self(Rect {
            x,
            y,
            width,
            height,
        }))
    }
}

/// Capture card picture controls: `switch`, or `name=value,...`.
#[derive(Debug, Clone, Default)]
pub struct CardControlsArg(pub Vec<(String, i64)>);

impl FromStr for CardControlsArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        if s == "switch" {
            // Nintendo Switch into an MS2109 card: the Switch sends limited
            // range; these expand it and undo the card's default saturation
            // boost (180), matching mGBA's colours within ~4 levels.
            return Ok(Self(vec![
                ("brightness".into(), -18),
                ("contrast".into(), 150),
                ("saturation".into(), 155),
                ("hue".into(), 0),
            ]));
        }
        s.split(',')
            .map(|kv| {
                let (k, v) = kv
                    .split_once('=')
                    .ok_or_else(|| format!("bad control {kv:?}, expected name=value"))?;
                let v = v
                    .trim()
                    .parse()
                    .map_err(|_| format!("bad value in {kv:?}"))?;
                Ok((k.trim().to_owned(), v))
            })
            .collect::<Result<_, _>>()
            .map(Self)
    }
}

/// Which game the emulator runs.
#[derive(Debug, Clone, clap::Args)]
pub struct EmulatorArgs {
    /// libretro core [env: VPB_CORE] (default: emulator/mgba_libretro.so)
    #[arg(long)]
    pub core: Option<PathBuf>,
    /// Game ROM [env: VPB_ROM] (default: first *.gba in roms/)
    #[arg(long)]
    pub rom: Option<PathBuf>,
    /// Cartridge save file (default: ROM path with .sav extension)
    #[arg(long)]
    pub save: Option<PathBuf>,
    /// Do not load or write a cartridge save
    #[arg(long)]
    pub no_save: bool,
}

#[derive(Debug, Clone, clap::Args)]
pub struct DeviceArgs {
    /// Video device: emulator | capture-card:<device> | replay:<session> | images:<dir>
    #[arg(long, default_value = "emulator")]
    pub video: VideoSpec,
    /// Controller device: emulator | pabotbase:<serial port> | esp32:<host[:port]> | null
    #[arg(long, default_value = "emulator")]
    pub controller: ControllerSpec,
    /// Game viewport inside captured frames as x,y,w,h (default: whole frame)
    #[arg(long)]
    pub viewport: Option<ViewportArg>,
    /// Capture card picture controls: `switch` (calibrated for a Switch into
    /// an MS2109 card) or `name=value,...` (see `v4l2-ctl --list-ctrls`)
    #[arg(long)]
    pub card_controls: Option<CardControlsArg>,
    /// Extra frames actions may take to show on screen (default: 0 for the
    /// emulator, 30 for real hardware)
    #[arg(long)]
    pub latency_frames: Option<u64>,
    /// Serial baud rate for pabotbase (default: try 921600, then 115200)
    #[arg(long)]
    pub baud: Option<u32>,
    #[command(flatten)]
    pub emulator: EmulatorArgs,
    /// Run the in-process emulator at console speed instead of one frame per read
    #[arg(long)]
    pub realtime: bool,
}

impl DeviceArgs {
    /// Input-to-screen latency allowance for the executor.
    pub fn latency_frames(&self) -> u64 {
        self.latency_frames.unwrap_or(match self.controller {
            ControllerSpec::Emulator | ControllerSpec::Null => 0,
            _ => 30,
        })
    }
}

pub fn open(args: &DeviceArgs) -> Result<Devices> {
    let normalizer = Normalizer::new(match args.viewport {
        Some(ViewportArg(rect)) => ViewportLocator::Fixed(rect),
        None => ViewportLocator::FullFrame,
    });
    let mut emulator_controller: Option<pokebot_emulator_libretro::EmulatorController> = None;
    let (video, video_name): (Box<dyn VideoSource + Send>, String) = match &args.video {
        VideoSpec::Emulator => {
            let mut config = emulator_config(&args.emulator)?;
            config.clock = if args.realtime {
                ClockMode::RealTime
            } else {
                ClockMode::Stepped
            };
            let (video, controller, info) = launch(config).context("starting emulator")?;
            emulator_controller = Some(controller);
            let name = format!("emulator:{} {}", info.library_name, info.library_version);
            eprintln!(
                "{name}: {}x{} @ {:.4} fps",
                info.width, info.height, info.fps
            );
            (Box::new(video), name)
        }
        VideoSpec::CaptureCard(device) => {
            let source = CaptureCardVideoSource::open(CaptureCardConfig {
                device: device.clone(),
                size: None,
                controls: args.card_controls.clone().unwrap_or_default().0,
            })?;
            let name = format!("capture-card:{}", source.description());
            eprintln!("{name}");
            (Box::new(source), name)
        }
        VideoSpec::Replay(dir) => {
            let session =
                Session::open(dir).with_context(|| format!("opening {}", dir.display()))?;
            (
                Box::new(session.video_source(0)),
                format!("replay:{}", dir.display()),
            )
        }
        VideoSpec::Images(dir) => (
            Box::new(ImageSequenceSource::open(dir)?),
            format!("images:{}", dir.display()),
        ),
    };
    let persist_save = emulator_controller.clone().map(|c| {
        Box::new(move || c.flush_battery_save()) as Box<dyn Fn() -> pokebot_core::Result<()> + Send>
    });
    let (controller, controller_name): (Box<dyn Controller + Send>, String) = match &args.controller
    {
        ControllerSpec::Emulator => match emulator_controller {
            Some(controller) => (Box::new(controller), "emulator".into()),
            None => bail!("--controller emulator requires --video emulator"),
        },
        ControllerSpec::Pabotbase(port) => {
            let mut config = PabotBaseConfig::new(port);
            if let Some(baud) = args.baud {
                config.baud_rates = vec![baud];
            }
            let controller = PabotBaseController::open(config)
                .with_context(|| format!("connecting to {}", port.display()))?;
            let info = controller.info();
            let name = format!(
                "pabotbase:{} \"{}\" {:?} @ {} baud",
                port.display(),
                info.name,
                info.controller,
                info.baud
            );
            eprintln!(
                "{name} (firmware {}, queue {})",
                info.firmware, info.queue_capacity
            );
            (Box::new(controller), name)
        }
        ControllerSpec::Esp32Wifi(address) => {
            let controller = Esp32WifiController::connect(Esp32WifiConfig::new(address))
                .with_context(|| format!("connecting to {address}"))?;
            let info = controller.info();
            let name = format!(
                "esp32:{} \"{}\" {} (firmware {})",
                controller.peer(),
                info.name,
                info.controller,
                info.firmware
            );
            eprintln!("{name}");
            // The device is a full Switch controller; the bot plays GBA on it.
            (Box::new(GbaOnSwitch(controller)), name)
        }
        ControllerSpec::Null => (
            Box::new(NullController::new(PressProfile::default())),
            "null".into(),
        ),
    };
    Ok(Devices {
        video,
        controller,
        normalizer,
        video_name,
        controller_name,
        persist_save,
    })
}

pub fn emulator_config(args: &EmulatorArgs) -> Result<EmulatorConfig> {
    let root = workspace_root();
    let core = args
        .core
        .clone()
        .or_else(|| std::env::var_os("VPB_CORE").map(PathBuf::from))
        .unwrap_or_else(|| root.join("emulator/mgba_libretro.so"));
    if !core.exists() {
        bail!(
            "libretro core not found at {} (run tools/fetch-emulator.sh)",
            core.display()
        );
    }
    let rom = match args
        .rom
        .clone()
        .or_else(|| std::env::var_os("VPB_ROM").map(PathBuf::from))
    {
        Some(rom) => rom,
        None => first_rom(&root.join("roms"))?,
    };
    let mut config = EmulatorConfig::new(core, &rom);
    config.battery_save = match (&args.save, args.no_save) {
        (_, true) => None,
        (Some(save), false) => Some(save.clone()),
        (None, false) => Some(rom.with_extension("sav")),
    };
    Ok(config)
}

fn first_rom(dir: &Path) -> Result<PathBuf> {
    let mut roms: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("no --rom given and cannot read {}", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("gba")))
        .collect();
    roms.sort();
    roms.into_iter()
        .next()
        .with_context(|| format!("no --rom given and no .gba file in {}", dir.display()))
}

/// Repository root when run via cargo, otherwise the current directory.
fn workspace_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    if manifest.join("Cargo.toml").exists() {
        manifest
    } else {
        PathBuf::from(".")
    }
}
