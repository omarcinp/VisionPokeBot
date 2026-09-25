//! [`SwitchController`] backed by the ESP32-S3 WiFi controller firmware (or
//! its simulator), speaking the line protocol in `pokebot_remote::protocol`.
//! Every Switch button and both sticks are available; the bot's GBA
//! `Controller` is `pokebot_core::GbaOnSwitch(Esp32WifiController)`.
//!
//! The device queues and times every input itself, so WiFi latency delays
//! when a command starts but never changes how long buttons are held.
//!
//! # A lost link never ends a task by itself
//!
//! One lost reply once ended a whole goal cycle on the Switch (`no reply to
//! request 622`: the board was fine a minute later). The link is now
//! restored, and the bot only sees an error when the device stays
//! unreachable for [`Esp32WifiConfig::unreachable_limit`] (30 s by default):
//!
//! - A broken socket (the board rebooted, WiFi dropped, a request went
//!   unanswered for [`REQUEST_TIMEOUT`]) is reopened by the next request,
//!   with a backoff doubling from [`Esp32WifiConfig::reconnect_backoff`]
//!   (about five tries in the first ten seconds), until the limit.
//! - After a reconnect a `Neutral` is sent first: it releases whatever the
//!   old link may have left held. A timed input is **never** re-sent: when
//!   its `accepted` was lost the command may well have run, and a duplicate
//!   press is worse than a missed one. Such a command gets a receipt with a
//!   local id ([`LOCAL_ID`] set) and counts as accepted; the game's video is
//!   the judge of what happened.
//! - A command whose `finished` never comes is inferred finished once its
//!   own input time plus [`FINISH_GRACE`] has passed (logged as `finished
//!   inferred`), so [`SwitchController::is_idle`] keeps going without the
//!   device's word.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pokebot_core::{
    ConsoleLink, ControllerReceipt, Error, PressProfile, Result, SwitchCommand, SwitchController,
};
use pokebot_remote::protocol::{
    ClientMessage, DeviceInfo, DeviceMessage, Status, CONTROL_PORT, PROTOCOL_VERSION,
};

/// How long a request may go unanswered before the link counts as lost.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// Slack after a command's own input time before a missing `finished` is
/// taken as finished.
pub const FINISH_GRACE: Duration = Duration::from_secs(2);
/// Longest pause between two reconnect attempts.
pub const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(4);
/// Set in the `command_id` of a receipt the device never acknowledged
/// (its `accepted` was lost); the low bits are the request's `seq`.
pub const LOCAL_ID: u64 = 1 << 63;
/// Kept for callers of the old constant; the first backoff step.
pub const RECONNECT_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct Esp32WifiConfig {
    /// `host` or `host:port` (default port 7878).
    pub address: String,
    pub press_profile: PressProfile,
    pub connect_timeout: Duration,
    /// How long the device may stay unreachable before a request fails
    /// with [`Error::Disconnected`]. Until then requests reconnect and wait.
    pub unreachable_limit: Duration,
    /// Pause after the first failed reconnect; doubles each try up to
    /// [`MAX_RECONNECT_BACKOFF`].
    pub reconnect_backoff: Duration,
}

impl Esp32WifiConfig {
    pub fn new(address: impl Into<String>) -> Self {
        Self {
            address: address.into(),
            press_profile: PressProfile::default(),
            connect_timeout: Duration::from_secs(5),
            unreachable_limit: Duration::from_secs(30),
            reconnect_backoff: RECONNECT_INTERVAL,
        }
    }
}

pub struct Esp32WifiController {
    conn: Connection,
    config: Esp32WifiConfig,
    profile: PressProfile,
    next_seq: u64,
    reconnects: u32,
    /// Finishes inferred on connections already replaced.
    inferred_before: u32,
}

/// One TCP connection to the device and the thread reading it.
struct Connection {
    inner: Arc<Inner>,
    writer: TcpStream,
    reader: Option<JoinHandle<()>>,
    info: DeviceInfo,
    peer: SocketAddr,
}

#[derive(Default)]
struct Inner {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    replies: HashMap<u64, DeviceMessage>,
    /// Accepted commands that have not finished, with the moment their
    /// finish is inferred if the device never reports it.
    outstanding: HashMap<u64, Instant>,
    /// When every input accepted so far will have played.
    queue_end: Option<Instant>,
    failure: Option<String>,
    /// First moment the link was found lost, carried across reconnects
    /// until the device answers again.
    down_since: Option<Instant>,
    inferred: u32,
}

impl Esp32WifiController {
    pub fn connect(config: Esp32WifiConfig) -> Result<Self> {
        let conn = Connection::open(&config, None)?;
        Ok(Self {
            conn,
            profile: config.press_profile,
            config,
            next_seq: 0,
            reconnects: 0,
            inferred_before: 0,
        })
    }

    pub fn info(&self) -> &DeviceInfo {
        &self.conn.info
    }

    pub fn peer(&self) -> SocketAddr {
        self.conn.peer
    }

    /// Connections reopened after being lost.
    pub fn reconnects(&self) -> u32 {
        self.reconnects
    }

    /// Commands taken as finished without the device saying so.
    pub fn inferred_finishes(&self) -> u32 {
        self.inferred_before + self.conn.inner.lock().inferred
    }

    /// Asks the device for its queue and USB state. Idempotent, so a lost
    /// reply is asked again over a fresh link (within the limit).
    pub fn status(&mut self) -> Result<Status> {
        loop {
            self.ensure_connected()?;
            let seq = self.next_seq();
            if self.write(&ClientMessage::Status { seq }).is_err() {
                continue;
            }
            match self.wait_reply(seq) {
                Ok(DeviceMessage::Status(status)) => return Ok(status),
                Ok(other) => return Err(Error::Device(format!("unexpected reply {other:?}"))),
                Err(_) => continue,
            }
        }
    }

    /// Restores a lost link: reconnects with backoff until the device
    /// answers a `Neutral`, or fails once it has been unreachable for the
    /// configured limit.
    fn ensure_connected(&mut self) -> Result<()> {
        let Err(lost) = self.conn.inner.check() else {
            return Ok(());
        };
        let peer = self.conn.peer;
        let limit = self.config.unreachable_limit;
        let down_since = self
            .conn
            .inner
            .lock()
            .down_since
            .unwrap_or_else(Instant::now);
        let mut last = lost.to_string();
        let mut backoff = self.config.reconnect_backoff;
        let mut tries = 0u32;
        loop {
            let down = down_since.elapsed();
            if down >= limit {
                return Err(Error::Disconnected(format!(
                    "{peer}: unreachable for {:.1} s after {tries} reconnect attempts ({last})",
                    down.as_secs_f64()
                )));
            }
            tries += 1;
            match Connection::open(&self.config, Some(down_since)) {
                Ok(conn) => {
                    self.replace(conn);
                    self.reconnects += 1;
                    eprintln!(
                        "esp32: {peer}: reconnected after {:.1} s (try {tries}; {last}); sending neutral",
                        down.as_secs_f64()
                    );
                    // Only the link is restored: neutral releases whatever
                    // the old one left held; no timed input is repeated.
                    match self.request_neutral() {
                        Ok(_) => return Ok(()),
                        Err(e) => last = format!("after reconnect: {e}"),
                    }
                }
                Err(e) => last = e.to_string(),
            }
            let left = limit.saturating_sub(down_since.elapsed());
            if left.is_zero() {
                continue;
            }
            std::thread::sleep(backoff.min(left));
            backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
        }
    }

    fn replace(&mut self, conn: Connection) {
        self.inferred_before += self.conn.inner.lock().inferred;
        self.conn = conn;
    }

    /// Sends `Neutral` on the current link and waits for its `accepted`.
    fn request_neutral(&mut self) -> Result<ControllerReceipt> {
        let seq = self.next_seq();
        self.write(&ClientMessage::Execute {
            seq,
            command: SwitchCommand::Neutral,
            profile: None,
        })?;
        match self.wait_reply(seq)? {
            DeviceMessage::Accepted { id, input_ms, .. } => {
                Ok(self.accepted(id, Duration::from_millis(input_ms), true))
            }
            DeviceMessage::Rejected { reason, .. } => Err(Error::Device(reason)),
            other => Err(Error::Device(format!("unexpected reply {other:?}"))),
        }
    }

    fn next_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }

    fn write(&self, message: &ClientMessage) -> Result<()> {
        let mut line = serde_json::to_vec(message)
            .map_err(|e| Error::Device(format!("encoding request: {e}")))?;
        line.push(b'\n');
        let conn = &self.conn;
        (&conn.writer).write_all(&line).map_err(|e| {
            let reason = format!("{}: write: {e}", conn.peer);
            conn.inner.fail(reason.clone());
            Error::Disconnected(reason)
        })
    }

    fn wait_reply(&self, seq: u64) -> Result<DeviceMessage> {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let inner = &self.conn.inner;
        let mut state = inner.lock();
        loop {
            if let Some(reply) = state.replies.remove(&seq) {
                return Ok(reply);
            }
            if let Some(failure) = &state.failure {
                return Err(Error::Disconnected(failure.clone()));
            }
            let now = Instant::now();
            if now >= deadline {
                // A board that rebooted leaves the socket half open: writes
                // vanish and nothing ever answers. Count it as lost, so the
                // next request reconnects.
                let reason = format!("{}: no reply to request {seq}", self.conn.peer);
                drop(state);
                inner.fail(reason.clone());
                return Err(Error::Disconnected(reason));
            }
            state = inner
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// The receipt of a command the device accepted (booked by the reader
    /// thread, where its `finished` cannot overtake it). A neutral drops
    /// the device's queue, so it drops ours too.
    fn accepted(&self, id: u64, input: Duration, neutral: bool) -> ControllerReceipt {
        let now = Instant::now();
        if neutral {
            let mut state = self.conn.inner.lock();
            // Their `finished` (cancelled) may still come and is then
            // ignored. The neutral itself stays outstanding until its own
            // `finished` (the release goes out on the device's next tick),
            // now due at once rather than behind the dropped queue.
            let pending = state.outstanding.remove(&id).is_some();
            state.outstanding.clear();
            state.queue_end = None;
            if pending {
                state.outstanding.insert(id, now + FINISH_GRACE);
            }
        }
        ControllerReceipt {
            command_id: id,
            issued_at: now,
            input_duration: input,
        }
    }

    /// The receipt of a command whose `accepted` was lost: booked here as
    /// if accepted now, so its finish is inferred in time.
    fn accepted_inferred(&self, seq: u64, input: Duration) -> ControllerReceipt {
        let id = LOCAL_ID | seq;
        let now = Instant::now();
        book(&mut self.conn.inner.lock(), id, input, now);
        ControllerReceipt {
            command_id: id,
            issued_at: now,
            input_duration: input,
        }
    }
}

/// Books an accepted command: when it will have played (behind what is
/// queued), and when its finish is inferred if never reported.
fn book(state: &mut State, id: u64, input: Duration, now: Instant) {
    let start = state.queue_end.map_or(now, |end| end.max(now));
    let end = start + input;
    state.queue_end = Some(end);
    state.outstanding.insert(id, end + FINISH_GRACE);
}

impl Connection {
    fn open(config: &Esp32WifiConfig, down_since: Option<Instant>) -> Result<Self> {
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
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                down_since,
                ..State::default()
            }),
            changed: Condvar::new(),
        });
        let reader = {
            let inner = Arc::clone(&inner);
            std::thread::Builder::new()
                .name("pokebot-esp32-wifi".into())
                .spawn(move || read_loop(&inner, reader, peer))
                .map_err(io)?
        };
        Ok(Self {
            inner,
            writer: stream,
            reader: Some(reader),
            info,
            peer,
        })
    }
}

impl SwitchController for Esp32WifiController {
    fn execute(&mut self, command: SwitchCommand) -> Result<ControllerReceipt> {
        let neutral = matches!(command, SwitchCommand::Neutral);
        let input: Duration = command
            .timeline(&self.profile)
            .iter()
            .map(|t| t.duration)
            .sum();
        loop {
            self.ensure_connected()?;
            let seq = self.next_seq();
            let request = ClientMessage::Execute {
                seq,
                command: command.clone(),
                profile: Some(self.profile),
            };
            if let Err(lost) = self.write(&request) {
                // The line never left this host: safe to send on a fresh
                // link (bounded by the unreachable limit).
                eprintln!("esp32: request {seq} not sent ({lost}); reconnecting");
                continue;
            }
            let lost = match self.wait_reply(seq) {
                Ok(DeviceMessage::Accepted { id, input_ms, .. }) => {
                    return Ok(self.accepted(id, Duration::from_millis(input_ms), neutral));
                }
                Ok(DeviceMessage::Rejected { reason, .. }) => return Err(Error::Device(reason)),
                Ok(other) => return Err(Error::Device(format!("unexpected reply {other:?}"))),
                Err(lost) => lost,
            };
            if neutral {
                // Releasing everything is idempotent: ask again on a fresh
                // link (which sends a neutral of its own first).
                continue;
            }
            // The request reached the device and may have played. Never
            // send it twice; count it as accepted and let the video tell.
            eprintln!(
                "esp32: accepted inferred for request {seq} ({command:?}, {} ms): {lost}; not re-sent",
                input.as_millis()
            );
            return Ok(self.accepted_inferred(seq, input));
        }
    }

    fn is_idle(&self) -> Result<bool> {
        let now = Instant::now();
        let peer = self.conn.peer;
        let mut state = self.conn.inner.lock();
        let due: Vec<u64> = state
            .outstanding
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            state.outstanding.remove(&id);
            state.inferred += 1;
            let local = if id & LOCAL_ID != 0 { " (local)" } else { "" };
            eprintln!(
                "esp32: finished inferred for #{}{local}: no finished from {peer} within input + {} s",
                id & !LOCAL_ID,
                FINISH_GRACE.as_secs()
            );
        }
        if let (Some(failure), Some(since)) = (&state.failure, state.down_since) {
            let down = now.saturating_duration_since(since);
            if down >= self.config.unreachable_limit {
                return Err(Error::Disconnected(format!(
                    "{peer}: unreachable for {:.1} s ({failure})",
                    down.as_secs_f64()
                )));
            }
        }
        Ok(state.outstanding.is_empty())
    }

    fn console_link(&mut self) -> Result<ConsoleLink> {
        let status = self.status()?;
        Ok(match (status.usb_mounted, status.usb_suspended) {
            (true, false) => ConsoleLink::Attached,
            (true, true) => ConsoleLink::Suspended,
            (false, _) => ConsoleLink::Detached,
        })
    }
}

impl Drop for Connection {
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
        let mut state = self.lock();
        state.failure.get_or_insert(reason);
        state.down_since.get_or_insert_with(Instant::now);
        drop(state);
        self.changed.notify_all();
    }
}

fn read_loop(inner: &Inner, mut reader: BufReader<TcpStream>, peer: SocketAddr) {
    loop {
        let message = match read_message(&mut reader) {
            Ok(Some(message)) => message,
            Ok(None) => return inner.fail(format!("{peer}: device closed the connection")),
            Err(e) => return inner.fail(format!("{peer}: {e}")),
        };
        let mut state = inner.lock();
        // Anything the device says means the link is alive.
        state.down_since = None;
        match message {
            DeviceMessage::Accepted { seq, id, input_ms } => {
                // Booked here, not by the waiting caller, because the
                // matching `finished` may be the very next line.
                book(
                    &mut state,
                    id,
                    Duration::from_millis(input_ms),
                    Instant::now(),
                );
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
