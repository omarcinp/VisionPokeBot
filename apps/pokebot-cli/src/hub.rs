//! `pokebot hub`: the always-on front door on port 8080 that puts every bot
//! instance's web UI under one address (`/switch/`, `/emu/`). The proxy
//! itself lives in `pokebot_telemetry::hub_proxy`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

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
}

pub fn run(args: HubArgs, stop: Arc<AtomicBool>) -> Result<()> {
    let dir = hub_proxy::resolve_instances_dir(args.instances_dir);
    hub_proxy::run_blocking(args.listen, dir, stop)?;
    Ok(())
}
