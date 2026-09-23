//! Reliable link endpoint (sans-IO). Both sides use it:
//!
//! * Every reliable packet takes the next 8-bit seq and is retransmitted
//!   (with the retransmit flag) until acknowledged.
//! * Stream data (`ASK_STREAM_DATA`) carries a u16 stream offset and is
//!   acknowledged with `RET_STREAM_DATA` + free receive bytes.
//! * The receiver tracks the peer's seq space: control requests and stream
//!   packets both occupy a slot, and stream bytes are released in seq order.

use std::collections::{HashMap, VecDeque};

use crate::packet::{self, op, Parsed, Parser, RETRANSMIT_FLAG};

/// Receive window, matching the firmware's reorder window.
pub const WINDOW: u8 = 128;
/// Receive buffer the peer may fill before we read.
pub const BUFFER_BYTES: u32 = 16384;
const STREAM_OVERHEAD: usize = packet::MIN_PACKET_BYTES + 2;
/// Unacked packets are resent once they are this many ticks old.
const RETRANSMIT_AFTER_TICKS: u32 = 2;

#[derive(Debug)]
struct Unacked {
    seq: u8,
    body: Vec<u8>, // header + payload, without CRC
    crc_seed: u32,
    sent_tick: u32,
}

#[derive(Debug)]
pub struct Link {
    session: u32,
    parser: Parser,
    // sending
    next_seq: u8,
    unacked: VecDeque<Unacked>,
    stream_offset: u16,
    max_packet_bytes: usize,
    tick: u32,
    // receiving
    recv_head: u8,
    received: HashMap<u8, Option<Vec<u8>>>,
    ready: Vec<u8>,
}

impl Link {
    pub fn new(session: u32, max_packet_bytes: usize) -> Self {
        Self {
            session,
            parser: Parser::default(),
            next_seq: 0,
            unacked: VecDeque::new(),
            stream_offset: 0,
            max_packet_bytes,
            tick: 0,
            recv_head: 0,
            received: HashMap::new(),
            ready: Vec::new(),
        }
    }

    /// Starts a new session: both seq spaces and the stream restart at 0.
    pub fn reset(&mut self, session: u32) {
        let max = self.max_packet_bytes;
        *self = Self::new(session, max);
    }

    pub fn session(&self) -> u32 {
        self.session
    }

    pub fn set_max_packet_bytes(&mut self, bytes: usize) {
        self.max_packet_bytes = bytes.clamp(STREAM_OVERHEAD + 1, packet::MAX_PACKET_BYTES);
    }

    pub fn unacked(&self) -> usize {
        self.unacked.len()
    }

    pub fn parse(&mut self, bytes: &[u8]) -> Vec<Parsed> {
        self.parser.push(bytes, self.session)
    }

    /// A reliable request (retransmitted until acked). Returns wire bytes.
    pub fn send_request(&mut self, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let seed = if opcode == op::ASK_RESET {
            packet::RESET_CRC_SEED
        } else {
            self.session
        };
        self.push_reliable(opcode, payload, seed)
    }

    /// Queues stream bytes, split into as many packets as needed. Returns
    /// wire bytes for all of them.
    pub fn send_stream(&mut self, mut data: &[u8]) -> Vec<u8> {
        let mut wire = Vec::new();
        while !data.is_empty() {
            let chunk = data.len().min(self.max_packet_bytes - STREAM_OVERHEAD);
            let mut payload = self.stream_offset.to_le_bytes().to_vec();
            payload.extend_from_slice(&data[..chunk]);
            self.stream_offset = self.stream_offset.wrapping_add(chunk as u16);
            wire.extend(self.push_reliable(op::ASK_STREAM_DATA, &payload, self.session));
            data = &data[chunk..];
        }
        wire
    }

    /// Stream packets `data` would need at the current packet size.
    pub fn packets_for(&self, bytes: usize) -> usize {
        bytes.div_ceil(self.max_packet_bytes - STREAM_OVERHEAD)
    }

    /// An unreliable (never retransmitted) packet, e.g. an ack.
    pub fn out_of_band(&self, seq: u8, opcode: u8, payload: &[u8]) -> Vec<u8> {
        packet::encode(seq, opcode, payload, self.session)
    }

    /// The peer acknowledged `seq`.
    pub fn acknowledge(&mut self, seq: u8) -> bool {
        let before = self.unacked.len();
        self.unacked.retain(|p| p.seq != seq);
        before != self.unacked.len()
    }

    /// Advances the retransmit clock; returns packets to resend.
    pub fn tick(&mut self) -> Vec<u8> {
        self.tick = self.tick.wrapping_add(1);
        let mut wire = Vec::new();
        for pending in &mut self.unacked {
            if self.tick.wrapping_sub(pending.sent_tick) >= RETRANSMIT_AFTER_TICKS {
                pending.body[3] |= RETRANSMIT_FLAG;
                pending.sent_tick = self.tick;
                let mut bytes = pending.body.clone();
                packet::seal(&mut bytes, pending.crc_seed);
                wire.extend(bytes);
            }
        }
        wire
    }

    /// Records a non-stream request from the peer (occupies its seq slot).
    pub fn accept_control(&mut self, seq: u8) {
        if self.in_window(seq) {
            self.received.insert(seq, None);
            self.advance();
        }
    }

    /// Records stream data from the peer. Returns true if it should be acked
    /// (new, or an old duplicate); false if it is too far ahead.
    pub fn accept_stream(&mut self, seq: u8, payload: &[u8]) -> bool {
        if payload.len() < 2 {
            return false;
        }
        if !self.in_window(seq) {
            // Behind the head: already delivered, ack again.
            return seq.wrapping_sub(self.recv_head) & 0x80 != 0;
        }
        self.received.insert(seq, Some(payload[2..].to_vec()));
        self.advance();
        true
    }

    /// Stream bytes delivered in order since the last call.
    pub fn take_stream(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.ready)
    }

    pub fn free_bytes(&self) -> u32 {
        BUFFER_BYTES.saturating_sub(self.ready.len() as u32)
    }

    fn in_window(&self, seq: u8) -> bool {
        seq.wrapping_sub(self.recv_head) < WINDOW
    }

    fn advance(&mut self) {
        while let Some(slot) = self.received.remove(&self.recv_head) {
            if let Some(data) = slot {
                self.ready.extend(data);
            }
            self.recv_head = self.recv_head.wrapping_add(1);
        }
    }

    fn push_reliable(&mut self, opcode: u8, payload: &[u8], crc_seed: u32) -> Vec<u8> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let wire = packet::encode(seq, opcode, payload, crc_seed);
        self.unacked.push_back(Unacked {
            seq,
            body: wire[..wire.len() - 4].to_vec(),
            crc_seed,
            sent_tick: self.tick,
        });
        wire
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::Packet;

    fn valid(parsed: Vec<Parsed>) -> Vec<Packet> {
        parsed
            .into_iter()
            .map(|p| match p {
                Parsed::Valid(p) => p,
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn stream_is_split_reassembled_and_reordered() {
        let mut tx = Link::new(9, 24);
        let mut rx = Link::new(9, 256);
        let data: Vec<u8> = (0..40).collect();
        let wire = tx.send_stream(&data);
        let mut packets = valid(rx.parse(&wire));
        assert_eq!(packets.len(), tx.packets_for(40));
        packets.swap(0, 1); // simulate a retransmit arriving late
        for p in &packets {
            assert!(rx.accept_stream(p.seq, &p.payload));
        }
        assert_eq!(rx.take_stream(), data);
        // A duplicate of an already-delivered packet is acked again.
        assert!(rx.accept_stream(packets[0].seq, &packets[0].payload));
        assert!(rx.take_stream().is_empty());
    }

    #[test]
    fn control_slots_gate_stream_delivery() {
        let mut rx = Link::new(1, 256);
        assert!(rx.accept_stream(1, &[0, 0, 42]));
        assert!(rx.take_stream().is_empty(), "seq 0 not seen yet");
        rx.accept_control(0);
        assert_eq!(rx.take_stream(), vec![42]);
    }

    #[test]
    fn unacked_packets_are_retransmitted_with_flag() {
        let mut tx = Link::new(3, 256);
        tx.send_request(op::ASK_VERSION, &[]);
        assert!(tx.tick().is_empty());
        let resent = tx.tick();
        let packets = valid(Link::new(3, 256).parse(&resent));
        assert_eq!(packets[0].opcode, op::ASK_VERSION | RETRANSMIT_FLAG);
        assert!(tx.acknowledge(0));
        assert!(tx.tick().is_empty() && tx.tick().is_empty());
    }
}
