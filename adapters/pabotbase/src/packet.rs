//! Link-layer packets:
//! `[0x81][seq u8][packet_bytes u8 (0 = 256)][opcode u8][payload][crc32c u32 LE]`.
//! The CRC covers everything before it and is seeded with the session id,
//! except `ASK_RESET`, which is seeded with `0xFFFFFFFF`.

use crate::crc::crc32c;

pub const MAGIC: u8 = 0x81;
pub const PROTOCOL_VERSION: u32 = 2026061800;
pub const RETRANSMIT_FLAG: u8 = 0x80;
pub const OPCODE_MASK: u8 = 0x7f;
pub const HEADER_BYTES: usize = 4;
pub const MIN_PACKET_BYTES: usize = HEADER_BYTES + 4;
pub const MAX_PACKET_BYTES: usize = 256;
pub const RESET_CRC_SEED: u32 = 0xFFFF_FFFF;

pub mod op {
    pub const INVALID: u8 = 0x00;
    pub const ASK_RESET: u8 = 0x01;
    pub const ASK_VERSION: u8 = 0x02;
    pub const ASK_PACKET_SIZE: u8 = 0x03;
    pub const ASK_BUFFER_SLOTS: u8 = 0x04;
    pub const ASK_BUFFER_BYTES: u8 = 0x05;
    pub const REBOOT_TO_BOOTLOADER: u8 = 0x06;
    pub const INFO_STREAM_DEAD: u8 = 0x10;
    pub const INFO_STREAM_NOT_READY: u8 = 0x11;
    pub const ASK_STREAM_DATA: u8 = 0x12;
    pub const INFO_STREAM_SEND_FULL: u8 = 0x18;
    pub const INFO_STREAM_RECV_FULL: u8 = 0x19;
    pub const INFO: u8 = 0x20;
    pub const INFO_STR: u8 = 0x25;
    pub const INFO_LABEL_I32: u8 = 0x28;
    pub const INVALID_LENGTH: u8 = 0x30;
    pub const INVALID_CHECKSUM_FAIL: u8 = 0x31;
    pub const UNKNOWN_OPCODE: u8 = 0x32;
    pub const RET_RESET: u8 = 0x41;
    pub const RET_VERSION: u8 = 0x42;
    pub const RET_PACKET_SIZE: u8 = 0x43;
    pub const RET_BUFFER_SLOTS: u8 = 0x44;
    pub const RET_BUFFER_BYTES: u8 = 0x45;
    pub const RET_STREAM_DATA: u8 = 0x52;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub seq: u8,
    /// Raw opcode byte, including [`RETRANSMIT_FLAG`] if set.
    pub opcode: u8,
    pub payload: Vec<u8>,
}

impl Packet {
    pub fn base_opcode(&self) -> u8 {
        self.opcode & OPCODE_MASK
    }

    /// First four payload bytes as a little-endian u32.
    pub fn payload_u32(&self) -> Option<u32> {
        self.payload
            .get(..4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Valid(Packet),
    /// Length byte out of range; the header bytes were discarded.
    Invalid {
        seq: u8,
    },
    ChecksumFail {
        seq: u8,
    },
}

/// Panics if the packet would exceed 256 bytes.
pub fn encode(seq: u8, opcode: u8, payload: &[u8], crc_seed: u32) -> Vec<u8> {
    let total = MIN_PACKET_BYTES + payload.len();
    assert!(total <= MAX_PACKET_BYTES, "packet of {total} bytes");
    let mut bytes = Vec::with_capacity(total);
    bytes.extend_from_slice(&[MAGIC, seq, (total % 256) as u8, opcode]);
    bytes.extend_from_slice(payload);
    seal(&mut bytes, crc_seed);
    bytes
}

/// Appends the CRC to a packet body (header + payload).
pub fn seal(bytes: &mut Vec<u8>, crc_seed: u32) {
    let crc = crc32c(crc_seed, bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
}

/// Incremental parser; tolerates garbage between packets by resynchronising
/// on the magic byte.
#[derive(Debug, Default)]
pub struct Parser {
    buffer: Vec<u8>,
}

impl Parser {
    pub fn reset(&mut self) {
        self.buffer.clear();
    }

    pub fn push(&mut self, mut bytes: &[u8], session_id: u32) -> Vec<Parsed> {
        let mut out = Vec::new();
        while !bytes.is_empty() {
            if self.buffer.is_empty() {
                match bytes.iter().position(|b| *b == MAGIC) {
                    Some(start) => bytes = &bytes[start..],
                    None => break,
                }
            }
            let want = if self.buffer.len() < MIN_PACKET_BYTES {
                MIN_PACKET_BYTES
            } else {
                packet_len(self.buffer[2])
            };
            let take = (want - self.buffer.len()).min(bytes.len());
            self.buffer.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buffer.len() < want {
                break;
            }
            let total = packet_len(self.buffer[2]);
            if total < MIN_PACKET_BYTES {
                out.push(Parsed::Invalid {
                    seq: self.buffer[1],
                });
                self.buffer.clear();
                continue;
            }
            if self.buffer.len() < total {
                continue; // header complete, keep reading the body
            }
            out.push(verify(&self.buffer, session_id));
            self.buffer.clear();
        }
        out
    }
}

fn packet_len(byte: u8) -> usize {
    if byte == 0 {
        MAX_PACKET_BYTES
    } else {
        usize::from(byte)
    }
}

fn verify(bytes: &[u8], session_id: u32) -> Parsed {
    let (body, crc) = bytes.split_at(bytes.len() - 4);
    let (seq, opcode) = (body[1], body[3]);
    // Upstream compares the raw opcode byte, so a retransmitted reset (0x81)
    // is checked against the session id.
    let seed = if opcode == op::ASK_RESET {
        RESET_CRC_SEED
    } else {
        session_id
    };
    if crc32c(seed, body).to_le_bytes() != crc {
        return Parsed::ChecksumFail { seq };
    }
    Parsed::Valid(Packet {
        seq,
        opcode,
        payload: body[HEADER_BYTES..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_with_garbage_and_split_reads() {
        let session = 0x1234_5678;
        let a = encode(3, op::ASK_VERSION, &[], session);
        let b = encode(4, op::ASK_STREAM_DATA, &[0, 0, 9, 8, 7], session);
        let mut wire = vec![0x00, 0x55];
        wire.extend(&a);
        wire.push(0x42);
        wire.extend(&b);
        let mut parser = Parser::default();
        let mut parsed = Vec::new();
        for chunk in wire.chunks(3) {
            parsed.extend(parser.push(chunk, session));
        }
        assert_eq!(
            parsed,
            vec![
                Parsed::Valid(Packet {
                    seq: 3,
                    opcode: op::ASK_VERSION,
                    payload: vec![]
                }),
                Parsed::Valid(Packet {
                    seq: 4,
                    opcode: op::ASK_STREAM_DATA,
                    payload: vec![0, 0, 9, 8, 7]
                }),
            ]
        );
    }

    #[test]
    fn reset_uses_fixed_seed_and_bad_crc_is_reported() {
        let reset = encode(0, op::ASK_RESET, &7u32.to_le_bytes(), RESET_CRC_SEED);
        let mut parser = Parser::default();
        assert!(matches!(parser.push(&reset, 99)[..], [Parsed::Valid(_)]));
        let mut corrupt = encode(5, op::ASK_VERSION, &[], 99);
        corrupt[4] ^= 1;
        assert_eq!(
            parser.push(&corrupt, 99),
            vec![Parsed::ChecksumFail { seq: 5 }]
        );
    }

    #[test]
    fn length_byte_zero_means_256() {
        let payload = vec![0xAB; 248];
        let bytes = encode(1, op::ASK_STREAM_DATA, &payload, 5);
        assert_eq!((bytes.len(), bytes[2]), (256, 0));
        let parsed = Parser::default().push(&bytes, 5);
        assert!(matches!(&parsed[..], [Parsed::Valid(p)] if p.payload == payload));
    }
}
