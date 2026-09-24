//! The PC-side PABotBase2 client talking to the virtual ESP32 through a real
//! pseudo-terminal: the same path the bot takes to the emulator (and, with a
//! different port, to real hardware).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pokebot_core::{Button, ButtonSet, Controller, ControllerCommand, TimedInput};
use pokebot_pabotbase::device::ButtonSink;
use pokebot_pabotbase::{ControllerKind, PabotBaseConfig, PabotBaseController};
use pokebot_virtual_console::{spawn_virtual_esp32, VirtualSerialPort};

/// Records holds and completes them after their duration in wall time.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<(ButtonSet, u16, Instant)>>>);

impl ButtonSink for Recorder {
    fn hold(&mut self, buttons: ButtonSet, ms: u16) -> u64 {
        let mut holds = self.0.lock().unwrap();
        let start = holds.last().map_or(Instant::now(), |(_, d, at)| {
            (*at + Duration::from_millis(u64::from(*d))).max(Instant::now())
        });
        holds.push((buttons, ms, start));
        holds.len() as u64 - 1
    }

    fn completed_through(&self) -> Option<u64> {
        let holds = self.0.lock().unwrap();
        let now = Instant::now();
        holds
            .iter()
            .rposition(|(_, ms, at)| *at + Duration::from_millis(u64::from(*ms)) <= now)
            .map(|i| i as u64)
    }

    fn cancel(&mut self) {}
}

#[test]
fn client_handshakes_and_commands_reach_the_device() {
    let port = VirtualSerialPort::create(None).unwrap();
    let path = port.path().to_path_buf();
    let recorder = Recorder::default();
    let (log_tx, log_rx) = std::sync::mpsc::channel();
    let _device = spawn_virtual_esp32(port, recorder.clone(), "test device", log_tx).unwrap();

    let mut controller = PabotBaseController::open(PabotBaseConfig::new(&path)).expect("handshake");
    let info = controller.info().clone();
    assert_eq!(info.name, "test device");
    assert_eq!(info.controller, ControllerKind::WirelessProController);
    assert_eq!(info.queue_capacity, 32);
    assert_eq!(info.controllers, vec![0x1180, 0x1000]);

    controller
        .execute(ControllerCommand::Press(Button::A))
        .unwrap();
    controller
        .execute(ControllerCommand::Sequence(vec![
            TimedInput {
                buttons: "B+Up".parse().unwrap(),
                duration: Duration::from_millis(30),
            },
            TimedInput {
                buttons: ButtonSet::NONE,
                duration: Duration::from_millis(20),
            },
        ]))
        .unwrap();
    // 40 more commands than the device queue holds: the client must pace them.
    for _ in 0..40 {
        controller
            .execute(ControllerCommand::Hold {
                buttons: Button::Start.into(),
                duration: Duration::from_millis(1),
            })
            .unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !controller.is_idle().unwrap() {
        assert!(Instant::now() < deadline, "commands never finished");
        std::thread::sleep(Duration::from_millis(5));
    }
    let holds: Vec<(ButtonSet, u16)> = recorder
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|(b, ms, _)| (*b, *ms))
        .collect();
    assert_eq!(holds.len(), 44);
    assert_eq!(holds[0], (Button::A.into(), 80));
    assert_eq!(holds[1], (ButtonSet::NONE, 80));
    assert_eq!(holds[2], ("B+Up".parse().unwrap(), 30));
    assert_eq!(holds[3], (ButtonSet::NONE, 20));
    assert!(holds[4..].iter().all(|h| *h == (Button::Start.into(), 1)));

    let log: Vec<String> = log_rx.try_iter().collect();
    assert!(
        log.iter().any(|l| l.starts_with("host connected")),
        "{log:?}"
    );

    // Reconnecting (new session) works while the device keeps running.
    drop(controller);
    let controller = PabotBaseController::open(PabotBaseConfig::new(&path)).expect("reconnect");
    assert_eq!(controller.info().name, "test device");
}
