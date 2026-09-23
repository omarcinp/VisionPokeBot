//! Battery-backed cartridge save persistence.
//!
//! A real cartridge keeps its save in battery/flash memory; libretro cores
//! instead hand that buffer to the frontend to persist. This module is the
//! ONLY place allowed to touch core memory, and it may only use the save-RAM
//! region, copying it opaquely to and from a `.sav` file. The contents are
//! never parsed or exposed to the bot. `tests/no_privileged_access.rs`
//! enforces this.

use std::os::raw::{c_uint, c_void};
use std::path::{Path, PathBuf};

use libloading::Library;
use pokebot_core::{Error, Result};

const RETRO_MEMORY_SAVE_RAM: c_uint = 0;

type GetMemoryData = unsafe extern "C" fn(id: c_uint) -> *mut c_void;
type GetMemorySize = unsafe extern "C" fn(id: c_uint) -> usize;

pub struct BatterySave {
    path: PathBuf,
    get_data: GetMemoryData,
    get_size: GetMemorySize,
    /// Contents at load time, to avoid rewriting an unchanged save.
    baseline: Vec<u8>,
}

impl BatterySave {
    /// Binds save-RAM access and restores `path` into it if the file exists.
    /// Must be called after the game is loaded.
    pub fn attach(library: &Library, path: &Path) -> Result<Self> {
        // SAFETY: signatures follow libretro.h.
        let get_data: GetMemoryData = *unsafe { library.get(b"retro_get_memory_data\0") }
            .map_err(|e| Error::Device(format!("core lacks save RAM access: {e}")))?;
        // SAFETY: as above.
        let get_size: GetMemorySize = *unsafe { library.get(b"retro_get_memory_size\0") }
            .map_err(|e| Error::Device(format!("core lacks save RAM access: {e}")))?;
        let mut save = Self {
            path: path.to_path_buf(),
            get_data,
            get_size,
            baseline: Vec::new(),
        };
        if path.exists() {
            let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
            if let Some(ram) = save.ram() {
                let n = ram.len().min(bytes.len());
                ram[..n].copy_from_slice(&bytes[..n]);
            }
        }
        save.baseline = save.ram().map(|ram| ram.to_vec()).unwrap_or_default();
        Ok(save)
    }

    /// Writes save RAM to disk if it changed since load / the last store.
    pub fn store(&mut self) -> Result<()> {
        let Some(current) = self.ram().map(|ram| ram.to_vec()) else {
            return Ok(());
        };
        if current == self.baseline {
            return Ok(());
        }
        let tmp = self.path.with_extension("sav.tmp");
        std::fs::write(&tmp, &current).map_err(|e| Error::io(&tmp, e))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| Error::io(&self.path, e))?;
        self.baseline = current;
        Ok(())
    }

    fn ram(&mut self) -> Option<&mut [u8]> {
        // SAFETY: the game is loaded and the core is used from this thread only.
        let (data, size) = unsafe {
            (
                (self.get_data)(RETRO_MEMORY_SAVE_RAM),
                (self.get_size)(RETRO_MEMORY_SAVE_RAM),
            )
        };
        if data.is_null() || size == 0 {
            return None;
        }
        // SAFETY: the core owns `size` bytes at `data` while the game is loaded.
        Some(unsafe { std::slice::from_raw_parts_mut(data as *mut u8, size) })
    }
}
