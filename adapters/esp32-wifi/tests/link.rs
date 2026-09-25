//! A lost link never ends a task by itself: the client against a scripted
//! fake device (one script per connection) that drops replies, hangs up
//! mid-command, or goes away. Regression for the Switch goal run of
//! 2026-09-25, where one `no reply to request 622` ended a whole cycle.

use std::time::{Duration, Instant};

use pokebot_core::{SwitchButton, SwitchCommand, SwitchController};
use pokebot_esp32_wifi::{Esp32WifiConfig, Esp32WifiController, LOCAL_ID};
use pokebot_remote::protocol::ClientMessage;

mod fake {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::mpsc::{channel, Sender};
    use std::thread::JoinHandle;

    use pokebot_remote::protocol::{ClientMessage, DeviceInfo, DeviceMessage, PROTOCOL_VERSION};
    use pokebot_remote::Status;

    /// One accepted connection; what the script reads on it is logged.
    pub struct Fake {
        stream: Option<TcpStream>,
        reader: BufReader<TcpStream>,
        pub seen: Vec<ClientMessage>,
    }

    impl Fake {
        fn line(&mut self, m: &DeviceMessage) {
            if let Some(stream) = &mut self.stream {
                let _ = writeln!(stream, "{}", serde_json::to_string(m).unwrap());
            }
        }

        pub fn hello(&mut self) {
            self.line(&DeviceMessage::Hello(DeviceInfo {
                name: "fake".into(),
                firmware: "0".into(),
                protocol: PROTOCOL_VERSION,
                controller: "fake".into(),
            }));
        }

        /// The next request, or `None` once the client hung up.
        pub fn read(&mut self) -> Option<ClientMessage> {
            let mut request = String::new();
            match self.reader.read_line(&mut request) {
                Ok(n) if n > 0 => {
                    let message = serde_json::from_str(&request).unwrap();
                    self.seen.push(message);
                    self.seen.last().cloned()
                }
                _ => None,
            }
        }

        pub fn accept(&mut self, seq: u64, id: u64, input_ms: u64) {
            self.line(&DeviceMessage::Accepted { seq, id, input_ms });
        }

        pub fn finish(&mut self, id: u64) {
            self.line(&DeviceMessage::Finished {
                id,
                cancelled: false,
            });
        }

        pub fn status(&mut self, seq: u64) {
            self.line(&DeviceMessage::Status(Status {
                seq: Some(seq),
                idle: true,
                pending: 0,
                usb_mounted: true,
                usb_suspended: false,
                keepalive_secs: None,
                keepalives: 0,
            }));
        }

        /// Reads one execute, accepts and finishes it; the command.
        pub fn serve(&mut self, id: u64) -> pokebot_core::SwitchCommand {
            let Some(ClientMessage::Execute { seq, command, .. }) = self.read() else {
                panic!("expected an execute")
            };
            self.accept(seq, id, 160);
            self.finish(id);
            command
        }

        /// Hangs up now (otherwise the socket stays open until `finish`).
        pub fn close(&mut self) {
            // The reader holds a clone of the socket: shut both down.
            if let Some(stream) = self.stream.take() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    pub struct Server {
        addr: SocketAddr,
        stop: Sender<()>,
        thread: JoinHandle<Vec<Vec<ClientMessage>>>,
    }

    impl Server {
        pub fn addr(&self) -> String {
            self.addr.to_string()
        }

        /// Closes every connection; the requests seen per connection.
        pub fn finish(self) -> Vec<Vec<ClientMessage>> {
            let _ = self.stop.send(());
            self.thread.join().unwrap()
        }
    }

    /// Runs `script` for each connection in turn until it returns `false`
    /// (then the listener closes: connections are refused).
    pub fn start(mut script: impl FnMut(usize, &mut Fake) -> bool + Send + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = channel();
        let thread = std::thread::spawn(move || {
            let mut parked: Vec<Fake> = Vec::new();
            let mut listener = Some(listener);
            let mut n = 0;
            while let Some(l) = &listener {
                let (stream, _) = l.accept().unwrap();
                let mut fake = Fake {
                    reader: BufReader::new(stream.try_clone().unwrap()),
                    stream: Some(stream),
                    seen: Vec::new(),
                };
                let more = script(n, &mut fake);
                parked.push(fake);
                n += 1;
                if !more {
                    listener = None;
                }
            }
            let _ = stopped.recv();
            parked.into_iter().map(|f| f.seen).collect()
        });
        Server { addr, stop, thread }
    }
}

fn wait_idle(is_idle: impl Fn() -> bool, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !is_idle() {
        assert!(Instant::now() < deadline, "controller never became idle");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn press(button: SwitchButton) -> SwitchCommand {
    SwitchCommand::Press(button)
}

fn is_execute(message: &ClientMessage, command: &SwitchCommand) -> bool {
    matches!(message, ClientMessage::Execute { command: c, .. } if c == command)
}

fn config(addr: String) -> Esp32WifiConfig {
    let mut config = Esp32WifiConfig::new(addr);
    config.reconnect_backoff = Duration::from_millis(50);
    config
}

/// Live (`no reply to request 622`): the device took a press and never
/// answered. The press is not sent again (it may have played); it gets a
/// local receipt, is inferred finished, and the next command reopens the
/// link with a neutral first.
#[test]
fn a_lost_accepted_is_not_resent_and_the_link_is_restored() {
    let server = fake::start(|n, fake| {
        fake.hello();
        match n {
            0 => {
                fake.read();
                true // silence, socket left open
            }
            _ => {
                assert_eq!(fake.serve(10), SwitchCommand::Neutral);
                assert_eq!(fake.serve(11), press(SwitchButton::B));
                false
            }
        }
    });
    let mut controller = Esp32WifiController::connect(config(server.addr())).unwrap();
    let started = Instant::now();
    let receipt = controller.execute(press(SwitchButton::A)).unwrap();
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "waited for the reply"
    );
    assert_ne!(receipt.command_id & LOCAL_ID, 0);
    assert_eq!(receipt.input_duration, Duration::from_millis(160));
    assert!(!controller.is_idle().unwrap(), "may still be playing");
    wait_idle(|| controller.is_idle().unwrap(), Duration::from_secs(3));
    assert_eq!(controller.inferred_finishes(), 1);
    assert_eq!(controller.reconnects(), 0);

    controller.execute(press(SwitchButton::B)).unwrap();
    wait_idle(|| controller.is_idle().unwrap(), Duration::from_secs(2));
    assert_eq!(controller.reconnects(), 1);
    drop(controller);
    let seen = server.finish();
    assert_eq!(seen[0].len(), 1);
    assert!(is_execute(&seen[0][0], &press(SwitchButton::A)));
    assert_eq!(seen[1].len(), 2, "{:?}", seen[1]);
    assert!(is_execute(&seen[1][0], &SwitchCommand::Neutral));
    assert!(is_execute(&seen[1][1], &press(SwitchButton::B)));
}

#[test]
fn a_dropped_finished_is_inferred_after_the_input_time() {
    let server = fake::start(|_, fake| {
        fake.hello();
        let Some(ClientMessage::Execute { seq, .. }) = fake.read() else {
            panic!("expected an execute")
        };
        fake.accept(seq, 1, 160);
        // No `finished`, link fine.
        false
    });
    let mut controller = Esp32WifiController::connect(config(server.addr())).unwrap();
    let receipt = controller.execute(press(SwitchButton::A)).unwrap();
    assert_eq!(receipt.command_id, 1);
    let accepted = Instant::now();
    assert!(!controller.is_idle().unwrap());
    std::thread::sleep(Duration::from_millis(1000));
    assert!(!controller.is_idle().unwrap(), "too early to infer");
    wait_idle(|| controller.is_idle().unwrap(), Duration::from_secs(3));
    let inferred_after = accepted.elapsed();
    assert!(
        inferred_after >= Duration::from_millis(2100),
        "inferred after {inferred_after:?}"
    );
    assert_eq!(controller.inferred_finishes(), 1);
    assert_eq!(controller.reconnects(), 0);
    drop(controller);
    server.finish();
}

#[test]
fn a_socket_closed_mid_command_reconnects_and_continues() {
    let server = fake::start(|n, fake| {
        fake.hello();
        match n {
            0 => {
                let Some(ClientMessage::Execute { seq, .. }) = fake.read() else {
                    panic!("expected an execute")
                };
                fake.accept(seq, 1, 160);
                fake.close(); // the board reboots mid-press
                true
            }
            _ => {
                assert_eq!(fake.serve(2), SwitchCommand::Neutral);
                assert_eq!(fake.serve(3), press(SwitchButton::B));
                false
            }
        }
    });
    let mut controller = Esp32WifiController::connect(config(server.addr())).unwrap();
    controller.execute(press(SwitchButton::A)).unwrap();
    // The link is gone, but nothing errors: the press finishes by inference.
    wait_idle(|| controller.is_idle().unwrap(), Duration::from_secs(3));
    assert_eq!(controller.inferred_finishes(), 1);
    controller.execute(press(SwitchButton::B)).unwrap();
    wait_idle(|| controller.is_idle().unwrap(), Duration::from_secs(2));
    assert_eq!(controller.reconnects(), 1);
    drop(controller);
    let seen = server.finish();
    assert_eq!(seen[0].len(), 1, "{:?}", seen[0]);
    assert!(is_execute(&seen[1][0], &SwitchCommand::Neutral));
    assert!(is_execute(&seen[1][1], &press(SwitchButton::B)));
}

#[test]
fn an_unreachable_device_fails_after_the_limit() {
    let server = fake::start(|_, fake| {
        fake.hello();
        fake.close();
        false // and the listener goes away: connection refused
    });
    let mut config = config(server.addr());
    config.unreachable_limit = Duration::from_millis(1500);
    let mut controller = Esp32WifiController::connect(config).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let started = Instant::now();
    let err = controller.execute(press(SwitchButton::A)).unwrap_err();
    let took = started.elapsed();
    assert!(
        matches!(err, pokebot_core::Error::Disconnected(_))
            && err.to_string().contains("unreachable"),
        "{err}"
    );
    assert!(
        took >= Duration::from_millis(1400) && took < Duration::from_secs(5),
        "failed after {took:?}"
    );
    assert_eq!(controller.reconnects(), 0);
    let idle = controller.is_idle();
    assert!(
        matches!(idle, Err(pokebot_core::Error::Disconnected(_))),
        "{idle:?}"
    );
    server.finish();
}

/// `is_idle` alone (no command to reconnect on) reports a link that has
/// been down for longer than the limit, and nothing before.
#[test]
fn connection_loss_is_reported_after_the_limit() {
    let server = fake::start(|_, fake| {
        fake.hello();
        fake.close();
        false
    });
    let mut config = config(server.addr());
    config.unreachable_limit = Duration::from_millis(300);
    let controller = Esp32WifiController::connect(config).unwrap();
    let started = Instant::now();
    while controller.is_idle().is_ok() {
        assert!(started.elapsed() < Duration::from_secs(2), "never reported");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(started.elapsed() >= Duration::from_millis(300));
    server.finish();
}

#[test]
fn a_dropped_connection_is_reopened_on_the_next_command() {
    let server = fake::start(|n, fake| {
        fake.hello();
        match n {
            0 => {
                fake.close(); // the board reboots
                true
            }
            _ => {
                assert_eq!(fake.serve(1), SwitchCommand::Neutral);
                assert_eq!(fake.serve(2), press(SwitchButton::A));
                false
            }
        }
    });
    let mut controller = Esp32WifiController::connect(config(server.addr())).unwrap();
    // Let the reader see the hang-up.
    std::thread::sleep(Duration::from_millis(50));
    controller.execute(press(SwitchButton::A)).unwrap();
    wait_idle(|| controller.is_idle().unwrap(), Duration::from_secs(2));
    assert_eq!(controller.reconnects(), 1);
    drop(controller);
    let seen = server.finish();
    assert!(seen[0].is_empty());
    assert_eq!(seen[1].len(), 2);
}

/// Live: the board rebooted (a flash) without closing the socket; requests
/// went unanswered forever. A status request is idempotent, so it is asked
/// again over a fresh link and the caller never sees the hiccup.
#[test]
fn an_unanswered_status_is_asked_again_over_a_fresh_link() {
    let server = fake::start(|n, fake| {
        fake.hello();
        match n {
            0 => {
                fake.read(); // silence, socket left open
                true
            }
            _ => {
                assert_eq!(fake.serve(1), SwitchCommand::Neutral);
                let Some(ClientMessage::Status { seq }) = fake.read() else {
                    panic!("expected status")
                };
                fake.status(seq);
                false
            }
        }
    });
    let mut controller = Esp32WifiController::connect(config(server.addr())).unwrap();
    assert!(controller.status().unwrap().usb_mounted);
    assert_eq!(controller.reconnects(), 1);
    drop(controller);
    let seen = server.finish();
    assert!(matches!(seen[0][0], ClientMessage::Status { .. }));
}
