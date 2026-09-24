use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pokebot_core::{ButtonSet, Controller, ControllerCommand};
use pokebot_emulator_libretro::EmulatorController;
use pokebot_pabotbase::device::{ButtonSink, VirtualDevice};

use crate::VirtualSerialPort;

/// Routes PABotBase2 commands to the emulator's joypad.
pub struct EmulatorButtons(pub EmulatorController);

impl ButtonSink for EmulatorButtons {
    fn hold(&mut self, buttons: ButtonSet, milliseconds: u16) -> u64 {
        let command = ControllerCommand::Hold {
            buttons,
            duration: Duration::from_millis(u64::from(milliseconds)),
        };
        // The emulator only fails once it has shut down; the id then never
        // completes, which the host sees as a stalled device.
        self.0
            .execute(command)
            .map_or(u64::MAX, |receipt| receipt.command_id)
    }

    fn completed_through(&self) -> Option<u64> {
        self.0.completed_through()
    }

    fn cancel(&mut self) {
        let _ = self.0.execute(ControllerCommand::Neutral);
    }
}

/// A running fake ESP32. Stops when dropped.
pub struct VirtualEsp32 {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for VirtualEsp32 {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Serves PABotBase2 on `port`, pressing buttons on `sink`. Device events
/// (connections, commands) are sent to `log`.
pub fn spawn_virtual_esp32<S: ButtonSink + Send + 'static>(
    port: VirtualSerialPort,
    sink: S,
    name: &str,
    log: Sender<String>,
) -> std::io::Result<VirtualEsp32> {
    let stop = Arc::new(AtomicBool::new(false));
    let mut device = VirtualDevice::new(sink, name);
    let worker = {
        let stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("pokebot-virtual-esp32".into())
            .spawn(move || {
                let mut port = port;
                let mut buffer = [0u8; 1024];
                let mut last_unlock = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    let mut out = Vec::new();
                    if wait_readable(&port, Duration::from_millis(2)) {
                        loop {
                            match port.master.read(&mut buffer) {
                                Ok(0) => break,
                                Ok(n) => out.extend(device.on_bytes(&buffer[..n])),
                                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                                Err(_) => break,
                            }
                        }
                    }
                    out.extend(device.poll(Instant::now()));
                    if !out.is_empty() {
                        // Like a UART with nobody listening: if the host is gone
                        // and the line buffer is full, bytes are lost.
                        let _ = port.master.write_all(&out);
                    }
                    for line in device.take_log() {
                        let _ = log.send(line);
                    }
                    if last_unlock.elapsed() > Duration::from_millis(250) {
                        port.release_exclusive_lock();
                        last_unlock = Instant::now();
                    }
                }
            })?
    };
    Ok(VirtualEsp32 {
        stop,
        worker: Some(worker),
    })
}

fn wait_readable(port: &VirtualSerialPort, timeout: Duration) -> bool {
    let mut fd = libc::pollfd {
        fd: port.master.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    let rc = unsafe { libc::poll(&mut fd, 1, timeout.as_millis() as libc::c_int) };
    rc > 0 && fd.revents & libc::POLLIN != 0
}
