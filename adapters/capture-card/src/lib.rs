//! V4L2 video source: HDMI capture cards (UVC) and virtual cameras such as
//! the v4l2loopback device written by `pokebot emulator serve`.
//!
//! Capture runs on a background thread that keeps only the newest frame, so a
//! slow reader never sees stale video. Frame ids follow the driver's sequence
//! numbers, so frames the bot skipped show up as gaps.

pub mod broker;
mod decode;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pokebot_core::{CapturedFrame, Error, Result, VideoSource};
use v4l::buffer::Type;
use v4l::io::traits::CaptureStream;
use v4l::video::capture::Parameters;
use v4l::video::Capture;
use v4l::{Device, FourCC};

pub use decode::PixelFormat;

/// Sets integer controls by name (case-insensitive, as `v4l2-ctl` lists them).
fn set_controls(device: &Device, controls: &[(String, i64)]) -> std::io::Result<()> {
    let known = device.query_controls()?;
    for (name, value) in controls {
        let wanted = name.to_lowercase().replace(' ', "_");
        let description = known
            .iter()
            .find(|d| d.name.to_lowercase().replace(' ', "_") == wanted)
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, format!("no control {name:?}"))
            })?;
        device.set_control(v4l::control::Control {
            id: description.id,
            value: v4l::control::Value::Integer(*value),
        })?;
    }
    Ok(())
}

const BUFFERS: u32 = 4;

#[derive(Debug, Clone)]
pub struct CaptureCardConfig {
    /// e.g. `/dev/video0`
    pub device: PathBuf,
    /// Requested size; `None` keeps whatever the device is set to (a
    /// loopback device always uses its writer's size).
    pub size: Option<(u32, u32)>,
    /// Picture controls to set on open, by V4L2 name (e.g. `("saturation",
    /// 155)`). Cards forget them when unplugged, so the bot sets them.
    pub controls: Vec<(String, i64)>,
}

pub struct CaptureCardVideoSource {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    last_frame_id: Option<u64>,
    description: String,
}

struct Shared {
    latest: Mutex<Latest>,
    ready: Condvar,
}

#[derive(Default)]
struct Latest {
    frame: Option<CapturedFrame>,
    failure: Option<String>,
}

impl CaptureCardVideoSource {
    pub fn open(config: CaptureCardConfig) -> Result<Self> {
        let path = &config.device;
        let device_err = |what: &str, e: std::io::Error| {
            Error::Device(format!("{}: {what}: {e}", path.display()))
        };
        let device = Device::with_path(path).map_err(|e| device_err("open", e))?;
        let caps = device
            .query_caps()
            .map_err(|e| device_err("query capabilities", e))?;
        let mut format = Capture::format(&device).map_err(|e| {
            device_err(
                "read capture format (for a loopback device, start its writer first)",
                e,
            )
        })?;
        if let Some((width, height)) = config.size {
            format.width = width;
            format.height = height;
            format =
                Capture::set_format(&device, &format).map_err(|e| device_err("set format", e))?;
            // A new size resets the frame rate to the driver's default; ask
            // for the GBA's 60 fps (the driver picks the nearest it has).
            Capture::set_params(&device, &Parameters::with_fps(60))
                .map_err(|e| device_err("set frame rate", e))?;
        }
        if !config.controls.is_empty() {
            set_controls(&device, &config.controls).map_err(|e| device_err("set controls", e))?;
        }
        if !config.controls.is_empty() {
            set_controls(&device, &config.controls).map_err(|e| device_err("set controls", e))?;
        }
        let pixel_format = PixelFormat::from_fourcc(format.fourcc.repr).ok_or_else(|| {
            Error::Unsupported(format!(
                "{}: pixel format {} (supported: RGB3, BGR3, YUYV, MJPG)",
                path.display(),
                format.fourcc
            ))
        })?;
        let (width, height, stride) = (format.width, format.height, format.stride);
        let description = format!(
            "{} \"{}\" {width}x{height} {}",
            path.display(),
            caps.card,
            FourCC::new(&format.fourcc.repr)
        );

        let mut stream = v4l::io::mmap::Stream::with_buffers(&device, Type::VideoCapture, BUFFERS)
            .map_err(|e| device_err("allocate capture buffers", e))?;
        stream.set_timeout(Duration::from_millis(250));

        let shared = Arc::new(Shared {
            latest: Mutex::new(Latest::default()),
            ready: Condvar::new(),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (shared, stop) = (Arc::clone(&shared), Arc::clone(&stop));
            std::thread::Builder::new()
                .name("pokebot-capture".into())
                .spawn(move || {
                    let _device = device; // keep the fd open while streaming
                    let mut last_sequence: Option<u32> = None;
                    let mut frame_id = 0u64;
                    let mut delivered = 0u64;
                    // Buffers queued before streaming started can hold stale
                    // frames (v4l2loopback keeps its last ones), which would
                    // show up as a huge sequence gap. Skip one full ring.
                    let mut stale = BUFFERS;
                    while !stop.load(Ordering::Relaxed) {
                        let (bytes, meta) = match stream.next() {
                            Ok(next) => next,
                            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                            Err(e) => {
                                lock(&shared.latest).failure =
                                    Some(format!("capture stopped: {e}"));
                                shared.ready.notify_all();
                                return;
                            }
                        };
                        if stale > 0 {
                            stale -= 1;
                            continue;
                        }
                        let used = (meta.bytesused as usize).min(bytes.len());
                        // Torn/corrupt buffers are skipped but keep their frame
                        // id: they count as frames the card never delivered.
                        // An MS2109 at 1080p sends every other frame as an
                        // empty 4-byte buffer.
                        let image = match pixel_format.decode(&bytes[..used], width, height, stride)
                        {
                            Ok(image) => image,
                            Err(_) => continue,
                        };
                        // Advance by the driver's sequence gap; restarts count as +1.
                        frame_id += match last_sequence {
                            Some(last) if meta.sequence > last => u64::from(meta.sequence - last),
                            Some(_) => 1,
                            None => 0,
                        };
                        last_sequence = Some(meta.sequence);
                        lock(&shared.latest).frame = Some(CapturedFrame {
                            frame_id,
                            delivered,
                            captured_at: Instant::now(),
                            image,
                        });
                        delivered += 1;
                        shared.ready.notify_all();
                    }
                })
                .map_err(|e| Error::Device(format!("cannot spawn capture thread: {e}")))?
        };
        Ok(Self {
            shared,
            stop,
            worker: Some(worker),
            last_frame_id: None,
            description,
        })
    }

    /// Device path, card name, size and pixel format.
    pub fn description(&self) -> &str {
        &self.description
    }
}

impl VideoSource for CaptureCardVideoSource {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        const STALL: Duration = Duration::from_secs(3);
        let mut latest = lock(&self.shared.latest);
        loop {
            if let Some(frame) = &latest.frame {
                if self.last_frame_id.is_none_or(|last| frame.frame_id > last) {
                    self.last_frame_id = Some(frame.frame_id);
                    return Ok(frame.clone());
                }
            }
            if let Some(failure) = &latest.failure {
                return Err(Error::Disconnected(failure.clone()));
            }
            let (guard, timeout) = self
                .shared
                .ready
                .wait_timeout(latest, STALL)
                .unwrap_or_else(|e| e.into_inner());
            latest = guard;
            if timeout.timed_out() {
                return Err(Error::Device(format!(
                    "no video for {STALL:?} from {}",
                    self.description
                )));
            }
        }
    }
}

impl Drop for CaptureCardVideoSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}
