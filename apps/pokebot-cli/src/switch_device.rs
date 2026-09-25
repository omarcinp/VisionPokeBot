//! Persistent Switch capture owned by the hub, independent of bot lifetime.
use crate::devices::{CaptureSizeArg, CardControlsArg, ControllerSpec, ViewportArg};
use anyhow::Result;
use pokebot_capture_card::{broker::CaptureBroker, CaptureCardConfig};
use pokebot_core::{
    ConsoleLink, Controller, ControllerCommand, ControllerReceipt, Error, GbaOnSwitch,
};
use pokebot_telemetry::{hub_proxy, GameControl, Telemetry};
use pokebot_video::{Normalizer, ViewportLocator};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

#[derive(Debug, clap::Args)]
pub struct SwitchArgs {
    /// Physical capture card; hotplug is retried while the hub runs.
    #[arg(long, default_value = "/dev/video0")]
    switch_device: PathBuf,
    /// Standby manual controller, e.g. esp32:10.10.100.185.
    #[arg(long)]
    switch_controller: Option<String>,
}

pub struct SwitchDevice {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl SwitchDevice {
    pub fn start(args: SwitchArgs, registry: PathBuf) -> Result<Self> {
        let config: Value = std::fs::read(registry.join("switch-config.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Value::Null);
        let device = config["device"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or(args.switch_device);
        let size = config["capture_size"]
            .as_str()
            .and_then(|v| v.parse::<CaptureSizeArg>().ok())
            .map(|s| (s.0, s.1));
        let controls = config["card_controls"]
            .as_str()
            .and_then(|v| v.parse::<CardControlsArg>().ok())
            .unwrap_or_default()
            .0;
        let viewport = config["viewport"]
            .as_str()
            .and_then(|v| v.parse::<ViewportArg>().ok());
        let normalizer = Normalizer::new(
            viewport.map_or(ViewportLocator::FullFrame, |v| ViewportLocator::Fixed(v.0)),
        );
        let controller = args
            .switch_controller
            .or_else(|| config["controller"].as_str().map(str::to_owned));
        let broker = CaptureBroker::start(CaptureCardConfig {
            device: device.clone(),
            size,
            controls,
        })?;
        let mut telemetry = Telemetry::new(
            &format!("capture-card:{}", device.display()),
            controller.as_deref().unwrap_or("unconfigured"),
        );
        if let Some(spec) = controller {
            telemetry = telemetry.with_control(GameControl::new(
                Box::new(LazyController { spec, inner: None }),
                false,
            ));
        }
        telemetry.info("Switch capture is independent of the bot");
        let server = pokebot_telemetry::serve(telemetry.clone(), "127.0.0.1:0".parse()?, "Switch")?;
        let manifest = registry.join("switch-device.json");
        hub_proxy::register_worker(&manifest, "Switch capture", server.addr.port())?;
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut last = None;
            let mut connected = None;
            while !done.load(Ordering::Relaxed) {
                let frame = broker.latest();
                let active = (device.exists(), frame.is_some());
                if connected != Some(active) {
                    connected = Some(active);
                    if let Ok(bytes) = std::fs::read(&manifest) {
                        if let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) {
                            value["connected"] = json!(active.0);
                            value["video_ready"] = json!(active.1);
                            let temp = manifest.with_extension("tmp");
                            if std::fs::write(&temp, value.to_string()).is_ok() {
                                let _ = std::fs::rename(temp, &manifest);
                            }
                        }
                    }
                    if !active.1 {
                        telemetry.clear_preview();
                    }
                }
                if let Some(frame) = frame.filter(|f| Some(f.frame_id) != last) {
                    last = Some(frame.frame_id);
                    match normalizer.normalize(&frame) {
                        Ok(f) => telemetry.publish_preview(f.frame_id, f.image().clone()),
                        Err(error) => telemetry.error(error.to_string()),
                    }
                }
                std::thread::sleep(Duration::from_millis(33));
            }
            drop(broker);
            let _ = std::fs::remove_file(manifest);
        });
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}
impl Drop for SwitchDevice {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Viewing a card does not require the controller to be online. Connect only
/// when a browser requests input, and retry on the next request after failure.
struct LazyController {
    spec: String,
    inner: Option<Box<dyn Controller + Send>>,
}
impl LazyController {
    fn get(&mut self) -> pokebot_core::Result<&mut Box<dyn Controller + Send>> {
        if self.inner.is_none() {
            self.inner = Some(
                match self
                    .spec
                    .parse::<ControllerSpec>()
                    .map_err(Error::InvalidData)?
                {
                    ControllerSpec::Esp32Wifi(address) => Box::new(GbaOnSwitch(
                        pokebot_esp32_wifi::Esp32WifiController::connect(
                            pokebot_esp32_wifi::Esp32WifiConfig::new(address),
                        )?,
                    )),
                    ControllerSpec::Pabotbase(path) => {
                        Box::new(pokebot_pabotbase::PabotBaseController::open(
                            pokebot_pabotbase::PabotBaseConfig::new(path),
                        )?)
                    }
                    _ => {
                        return Err(Error::Unsupported(
                            "Configure a Switch controller with --switch-controller".into(),
                        ))
                    }
                },
            );
        }
        Ok(self.inner.as_mut().unwrap())
    }
}
impl Controller for LazyController {
    fn execute(&mut self, command: ControllerCommand) -> pokebot_core::Result<ControllerReceipt> {
        let result = self.get()?.execute(command);
        if result.is_err() {
            self.inner = None;
        }
        result
    }
    fn is_idle(&self) -> pokebot_core::Result<bool> {
        self.inner.as_ref().map_or(Ok(true), |c| c.is_idle())
    }
    fn console_link(&mut self) -> pokebot_core::Result<ConsoleLink> {
        self.get()?.console_link()
    }
}
