//! Host simulator for the ESP32-S3 controller: runs the firmware's device
//! code on this PC and prints every HID report the Switch would receive.
//!
//!     cargo run -p pokebot-remote --bin pokebot-remote-sim -- [--bind ADDR] [--control-port N] [--http-port N]
//!
//! Then point the bot at it with `--controller esp32:127.0.0.1`, or open the
//! control page at http://127.0.0.1:8078/.

use std::net::IpAddr;
use std::process::ExitCode;
use std::time::Instant;

use pokebot_remote::protocol::CONTROL_PORT;
use pokebot_remote::{Device, DeviceConfig, HidSink, SwitchReport};

/// Prints reports as they change.
struct PrintSink {
    started: Instant,
    last: Option<SwitchReport>,
}

impl HidSink for PrintSink {
    fn send(&mut self, report: &SwitchReport) -> bool {
        if self.last != Some(*report) {
            let bytes: Vec<String> = report
                .to_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let state = report.to_state();
            println!(
                "{:>10.3} ms  {:<24} L({:>3},{:>3}) R({:>3},{:>3})  [{}]",
                self.started.elapsed().as_secs_f64() * 1000.0,
                state.buttons.to_string(),
                state.left_stick.x,
                state.left_stick.y,
                state.right_stick.x,
                state.right_stick.y,
                bytes.join(" ")
            );
            self.last = Some(*report);
        }
        true
    }

    fn mounted(&self) -> bool {
        true
    }
}

fn main() -> ExitCode {
    let mut bind: IpAddr = [127, 0, 0, 1].into();
    let mut control_port = CONTROL_PORT;
    let mut http_port = 8078;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_default();
        let ok = match arg.as_str() {
            "--bind" => value().parse().map(|v| bind = v).is_ok(),
            "--control-port" => value().parse().map(|v| control_port = v).is_ok(),
            "--http-port" => value().parse().map(|v| http_port = v).is_ok(),
            _ => false,
        };
        if !ok {
            eprintln!("usage: pokebot-remote-sim [--bind ADDR] [--control-port N] [--http-port N]");
            return ExitCode::FAILURE;
        }
    }
    let config = DeviceConfig::new(
        "PokeBot controller simulator",
        bind,
        control_port,
        http_port,
    );
    let sink = PrintSink {
        started: Instant::now(),
        last: None,
    };
    let device = match Device::start(config, sink) {
        Ok(device) => device,
        Err(e) => {
            eprintln!("cannot start: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "control: {} (--controller esp32:{})",
        device.control_addr(),
        device.control_addr()
    );
    eprintln!("http:    http://{}/", device.http_addr());
    loop {
        std::thread::park();
    }
}
