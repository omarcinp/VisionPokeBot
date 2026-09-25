//! End to end over loopback TCP: the bot's controllers against the firmware's
//! device code, with a sink that records what the Switch would see. Covers
//! both faces of the same device: the full Switch layout, and the GBA
//! `Controller` through the `GbaOnSwitch` adapter.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pokebot_core::{
    Button, ButtonSet, Controller, ControllerCommand, GbaOnSwitch, Stick, SwitchButton,
    SwitchCommand, SwitchController, SwitchState, TimedInput,
};
use pokebot_esp32_wifi::{Esp32WifiConfig, Esp32WifiController};
use pokebot_remote::{Device, DeviceConfig, HidSink, SwitchReport};

type Log = Arc<Mutex<Vec<(Instant, SwitchState)>>>;

/// Records every change of controller state.
struct Recorder(Log);

impl HidSink for Recorder {
    fn send(&mut self, report: &SwitchReport) -> bool {
        let state = report.to_state();
        let mut log = self.0.lock().unwrap();
        if log.last().map(|(_, s)| *s) != Some(state) {
            log.push((Instant::now(), state));
        }
        true
    }

    fn mounted(&self) -> bool {
        true
    }
}

fn start() -> (Esp32WifiController, Log) {
    let log = Log::default();
    let device = Device::start(
        DeviceConfig::new("test", [127, 0, 0, 1].into(), 0, 0),
        Recorder(Arc::clone(&log)),
    )
    .unwrap();
    let controller =
        Esp32WifiController::connect(Esp32WifiConfig::new(device.control_addr().to_string()))
            .unwrap();
    (controller, log)
}

fn wait_idle(is_idle: impl Fn() -> bool, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !is_idle() {
        assert!(Instant::now() < deadline, "controller never became idle");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn states(log: &Log) -> Vec<SwitchState> {
    log.lock().unwrap().iter().map(|(_, s)| *s).collect()
}

/// What the GBA app sees.
fn gba_changes(log: &Log) -> Vec<ButtonSet> {
    let mut out: Vec<ButtonSet> = Vec::new();
    for state in states(log) {
        let buttons = state.gba_buttons();
        if out.last() != Some(&buttons) {
            out.push(buttons);
        }
    }
    out
}

// ---- GBA controller through the adapter ----

#[test]
fn gba_press_is_held_for_the_profile_and_completes() {
    let (controller, log) = start();
    assert_eq!(controller.info().protocol, 2);
    let mut gba = GbaOnSwitch(controller);
    let receipt = gba.execute(ControllerCommand::Press(Button::A)).unwrap();
    assert_eq!(receipt.input_duration, Duration::from_millis(160));
    assert!(!gba.is_idle().unwrap());
    wait_idle(|| gba.is_idle().unwrap(), Duration::from_secs(2));
    assert_eq!(
        gba_changes(&log),
        [ButtonSet::NONE, Button::A.into(), ButtonSet::NONE]
    );
    let log = log.lock().unwrap();
    let held = log[2].0 - log[1].0;
    assert!(
        held >= Duration::from_millis(79) && held < Duration::from_millis(100),
        "A held for {held:?}"
    );
}

#[test]
fn gba_start_and_select_land_on_plus_and_minus() {
    let (controller, log) = start();
    let mut gba = GbaOnSwitch(controller);
    gba.execute(ControllerCommand::Chord("Start+Select".parse().unwrap()))
        .unwrap();
    wait_idle(|| gba.is_idle().unwrap(), Duration::from_secs(2));
    assert_eq!(states(&log)[1].buttons.to_string(), "Minus+Plus");
}

#[test]
fn gba_sequences_play_in_order() {
    let (controller, log) = start();
    let mut gba = GbaOnSwitch(controller);
    let step = |buttons: &str| TimedInput {
        buttons: buttons.parse().unwrap(),
        duration: Duration::from_millis(20),
    };
    gba.execute(ControllerCommand::Sequence(vec![
        step("Up"),
        step("Up+B"),
        step("none"),
    ]))
    .unwrap();
    gba.execute(ControllerCommand::Press(Button::Start))
        .unwrap();
    wait_idle(|| gba.is_idle().unwrap(), Duration::from_secs(2));
    assert_eq!(
        gba_changes(&log),
        [
            ButtonSet::NONE,
            Button::Up.into(),
            "Up+B".parse().unwrap(),
            ButtonSet::NONE,
            Button::Start.into(),
            ButtonSet::NONE,
        ]
    );
}

#[test]
fn gba_neutral_cancels_a_long_hold() {
    let (controller, log) = start();
    let mut gba = GbaOnSwitch(controller);
    gba.execute(ControllerCommand::Hold {
        buttons: Button::Right.into(),
        duration: Duration::from_secs(30),
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(20));
    gba.execute(ControllerCommand::Neutral).unwrap();
    wait_idle(|| gba.is_idle().unwrap(), Duration::from_millis(500));
    assert_eq!(
        gba_changes(&log),
        [ButtonSet::NONE, Button::Right.into(), ButtonSet::NONE]
    );
}

// ---- full Switch layout ----

#[test]
fn switch_only_buttons_and_sticks_reach_the_report() {
    let (mut controller, log) = start();
    controller
        .execute(SwitchCommand::Press(SwitchButton::Home))
        .unwrap();
    let tilt = SwitchState {
        buttons: SwitchButton::ZL.into(),
        left_stick: Stick::UP,
        right_stick: Stick { x: 30, y: 200 },
    };
    controller
        .execute(SwitchCommand::Hold {
            state: tilt,
            duration: Duration::from_millis(50),
        })
        .unwrap();
    wait_idle(|| controller.is_idle().unwrap(), Duration::from_secs(2));
    assert_eq!(
        states(&log),
        [
            SwitchState::NEUTRAL,
            SwitchButton::Home.into(),
            SwitchState::NEUTRAL,
            tilt,
            SwitchState::NEUTRAL,
        ]
    );
    // None of that is a GBA button.
    assert_eq!(gba_changes(&log), [ButtonSet::NONE]);
}

#[test]
fn status_reports_queue_and_usb() {
    let (mut controller, _log) = start();
    controller
        .execute(SwitchCommand::Press(SwitchButton::X))
        .unwrap();
    controller
        .execute(SwitchCommand::Press(SwitchButton::Y))
        .unwrap();
    let status = controller.status().unwrap();
    assert!(!status.idle);
    assert!(status.usb_mounted);
    wait_idle(|| controller.is_idle().unwrap(), Duration::from_secs(2));
    assert!(controller.status().unwrap().idle);
}

#[test]
fn old_protocol_is_refused() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        use std::io::Write;
        writeln!(
            stream,
            r#"{{"type":"hello","name":"x","firmware":"0","protocol":1,"controller":"hori-pokken-wired"}}"#
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
    });
    let err = Esp32WifiController::connect(Esp32WifiConfig::new(addr.to_string()))
        .err()
        .expect("protocol 1 must be refused");
    assert!(err.to_string().contains("protocol 1"), "{err}");
}

// ---- keepalive ----

/// One HTTP request to the device; the response body.
fn http(addr: std::net::SocketAddr, method: &str, path: &str, body: &str) -> String {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: device\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default()
}

#[test]
fn keepalive_nudges_an_idle_switch_and_steps_aside_for_the_bot() {
    let log = Log::default();
    let mut config = DeviceConfig::new("test", [127, 0, 0, 1].into(), 0, 0);
    config.keepalive = Some(pokebot_remote::Keepalive {
        after: Duration::from_millis(300),
        routine: pokebot_remote::Keepalive::right_stick_nudge(),
    });
    let device = Device::start(config, Recorder(Arc::clone(&log))).unwrap();
    let mut controller =
        Esp32WifiController::connect(Esp32WifiConfig::new(device.control_addr().to_string()))
            .unwrap();

    std::thread::sleep(Duration::from_millis(500));
    let up = SwitchState {
        right_stick: Stick::UP,
        ..SwitchState::NEUTRAL
    };
    assert!(
        states(&log).contains(&up),
        "no keepalive: {:?}",
        states(&log)
    );
    assert!(states(&log).iter().all(|s| s.buttons.is_empty()));
    assert!(device.status().keepalives >= 1);
    assert_eq!(device.status().keepalive_secs, Some(0));

    // The bot's commands still complete (keepalives are never reported to
    // it as finished commands of its own).
    controller
        .execute(SwitchCommand::Press(SwitchButton::A))
        .unwrap();
    wait_idle(|| controller.is_idle().unwrap(), Duration::from_secs(2));

    // Turned off over HTTP: no more nudges.
    let body = http(
        device.http_addr(),
        "POST",
        "/api/keepalive",
        r#"{"after_secs": null}"#,
    );
    assert!(body.contains(r#""after_secs":null"#), "{body}");
    let played = device.status().keepalives;
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(device.status().keepalives, played);

    // And back on, keeping the routine.
    let body = http(
        device.http_addr(),
        "POST",
        "/api/keepalive",
        r#"{"after_secs": 240}"#,
    );
    assert!(body.contains(r#""after_secs":240"#), "{body}");
    assert!(body.contains("right_stick"), "{body}");
    let status = http(device.http_addr(), "GET", "/api/status", "");
    assert!(status.contains(r#""keepalive_secs":240"#), "{status}");
}

// ---- console link, remote wakeup, reconnection ----

/// A sink whose host can be put to sleep; counts wakeup requests.
struct Sleepy {
    suspended: Arc<std::sync::atomic::AtomicBool>,
    wakes: Arc<std::sync::atomic::AtomicU32>,
}

impl HidSink for Sleepy {
    fn send(&mut self, _: &SwitchReport) -> bool {
        true
    }

    fn mounted(&self) -> bool {
        true
    }

    fn suspended(&self) -> bool {
        self.suspended.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn wake(&mut self) {
        self.wakes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[test]
fn console_link_and_remote_wakeup_on_a_button_only() {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    let suspended = Arc::new(AtomicBool::new(false));
    let wakes = Arc::new(AtomicU32::new(0));
    let device = Device::start(
        DeviceConfig::new("test", [127, 0, 0, 1].into(), 0, 0),
        Sleepy {
            suspended: Arc::clone(&suspended),
            wakes: Arc::clone(&wakes),
        },
    )
    .unwrap();
    let mut controller =
        Esp32WifiController::connect(Esp32WifiConfig::new(device.control_addr().to_string()))
            .unwrap();
    assert_eq!(
        controller.console_link().unwrap(),
        pokebot_core::ConsoleLink::Attached
    );

    suspended.store(true, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(
        controller.console_link().unwrap(),
        pokebot_core::ConsoleLink::Suspended
    );
    // A stick move (what the keepalive sends) doesn't wake it...
    controller
        .execute(SwitchCommand::Hold {
            state: SwitchState {
                right_stick: Stick::UP,
                ..SwitchState::NEUTRAL
            },
            duration: Duration::from_millis(50),
        })
        .unwrap();
    wait_idle(|| controller.is_idle().unwrap(), Duration::from_secs(2));
    assert_eq!(wakes.load(Ordering::Relaxed), 0);
    // ...a button does, at most once per 100 ms while held.
    controller
        .execute(SwitchCommand::Press(SwitchButton::Home))
        .unwrap();
    wait_idle(|| controller.is_idle().unwrap(), Duration::from_secs(2));
    assert_eq!(wakes.load(Ordering::Relaxed), 1);
}
