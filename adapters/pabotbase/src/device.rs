//! Device side of PABotBase2 (sans-IO): behaves like an ESP32 running the
//! firmware, but presses buttons on a [`ButtonSink`] (e.g. the emulator)
//! instead of a Switch. Lets development run the exact protocol the real
//! hardware will speak.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use pokebot_core::ButtonSet;

use crate::link::{Link, BUFFER_BYTES, WINDOW};
use crate::message::{self, Message, MessageReader};
use crate::packet::{self, op, Parsed, MAX_PACKET_BYTES};
use crate::report::{decode_command, ControllerKind};

/// Firmware poll/retransmit period.
const POLL: Duration = Duration::from_millis(50);
const COMMAND_QUEUE_CAPACITY: u32 = 32;
const FIRMWARE_VERSION: u32 = 2026090200;
/// `PABB_PID_PABOTBASE_ESP32`
const DEVICE_IDENTIFIER: u32 = 0x10;

/// Whatever ultimately receives the button presses.
pub trait ButtonSink {
    /// Queues `buttons` for `milliseconds` behind earlier holds. Returns an
    /// id that increases with every call.
    fn hold(&mut self, buttons: ButtonSet, milliseconds: u16) -> u64;
    /// Newest id whose hold has fully elapsed.
    fn completed_through(&self) -> Option<u64>;
    /// Drops queued holds and releases everything.
    fn cancel(&mut self);
}

pub struct VirtualDevice<S> {
    sink: S,
    name: String,
    link: Link,
    messages: MessageReader,
    stream_ready: bool,
    mode: ControllerKind,
    queue: VecDeque<(u8, u64)>,
    replace_on_next: bool,
    started: Instant,
    last_poll: Instant,
    log: Vec<String>,
}

impl<S: ButtonSink> VirtualDevice<S> {
    pub fn new(sink: S, name: impl Into<String>) -> Self {
        let now = Instant::now();
        Self {
            sink,
            name: name.into(),
            link: Link::new(0, MAX_PACKET_BYTES),
            messages: MessageReader::default(),
            stream_ready: false,
            mode: ControllerKind::WirelessProController,
            queue: VecDeque::new(),
            replace_on_next: false,
            started: now,
            last_poll: now,
            log: Vec::new(),
        }
    }

    /// Feeds bytes received from the host; returns bytes to send back.
    pub fn on_bytes(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for parsed in self.link.parse(bytes) {
            match parsed {
                Parsed::Valid(packet) => self.on_packet(packet, &mut out),
                Parsed::Invalid { seq } => {
                    out.extend(self.link.out_of_band(seq, op::INVALID_LENGTH, &[]))
                }
                Parsed::ChecksumFail { seq } => {
                    out.extend(self.link.out_of_band(seq, op::INVALID_CHECKSUM_FAIL, &[]))
                }
            }
        }
        out
    }

    /// Reports finished commands and retransmits; call every few ms.
    pub fn poll(&mut self, now: Instant) -> Vec<u8> {
        let mut out = Vec::new();
        let done = self.sink.completed_through();
        while let Some(&(cq_id, sink_id)) = self.queue.front() {
            if done.is_none_or(|d| sink_id > d) {
                break;
            }
            self.queue.pop_front();
            let timestamp = now.duration_since(self.started).as_millis() as u32;
            self.send(
                &Message::u32(message::op::CQ_COMMAND_FINISHED, cq_id, timestamp),
                &mut out,
            );
        }
        if now.duration_since(self.last_poll) >= POLL {
            self.last_poll = now;
            out.extend(self.link.tick());
        }
        out
    }

    /// Human-readable events since the last call (connections, commands).
    pub fn take_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.log)
    }

    pub fn sink(&self) -> &S {
        &self.sink
    }

    fn on_packet(&mut self, packet: packet::Packet, out: &mut Vec<u8>) {
        let seq = packet.seq;
        let reply_u32 = |link: &Link, opcode: u8, value: u32| {
            link.out_of_band(seq, opcode, &value.to_le_bytes())
        };
        match packet.base_opcode() {
            op::ASK_RESET => {
                let Some(session) = packet.payload_u32() else {
                    return;
                };
                self.link.reset(session);
                self.link.accept_control(0);
                self.messages.reset();
                self.stream_ready = false;
                self.queue.clear();
                self.sink.cancel();
                self.log
                    .push(format!("host connected (session {session:#010x})"));
                out.extend(self.link.out_of_band(seq, op::RET_RESET, &[]));
            }
            op::ASK_VERSION => {
                self.link.accept_control(seq);
                out.extend(reply_u32(
                    &self.link,
                    op::RET_VERSION,
                    packet::PROTOCOL_VERSION,
                ));
            }
            op::ASK_PACKET_SIZE => {
                self.link.accept_control(seq);
                out.extend(reply_u32(
                    &self.link,
                    op::RET_PACKET_SIZE,
                    MAX_PACKET_BYTES as u32,
                ));
            }
            op::ASK_BUFFER_SLOTS => {
                self.link.accept_control(seq);
                out.extend(reply_u32(
                    &self.link,
                    op::RET_BUFFER_SLOTS,
                    u32::from(WINDOW),
                ));
            }
            op::ASK_BUFFER_BYTES => {
                self.link.accept_control(seq);
                out.extend(reply_u32(&self.link, op::RET_BUFFER_BYTES, BUFFER_BYTES));
            }
            op::ASK_STREAM_DATA => {
                self.stream_ready = true;
                if self.link.accept_stream(seq, &packet.payload) {
                    out.extend(reply_u32(
                        &self.link,
                        op::RET_STREAM_DATA,
                        self.link.free_bytes(),
                    ));
                }
                let bytes = self.link.take_stream();
                for message in self.messages.push(&bytes) {
                    self.on_message(message, out);
                }
            }
            op::RET_STREAM_DATA => {
                self.link.acknowledge(seq);
            }
            op::REBOOT_TO_BOOTLOADER => {}
            other => out.extend(reply_u32(&self.link, op::UNKNOWN_OPCODE, u32::from(other))),
        }
    }

    fn on_message(&mut self, message: Message, out: &mut Vec<u8>) {
        use message::op as m;
        let id = message.id;
        let ret_u32 = |value| Message::u32(m::RET_U32, id, value);
        let reply = match message.opcode {
            m::PROTOCOL_VERSION => Some(ret_u32(message::PROTOCOL_VERSION)),
            m::FIRMWARE_VERSION => Some(ret_u32(FIRMWARE_VERSION)),
            m::DEVICE_IDENTIFIER => Some(ret_u32(DEVICE_IDENTIFIER)),
            m::DEVICE_NAME => Some(Message::new(m::RET_DATA, id, self.name.as_bytes().to_vec())),
            m::CONTROLLER_LIST => {
                let ids = [
                    ControllerKind::WirelessProController.id(),
                    ControllerKind::WiredController.id(),
                ];
                Some(Message::new(
                    m::RET_DATA,
                    id,
                    ids.iter().flat_map(|i| i.to_le_bytes()).collect(),
                ))
            }
            m::CQ_CAPACITY => Some(ret_u32(COMMAND_QUEUE_CAPACITY)),
            m::READ_CONTROLLER_MODE => Some(ret_u32(self.mode.id())),
            m::REQUEST_STATUS => Some(ret_u32(0)),
            m::REQUEST_SESSION_NUM => Some(ret_u32(self.link.session())),
            m::SET_LOGGING_FLAG => None,
            m::CHANGE_CONTROLLER_MODE | m::RESET_TO_CONTROLLER => {
                match message.body_u32().and_then(ControllerKind::from_id) {
                    Some(kind) => {
                        self.mode = kind;
                        self.log.push(format!("controller mode set to {kind:?}"));
                    }
                    None => self.log.push(format!(
                        "unsupported controller mode {:?}",
                        message.body_u32()
                    )),
                }
                None
            }
            m::CQ_CANCEL => {
                self.sink.cancel();
                self.queue.clear();
                self.log.push("command queue cancelled".into());
                None
            }
            m::CQ_REPLACE_ON_NEXT => {
                self.replace_on_next = true;
                None
            }
            m::CMD_NS1_OEM_CONTROLLER_BUTTONS | m::CMD_NS_WIRED_CONTROLLER_STATE => {
                match decode_command(&message) {
                    Some((buttons, ms)) => {
                        if std::mem::take(&mut self.replace_on_next) {
                            self.sink.cancel();
                            self.queue.clear();
                        }
                        let sink_id = self.sink.hold(buttons, ms);
                        self.queue.push_back((id, sink_id));
                        self.log.push(format!("cmd #{id}: {buttons} for {ms} ms"));
                        None
                    }
                    None => Some(Message::new(m::CQ_COMMAND_DROPPED, id, vec![])),
                }
            }
            _ if id != 0 => Some(Message::new(m::REQUEST_DROPPED, id, vec![])),
            _ => None,
        };
        if let Some(reply) = reply {
            self.send(&reply, out);
        }
    }

    fn send(&mut self, message: &Message, out: &mut Vec<u8>) {
        if !self.stream_ready {
            out.extend(self.link.out_of_band(0, op::INFO_STREAM_NOT_READY, &[]));
            return;
        }
        out.extend(self.link.send_stream(&message.encode()));
    }
}
