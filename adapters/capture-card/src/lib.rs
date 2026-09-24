//! V4L2 video source: HDMI capture cards (UVC) and virtual cameras such as
//! the v4l2loopback device written by `pokebot emulator serve`.
//!
//! Capture runs on a background thread that keeps only the newest frame, so a
//! slow reader never sees stale video. Frame ids follow the driver's sequence
//! numbers, so frames the bot skipped show up as gaps.

mod decode;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pokebot_core::{CapturedFrame, Error, Result, VideoSource};
use v4l::buffer::Type;
use v4l::io::traits::CaptureStream;
use v4l::video::Capture;
use v4l::{Device, FourCC};

pub use decode::PixelFormat;

const BUFFERS: u32 = 4;

#[derive(Debug, Clone)]
pub struct CaptureCardConfig {
    /// e.g. `/dev/video0`
    pub device: PathBuf,
    /// Requested size; `None` keeps whatever the device is set to (a
    /// loopback device always uses its writer's size).
    pub size: Option<(u32, u32)>,
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
                        let image = match pixel_format.decode(&bytes[..used], width, height, stride)
                        {
                            Ok(image) => image,
                            Err(_) => continue, // torn/corrupt buffer: wait for the next one
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
                            captured_at: Instant::now(),
                            image,
                        });
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
