//! Frontend callbacks handed to the core. Libretro callbacks carry no user
//! pointer, so their state lives in a thread-local on the emulator thread.

use std::cell::RefCell;
use std::ffi::{c_char, CString};
use std::os::raw::{c_uint, c_void};

use pokebot_core::{Button, ButtonSet, RgbImage};

use crate::ffi::*;

struct HostState {
    pixel_format: c_uint,
    buttons: ButtonSet,
    frame: Option<RgbImage>,
    system_dir: CString,
    save_dir: CString,
}

thread_local! {
    static HOST: RefCell<HostState> = RefCell::new(HostState {
        pixel_format: RETRO_PIXEL_FORMAT_0RGB1555,
        buttons: ButtonSet::NONE,
        frame: None,
        system_dir: CString::default(),
        save_dir: CString::default(),
    });
}

pub fn reset(system_dir: CString, save_dir: CString) {
    HOST.with_borrow_mut(|host| {
        host.pixel_format = RETRO_PIXEL_FORMAT_0RGB1555;
        host.buttons = ButtonSet::NONE;
        host.frame = None;
        host.system_dir = system_dir;
        host.save_dir = save_dir;
    });
}

pub fn set_buttons(buttons: ButtonSet) {
    HOST.with_borrow_mut(|host| host.buttons = buttons);
}

/// Most recent frame presented by the core (duplicated frames keep it).
pub fn latest_frame() -> Option<RgbImage> {
    HOST.with_borrow(|host| host.frame.clone())
}

pub unsafe extern "C" fn environment(cmd: c_uint, data: *mut c_void) -> bool {
    match cmd {
        RETRO_ENVIRONMENT_GET_CAN_DUPE if !data.is_null() => {
            // SAFETY: libretro passes a bool* for this command.
            unsafe { *(data as *mut bool) = true };
            true
        }
        RETRO_ENVIRONMENT_SET_PIXEL_FORMAT if !data.is_null() => {
            // SAFETY: libretro passes an enum retro_pixel_format* (C int).
            let format = unsafe { *(data as *const c_uint) };
            let supported = matches!(
                format,
                RETRO_PIXEL_FORMAT_0RGB1555
                    | RETRO_PIXEL_FORMAT_XRGB8888
                    | RETRO_PIXEL_FORMAT_RGB565
            );
            if supported {
                HOST.with_borrow_mut(|host| host.pixel_format = format);
            }
            supported
        }
        RETRO_ENVIRONMENT_GET_SYSTEM_DIRECTORY | RETRO_ENVIRONMENT_GET_SAVE_DIRECTORY
            if !data.is_null() =>
        {
            HOST.with_borrow(|host| {
                let dir = if cmd == RETRO_ENVIRONMENT_GET_SYSTEM_DIRECTORY {
                    &host.system_dir
                } else {
                    &host.save_dir
                };
                // SAFETY: libretro passes a const char**; the CString outlives
                // the core session because it is only replaced by `reset`.
                unsafe { *(data as *mut *const c_char) = dir.as_ptr() };
            });
            true
        }
        RETRO_ENVIRONMENT_GET_VARIABLE_UPDATE if !data.is_null() => {
            // SAFETY: libretro passes a bool* for this command.
            unsafe { *(data as *mut bool) = false };
            true
        }
        RETRO_ENVIRONMENT_SET_INPUT_DESCRIPTORS
        | RETRO_ENVIRONMENT_SET_VARIABLES
        | RETRO_ENVIRONMENT_SET_CONTROLLER_INFO
        | RETRO_ENVIRONMENT_SET_GEOMETRY => true,
        // Everything else (core options, logging, rumble, sensors, ...) is
        // declined so the core falls back to its defaults.
        _ => false,
    }
}

pub unsafe extern "C" fn video_refresh(
    data: *const c_void,
    width: c_uint,
    height: c_uint,
    pitch: usize,
) {
    if data.is_null() {
        return; // duplicated frame: keep the previous image
    }
    HOST.with_borrow_mut(|host| {
        let bytes_per_pixel = if host.pixel_format == RETRO_PIXEL_FORMAT_XRGB8888 {
            4
        } else {
            2
        };
        let (w, h) = (width as usize, height as usize);
        if w == 0 || h == 0 || pitch < w * bytes_per_pixel {
            return;
        }
        // SAFETY: the core guarantees `height` rows of `pitch` bytes.
        let src = unsafe {
            std::slice::from_raw_parts(data as *const u8, pitch * (h - 1) + w * bytes_per_pixel)
        };
        let mut rgb = Vec::with_capacity(w * h * 3);
        for row in 0..h {
            let line = &src[row * pitch..row * pitch + w * bytes_per_pixel];
            for px in line.chunks_exact(bytes_per_pixel) {
                rgb.extend_from_slice(&decode_pixel(host.pixel_format, px));
            }
        }
        host.frame = RgbImage::from_raw(width, height, rgb).ok();
    });
}

fn decode_pixel(format: c_uint, px: &[u8]) -> [u8; 3] {
    let expand5 = |v: u32| ((v << 3) | (v >> 2)) as u8;
    let expand6 = |v: u32| ((v << 2) | (v >> 4)) as u8;
    match format {
        RETRO_PIXEL_FORMAT_XRGB8888 => {
            let p = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
            [(p >> 16) as u8, (p >> 8) as u8, p as u8]
        }
        RETRO_PIXEL_FORMAT_RGB565 => {
            let p = u32::from(u16::from_le_bytes([px[0], px[1]]));
            [
                expand5((p >> 11) & 0x1f),
                expand6((p >> 5) & 0x3f),
                expand5(p & 0x1f),
            ]
        }
        _ => {
            let p = u32::from(u16::from_le_bytes([px[0], px[1]]));
            [
                expand5((p >> 10) & 0x1f),
                expand5((p >> 5) & 0x1f),
                expand5(p & 0x1f),
            ]
        }
    }
}

pub unsafe extern "C" fn audio_sample(_left: i16, _right: i16) {}

pub unsafe extern "C" fn audio_sample_batch(_data: *const i16, frames: usize) -> usize {
    frames
}

pub unsafe extern "C" fn input_poll() {}

pub unsafe extern "C" fn input_state(
    port: c_uint,
    device: c_uint,
    _index: c_uint,
    id: c_uint,
) -> i16 {
    if port != 0 || device != RETRO_DEVICE_JOYPAD {
        return 0;
    }
    let button = match id {
        RETRO_DEVICE_ID_JOYPAD_A => Button::A,
        RETRO_DEVICE_ID_JOYPAD_B => Button::B,
        RETRO_DEVICE_ID_JOYPAD_L => Button::L,
        RETRO_DEVICE_ID_JOYPAD_R => Button::R,
        RETRO_DEVICE_ID_JOYPAD_START => Button::Start,
        RETRO_DEVICE_ID_JOYPAD_SELECT => Button::Select,
        RETRO_DEVICE_ID_JOYPAD_UP => Button::Up,
        RETRO_DEVICE_ID_JOYPAD_DOWN => Button::Down,
        RETRO_DEVICE_ID_JOYPAD_LEFT => Button::Left,
        RETRO_DEVICE_ID_JOYPAD_RIGHT => Button::Right,
        _ => return 0,
    };
    HOST.with_borrow(|host| i16::from(host.buttons.contains(button)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_pixel_formats() {
        assert_eq!(
            decode_pixel(RETRO_PIXEL_FORMAT_XRGB8888, &[0x30, 0x20, 0x10, 0xff]),
            [0x10, 0x20, 0x30]
        );
        assert_eq!(
            decode_pixel(RETRO_PIXEL_FORMAT_RGB565, &0xffffu16.to_le_bytes()),
            [255, 255, 255]
        );
        assert_eq!(
            decode_pixel(RETRO_PIXEL_FORMAT_RGB565, &0xf800u16.to_le_bytes()),
            [255, 0, 0]
        );
        assert_eq!(
            decode_pixel(RETRO_PIXEL_FORMAT_0RGB1555, &0x03e0u16.to_le_bytes()),
            [0, 255, 0]
        );
    }
}
