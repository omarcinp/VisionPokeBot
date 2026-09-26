//! Frontend callbacks handed to the core. Libretro callbacks carry no user
//! pointer, so their state lives in a thread-local on the emulator thread.

use std::cell::RefCell;
use std::ffi::{c_char, CString};
use std::os::raw::{c_uint, c_void};
use std::time::Duration;

use pokebot_core::{Button, ButtonSet, RgbImage};

use crate::ffi::*;
use crate::link_port::{LinkEvent, LinkPort};

struct HostState {
    pixel_format: c_uint,
    buttons: ButtonSet,
    frame: Option<RgbImage>,
    system_dir: CString,
    save_dir: CString,
    /// The core's link-port interface, if it has one.
    netpacket: Option<RetroNetpacketCallback>,
    link: Option<LinkPort>,
    session: LinkSession,
}

#[derive(Default)]
struct LinkSession {
    /// The core has been told a session started (and not yet that it ended).
    started: bool,
    /// A peer is connected.
    connected: bool,
    /// Frames this side finished since the peer connected.
    frames: u32,
    /// Frames the peer reported finished.
    peer_frames: u32,
    /// The peer's count when this side stopped waiting for it.
    stalled_at: Option<u32>,
}

thread_local! {
    static HOST: RefCell<HostState> = RefCell::new(HostState {
        pixel_format: RETRO_PIXEL_FORMAT_0RGB1555,
        buttons: ButtonSet::NONE,
        frame: None,
        system_dir: CString::default(),
        save_dir: CString::default(),
        netpacket: None,
        link: None,
        session: LinkSession {
            started: false,
            connected: false,
            frames: 0,
            peer_frames: 0,
            stalled_at: None,
        },
    });
}

pub fn reset(system_dir: CString, save_dir: CString) {
    HOST.with_borrow_mut(|host| {
        host.pixel_format = RETRO_PIXEL_FORMAT_0RGB1555;
        host.buttons = ButtonSet::NONE;
        host.frame = None;
        host.system_dir = system_dir;
        host.save_dir = save_dir;
        host.netpacket = None;
        host.link = None;
        host.session = LinkSession::default();
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
        RETRO_ENVIRONMENT_SET_NETPACKET_INTERFACE if !data.is_null() => {
            // SAFETY: libretro passes a retro_netpacket_callback*; it is
            // copied, so the core's pointer need not outlive this call.
            let callback = unsafe { *(data as *const RetroNetpacketCallback) };
            HOST.with_borrow_mut(|host| host.netpacket = Some(callback));
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

/// The protocol name of the core's link port, if it has one.
pub fn link_protocol() -> Option<String> {
    HOST.with_borrow(|host| {
        let callback = host.netpacket?;
        let name = if callback.protocol_version.is_null() {
            String::new()
        } else {
            // SAFETY: a static NUL-terminated string owned by the core.
            unsafe { std::ffi::CStr::from_ptr(callback.protocol_version) }
                .to_string_lossy()
                .into_owned()
        };
        Some(name)
    })
}

/// How many frames an emulator may run ahead of its link peer. At 0 both
/// still emulate the same frame in parallel; 1 let gpSP drop packets.
const LOCKSTEP_SLACK: u32 = 0;
/// How long to wait for the peer's next frame before running on alone.
const LOCKSTEP_TIMEOUT: Duration = Duration::from_millis(250);

/// libretro client ids: the host is 0, the (single) guest 1.
const HOST_CLIENT_ID: u16 = 0;
const GUEST_CLIENT_ID: u16 = 1;

/// Plugs the cable into the core. A host's session starts now (it waits
/// for players); a guest's starts once it reaches the host.
pub fn attach_link(port: LinkPort) {
    let host_side = port.is_host();
    let callback = HOST.with_borrow_mut(|host| {
        host.link = Some(port);
        host.netpacket
    });
    if let (true, Some(callback)) = (host_side, callback) {
        start_session(&callback, HOST_CLIENT_ID);
    }
}

/// Unplugs the cable, ending the core's session.
pub fn detach_link() {
    let (callback, started) = HOST.with_borrow_mut(|host| {
        let started = std::mem::take(&mut host.session.started);
        (host.netpacket, started)
    });
    if let (Some(callback), true) = (callback, started) {
        if let Some(stop) = callback.stop {
            // SAFETY: session started on this thread; the core is loaded.
            unsafe { stop() };
        }
    }
    // Dropping the port closes the connection and joins its thread.
    let port = HOST.with_borrow_mut(|host| host.link.take());
    drop(port);
}

/// Before each frame: delivers what arrived on the link port and holds this
/// emulator until its peer is at most [`LOCKSTEP_SLACK`] frames behind, as
/// if both consoles shared a clock. A peer that stops for longer than
/// [`LOCKSTEP_TIMEOUT`] is not waited for again until it moves.
pub fn link_before_frame() {
    let Some(callback) = link_callback() else {
        return;
    };
    drain_link(&callback);
    loop {
        let behind = HOST.with_borrow(|host| {
            let s = &host.session;
            s.connected
                && s.peer_frames.saturating_add(LOCKSTEP_SLACK) < s.frames
                && s.stalled_at != Some(s.peer_frames)
        });
        if !behind {
            break;
        }
        let event = HOST.with_borrow(|host| {
            host.link
                .as_ref()
                .and_then(|port| port.wait_event(LOCKSTEP_TIMEOUT))
        });
        match event {
            Some(event) => dispatch(&callback, event),
            None => {
                HOST.with_borrow_mut(|host| {
                    let s = &mut host.session;
                    eprintln!(
                        "link: peer stalled {} frames behind; running on alone until it moves",
                        s.frames - s.peer_frames
                    );
                    s.stalled_at = Some(s.peer_frames);
                });
                break;
            }
        }
    }
    if HOST.with_borrow(|host| host.session.started) {
        if let Some(poll) = callback.poll {
            // SAFETY: the session was started on this thread.
            unsafe { poll() };
        }
    }
}

/// After each frame: reports it to the peer.
pub fn link_after_frame() {
    HOST.with_borrow_mut(|host| {
        let s = &mut host.session;
        if s.connected {
            s.frames = s.frames.saturating_add(1);
            if let Some(port) = &host.link {
                port.frames_done(s.frames);
            }
        }
    });
}

fn link_callback() -> Option<RetroNetpacketCallback> {
    HOST.with_borrow(|host| host.netpacket.filter(|_| host.link.is_some()))
}

/// Delivers everything that has arrived, without waiting.
fn drain_link(callback: &RetroNetpacketCallback) {
    // One event at a time, without holding the borrow: the core answers
    // packets by sending (which borrows the state) from inside `receive`.
    while let Some(event) =
        HOST.with_borrow(|host| host.link.as_ref().and_then(LinkPort::try_event))
    {
        dispatch(callback, event);
    }
}

fn dispatch(callback: &RetroNetpacketCallback, event: LinkEvent) {
    let host_side = HOST.with_borrow(|host| host.link.as_ref().is_some_and(LinkPort::is_host));
    let peer = if host_side {
        GUEST_CLIENT_ID
    } else {
        HOST_CLIENT_ID
    };
    match event {
        LinkEvent::Connected => {
            HOST.with_borrow_mut(|host| {
                host.session = LinkSession {
                    started: host.session.started,
                    connected: true,
                    ..LinkSession::default()
                };
            });
            if !host_side {
                start_session(callback, GUEST_CLIENT_ID);
                return;
            }
            // SAFETY: the host's session was started on this thread.
            let accepted = callback.connected.is_none_or(|f| unsafe { f(peer) });
            if !accepted {
                HOST.with_borrow_mut(|host| host.session.connected = false);
                with_port(LinkPort::hang_up);
            }
        }
        LinkEvent::Packet(packet) => {
            if HOST.with_borrow(|host| host.session.started) {
                // SAFETY: the buffer outlives the call; session started.
                unsafe { (callback.receive)(packet.as_ptr().cast(), packet.len(), peer) };
            }
        }
        LinkEvent::PeerFrames(frames) => {
            HOST.with_borrow_mut(|host| host.session.peer_frames = frames);
        }
        LinkEvent::Disconnected => {
            let (was_connected, guest_started) = HOST.with_borrow_mut(|host| {
                let s = &mut host.session;
                let was_connected = std::mem::take(&mut s.connected);
                (was_connected, !host_side && std::mem::take(&mut s.started))
            });
            if host_side && was_connected {
                if let Some(disconnected) = callback.disconnected {
                    // SAFETY: the session was started on this thread.
                    unsafe { disconnected(peer) };
                }
            }
            if guest_started {
                if let Some(stop) = callback.stop {
                    // SAFETY: as above.
                    unsafe { stop() };
                }
            }
        }
    }
}

fn start_session(callback: &RetroNetpacketCallback, client_id: u16) {
    HOST.with_borrow_mut(|host| host.session.started = true);
    // SAFETY: the core is loaded on this thread; the functions handed over
    // stay valid until `stop`.
    unsafe { (callback.start)(client_id, netpacket_send, netpacket_poll_receive) };
}

fn with_port(f: impl FnOnce(&LinkPort)) {
    HOST.with_borrow(|host| {
        if let Some(port) = &host.link {
            f(port);
        }
    });
}

unsafe extern "C" fn netpacket_send(
    _flags: std::os::raw::c_int,
    buf: *const c_void,
    len: usize,
    _client_id: u16,
) {
    if buf.is_null() && len > 0 {
        return;
    }
    // Two players: whether broadcast or addressed, it goes to the peer.
    let payload = if len == 0 {
        &[][..]
    } else {
        // SAFETY: the core passes `len` readable bytes.
        unsafe { std::slice::from_raw_parts(buf as *const u8, len) }
    };
    with_port(|port| port.send(payload));
}

/// The core waits for data mid-frame: deliver what has arrived.
unsafe extern "C" fn netpacket_poll_receive() {
    if let Some(callback) = link_callback() {
        drain_link(&callback);
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
