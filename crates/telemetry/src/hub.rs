use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use pokebot_core::{NormalizedFrame, RgbImage};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{broadcast, watch};

const LOG_CAPACITY: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogKind {
    Action,
    Event,
    /// Planner/goal decisions and progress.
    Goal,
    Info,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub seq: u64,
    /// Milliseconds since telemetry started.
    pub t_ms: u64,
    pub frame_id: Option<u64>,
    pub kind: LogKind,
    pub summary: String,
    pub detail: Value,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    /// Identifies this bot run (start time, unix ms); the page resets when
    /// it changes.
    pub session: u64,
    pub video_source: String,
    pub controller: String,
    pub frame_id: Option<u64>,
    pub frames_seen: u64,
    /// Frames observed per second over the last second.
    pub fps: f64,
    pub uptime_ms: u64,
}

/// Everything the UI shows besides the picture.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Status {
    pub stats: Stats,
    pub state: Value,
    pub observation: Value,
}

#[derive(Clone)]
pub(crate) struct FrameSnapshot {
    pub frame_id: u64,
    pub image: Arc<RgbImage>,
}

/// Cheap to clone; all clones share one hub.
#[derive(Clone)]
pub struct Telemetry {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    started: Instant,
    pub(crate) frame: watch::Sender<Option<FrameSnapshot>>,
    pub(crate) status: watch::Sender<Status>,
    pub(crate) log_tx: broadcast::Sender<LogEntry>,
    log: Mutex<Log>,
    fps_window: Mutex<VecDeque<Instant>>,
}

struct Log {
    entries: VecDeque<LogEntry>,
    next_seq: u64,
}

impl Telemetry {
    pub fn new(video_source: &str, controller: &str) -> Self {
        let status = Status {
            stats: Stats {
                session: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as u64),
                video_source: video_source.to_owned(),
                controller: controller.to_owned(),
                ..Stats::default()
            },
            ..Status::default()
        };
        Self {
            inner: Arc::new(Inner {
                started: Instant::now(),
                frame: watch::Sender::new(None),
                status: watch::Sender::new(status),
                log_tx: broadcast::Sender::new(1024),
                log: Mutex::new(Log {
                    entries: VecDeque::new(),
                    next_seq: 0,
                }),
                fps_window: Mutex::new(VecDeque::new()),
            }),
        }
    }

    /// Publishes the frame the bot just observed, with the state and
    /// observation derived from it.
    pub fn publish_frame(
        &self,
        frame: &NormalizedFrame,
        state: &impl Serialize,
        observation: &impl Serialize,
    ) {
        let now = Instant::now();
        let fps = {
            let mut window = lock(&self.inner.fps_window);
            window.push_back(now);
            while window
                .front()
                .is_some_and(|t| now.duration_since(*t).as_secs_f64() > 1.0)
            {
                window.pop_front();
            }
            window.len() as f64
        };
        self.inner.frame.send_replace(Some(FrameSnapshot {
            frame_id: frame.frame_id,
            image: Arc::new(frame.image().clone()),
        }));
        let state = serde_json::to_value(state).unwrap_or(Value::Null);
        let observation = serde_json::to_value(observation).unwrap_or(Value::Null);
        let uptime_ms = self.elapsed_ms();
        self.inner.status.send_modify(|status| {
            status.stats.frame_id = Some(frame.frame_id);
            status.stats.frames_seen += 1;
            status.stats.fps = fps;
            status.stats.uptime_ms = uptime_ms;
            status.state = state;
            status.observation = observation;
        });
    }

    pub fn log(
        &self,
        kind: LogKind,
        frame_id: Option<u64>,
        summary: impl Into<String>,
        detail: &impl Serialize,
    ) {
        let entry = {
            let mut log = lock(&self.inner.log);
            let entry = LogEntry {
                seq: log.next_seq,
                t_ms: self.elapsed_ms(),
                frame_id,
                kind,
                summary: summary.into(),
                detail: serde_json::to_value(detail).unwrap_or(Value::Null),
            };
            log.next_seq += 1;
            if log.entries.len() == LOG_CAPACITY {
                log.entries.pop_front();
            }
            log.entries.push_back(entry.clone());
            entry
        };
        // No receivers just means no browser is connected.
        let _ = self.inner.log_tx.send(entry);
    }

    pub fn info(&self, summary: impl Into<String>) {
        self.log(LogKind::Info, None, summary, &Value::Null);
    }

    pub fn error(&self, summary: impl Into<String>) {
        self.log(LogKind::Error, None, summary, &Value::Null);
    }

    pub(crate) fn recent_log(&self) -> Vec<LogEntry> {
        lock(&self.inner.log).entries.iter().cloned().collect()
    }

    fn elapsed_ms(&self) -> u64 {
        self.inner.started.elapsed().as_millis() as u64
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}
