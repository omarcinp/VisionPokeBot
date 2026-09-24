//! PABotBase2 serial protocol (the protocol spoken by current PokemonAutomation
//! ESP32 / RP2040 firmware that emulates a Nintendo Switch controller).
//!
//! Implemented from the wire format in PokemonAutomation/Arduino-Source
//! (`Common/PABotBase2`, MIT), not from its code. Layers:
//!
//! 1. [`packet`] — framed packets: magic, seq, length, opcode, payload, CRC32C.
//! 2. [`link`] — reliable, ordered byte stream over those packets.
//! 3. [`message`] — length-prefixed messages carried in the stream.
//! 4. [`report`] — Switch controller button reports, and the GBA → Switch map.
//!
//! [`PabotBaseController`] is the PC side (implements `Controller`);
//! [`device::VirtualDevice`] is a device-side peer used by the emulator's
//! virtual console, so development exercises the same protocol end to end.

pub mod crc;
pub mod device;
pub mod link;
pub mod message;
pub mod packet;
pub mod report;

mod client;

pub use client::{DeviceInfo, PabotBaseConfig, PabotBaseController};
pub use report::{switch_input, ControllerKind, SwitchInput};
