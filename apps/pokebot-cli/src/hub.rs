//! `pokebot hub`: the always-on front door on port 8080 that puts every bot
//! instance's web UI under one address (`/switch/`, `/emulators/`). The proxy
//! itself lives in `pokebot_telemetry::hub_proxy`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::fleet::{Fleet, FleetArgs};
use anyhow::Result;
use pokebot_telemetry::hub_proxy;

#[derive(Debug, clap::Args)]
pub struct HubArgs {
    /// Address to serve on
    #[arg(long, default_value = "0.0.0.0:8080")]
    listen: SocketAddr,
    /// Instance files written by tools/live-run.sh
    /// [default: $POKEBOT_INSTANCES_DIR, else /tmp/pokebot-instances]
    #[arg(long)]
    instances_dir: Option<PathBuf>,
    #[command(flatten)]
    fleet: FleetArgs,
    #[command(flatten)]
    switch: crate::switch_device::SwitchArgs,
}

pub fn run(args: HubArgs, stop: Arc<AtomicBool>) -> Result<()> {
    let dir = hub_proxy::resolve_instances_dir(args.instances_dir);
    let fleet = Arc::new(Fleet::new(args.fleet, dir.clone())?);
    let _switch = crate::switch_device::SwitchDevice::start(args.switch, dir.clone())?;
    let monitor = Arc::clone(&fleet);
    let monitor_stop = Arc::new(AtomicBool::new(false));
    let done = Arc::clone(&monitor_stop);
    let reaper = std::thread::spawn(move || {
        while !done.load(std::sync::atomic::Ordering::Relaxed) {
            monitor.reap();
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });
    let result = hub_proxy::run_blocking_with_fleet(args.listen, dir, stop, Some(fleet.clone()));
    monitor_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = reaper.join();
    fleet.shutdown();
    result?;
    Ok(())
}
