//! [`SwitchController`] backed by the ESP32-S3 WiFi controller firmware (or
//! its simulator), speaking the line protocol in `pokebot_remote::protocol`.
//! Every Switch button and both sticks are available; the bot's GBA
//! `Controller` is `pokebot_core::GbaOnSwitch(Esp32WifiController)`.
//!
//! The device queues and times every input itself, so WiFi latency delays
//! when a command starts but never changes how long buttons are held.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pokebot_core::{
    ControllerReceipt, Error, PressProfile, Result, SwitchCommand, SwitchController,
};
use pokebot_remote::protocol::{
    ClientMessage, DeviceInfo, DeviceMessage, Status, CONTROL_PORT, PROTOCOL_VERSION,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct Esp32WifiConfig {
    /// `host` or `host:port` (default port 7878).
    pub address: String,
    pub press_profile: PressProfile,
    pub connect_timeout: Duration,
}

impl Esp32WifiConfig {
    pub fn new(address: impl Into<String>) -> Self {
        Self {
            address: address.into(),
            press_profile: PressProfile::default(),
            connect_timeout: Duration::from_secs(5),
        }
    }
}

pub struct Esp32WifiController {
    inner: Arc<Inner>,
    writer: TcpStream,
    reader: Option<JoinHandle<()>>,
    info: DeviceInfo,
    peer: SocketAddr,
    profile: PressProfile,
    next_seq: u64,
}

#[derive(Default)]
struct Inner {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    replies: HashMap<u64, DeviceMessage>,
    /// Accepted commands that have not finished.
    outstanding: HashSet<u64>,
    failure: Option<String>,
}

impl Esp32WifiController {
    pub fn connect(config: Esp32WifiConfig) -> Result<Self> {
        let peer = resolve(&config.address)?;
        let stream = TcpStream::connect_timeout(&peer, config.connect_timeout)
            .map_err(|e| Error::Device(format!("{peer}: {e}")))?;
        let io = |e: std::io::Error| Error::Device(format!("{peer}: {e}"));
        stream.set_nodelay(true).map_err(io)?;
        stream.set_read_timeout(Some(REQUEST_TIMEOUT)).map_err(io)?;
        let mut reader = BufReader::new(stream.try_clone().map_err(io)?);
        let info = match read_message(&mut reader)? {
            Some(DeviceMessage::Hello(info)) => info,
            Some(DeviceMessage::Error { message }) => return Err(Error::Device(message)),
            other => return Err(Error::Device(format!("expected hello, got {other:?}"))),
        };
        if info.protocol != PROTOCOL_VERSION {
            return Err(Error::Device(format!(
                "device speaks protocol {}, expected {PROTOCOL_VERSION}",
                info.protocol
            )));
        }
        reader.get_ref().set_read_timeout(None).map_err(io)?;
        let inner = Arc::new(Inner::default());
        let reader = {
            let inner = Arc::clone(&inner);
            std::thread::Builder::new()
                .name("pokebot-esp32-wifi".into())
                .spawn(move || read_loop(&inner, reader))
                .map_err(io)?
        };
        Ok(Self {
            inner,
            writer: stream,
            reader: Some(reader),
            info,
            peer,
            profile: config.press_profile,
            next_seq: 0,
        })
    }

    pub fn info(&self) -> &DeviceInfo {
        &self.info
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// Asks the device for its queue and USB state.
    pub fn status(&mut self) -> Result<Status> {
        let seq = self.send(|seq| ClientMessage::Status { seq })?;
        match self.wait_reply(seq)? {
            DeviceMessage::Status(status) => Ok(status),
            other => Err(Error::Device(format!("unexpected reply {other:?}"))),
        }
    }

    fn send(&mut self, message: impl FnOnce(u64) -> ClientMessage) -> Result<u64> {
        self.inner.check()?;
        let seq = self.next_seq;
        self.next_seq += 1;
        let mut line = serde_json::to_vec(&message(seq))
            .map_err(|e| Error::Device(format!("encoding request: {e}")))?;
        line.push(b'\n');
        self.writer.write_all(&line).map_err(|e| {
            self.inner.fail(format!("write: {e}"));
            Error::Disconnected(format!("{}: {e}", self.peer))
        })?;
        Ok(seq)
    }

    fn wait_reply(&self, seq: u64) -> Result<DeviceMessage> {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let mut state = self.inner.lock();
        loop {
            if let Some(reply) = state.replies.remove(&seq) {
                return Ok(reply);
            }
            if let Some(failure) = &state.failure {
                return Err(Error::Disconnected(failure.clone()));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::Device(format!(
                    "{}: no reply to request {seq}",
                    self.peer
                )));
            }
            state = self
                .inner
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

impl SwitchController for Esp32WifiController {
    fn execute(&mut self, command: SwitchCommand) -> Result<ControllerReceipt> {
        let profile = Some(self.profile);
        let seq = self.send(|seq| ClientMessage::Execute {
            seq,
            command,
            profile,
        })?;
        match self.wait_reply(seq)? {
            DeviceMessage::Accepted { id, input_ms, .. } => Ok(ControllerReceipt {
                command_id: id,
                issued_at: Instant::now(),
                input_duration: Duration::from_millis(input_ms),
            }),
            DeviceMessage::Rejected { reason, .. } => Err(Error::Device(reason)),
            other => Err(Error::Device(format!("unexpected reply {other:?}"))),
        }
    }

    fn is_idle(&self) -> Result<bool> {
        let state = self.inner.lock();
        match &state.failure {
            Some(failure) => Err(Error::Disconnected(failure.clone())),
            None => Ok(state.outstanding.is_empty()),
        }
    }
}

impl Drop for Esp32WifiController {
    fn drop(&mut self) {
        let _ = self.writer.shutdown(Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn check(&self) -> Result<()> {
        match &self.lock().failure {
            Some(failure) => Err(Error::Disconnected(failure.clone())),
            None => Ok(()),
        }
    }

    fn fail(&self, reason: String) {
        self.lock().failure.get_or_insert(reason);
        self.changed.notify_all();
    }
}

fn read_loop(inner: &Inner, mut reader: BufReader<TcpStream>) {
    loop {
        let message = match read_message(&mut reader) {
            Ok(Some(message)) => message,
            Ok(None) => return inner.fail("device closed the connection".into()),
            Err(e) => return inner.fail(e.to_string()),
        };
        let mut state = inner.lock();
        match message {
            DeviceMessage::Accepted { seq, id, .. } => {
                // Recorded here, not by the waiting caller, because the
                // matching `finished` may be the very next line.
                state.outstanding.insert(id);
                state.replies.insert(seq, message);
            }
            DeviceMessage::Rejected { seq, .. } => {
                state.replies.insert(seq, message);
            }
            DeviceMessage::Status(Status { seq: Some(seq), .. }) => {
                state.replies.insert(seq, message);
            }
            DeviceMessage::Finished { id, .. } => {
                state.outstanding.remove(&id);
            }
            DeviceMessage::Error { message } => {
                state
                    .failure
                    .get_or_insert(format!("device error: {message}"));
            }
            DeviceMessage::Hello(_) | DeviceMessage::Status(_) => {}
        }
        drop(state);
        inner.changed.notify_all();
    }
}

fn read_message(reader: &mut BufReader<TcpStream>) -> Result<Option<DeviceMessage>> {
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .map_err(|e| Error::Disconnected(e.to_string()))?;
    if n == 0 {
        return Ok(None);
    }
    serde_json::from_str(&line)
        .map(Some)
        .map_err(|e| Error::Device(format!("bad message {line:?}: {e}")))
}

fn resolve(address: &str) -> Result<SocketAddr> {
    let with_port = if address
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
    {
        address.to_owned()
    } else {
        format!("{address}:{CONTROL_PORT}")
    };
    with_port
        .to_socket_addrs()
        .map_err(|e| Error::Device(format!("{address}: {e}")))?
        .next()
        .ok_or_else(|| Error::Device(format!("{address}: no address")))
}
