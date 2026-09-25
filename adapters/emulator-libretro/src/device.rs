use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pokebot_controller::{FrameRate, InputSchedule};
use std::collections::VecDeque;

use pokebot_core::{
    ButtonSet, CapturedFrame, Controller, ControllerCommand, ControllerReceipt, Error,
    PressProfile, Result, RgbImage, VideoSource,
};

use crate::cartridge::BatterySave;
use crate::ffi::*;
use crate::host;

/// How emulated time advances.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClockMode {
    /// One emulated frame per [`VideoSource::next_frame`] call. Fully
    /// deterministic and as fast as the host allows; used by tests and
    /// replays of scripted runs.
    #[default]
    Stepped,
    /// The emulator runs at console speed on its own, like real hardware.
    /// `next_frame` returns the newest frame and skips any the consumer was
    /// too slow to read (visible as gaps in `frame_id`).
    RealTime,
}

#[derive(Debug, Clone)]
pub struct EmulatorConfig {
    pub core_path: PathBuf,
    pub rom_path: PathBuf,
    /// Cartridge save file (`.sav`), created when the game first saves.
    pub battery_save: Option<PathBuf>,
    pub clock: ClockMode,
    pub press_profile: PressProfile,
}

impl EmulatorConfig {
    pub fn new(core_path: impl Into<PathBuf>, rom_path: impl Into<PathBuf>) -> Self {
        Self {
            core_path: core_path.into(),
            rom_path: rom_path.into(),
            battery_save: None,
            clock: ClockMode::default(),
            press_profile: PressProfile::default(),
        }
    }
}

/// What the loaded core reports about itself.
#[derive(Debug, Clone)]
pub struct CoreInfo {
    pub library_name: String,
    pub library_version: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

/// Starts the emulator and returns its two device faces. The emulator stops
/// (and flushes the battery save) once both handles are dropped.
///
/// Libretro cores are process-global, so only one emulator may run at a time.
pub fn launch(
    config: EmulatorConfig,
) -> Result<(EmulatorVideoSource, EmulatorController, CoreInfo)> {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            schedule: InputSchedule::new(FrameRate::GBA, config.press_profile),
            latest: None,
            ready: VecDeque::new(),
            emulated: 0,
            failure: None,
        }),
        frame_ready: Condvar::new(),
    });
    let (requests, request_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let clock = config.clock;
    let worker_shared = Arc::clone(&shared);
    let worker = std::thread::Builder::new()
        .name("pokebot-emulator".into())
        .spawn(move || run_worker(config, worker_shared, request_rx, ready_tx))
        .map_err(|e| Error::Device(format!("cannot spawn emulator thread: {e}")))?;
    let info = match ready_rx.recv() {
        Ok(Ok(info)) => info,
        Ok(Err(e)) => {
            let _ = worker.join();
            return Err(e);
        }
        Err(_) => {
            let _ = worker.join();
            return Err(Error::Device(
                "emulator thread exited during startup".into(),
            ));
        }
    };
    let link = Arc::new(Link {
        shared,
        requests,
        worker: Mutex::new(Some(worker)),
        clock,
    });
    Ok((
        EmulatorVideoSource {
            link: Arc::clone(&link),
            last_frame_id: None,
            primed: false,
        },
        EmulatorController { link },
        info,
    ))
}

/// The emulator's rendered output, seen the way a capture card would.
///
/// Stepped: pipelined one frame deep. Each `next_frame` samples the
/// buttons for the frame after the one it returns (in the caller's thread,
/// so what the bot queued before the call decides it) and hands them to
/// the emulator thread, which emulates that frame while the bot perceives
/// the one returned. Input thus shows one frame later than unpipelined,
/// deterministically; the core and perception overlap instead of
/// alternating (1.36 + 1.74 ms per frame became about max of the two).
pub struct EmulatorVideoSource {
    link: Arc<Link>,
    last_frame_id: Option<u64>,
    /// The first frame has been requested (stepped).
    primed: bool,
}

/// How long to wait for a frame before calling the emulator stalled.
const STALL: Duration = Duration::from_secs(2);

impl EmulatorVideoSource {
    /// Samples the buttons for the next frame and queues its emulation.
    fn request_step(&self) -> Result<()> {
        let buttons = self.link.shared.lock().schedule.advance();
        self.link
            .requests
            .send(Request::Step { buttons })
            .map_err(|_| Error::Disconnected("emulator thread has stopped".into()))
    }

    fn next_stepped(&mut self) -> Result<CapturedFrame> {
        if !self.primed {
            self.primed = true;
            self.request_step()?;
        }
        self.request_step()?;
        let mut state = self.link.shared.lock();
        loop {
            if let Some(failure) = &state.failure {
                return Err(Error::Disconnected(failure.clone()));
            }
            if let Some(frame) = state.ready.pop_front() {
                self.last_frame_id = Some(frame.frame_id);
                return Ok(frame);
            }
            let (guard, timeout) = self
                .link
                .shared
                .frame_ready
                .wait_timeout(state, STALL)
                .unwrap_or_else(|e| e.into_inner());
            state = guard;
            if timeout.timed_out() {
                return Err(Error::Device(format!("no new frame for {STALL:?}")));
            }
        }
    }
}

impl VideoSource for EmulatorVideoSource {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        if self.link.clock == ClockMode::Stepped {
            return self.next_stepped();
        }
        let mut state = self.link.shared.lock();
        loop {
            if let Some(failure) = &state.failure {
                return Err(Error::Disconnected(failure.clone()));
            }
            if let Some(frame) = &state.latest {
                if self.last_frame_id.is_none_or(|last| frame.frame_id > last) {
                    self.last_frame_id = Some(frame.frame_id);
                    return Ok(frame.clone());
                }
            }
            let (guard, timeout) = self
                .link
                .shared
                .frame_ready
                .wait_timeout(state, STALL)
                .unwrap_or_else(|e| e.into_inner());
            state = guard;
            if timeout.timed_out() {
                return Err(Error::Device(format!("no new frame for {STALL:?}")));
            }
        }
    }
}

/// The emulator's joypad, driven the way the ESP32 bridge would be. Clones
/// share the same emulator.
#[derive(Clone)]
pub struct EmulatorController {
    link: Arc<Link>,
}

impl EmulatorController {
    /// Id (as in [`ControllerReceipt::command_id`]) of the newest command whose
    /// input the emulator has completely applied.
    pub fn completed_through(&self) -> Option<u64> {
        self.link.shared.lock().schedule.completed_through()
    }

    /// Persists the cartridge save now instead of waiting for shutdown.
    pub fn flush_battery_save(&self) -> Result<()> {
        self.link.request(|reply| Request::FlushSave { reply })
    }
}

impl Controller for EmulatorController {
    fn execute(&mut self, command: ControllerCommand) -> Result<ControllerReceipt> {
        let mut state = self.link.shared.lock();
        if let Some(failure) = &state.failure {
            return Err(Error::Disconnected(failure.clone()));
        }
        let (command_id, frames) = state.schedule.enqueue(&command);
        Ok(ControllerReceipt {
            command_id,
            issued_at: Instant::now(),
            input_duration: state.schedule.rate().duration_of(frames),
        })
    }

    fn is_idle(&self) -> Result<bool> {
        Ok(self.link.shared.lock().schedule.is_idle())
    }
}

struct Shared {
    state: Mutex<State>,
    frame_ready: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

struct State {
    schedule: InputSchedule,
    /// Real time: the newest frame.
    latest: Option<CapturedFrame>,
    /// Stepped: emulated frames not yet read, oldest first (at most two).
    ready: VecDeque<CapturedFrame>,
    /// Frames emulated so far (the next frame's id).
    emulated: u64,
    failure: Option<String>,
}

enum Request {
    /// Emulate one frame with these buttons (stepped).
    Step {
        buttons: ButtonSet,
    },
    FlushSave {
        reply: Sender<Result<()>>,
    },
    Shutdown,
}

/// Shared by both device handles; stopping the worker when the last one goes.
struct Link {
    shared: Arc<Shared>,
    requests: Sender<Request>,
    worker: Mutex<Option<JoinHandle<()>>>,
    clock: ClockMode,
}

impl Link {
    fn request(&self, make: impl FnOnce(Sender<Result<()>>) -> Request) -> Result<()> {
        let (reply, response) = mpsc::channel();
        let disconnected = || Error::Disconnected("emulator thread has stopped".into());
        self.requests
            .send(make(reply))
            .map_err(|_| disconnected())?;
        response.recv().map_err(|_| disconnected())?
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Shutdown);
        if let Some(worker) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = worker.join();
        }
    }
}

fn run_worker(
    config: EmulatorConfig,
    shared: Arc<Shared>,
    requests: Receiver<Request>,
    ready: Sender<Result<CoreInfo>>,
) {
    let mut core = match Core::open(&config) {
        Ok(core) => core,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let rate = frame_rate(core.info.fps);
    shared.lock().schedule = InputSchedule::new(rate, config.press_profile);
    if ready.send(Ok(core.info.clone())).is_err() {
        return;
    }

    let outcome = match config.clock {
        ClockMode::Stepped => run_stepped(&mut core, &shared, &requests),
        ClockMode::RealTime => run_realtime(&mut core, &shared, &requests, rate),
    };
    let outcome = outcome.and_then(|()| core.store_save());
    if let Err(e) = outcome {
        shared.lock().failure = Some(e.to_string());
        shared.frame_ready.notify_all();
    }
}

fn run_stepped(core: &mut Core, shared: &Shared, requests: &Receiver<Request>) -> Result<()> {
    for request in requests {
        match request {
            Request::Step { buttons } => {
                let image = core.run_frame(buttons);
                let mut state = shared.lock();
                let frame_id = state.emulated;
                state.emulated += 1;
                state.ready.push_back(CapturedFrame {
                    frame_id,
                    delivered: frame_id,
                    captured_at: Instant::now(),
                    image,
                });
                drop(state);
                shared.frame_ready.notify_all();
            }
            Request::FlushSave { reply } => {
                let _ = reply.send(core.store_save());
            }
            Request::Shutdown => break,
        }
    }
    Ok(())
}

fn run_realtime(
    core: &mut Core,
    shared: &Shared,
    requests: &Receiver<Request>,
    rate: FrameRate,
) -> Result<()> {
    let period = rate.frame_period();
    let mut deadline = Instant::now();
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        match requests.recv_timeout(wait) {
            Ok(Request::Shutdown) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
            Ok(Request::FlushSave { reply }) => {
                let _ = reply.send(core.store_save());
                continue;
            }
            // Only the stepped source sends steps.
            Ok(Request::Step { .. }) => continue,
            Err(RecvTimeoutError::Timeout) => {}
        }
        step(core, shared)?;
        deadline += period;
        // After a long stall, resynchronise instead of fast-forwarding.
        if Instant::now().saturating_duration_since(deadline) > period * 8 {
            deadline = Instant::now();
        }
    }
}

fn step(core: &mut Core, shared: &Shared) -> Result<()> {
    let buttons = shared.lock().schedule.advance();
    let image = core.run_frame(buttons);
    let mut state = shared.lock();
    let frame_id = state.latest.as_ref().map_or(0, |f| f.frame_id + 1);
    state.latest = Some(CapturedFrame {
        frame_id,
        delivered: frame_id,
        captured_at: Instant::now(),
        image,
    });
    drop(state);
    shared.frame_ready.notify_all();
    Ok(())
}

fn frame_rate(fps: f64) -> FrameRate {
    let gba = FrameRate::GBA.numerator as f64 / FrameRate::GBA.denominator as f64;
    if (fps - gba).abs() < 0.01 || !fps.is_finite() || fps <= 0.0 {
        FrameRate::GBA
    } else {
        FrameRate {
            numerator: (fps * 1000.0).round() as u64,
            denominator: 1000,
        }
    }
}

static CORE_IN_USE: AtomicBool = AtomicBool::new(false);

/// A loaded core with a running game. Lives on the emulator thread only.
struct Core {
    api: CoreApi,
    info: CoreInfo,
    battery: Option<BatterySave>,
    // Kept alive for cores that reference the buffers after load.
    _rom: Vec<u8>,
    _rom_path: CString,
}

impl Core {
    fn open(config: &EmulatorConfig) -> Result<Self> {
        if CORE_IN_USE.swap(true, Ordering::SeqCst) {
            return Err(Error::Device(
                "an emulator core is already running in this process".into(),
            ));
        }
        Self::open_exclusive(config).inspect_err(|_| CORE_IN_USE.store(false, Ordering::SeqCst))
    }

    fn open_exclusive(config: &EmulatorConfig) -> Result<Self> {
        let rom = std::fs::read(&config.rom_path).map_err(|e| Error::io(&config.rom_path, e))?;
        let rom_path = path_cstring(&config.rom_path)?;
        let core_dir = config.core_path.parent().unwrap_or(Path::new("."));
        host::reset(path_cstring(core_dir)?, path_cstring(core_dir)?);

        // SAFETY: the configured path is expected to be a libretro core.
        let api = unsafe { CoreApi::load(&config.core_path)? };
        // SAFETY: standard libretro start-up sequence, all on this thread.
        let info = unsafe {
            if (api.api_version)() != RETRO_API_VERSION {
                return Err(Error::Device(
                    "core uses an unsupported libretro API version".into(),
                ));
            }
            (api.set_environment)(host::environment);
            (api.set_video_refresh)(host::video_refresh);
            (api.set_audio_sample)(host::audio_sample);
            (api.set_audio_sample_batch)(host::audio_sample_batch);
            (api.set_input_poll)(host::input_poll);
            (api.set_input_state)(host::input_state);
            (api.init)();

            let mut system = RetroSystemInfo {
                library_name: std::ptr::null(),
                library_version: std::ptr::null(),
                valid_extensions: std::ptr::null(),
                need_fullpath: false,
                block_extract: false,
            };
            (api.get_system_info)(&mut system);
            let game = RetroGameInfo {
                path: rom_path.as_ptr(),
                data: if system.need_fullpath {
                    std::ptr::null()
                } else {
                    rom.as_ptr().cast()
                },
                size: if system.need_fullpath { 0 } else { rom.len() },
                meta: std::ptr::null(),
            };
            if !(api.load_game)(&game) {
                (api.deinit)();
                return Err(Error::Device(format!(
                    "core rejected ROM {}",
                    config.rom_path.display()
                )));
            }
            (api.set_controller_port_device)(0, RETRO_DEVICE_JOYPAD);
            let mut av = RetroSystemAvInfo::default();
            (api.get_system_av_info)(&mut av);
            CoreInfo {
                library_name: c_str(system.library_name),
                library_version: c_str(system.library_version),
                width: av.geometry.base_width,
                height: av.geometry.base_height,
                fps: av.timing.fps,
            }
        };
        let battery = match &config.battery_save {
            Some(path) => match BatterySave::attach(api.library(), path) {
                Ok(save) => Some(save),
                Err(e) => {
                    // SAFETY: game loaded above on this thread.
                    unsafe {
                        (api.unload_game)();
                        (api.deinit)();
                    }
                    return Err(e);
                }
            },
            None => None,
        };
        Ok(Self {
            api,
            info,
            battery,
            _rom: rom,
            _rom_path: rom_path,
        })
    }

    fn run_frame(&mut self, buttons: ButtonSet) -> RgbImage {
        host::set_buttons(buttons);
        // SAFETY: game is loaded; called on the thread that loaded it.
        unsafe { (self.api.run)() };
        host::latest_frame().unwrap_or_else(|| {
            RgbImage::filled(self.info.width.max(1), self.info.height.max(1), [0, 0, 0])
        })
    }

    fn store_save(&mut self) -> Result<()> {
        self.battery.as_mut().map_or(Ok(()), BatterySave::store)
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        let _ = self.store_save();
        // SAFETY: reverse of the start-up sequence, on the same thread.
        unsafe {
            (self.api.unload_game)();
            (self.api.deinit)();
        }
        CORE_IN_USE.store(false, Ordering::SeqCst);
    }
}

fn path_cstring(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::InvalidData(format!("path contains NUL: {}", path.display())))
}

fn c_str(ptr: *const std::os::raw::c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: libretro strings are static NUL-terminated C strings.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}
