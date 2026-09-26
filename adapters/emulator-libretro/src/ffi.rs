//! Minimal libretro ABI: only the entry points needed to run a game, receive
//! video, supply joypad input, and carry link-port packets.

use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::path::Path;

use libloading::Library;
use pokebot_core::{Error, Result};

pub const RETRO_API_VERSION: c_uint = 1;
pub const RETRO_DEVICE_JOYPAD: c_uint = 1;

pub const RETRO_DEVICE_ID_JOYPAD_B: c_uint = 0;
pub const RETRO_DEVICE_ID_JOYPAD_SELECT: c_uint = 2;
pub const RETRO_DEVICE_ID_JOYPAD_START: c_uint = 3;
pub const RETRO_DEVICE_ID_JOYPAD_UP: c_uint = 4;
pub const RETRO_DEVICE_ID_JOYPAD_DOWN: c_uint = 5;
pub const RETRO_DEVICE_ID_JOYPAD_LEFT: c_uint = 6;
pub const RETRO_DEVICE_ID_JOYPAD_RIGHT: c_uint = 7;
pub const RETRO_DEVICE_ID_JOYPAD_A: c_uint = 8;
pub const RETRO_DEVICE_ID_JOYPAD_L: c_uint = 10;
pub const RETRO_DEVICE_ID_JOYPAD_R: c_uint = 11;

pub const RETRO_ENVIRONMENT_GET_CAN_DUPE: c_uint = 3;
pub const RETRO_ENVIRONMENT_GET_SYSTEM_DIRECTORY: c_uint = 9;
pub const RETRO_ENVIRONMENT_SET_PIXEL_FORMAT: c_uint = 10;
pub const RETRO_ENVIRONMENT_SET_INPUT_DESCRIPTORS: c_uint = 11;
pub const RETRO_ENVIRONMENT_SET_VARIABLES: c_uint = 16;
pub const RETRO_ENVIRONMENT_GET_VARIABLE_UPDATE: c_uint = 17;
pub const RETRO_ENVIRONMENT_GET_SAVE_DIRECTORY: c_uint = 31;
pub const RETRO_ENVIRONMENT_SET_CONTROLLER_INFO: c_uint = 35;
pub const RETRO_ENVIRONMENT_SET_GEOMETRY: c_uint = 37;
pub const RETRO_ENVIRONMENT_SET_NETPACKET_INTERFACE: c_uint = 78;

pub const RETRO_PIXEL_FORMAT_0RGB1555: c_uint = 0;
pub const RETRO_PIXEL_FORMAT_XRGB8888: c_uint = 1;
pub const RETRO_PIXEL_FORMAT_RGB565: c_uint = 2;

/// Frontend function the core calls to send a packet to other players.
pub type NetpacketSendFn =
    unsafe extern "C" fn(flags: c_int, buf: *const c_void, len: usize, client_id: u16);
/// Frontend function the core may call to receive packets mid-frame.
pub type NetpacketPollReceiveFn = unsafe extern "C" fn();

/// `struct retro_netpacket_callback`: how a core exchanges packets with the
/// cores of other players (for GBA cores, the link port).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RetroNetpacketCallback {
    pub start: unsafe extern "C" fn(
        client_id: u16,
        send: NetpacketSendFn,
        poll_receive: NetpacketPollReceiveFn,
    ),
    pub receive: unsafe extern "C" fn(buf: *const c_void, len: usize, client_id: u16),
    pub stop: Option<unsafe extern "C" fn()>,
    pub poll: Option<unsafe extern "C" fn()>,
    pub connected: Option<unsafe extern "C" fn(client_id: u16) -> bool>,
    pub disconnected: Option<unsafe extern "C" fn(client_id: u16)>,
    pub protocol_version: *const c_char,
}

#[repr(C)]
pub struct RetroGameInfo {
    pub path: *const c_char,
    pub data: *const c_void,
    pub size: usize,
    pub meta: *const c_char,
}

#[repr(C)]
pub struct RetroSystemInfo {
    pub library_name: *const c_char,
    pub library_version: *const c_char,
    pub valid_extensions: *const c_char,
    pub need_fullpath: bool,
    pub block_extract: bool,
}

#[repr(C)]
#[derive(Default)]
pub struct RetroGameGeometry {
    pub base_width: c_uint,
    pub base_height: c_uint,
    pub max_width: c_uint,
    pub max_height: c_uint,
    pub aspect_ratio: f32,
}

#[repr(C)]
#[derive(Default)]
pub struct RetroSystemTiming {
    pub fps: f64,
    pub sample_rate: f64,
}

#[repr(C)]
#[derive(Default)]
pub struct RetroSystemAvInfo {
    pub geometry: RetroGameGeometry,
    pub timing: RetroSystemTiming,
}

pub type EnvironmentFn = unsafe extern "C" fn(cmd: c_uint, data: *mut c_void) -> bool;
pub type VideoRefreshFn =
    unsafe extern "C" fn(data: *const c_void, width: c_uint, height: c_uint, pitch: usize);
pub type AudioSampleFn = unsafe extern "C" fn(left: i16, right: i16);
pub type AudioSampleBatchFn = unsafe extern "C" fn(data: *const i16, frames: usize) -> usize;
pub type InputPollFn = unsafe extern "C" fn();
pub type InputStateFn =
    unsafe extern "C" fn(port: c_uint, device: c_uint, index: c_uint, id: c_uint) -> i16;

/// Function table of a loaded core. Holds the library open for as long as the
/// pointers are in use.
pub struct CoreApi {
    pub api_version: unsafe extern "C" fn() -> c_uint,
    pub set_environment: unsafe extern "C" fn(EnvironmentFn),
    pub set_video_refresh: unsafe extern "C" fn(VideoRefreshFn),
    pub set_audio_sample: unsafe extern "C" fn(AudioSampleFn),
    pub set_audio_sample_batch: unsafe extern "C" fn(AudioSampleBatchFn),
    pub set_input_poll: unsafe extern "C" fn(InputPollFn),
    pub set_input_state: unsafe extern "C" fn(InputStateFn),
    pub init: unsafe extern "C" fn(),
    pub deinit: unsafe extern "C" fn(),
    pub get_system_info: unsafe extern "C" fn(*mut RetroSystemInfo),
    pub get_system_av_info: unsafe extern "C" fn(*mut RetroSystemAvInfo),
    pub set_controller_port_device: unsafe extern "C" fn(port: c_uint, device: c_uint),
    pub load_game: unsafe extern "C" fn(*const RetroGameInfo) -> bool,
    pub unload_game: unsafe extern "C" fn(),
    pub run: unsafe extern "C" fn(),
    library: Library,
}

impl CoreApi {
    /// # Safety
    /// `path` must be a libretro core; loading runs its initializers.
    pub unsafe fn load(path: &Path) -> Result<Self> {
        // SAFETY: caller guarantees the path is a libretro core.
        let library = unsafe { Library::new(path) }
            .map_err(|e| Error::Device(format!("cannot load core {}: {e}", path.display())))?;
        macro_rules! sym {
            ($name:literal) => {
                // SAFETY: symbol types follow libretro.h for API version 1.
                *unsafe { library.get(concat!($name, "\0").as_bytes()) }
                    .map_err(|e| Error::Device(format!("core is missing {}: {e}", $name)))?
            };
        }
        Ok(Self {
            api_version: sym!("retro_api_version"),
            set_environment: sym!("retro_set_environment"),
            set_video_refresh: sym!("retro_set_video_refresh"),
            set_audio_sample: sym!("retro_set_audio_sample"),
            set_audio_sample_batch: sym!("retro_set_audio_sample_batch"),
            set_input_poll: sym!("retro_set_input_poll"),
            set_input_state: sym!("retro_set_input_state"),
            init: sym!("retro_init"),
            deinit: sym!("retro_deinit"),
            get_system_info: sym!("retro_get_system_info"),
            get_system_av_info: sym!("retro_get_system_av_info"),
            set_controller_port_device: sym!("retro_set_controller_port_device"),
            load_game: sym!("retro_load_game"),
            unload_game: sym!("retro_unload_game"),
            run: sym!("retro_run"),
            library,
        })
    }

    pub fn library(&self) -> &Library {
        &self.library
    }
}
