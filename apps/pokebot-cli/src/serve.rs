//! `pokebot emulator serve`: runs the emulator as a stand-alone "console"
//! that exposes the same interfaces as real hardware — video on a V4L2
//! device, controller input over a PABotBase2 serial port and over the ESP32-S3
//! WiFi controller protocol.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use pokebot_core::{VideoSource, CANONICAL_HEIGHT, CANONICAL_WIDTH};
use pokebot_emulator_libretro::{launch, ClockMode};
use pokebot_remote::protocol::CONTROL_PORT;
use pokebot_remote::{Device, DeviceConfig};
use pokebot_virtual_console::{
    spawn_virtual_esp32, EmulatorButtons, EmulatorHid, V4l2Sink, VirtualSerialPort,
};

use crate::devices::{emulator_config, EmulatorArgs};

#[derive(Debug, clap::Args)]
pub struct ServeArgs {
    #[command(flatten)]
    pub emulator: EmulatorArgs,
    /// V4L2 output device to write video to (see tools/setup-virtual-camera.sh)
    #[arg(long, default_value = "/dev/video10")]
    pub video_out: PathBuf,
    /// Integer upscale of the 240x160 picture, like a console's HDMI output
    #[arg(long, default_value_t = 3)]
    pub scale: u32,
    /// Path of the virtual ESP32 serial port (a symlink to a pseudo-terminal)
    #[arg(long, default_value = "/tmp/pokebot-esp32")]
    pub serial: PathBuf,
    /// Address the virtual WiFi controller listens on
    #[arg(long, default_value = "127.0.0.1")]
    pub wifi_bind: IpAddr,
    /// Control port of the virtual WiFi controller (0 disables it)
    #[arg(long, default_value_t = CONTROL_PORT)]
    pub wifi_port: u16,
    /// HTTP port of the virtual WiFi controller's API and control page
    #[arg(long, default_value_t = 8078)]
    pub wifi_http_port: u16,
}

/// How often the cartridge save is written to disk while serving.
const SAVE_FLUSH_INTERVAL: Duration = Duration::from_secs(60);

pub fn serve(args: ServeArgs, stop: Arc<AtomicBool>) -> Result<()> {
    let mut config = emulator_config(&args.emulator)?;
    config.clock = ClockMode::RealTime;
    let (mut video, controller, info) = launch(config).context("starting emulator")?;
    eprintln!(
        "emulator: {} {} @ {:.4} fps",
        info.library_name, info.library_version, info.fps
    );
    if let Some(addr) = info.link_addr {
        eprintln!("link:       listening on {addr} -> --link-connect {addr}");
    }

    let mut sink = V4l2Sink::open(
        &args.video_out,
        CANONICAL_WIDTH,
        CANONICAL_HEIGHT,
        args.scale,
    )
    .with_context(|| {
        format!(
            "opening {} (run tools/setup-virtual-camera.sh)",
            args.video_out.display()
        )
    })?;
    let (w, h) = sink.size();
    eprintln!(
        "video:      {} ({w}x{h} RGB24) -> --video capture-card:{}",
        args.video_out.display(),
        args.video_out.display()
    );

    let port =
        VirtualSerialPort::create(Some(&args.serial)).context("creating virtual serial port")?;
    eprintln!(
        "controller: {} (PABotBase2) -> --controller pabotbase:{}",
        port.path().display(),
        port.path().display()
    );
    let (log_tx, log_rx) = std::sync::mpsc::channel();
    let _wifi = if args.wifi_port == 0 {
        None
    } else {
        let config = DeviceConfig::new(
            "PokeBot virtual ESP32-S3 (emulator)",
            args.wifi_bind,
            args.wifi_port,
            args.wifi_http_port,
        );
        let device = Device::start(config, EmulatorHid::new(controller.clone()))
            .context("starting virtual WiFi controller")?;
        eprintln!(
            "controller: {} (WiFi controller protocol) -> --controller esp32:{}; http://{}/",
            device.control_addr(),
            device.control_addr(),
            device.http_addr()
        );
        Some(device)
    };
    let _esp32 = spawn_virtual_esp32(
        port,
        EmulatorButtons(controller.clone()),
        "PokeBot virtual ESP32 (emulator)",
        log_tx,
    )?;
    eprintln!("serving; Ctrl-C to stop");

    let mut frames = 0u64;
    let mut commands = 0u64;
    let mut last_report = Instant::now();
    // The cartridge save is written on exit, which a crash skips: on
    // 2026-09-25 the console died ("no new frame for 2s") after seven
    // hours of play and the .sav on disk was the one it had loaded, so
    // every in-game save of the day was lost. Write it every minute too
    // (only when it changed).
    let mut last_flush = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        if last_flush.elapsed() >= SAVE_FLUSH_INTERVAL {
            last_flush = Instant::now();
            if let Err(e) = controller.flush_battery_save() {
                eprintln!("cartridge save: {e}");
            }
        }
        let frame = video.next_frame()?;
        sink.write(&frame.image)?;
        frames += 1;
        for line in log_rx.try_iter() {
            if line.starts_with("cmd #") {
                commands += 1;
            } else {
                eprintln!("esp32: {line}");
            }
        }
        if last_report.elapsed() >= Duration::from_secs(10) {
            let secs = last_report.elapsed().as_secs_f64();
            eprintln!(
                "frame #{} | {:.1} fps | {commands} controller commands",
                frame.frame_id,
                frames as f64 / secs
            );
            (frames, commands, last_report) = (0, 0, Instant::now());
        }
    }
    eprintln!("stopping (cartridge save is written on exit)");
    Ok(())
}
