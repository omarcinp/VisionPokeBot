//! Messages carried in the reliable stream:
//! `[message_bytes u16 LE (incl. header)][opcode u8][id u8][body]`.

pub const PROTOCOL_VERSION: u32 = 2026061800;
pub const HEADER_BYTES: usize = 4;

pub mod op {
    pub const LOG_STRING: u8 = 0x01;
    pub const LOG_LABEL_H32: u8 = 0x02;
    pub const LOG_LABEL_U32: u8 = 0x03;
    pub const LOG_LABEL_I32: u8 = 0x04;
    pub const REQUEST_DROPPED: u8 = 0x10;
    pub const RET: u8 = 0x11;
    pub const RET_U32: u8 = 0x12;
    pub const RET_DATA: u8 = 0x13;
    pub const RET_U32_DATA: u8 = 0x14;
    pub const PROTOCOL_VERSION: u8 = 0x20;
    pub const FIRMWARE_VERSION: u8 = 0x21;
    pub const DEVICE_IDENTIFIER: u8 = 0x22;
    pub const DEVICE_NAME: u8 = 0x23;
    pub const CONTROLLER_LIST: u8 = 0x24;
    pub const SET_LOGGING_FLAG: u8 = 0x25;
    pub const CQ_CAPACITY: u8 = 0x26;
    pub const REQUEST_SESSION_NUM: u8 = 0x30;
    pub const REQUEST_STATUS: u8 = 0x31;
    pub const READ_CONTROLLER_MODE: u8 = 0x32;
    pub const CHANGE_CONTROLLER_MODE: u8 = 0x33;
    pub const RESET_TO_CONTROLLER: u8 = 0x34;
    pub const CQ_COMMAND_DROPPED: u8 = 0x40;
    pub const CQ_CANCEL: u8 = 0x41;
    pub const CQ_REPLACE_ON_NEXT: u8 = 0x42;
    pub const CQ_COMMAND_FINISHED: u8 = 0x43;
    pub const CMD_NS_WIRED_CONTROLLER_STATE: u8 = 0x90;
    pub const CMD_NS1_OEM_CONTROLLER_BUTTONS: u8 = 0x97;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub opcode: u8,
    pub id: u8,
    pub body: Vec<u8>,
}

impl Message {
    pub fn new(opcode: u8, id: u8, body: Vec<u8>) -> Self {
        Self { opcode, id, body }
    }

    pub fn u32(opcode: u8, id: u8, value: u32) -> Self {
        Self::new(opcode, id, value.to_le_bytes().to_vec())
    }

    pub fn body_u32(&self) -> Option<u32> {
        self.body
            .get(..4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn encode(&self) -> Vec<u8> {
        let total = (HEADER_BYTES + self.body.len()) as u16;
        let mut bytes = total.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[self.opcode, self.id]);
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

/// Splits the reassembled stream into messages.
#[derive(Debug, Default)]
pub struct MessageReader {
    buffer: Vec<u8>,
}

impl MessageReader {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Message> {
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            if self.buffer.len() < HEADER_BYTES {
                break;
            }
            let total = usize::from(u16::from_le_bytes([self.buffer[0], self.buffer[1]]));
            if total < HEADER_BYTES {
                // Corrupt framing: the stream cannot be resynchronised.
                self.buffer.clear();
                break;
            }
            if self.buffer.len() < total {
                break;
            }
            let rest = self.buffer.split_off(total);
            let raw = std::mem::replace(&mut self.buffer, rest);
            out.push(Message::new(raw[2], raw[3], raw[HEADER_BYTES..].to_vec()));
        }
        out
    }

    pub fn reset(&mut self) {
        self.buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_across_chunks() {
        let a = Message::u32(op::RET_U32, 7, PROTOCOL_VERSION);
        let b = Message::new(op::CQ_CANCEL, 0, vec![]);
        let mut wire = a.encode();
        wire.extend(b.encode());
        let mut reader = MessageReader::default();
        let mut got = reader.push(&wire[..5]);
        got.extend(reader.push(&wire[5..]));
        assert_eq!(got, vec![a, b]);
        assert_eq!(got[0].body_u32(), Some(PROTOCOL_VERSION));
    }
}
