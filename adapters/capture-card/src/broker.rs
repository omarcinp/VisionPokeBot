//! One V4L2 owner, independent latest-frame readers. Raw RGB goes to bots;
//! JPEG/PNG encoding happens only in telemetry, never on this path.
use crate::{CaptureCardConfig, CaptureCardVideoSource};
use pokebot_core::{CapturedFrame, Error, Result, RgbImage, VideoSource};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(3);

pub fn socket_path(device: &Path) -> PathBuf {
    let root = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("pokebot-{}", unsafe { libc::geteuid() }))
        });
    let path = std::fs::canonicalize(device).unwrap_or_else(|_| device.to_path_buf());
    // Deterministic path hash, independent of process and Rust hasher seeds.
    let hash = path
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
        });
    root.join(format!("pokebot-capture-{hash:x}.sock"))
}

pub fn open_shared(config: CaptureCardConfig) -> Result<Box<dyn VideoSource + Send>> {
    let path = socket_path(&config.device);
    if path.exists() {
        return Ok(Box::new(BrokerVideo::connect(&path)?));
    }
    Ok(Box::new(CaptureCardVideoSource::open(config)?))
}

struct Shared {
    frame: Mutex<Option<Arc<CapturedFrame>>>,
    ready: Condvar,
    stop: AtomicBool,
    clients: AtomicUsize,
}

pub struct CaptureBroker {
    shared: Arc<Shared>,
    path: PathBuf,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl CaptureBroker {
    /// Retry disconnected/busy cards in the background, including hotplug.
    pub fn start(config: CaptureCardConfig) -> Result<Self> {
        let path = socket_path(&config.device);
        let parent = path.parent().expect("socket directory");
        if !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| Error::io(parent, e))?;
        }
        if path.exists() {
            if UnixStream::connect(&path).is_ok() {
                return Err(Error::Device("capture broker already running".into()));
            }
            std::fs::remove_file(&path).map_err(|e| Error::io(&path, e))?;
        }
        let listener = UnixListener::bind(&path).map_err(|e| Error::io(&path, e))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| Error::io(&path, e))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| Error::io(&path, e))?;
        let shared = Arc::new(Shared {
            frame: Mutex::new(None),
            ready: Condvar::new(),
            stop: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
        });
        let capture = shared.clone();
        let producer = std::thread::spawn(move || {
            let mut next = 0;
            while !capture.stop.load(Ordering::Relaxed) {
                if let Ok(mut source) = CaptureCardVideoSource::open(config.clone()) {
                    let mut base = None;
                    while !capture.stop.load(Ordering::Relaxed) {
                        let Ok(mut frame) = source.next_frame() else {
                            break;
                        };
                        let offset = *base.get_or_insert(next);
                        frame.frame_id += offset;
                        next = frame.frame_id + 1;
                        *capture.frame.lock().unwrap() = Some(Arc::new(frame));
                        capture.ready.notify_all();
                    }
                }
                *capture.frame.lock().unwrap() = None;
                capture.ready.notify_all();
                std::thread::sleep(Duration::from_millis(500));
            }
        });
        let accept = shared.clone();
        let server = std::thread::spawn(move || {
            while !accept.stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if accept.clients.load(Ordering::Relaxed) >= 8 {
                            continue;
                        }
                        accept.clients.fetch_add(1, Ordering::Relaxed);
                        let client = accept.clone();
                        std::thread::spawn(move || {
                            let _ = serve_reader(stream, &client);
                            client.clients.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        });
        Ok(Self {
            shared,
            path,
            threads: vec![producer, server],
        })
    }

    pub fn latest(&self) -> Option<Arc<CapturedFrame>> {
        self.shared
            .frame
            .lock()
            .unwrap()
            .clone()
            .filter(|f| f.captured_at.elapsed() < TIMEOUT)
    }
}

impl Drop for CaptureBroker {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.ready.notify_all();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn monotonic_micros() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: CLOCK_MONOTONIC is supported on Linux and `time` is writable.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time);
    }
    time.tv_sec as u64 * 1_000_000 + time.tv_nsec as u64 / 1_000
}

fn serve_reader(mut stream: UnixStream, shared: &Shared) -> std::io::Result<()> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut request = [0; 8];
    while !shared.stop.load(Ordering::Relaxed) {
        match stream.read_exact(&mut request) {
            Ok(()) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue
            }
            Err(e) => return Err(e),
        }
        let last = u64::from_le_bytes(request);
        let until = Instant::now() + TIMEOUT;
        let frame = loop {
            let frame = shared.frame.lock().unwrap();
            if let Some(f) = frame.as_ref().filter(|f| {
                (last == u64::MAX || f.frame_id > last) && f.captured_at.elapsed() < TIMEOUT
            }) {
                break Some(f.clone());
            }
            if shared.stop.load(Ordering::Relaxed) || Instant::now() >= until {
                break None;
            }
            let _ = shared
                .ready
                .wait_timeout(frame, Duration::from_millis(100))
                .unwrap();
        };
        let Some(frame) = frame else {
            stream.write_all(&[0; 48])?;
            continue;
        };
        let fields = [
            frame.frame_id,
            frame.delivered,
            monotonic_micros().saturating_sub(frame.captured_at.elapsed().as_micros() as u64),
            u64::from(frame.image.width()),
            u64::from(frame.image.height()),
            frame.image.as_bytes().len() as u64,
        ];
        for value in fields {
            stream.write_all(&value.to_le_bytes())?;
        }
        // No capture mutex is held during a client's potentially slow write.
        stream.write_all(frame.image.as_bytes())?;
    }
    Ok(())
}

pub struct BrokerVideo {
    stream: UnixStream,
    last: u64,
}
impl BrokerVideo {
    pub fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path).map_err(|e| Error::io(path, e))?;
        stream
            .set_read_timeout(Some(TIMEOUT + Duration::from_secs(1)))
            .map_err(|e| Error::io(path, e))?;
        stream
            .set_write_timeout(Some(TIMEOUT))
            .map_err(|e| Error::io(path, e))?;
        Ok(Self {
            stream,
            last: u64::MAX,
        })
    }
}
impl VideoSource for BrokerVideo {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        let mut read = || -> std::io::Result<CapturedFrame> {
            self.stream.write_all(&self.last.to_le_bytes())?;
            let mut header = [0; 48];
            self.stream.read_exact(&mut header)?;
            let n: Vec<_> = header
                .chunks_exact(8)
                .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
                .collect();
            let (width, height, len) = (n[3], n[4], n[5]);
            if width == 0
                || width > 4096
                || height == 0
                || height > 2160
                || len != width * height * 3
            {
                return Err(std::io::Error::other(
                    "capture disconnected or invalid frame",
                ));
            }
            let mut bytes = vec![0; len as usize];
            self.stream.read_exact(&mut bytes)?;
            self.last = n[0];
            Ok(CapturedFrame {
                frame_id: n[0],
                delivered: n[1],
                captured_at: Instant::now()
                    .checked_sub(Duration::from_micros(
                        monotonic_micros().saturating_sub(n[2]),
                    ))
                    .unwrap_or_else(Instant::now),
                image: RgbImage::from_raw(width as u32, height as u32, bytes)
                    .map_err(std::io::Error::other)?,
            })
        };
        read().map_err(|e| Error::Disconnected(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stalled_preview_reader_does_not_block_capture_or_bot_reader() {
        let shared = Arc::new(Shared {
            frame: Mutex::new(Some(Arc::new(CapturedFrame {
                frame_id: 1,
                delivered: 1,
                captured_at: Instant::now(),
                image: RgbImage::filled(1280, 720, [1, 2, 3]),
            }))),
            ready: Condvar::new(),
            stop: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
        });
        let (mut slow, server) = UnixStream::pair().unwrap();
        let state = shared.clone();
        let thread = std::thread::spawn(move || {
            let _ = serve_reader(server, &state);
        });
        slow.write_all(&u64::MAX.to_le_bytes()).unwrap();
        // Read just the header, leaving a frame larger than the socket buffer
        // blocked in write_all. The producer must still replace the frame.
        slow.read_exact(&mut [0; 48]).unwrap();
        *shared.frame.lock().unwrap() = Some(Arc::new(CapturedFrame {
            frame_id: 99,
            delivered: 99,
            captured_at: Instant::now(),
            image: RgbImage::filled(240, 160, [9, 8, 7]),
        }));
        let (client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let state = shared.clone();
        let fast = std::thread::spawn(move || {
            let _ = serve_reader(server, &state);
        });
        let mut bot = BrokerVideo {
            stream: client,
            last: u64::MAX,
        };
        let frame = bot.next_frame().unwrap();
        assert_eq!(frame.frame_id, 99);
        assert_eq!(frame.image.pixel(0, 0), [9, 8, 7]);
        shared.stop.store(true, Ordering::Relaxed);
        drop(slow);
        drop(bot);
        thread.join().unwrap();
        fast.join().unwrap();
    }

    #[test]
    fn independent_readers_get_latest_frame_without_draining_each_other() {
        let shared = Arc::new(Shared {
            frame: Mutex::new(Some(Arc::new(CapturedFrame {
                frame_id: 42,
                delivered: 40,
                captured_at: Instant::now(),
                image: RgbImage::filled(240, 160, [1, 2, 3]),
            }))),
            ready: Condvar::new(),
            stop: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
        });
        let mut clients = Vec::new();
        for _ in 0..2 {
            let (client, server) = UnixStream::pair().unwrap();
            let state = shared.clone();
            std::thread::spawn(move || {
                let _ = serve_reader(server, &state);
            });
            clients.push(BrokerVideo {
                stream: client,
                last: u64::MAX,
            });
        }
        for c in &mut clients {
            let f = c.next_frame().unwrap();
            assert_eq!(f.frame_id, 42);
            assert_eq!(f.image.pixel(0, 0), [1, 2, 3]);
        }
        shared.stop.store(true, Ordering::Relaxed);
    }
}
