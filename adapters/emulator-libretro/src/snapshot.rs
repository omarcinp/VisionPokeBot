//! Development snapshots: the core's whole state as opaque bytes, for a
//! library of emulator scenarios that tests and development runs start
//! from (`pokebot goal --dev-snapshots`, `pokebot scenario`).
//!
//! They are a development tool, never a way for the bot to get anything
//! done: the bytes only travel between the core and files, are never
//! parsed, and are off unless [`crate::EmulatorConfig::dev_snapshots`] is
//! set, which only the command line's emulator devices do (a console run
//! through the capture card has none). The bot's crates may not name them.
//! `tests/no_privileged_access.rs` enforces all of this.

use std::os::raw::c_void;

use libloading::Library;
use pokebot_core::{Error, Result};

type SerializeSize = unsafe extern "C" fn() -> usize;
type Serialize = unsafe extern "C" fn(data: *mut c_void, size: usize) -> bool;
type Unserialize = unsafe extern "C" fn(data: *const c_void, size: usize) -> bool;

pub struct DevSnapshots {
    size: SerializeSize,
    serialize: Serialize,
    unserialize: Unserialize,
}

impl DevSnapshots {
    /// Binds the core's state entry points. Must be called after the game
    /// is loaded, on the core's thread.
    pub fn attach(library: &Library) -> Result<Self> {
        let missing = |e: libloading::Error| Error::Device(format!("core lacks snapshots: {e}"));
        // SAFETY: signatures follow libretro.h.
        let size: SerializeSize =
            *unsafe { library.get(b"retro_serialize_size\0") }.map_err(missing)?;
        // SAFETY: as above.
        let serialize: Serialize =
            *unsafe { library.get(b"retro_serialize\0") }.map_err(missing)?;
        // SAFETY: as above.
        let unserialize: Unserialize =
            *unsafe { library.get(b"retro_unserialize\0") }.map_err(missing)?;
        Ok(Self {
            size,
            serialize,
            unserialize,
        })
    }

    /// The core's state now.
    pub fn take(&self) -> Result<Vec<u8>> {
        // SAFETY: the game is loaded; called on the core's thread.
        let size = unsafe { (self.size)() };
        if size == 0 {
            return Err(Error::Device("core reports no snapshot size".into()));
        }
        let mut bytes = vec![0u8; size];
        // SAFETY: `bytes` holds `size` bytes, as the core asked.
        if !unsafe { (self.serialize)(bytes.as_mut_ptr().cast(), size) } {
            return Err(Error::Device("core failed to take a snapshot".into()));
        }
        Ok(bytes)
    }

    /// Puts the core back to `bytes` (a [`DevSnapshots::take`] of the same
    /// core and game).
    pub fn restore(&self, bytes: &[u8]) -> Result<()> {
        // SAFETY: the core reads `bytes.len()` bytes; the game is loaded.
        if unsafe { (self.unserialize)(bytes.as_ptr().cast(), bytes.len()) } {
            Ok(())
        } else {
            Err(Error::Device(
                "core refused the snapshot (another core or game?)".into(),
            ))
        }
    }
}
