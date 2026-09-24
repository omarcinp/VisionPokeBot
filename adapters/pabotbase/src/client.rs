//! PC side of PABotBase2: a [`Controller`] backed by a serial port.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pokebot_core::{
    ButtonSet, Controller, ControllerCommand, ControllerReceipt, Error, PressProfile, Result,
};
use serialport::SerialPort;

use crate::link::{Link, WINDOW};
use crate::message::{self, Message, MessageReader};
use crate::packet::{self, op, Parsed};
use crate::report::{encode_command, ControllerKind};

const RETRANSMIT_TICK: Duration = Duration::from_millis(80);
const RESET_TIMEOUT: Duration = Duration::from_millis(300);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// Packet size used until the device reports its own.
const INITIAL_PACKET_BYTES: usize = 24;
const DEVICE_LOG_LINES: usize = 200;

#[derive(Debug, Clone)]
pub struct PabotBaseConfig {
    /// Serial port of the ESP32, e.g. `/dev/ttyUSB0`.
    pub port: PathBuf,
    /// Tried in order until the device answers a reset.
    pub baud_rates: Vec<u32>,
    pub press_profile: PressProfile,
}

impl PabotBaseConfig {
    pub fn new(port: impl Into<PathBuf>) -> Self {
        Self {
            port: port.into(),
            baud_rates: vec![921_600, 115_200],
            press_profile: PressProfile::default(),
        }
    }
}

/// What the device reported during the handshake.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub baud: u32,
    pub protocol: u32,
    pub device_id: u32,
    pub firmware: u32,
    pub name: String,
    pub controllers: Vec<u32>,
    pub controller: ControllerKind,
    pub queue_capacity: u8,
}

pub struct PabotBaseController {
    inner: Arc<Inner>,
    reader: Option<JoinHandle<()>>,
    info: DeviceInfo,
    profile: PressProfile,
    next_receipt: u64,
}

struct Inner {
    state: Mutex<State>,
    changed: Condvar,
    port: Mutex<Box<dyn SerialPort>>,
    stop: AtomicBool,
}

struct State {
    link: Link,
    max_unacked: usize,
    messages: MessageReader,
    link_replies: HashMap<u8, u32>,
    request_seq: u8,
    responses: HashMap<u8, Option<Message>>,
    kind: ControllerKind,
    queue_capacity: usize,
    next_command_id: u8,
    in_flight: HashSet<u8>,
    local: VecDeque<(ButtonSet, u16)>,
    failure: Option<String>,
    device_log: VecDeque<String>,
}

impl PabotBaseController {
    pub fn open(config: PabotBaseConfig) -> Result<Self> {
        let mut last_error = None;
        for &baud in &config.baud_rates {
            match Self::connect(&config, baud) {
                Ok(controller) => return Ok(controller),
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error.unwrap_or_else(|| Error::Device("no baud rates configured".into())))
    }

    pub fn info(&self) -> &DeviceInfo {
        &self.info
    }

    /// Log lines the firmware sent since the last call.
    pub fn take_device_log(&self) -> Vec<String> {
        self.inner.lock().device_log.drain(..).collect()
    }

    fn connect(config: &PabotBaseConfig, baud: u32) -> Result<Self> {
        let path = config.port.to_string_lossy().into_owned();
        let mut port = serialport::new(&path, baud)
            .timeout(Duration::from_millis(10))
            .open()
            .map_err(|e| Error::Device(format!("{path}: {e}")))?;
        // Matches the reference client; pseudo-terminals may reject these.
        let _ = port.write_data_terminal_ready(true);
        let _ = port.write_request_to_send(false);
        let reader_port = port
            .try_clone()
            .map_err(|e| Error::Device(format!("{path}: {e}")))?;
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                link: Link::new(0, INITIAL_PACKET_BYTES),
                max_unacked: 1,
                messages: MessageReader::default(),
                link_replies: HashMap::new(),
                request_seq: 0,
                responses: HashMap::new(),
                kind: ControllerKind::WirelessProController,
                queue_capacity: 4,
                next_command_id: 0,
                in_flight: HashSet::new(),
                local: VecDeque::new(),
                failure: None,
                device_log: VecDeque::new(),
            }),
            changed: Condvar::new(),
            port: Mutex::new(port),
            stop: AtomicBool::new(false),
        });
        let reader = {
            let inner = Arc::clone(&inner);
            std::thread::Builder::new()
                .name("pokebot-pabotbase".into())
                .spawn(move || reader_loop(&inner, reader_port))
                .map_err(|e| Error::Device(format!("cannot spawn serial thread: {e}")))?
        };
        let mut controller = Self {
            inner,
            reader: Some(reader),
            info: DeviceInfo {
                baud,
                protocol: 0,
                device_id: 0,
                firmware: 0,
                name: String::new(),
                controllers: Vec::new(),
                controller: ControllerKind::WirelessProController,
                queue_capacity: 0,
            },
            profile: config.press_profile,
            next_receipt: 0,
        };
        controller.handshake()?;
        Ok(controller)
    }

    fn handshake(&mut self) -> Result<()> {
        let inner = &self.inner;
        // Link layer.
        {
            let mut state = inner.lock();
            let session = random_session();
            state.link.reset(session);
            let wire = state
                .link
                .send_request(op::ASK_RESET, &session.to_le_bytes());
            inner.write(&wire)?;
            let (state, ok) = inner.wait(state, RESET_TIMEOUT, |s| s.link.unacked() == 0);
            if !ok {
                drop(state);
                return Err(Error::Device(
                    "device did not answer the PABotBase2 reset".into(),
                ));
            }
        }
        let version = self.link_request(op::ASK_VERSION, op::RET_VERSION)?;
        // Same rule as the reference client: the major part (version / 100)
        // must match and the device's minor part must be >= ours, which is 0.
        if version / 100 != packet::PROTOCOL_VERSION / 100 {
            return Err(Error::Device(format!(
                "incompatible link protocol {version}"
            )));
        }
        let packet_size = self.link_request(op::ASK_PACKET_SIZE, op::RET_PACKET_SIZE)?;
        let slots = self.link_request(op::ASK_BUFFER_SLOTS, op::RET_BUFFER_SLOTS)?;
        {
            let mut state = inner.lock();
            let bytes = if packet_size == 0 || packet_size >= 256 {
                256
            } else {
                packet_size as usize
            };
            state.link.set_max_packet_bytes(bytes);
            state.max_unacked = slots.clamp(1, u32::from(WINDOW)) as usize;
        }

        // Message layer.
        use message::op as m;
        let protocol = self.query_u32(m::PROTOCOL_VERSION)?;
        if protocol / 100 != message::PROTOCOL_VERSION / 100 {
            return Err(Error::Device(format!(
                "incompatible message protocol {protocol}"
            )));
        }
        let device_id = self.query_u32(m::DEVICE_IDENTIFIER)?;
        let firmware = self.query_u32(m::FIRMWARE_VERSION)?;
        let name = String::from_utf8_lossy(&self.query(m::DEVICE_NAME, &[])?.body).into_owned();
        self.send_message(m::SET_LOGGING_FLAG, 0, &0u32.to_le_bytes())?;
        let list = self.query(m::CONTROLLER_LIST, &[])?.body;
        let controllers: Vec<u32> = list
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        let queue_capacity = self.query_u32(m::CQ_CAPACITY)?.clamp(4, 255) as u8;

        let mut mode = self.query_u32(m::READ_CONTROLLER_MODE)?;
        if ControllerKind::from_id(mode).is_none() {
            let wanted = [
                ControllerKind::WirelessProController,
                ControllerKind::WiredController,
            ]
            .into_iter()
            .find(|k| controllers.contains(&k.id()))
            .ok_or_else(|| {
                Error::Unsupported(format!(
                    "device offers no Switch controller: {controllers:#x?}"
                ))
            })?;
            self.send_message(m::CHANGE_CONTROLLER_MODE, 0, &wanted.id().to_le_bytes())?;
            mode = self.query_u32(m::READ_CONTROLLER_MODE)?;
        }
        let controller = ControllerKind::from_id(mode).ok_or_else(|| {
            Error::Unsupported(format!("device stayed in controller mode {mode:#x}"))
        })?;
        {
            let mut state = inner.lock();
            state.kind = controller;
            state.queue_capacity = usize::from(queue_capacity);
        }
        self.info = DeviceInfo {
            baud: self.info.baud,
            protocol,
            device_id,
            firmware,
            name,
            controllers,
            controller,
            queue_capacity,
        };
        Ok(())
    }

    fn link_request(&self, ask: u8, ret: u8) -> Result<u32> {
        let inner = &self.inner;
        let mut state = inner.lock();
        state.link_replies.remove(&ret);
        let wire = state.link.send_request(ask, &[]);
        inner.write(&wire)?;
        let (mut state, ok) = inner.wait(state, REQUEST_TIMEOUT, |s| {
            s.link_replies.contains_key(&ret)
        });
        match (ok, state.link_replies.remove(&ret)) {
            (true, Some(value)) => Ok(value),
            _ => Err(Error::Device(format!(
                "no reply to link request {ask:#04x}"
            ))),
        }
    }

    fn query_u32(&self, opcode: u8) -> Result<u32> {
        self.query(opcode, &[])?
            .body_u32()
            .ok_or_else(|| Error::Device(format!("short reply to request {opcode:#04x}")))
    }

    /// Request with a response: waits for the matching `RET*` message.
    fn query(&self, opcode: u8, body: &[u8]) -> Result<Message> {
        let inner = &self.inner;
        let mut state = inner.lock();
        let mut id = state.request_seq;
        while id == 0 || state.responses.contains_key(&id) {
            id = id.wrapping_add(1);
        }
        state.request_seq = id.wrapping_add(1);
        state.responses.insert(id, None);
        drop(state);
        self.send_message(opcode, id, body)?;
        let state = inner.lock();
        let (mut state, _) = inner.wait(state, REQUEST_TIMEOUT, |s| {
            s.responses.get(&id).is_some_and(Option::is_some)
        });
        match state.responses.remove(&id).flatten() {
            Some(reply) if reply.opcode == message::op::REQUEST_DROPPED => Err(Error::Device(
                format!("device dropped request {opcode:#04x}"),
            )),
            Some(reply) => Ok(reply),
            None => Err(Error::Device(format!(
                "no response to request {opcode:#04x}"
            ))),
        }
    }

    fn send_message(&self, opcode: u8, id: u8, body: &[u8]) -> Result<()> {
        let inner = &self.inner;
        let bytes = Message::new(opcode, id, body.to_vec()).encode();
        let state = inner.lock();
        let (mut state, ok) = inner.wait(state, REQUEST_TIMEOUT, |s| {
            s.link.unacked() + s.link.packets_for(bytes.len())
                <= s.max_unacked.max(s.link.packets_for(bytes.len()))
        });
        if !ok {
            return Err(Error::Device("serial link is not draining".into()));
        }
        let wire = state.link.send_stream(&bytes);
        inner.write(&wire)
    }
}

impl Controller for PabotBaseController {
    fn execute(&mut self, command: ControllerCommand) -> Result<ControllerReceipt> {
        let command_id = self.next_receipt;
        self.next_receipt += 1;
        let issued_at = Instant::now();
        let inner = &self.inner;
        let mut state = inner.lock();
        if let Some(failure) = &state.failure {
            return Err(Error::Disconnected(failure.clone()));
        }
        if matches!(command, ControllerCommand::Neutral) {
            state.local.clear();
            if !state.in_flight.is_empty() {
                state.in_flight.clear();
                let wire = state
                    .link
                    .send_stream(&Message::new(message::op::CQ_CANCEL, 0, vec![]).encode());
                inner.write(&wire)?;
            }
            return Ok(ControllerReceipt {
                command_id,
                issued_at,
                input_duration: Duration::ZERO,
            });
        }
        let mut input_duration = Duration::ZERO;
        for input in command.timeline(&self.profile) {
            input_duration += input.duration;
            let mut ms = input.duration.as_nanos().div_ceil(1_000_000) as u64;
            while ms > 0 {
                let chunk = ms.min(u64::from(u16::MAX));
                state.local.push_back((input.buttons, chunk as u16));
                ms -= chunk;
            }
        }
        pump(inner, &mut state)?;
        Ok(ControllerReceipt {
            command_id,
            issued_at,
            input_duration,
        })
    }

    fn is_idle(&self) -> Result<bool> {
        let state = self.inner.lock();
        if let Some(failure) = &state.failure {
            return Err(Error::Disconnected(failure.clone()));
        }
        Ok(state.local.is_empty() && state.in_flight.is_empty())
    }
}

impl Drop for PabotBaseController {
    fn drop(&mut self) {
        let _ = self.execute(ControllerCommand::Neutral);
        self.inner.stop.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut port = self.port.lock().unwrap_or_else(|e| e.into_inner());
        port.write_all(bytes)
            .and_then(|()| port.flush())
            .map_err(|e| Error::Disconnected(format!("serial write failed: {e}")))
    }

    /// Waits until `ready` holds, the link fails, or `timeout` passes.
    fn wait<'a>(
        &self,
        mut state: MutexGuard<'a, State>,
        timeout: Duration,
        ready: impl Fn(&State) -> bool,
    ) -> (MutexGuard<'a, State>, bool) {
        let deadline = Instant::now() + timeout;
        while !ready(&state) && state.failure.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return (state, false);
            }
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        let ok = ready(&state);
        (state, ok)
    }
}

/// Moves locally queued holds to the device while it has queue space.
fn pump(inner: &Inner, state: &mut State) -> Result<()> {
    while state.in_flight.len() < state.queue_capacity && state.link.unacked() < state.max_unacked {
        let id = state.next_command_id;
        if state.in_flight.contains(&id) {
            break;
        }
        let Some((buttons, ms)) = state.local.pop_front() else {
            break;
        };
        let wire = state
            .link
            .send_stream(&encode_command(state.kind, id, buttons, ms).encode());
        state.in_flight.insert(id);
        state.next_command_id = id.wrapping_add(1);
        inner.write(&wire)?;
    }
    Ok(())
}

fn reader_loop(inner: &Inner, mut port: Box<dyn SerialPort>) {
    let mut buffer = [0u8; 1024];
    let mut last_tick = Instant::now();
    while !inner.stop.load(Ordering::Relaxed) {
        let received = match port.read(&mut buffer) {
            Ok(n) => &buffer[..n],
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                &[]
            }
            Err(e) => {
                inner.lock().failure = Some(format!("serial read failed: {e}"));
                inner.changed.notify_all();
                return;
            }
        };
        let mut state = inner.lock();
        let mut outgoing = Vec::new();
        for parsed in state.link.parse(received) {
            match parsed {
                Parsed::Valid(packet) => on_packet(&mut state, packet, &mut outgoing),
                Parsed::Invalid { .. } => {
                    log_device(&mut state, "received malformed packet".into())
                }
                Parsed::ChecksumFail { .. } => {
                    log_device(&mut state, "received packet with bad checksum".into())
                }
            }
        }
        if last_tick.elapsed() >= RETRANSMIT_TICK {
            last_tick = Instant::now();
            outgoing.extend(state.link.tick());
        }
        let result = inner
            .write(&outgoing)
            .and_then(|()| pump(inner, &mut state));
        if let Err(e) = result {
            state.failure = Some(e.to_string());
        }
        drop(state);
        inner.changed.notify_all();
    }
}

fn on_packet(state: &mut State, packet: packet::Packet, out: &mut Vec<u8>) {
    match packet.base_opcode() {
        op::RET_RESET | op::RET_STREAM_DATA => {
            state.link.acknowledge(packet.seq);
        }
        op::RET_VERSION | op::RET_PACKET_SIZE | op::RET_BUFFER_SLOTS | op::RET_BUFFER_BYTES => {
            state.link.acknowledge(packet.seq);
            if let Some(value) = packet.payload_u32() {
                state.link_replies.insert(packet.base_opcode(), value);
            }
        }
        op::ASK_STREAM_DATA => {
            if state.link.accept_stream(packet.seq, &packet.payload) {
                let free = state.link.free_bytes();
                out.extend(state.link.out_of_band(
                    packet.seq,
                    op::RET_STREAM_DATA,
                    &free.to_le_bytes(),
                ));
            }
            let bytes = state.link.take_stream();
            for message in state.messages.push(&bytes) {
                on_message(state, message);
            }
        }
        op::INVALID_LENGTH | op::INVALID_CHECKSUM_FAIL => {
            log_device(
                state,
                format!(
                    "device rejected packet {} ({:#04x})",
                    packet.seq,
                    packet.base_opcode()
                ),
            );
        }
        op::UNKNOWN_OPCODE => log_device(
            state,
            format!(
                "device did not understand opcode {:?}",
                packet.payload_u32()
            ),
        ),
        op::INFO_STR => log_device(state, String::from_utf8_lossy(&packet.payload).into_owned()),
        other => log_device(state, format!("device info packet {other:#04x}")),
    }
}

fn on_message(state: &mut State, message: Message) {
    use message::op as m;
    match message.opcode {
        m::CQ_COMMAND_FINISHED => {
            state.in_flight.remove(&message.id);
        }
        m::CQ_COMMAND_DROPPED => {
            state.in_flight.remove(&message.id);
            log_device(state, format!("device dropped command {}", message.id));
        }
        m::RET | m::RET_U32 | m::RET_DATA | m::RET_U32_DATA | m::REQUEST_DROPPED => {
            if let Some(slot) = state.responses.get_mut(&message.id) {
                *slot = Some(message);
            }
        }
        m::LOG_STRING | m::LOG_LABEL_H32 | m::LOG_LABEL_U32 | m::LOG_LABEL_I32 => {
            log_device(state, String::from_utf8_lossy(&message.body).into_owned());
        }
        other => log_device(state, format!("unhandled message {other:#04x}")),
    }
}

fn log_device(state: &mut State, line: String) {
    if state.device_log.len() == DEVICE_LOG_LINES {
        state.device_log.pop_front();
    }
    state.device_log.push_back(line);
}

fn random_session() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mixed = (nanos as u64) ^ (u64::from(std::process::id()) << 32);
    (mixed ^ (mixed >> 29)).wrapping_mul(0xBF58_476D_1CE4_E5B9) as u32
}
