//! The device: an [`InputQueue`] played out to a [`HidSink`], controlled over
//! the TCP line protocol and a small HTTP API. Plain `std::net` and
//! `std::thread`, so the same code runs on the ESP32-S3 (ESP-IDF's lwIP and
//! pthreads) and on a PC as a simulator.
//!
//! Threads: executor (ticks the queue, feeds the sink), notifier (broadcasts
//! `finished`), control acceptor + one reader per client, HTTP.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use pokebot_core::{PressProfile, SwitchCommand};

use crate::hid::SwitchReport;
use crate::protocol::{
    ClientMessage, CommandRequest, DeviceInfo, DeviceMessage, Status, CONTROLLER_KIND,
    PROTOCOL_VERSION,
};
use crate::queue::{Accepted, Finished, InputQueue, Keepalive, Rejected};

const MAX_CLIENTS: usize = 4;
const MAX_LINE_BYTES: u64 = 16 * 1024;
const MAX_HTTP_BODY: usize = 16 * 1024;
const WRITE_TIMEOUT: Duration = Duration::from_millis(500);
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);
/// Enough for serde_json and formatting on the ESP32; tiny on a PC.
const THREAD_STACK: usize = 12 * 1024;

const CONTROL_PAGE: &str = include_str!("page.html");

/// Where input reports go: TinyUSB on the ESP32, a log or the emulator on a PC.
pub trait HidSink: Send + 'static {
    /// Presents `report` to the host. Called every tick with the current
    /// report; returns false if it could not be sent (retried next tick).
    fn send(&mut self, report: &SwitchReport) -> bool;
    /// A host has configured the device.
    fn mounted(&self) -> bool;
    /// The host suspended the bus (the Switch is asleep).
    fn suspended(&self) -> bool {
        false
    }
    /// Asks a suspended host to wake up (USB remote wakeup), if it allows.
    fn wake(&mut self) {}
}

/// Least time between two remote-wakeup requests.
const WAKE_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct DeviceConfig {
    pub name: String,
    pub firmware: String,
    pub bind: IpAddr,
    /// TCP line protocol; 0 picks a free port.
    pub control_port: u16,
    /// HTTP API and control page; 0 picks a free port.
    pub http_port: u16,
    /// Executor period.
    pub tick: Duration,
    pub profile: PressProfile,
    /// Input played after a stretch without any, so the Switch never dims
    /// or sleeps. Off by default; the firmware turns it on.
    pub keepalive: Option<Keepalive>,
}

impl DeviceConfig {
    pub fn new(name: impl Into<String>, bind: IpAddr, control_port: u16, http_port: u16) -> Self {
        Self {
            name: name.into(),
            firmware: env!("CARGO_PKG_VERSION").into(),
            bind,
            control_port,
            http_port,
            tick: Duration::from_millis(1),
            profile: PressProfile::default(),
            keepalive: None,
        }
    }
}

/// A running device. Its threads live for the rest of the process.
pub struct Device {
    shared: Arc<Shared>,
    control_addr: SocketAddr,
    http_addr: SocketAddr,
}

struct Shared {
    queue: Mutex<InputQueue>,
    clients: Mutex<Vec<Client>>,
    next_client: AtomicU32,
    usb_mounted: AtomicBool,
    usb_suspended: AtomicBool,
    info: DeviceInfo,
    profile: PressProfile,
}

struct Client {
    // No 64-bit atomics on Xtensa.
    id: u32,
    conn: Arc<Conn>,
}

/// One control connection. ESP-IDF's sockets cannot be `dup`ed (so no
/// `TcpStream::try_clone`); the reader and the writers share the stream,
/// and `write` keeps lines from different threads from interleaving.
struct Conn {
    stream: TcpStream,
    write: Mutex<()>,
}

impl Conn {
    fn send(&self, message: &DeviceMessage) -> io::Result<()> {
        let _write = lock(&self.write);
        write_line(&self.stream, message)
    }
}

impl Device {
    pub fn start(config: DeviceConfig, sink: impl HidSink) -> io::Result<Self> {
        let control = TcpListener::bind((config.bind, config.control_port))?;
        let http = TcpListener::bind((config.bind, config.http_port))?;
        let (control_addr, http_addr) = (control.local_addr()?, http.local_addr()?);
        let mut queue = InputQueue::new();
        queue.set_keepalive(config.keepalive);
        let shared = Arc::new(Shared {
            queue: Mutex::new(queue),
            clients: Mutex::new(Vec::new()),
            next_client: AtomicU32::new(0),
            usb_mounted: AtomicBool::new(sink.mounted()),
            usb_suspended: AtomicBool::new(sink.suspended()),
            info: DeviceInfo {
                name: config.name,
                firmware: config.firmware,
                protocol: PROTOCOL_VERSION,
                controller: CONTROLLER_KIND.into(),
            },
            profile: config.profile,
        });
        let (finished_tx, finished_rx) = mpsc::channel();
        {
            let shared = Arc::clone(&shared);
            let tick = config.tick;
            spawn("executor", move || {
                executor(&shared, sink, tick, |f| {
                    let _ = finished_tx.send(f);
                })
            })?;
        }
        {
            let shared = Arc::clone(&shared);
            spawn("notifier", move || notifier(&shared, finished_rx))?;
        }
        {
            let shared = Arc::clone(&shared);
            spawn("control", move || accept_control(&shared, control))?;
        }
        {
            let shared = Arc::clone(&shared);
            spawn("http", move || serve_http(&shared, http))?;
        }
        Ok(Self {
            shared,
            control_addr,
            http_addr,
        })
    }

    pub fn control_addr(&self) -> SocketAddr {
        self.control_addr
    }

    pub fn http_addr(&self) -> SocketAddr {
        self.http_addr
    }

    pub fn status(&self) -> Status {
        self.shared.status(None)
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> io::Result<()> {
    thread::Builder::new()
        .name(format!("remote-{name}"))
        .stack_size(THREAD_STACK)
        .spawn(f)
        .map(drop)
}

impl Shared {
    fn queue(&self) -> MutexGuard<'_, InputQueue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn execute(
        &self,
        command: &SwitchCommand,
        profile: Option<PressProfile>,
    ) -> Result<Accepted, Rejected> {
        self.queue()
            .push(command, profile.as_ref().unwrap_or(&self.profile))
    }

    fn status(&self, seq: Option<u64>) -> Status {
        let queue = self.queue();
        Status {
            seq,
            idle: queue.is_idle(),
            pending: queue.pending(),
            usb_mounted: self.usb_mounted.load(Ordering::Relaxed),
            usb_suspended: self.usb_suspended.load(Ordering::Relaxed),
            keepalive_secs: queue.keepalive().map(|k| k.after.as_secs()),
            keepalives: queue.keepalives(),
        }
    }
}

fn executor(
    shared: &Shared,
    mut sink: impl HidSink,
    tick: Duration,
    mut on_finished: impl FnMut(Finished),
) {
    let mut last_wake: Option<Instant> = None;
    loop {
        let now = Instant::now();
        let (state, finished) = {
            let mut queue = shared.queue();
            let state = queue.tick(now);
            (state, queue.take_finished())
        };
        let suspended = sink.suspended();
        // A pressed button (not a stick: the keepalive only nudges one)
        // wakes a sleeping Switch, as HOME on a real wired pad does.
        if suspended
            && !state.buttons.is_empty()
            && last_wake.is_none_or(|t| now.duration_since(t) >= WAKE_INTERVAL)
        {
            sink.wake();
            last_wake = Some(now);
        }
        sink.send(&SwitchReport::from_state(&state));
        shared.usb_mounted.store(sink.mounted(), Ordering::Relaxed);
        shared.usb_suspended.store(suspended, Ordering::Relaxed);
        finished.into_iter().for_each(&mut on_finished);
        thread::sleep(tick);
    }
}

fn notifier(shared: &Shared, finished: Receiver<Finished>) {
    for f in finished {
        let message = DeviceMessage::Finished {
            id: f.id,
            cancelled: f.cancelled,
        };
        let conns: Vec<_> = lock(&shared.clients)
            .iter()
            .map(|c| (c.id, Arc::clone(&c.conn)))
            .collect();
        for (id, conn) in conns {
            if conn.send(&message).is_err() {
                lock(&shared.clients).retain(|c| c.id != id);
            }
        }
    }
}

fn accept_control(shared: &Arc<Shared>, listener: TcpListener) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if lock(&shared.clients).len() >= MAX_CLIENTS {
            let _ = write_line(
                &stream,
                &DeviceMessage::Error {
                    message: format!("too many clients (max {MAX_CLIENTS})"),
                },
            );
            continue;
        }
        let shared = Arc::clone(shared);
        let spawned = spawn("client", move || {
            let peer = stream.peer_addr().ok();
            if let Err(e) = serve_client(&shared, stream) {
                log_line(&format!("client {peer:?}: {e}"));
            }
        });
        if let Err(e) = spawned {
            log_line(&format!("cannot start client thread: {e}"));
        }
    }
}

fn serve_client(shared: &Shared, stream: TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let conn = Arc::new(Conn {
        stream,
        write: Mutex::new(()),
    });
    conn.send(&DeviceMessage::Hello(shared.info.clone()))?;
    let id = shared.next_client.fetch_add(1, Ordering::Relaxed);
    lock(&shared.clients).push(Client {
        id,
        conn: Arc::clone(&conn),
    });
    let result = read_requests(shared, &conn);
    lock(&shared.clients).retain(|c| c.id != id);
    result
}

fn read_requests(shared: &Shared, conn: &Conn) -> io::Result<()> {
    let mut reader = BufReader::new(&conn.stream);
    let mut line = String::new();
    loop {
        line.clear();
        let n = (&mut reader).take(MAX_LINE_BYTES).read_line(&mut line)?;
        if n == 0 {
            return Ok(());
        }
        if !line.ends_with('\n') && n as u64 == MAX_LINE_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<ClientMessage>(&line) {
            Ok(ClientMessage::Execute {
                seq,
                command,
                profile,
            }) => {
                // Hold the writer across the push so this client sees
                // `accepted` before the notifier can send `finished`.
                let _write = lock(&conn.write);
                let reply = match shared.execute(&command, profile) {
                    Ok(a) => DeviceMessage::Accepted {
                        seq,
                        id: a.id,
                        input_ms: a.input.as_millis() as u64,
                    },
                    Err(e) => DeviceMessage::Rejected {
                        seq,
                        reason: e.to_string(),
                    },
                };
                write_line(&conn.stream, &reply)?;
            }
            Ok(ClientMessage::Status { seq }) => {
                conn.send(&DeviceMessage::Status(shared.status(Some(seq))))?
            }
            Err(e) => conn.send(&DeviceMessage::Error {
                message: format!("bad request: {e}"),
            })?,
        }
    }
}

fn write_line(mut stream: &TcpStream, message: &DeviceMessage) -> io::Result<()> {
    let mut line = serde_json::to_vec(message).map_err(io::Error::other)?;
    line.push(b'\n');
    stream.write_all(&line)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn log_line(line: &str) {
    eprintln!("remote: {line}");
}

// ---------------------------------------------------------------- HTTP ----

fn serve_http(shared: &Shared, listener: TcpListener) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(HTTP_TIMEOUT));
        let _ = stream.set_write_timeout(Some(HTTP_TIMEOUT));
        let response = match read_http_request(&mut stream) {
            Ok(request) => route(shared, &request),
            Err(e) => Response::json(400, &error_body(&e.to_string())),
        };
        let _ = response.write(&mut stream);
    }
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn read_http_request(stream: &mut TcpStream) -> io::Result<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad request line",
        ));
    };
    let (method, path) = (
        method.to_owned(),
        target.split('?').next().unwrap_or("/").to_owned(),
    );
    let mut content_length = 0usize;
    for _ in 0..64 {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    if content_length > MAX_HTTP_BODY {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body)?;
    Ok(Request { method, path, body })
}

fn route(shared: &Shared, request: &Request) -> Response {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => Response {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: CONTROL_PAGE.as_bytes().to_vec(),
        },
        ("GET", "/api/info") => Response::json(200, &shared.info),
        ("GET", "/api/status") => Response::json(200, &shared.status(None)),
        ("POST", "/api/command") => match serde_json::from_slice::<CommandRequest>(&request.body) {
            Ok(req) => {
                let (command, profile) = req.into_parts();
                execute_response(shared, &command, profile)
            }
            Err(e) => Response::json(400, &error_body(&format!("bad command: {e}"))),
        },
        ("POST", "/api/neutral") => execute_response(shared, &SwitchCommand::Neutral, None),
        ("GET", "/api/keepalive") => Response::json(200, &keepalive_body(&shared.queue())),
        ("POST", "/api/keepalive") => set_keepalive(&mut shared.queue(), &request.body),
        ("OPTIONS", _) => Response {
            status: 204,
            content_type: "text/plain",
            body: Vec::new(),
        },
        _ => Response::json(404, &error_body("not found")),
    }
}

fn execute_response(
    shared: &Shared,
    command: &SwitchCommand,
    profile: Option<PressProfile>,
) -> Response {
    match shared.execute(command, profile) {
        Ok(a) => Response::json(
            200,
            &serde_json::json!({ "id": a.id, "input_ms": a.input.as_millis() as u64 }),
        ),
        Err(e) => Response::json(429, &error_body(&e.to_string())),
    }
}

/// `POST /api/keepalive`: `{"after_secs": 240}` sets the idle time and
/// `{"after_secs": null}` turns the keepalive off; `routine` (a
/// `SwitchCommand`) replaces the input. Omitted fields keep their value.
fn set_keepalive(queue: &mut InputQueue, body: &[u8]) -> Response {
    let request = match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(serde_json::Value::Object(map)) => map,
        Ok(_) => return Response::json(400, &error_body("bad keepalive: expected an object")),
        Err(e) => return Response::json(400, &error_body(&format!("bad keepalive: {e}"))),
    };
    let current = queue.keepalive().cloned();
    let after = match request.get("after_secs") {
        None => current.as_ref().map(|k| k.after),
        Some(serde_json::Value::Null) => None,
        Some(v) => match v.as_u64() {
            Some(secs) => Some(Duration::from_secs(secs.max(1))),
            None => return Response::json(400, &error_body("bad keepalive: after_secs")),
        },
    };
    let routine = match request.get("routine") {
        None => current.map_or_else(Keepalive::right_stick_nudge, |k| k.routine),
        Some(v) => match serde_json::from_value::<SwitchCommand>(v.clone()) {
            Ok(routine) => routine,
            Err(e) => return Response::json(400, &error_body(&format!("bad routine: {e}"))),
        },
    };
    queue.set_keepalive(after.map(|after| Keepalive { after, routine }));
    Response::json(200, &keepalive_body(queue))
}

fn keepalive_body(queue: &InputQueue) -> serde_json::Value {
    serde_json::json!({
        "after_secs": queue.keepalive().map(|k| k.after.as_secs()),
        "routine": queue.keepalive().map(|k| &k.routine),
        "played": queue.keepalives(),
    })
}

fn error_body(message: &str) -> serde_json::Value {
    serde_json::json!({ "error": message })
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn json(status: u16, value: &impl serde::Serialize) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(value).unwrap_or_default(),
        }
    }

    fn write(&self, stream: &mut TcpStream) -> io::Result<()> {
        let reason = match self.status {
            200 => "OK",
            204 => "No Content",
            400 => "Bad Request",
            404 => "Not Found",
            429 => "Too Many Requests",
            _ => "",
        };
        let head = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
             Access-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Content-Type\r\n\
             Connection: close\r\n\r\n",
            self.status,
            self.content_type,
            self.body.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(&self.body)?;
        stream.flush()
    }
}
