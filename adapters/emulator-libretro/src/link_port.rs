//! The console's link port, carried over TCP between two emulators.
//!
//! A real GBA exchanges data with another over a link cable or a Wireless
//! Adapter; the core emulates the port and hands its traffic to the frontend
//! as opaque packets (libretro's netpacket interface). This module moves
//! those packets to the other emulator: one side listens (the host, libretro
//! client 0), the other connects (the guest, client 1). Two players only.
//!
//! Linked consoles share one clock, and the core's link emulation relies on
//! it: gpSP's Wireless Adapter holds only four packets and gives up on a
//! reply after a fraction of a frame. So the two emulators also keep frame
//! lockstep: each reports every finished frame (see `host::pump_link`).
//!
//! Wire format: records of a 2-byte big-endian length and a body. The first
//! record each way is a hello naming the core's link protocol, so two
//! incompatible cores refuse each other instead of corrupting a trade. Then
//! each body starts with a kind byte: a core packet, or a finished frame.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use pokebot_core::{Error, Result};

/// Which end of the cable this emulator holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkRole {
    /// Wait for the other emulator on this address (port 0 picks a free one;
    /// see [`crate::CoreInfo::link_addr`]).
    Host { listen: SocketAddr },
    /// Connect to a host at `host:port`, retrying until it is up.
    Join { host: String },
}

pub(crate) enum LinkEvent {
    Connected,
    Packet(Vec<u8>),
    /// The peer finished this many frames since the connection.
    PeerFrames(u32),
    Disconnected,
}

const HELLO: &[u8] = b"VisionPokeBot link 2; core ";
const KIND_PACKET: u8 = 0;
const KIND_FRAMES: u8 = 1;
/// How long a peer has to send its hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause between connection attempts, and between accept polls.
const RETRY: Duration = Duration::from_millis(500);
const ACCEPT_POLL: Duration = Duration::from_millis(50);

struct PortShared {
    /// Write half of the current connection.
    writer: Mutex<Option<TcpStream>>,
    stop: AtomicBool,
}

pub(crate) struct LinkPort {
    shared: Arc<PortShared>,
    events: Receiver<LinkEvent>,
    thread: Option<JoinHandle<()>>,
    host: bool,
    local_addr: Option<SocketAddr>,
}

impl LinkPort {
    /// Opens the port. `protocol` names the core's link protocol; a peer with
    /// a different one is refused.
    pub fn open(role: &LinkRole, protocol: &str) -> Result<Self> {
        let shared = Arc::new(PortShared {
            writer: Mutex::new(None),
            stop: AtomicBool::new(false),
        });
        let (events_tx, events) = mpsc::channel();
        let mut hello = HELLO.to_vec();
        hello.extend_from_slice(protocol.as_bytes());
        let worker = Arc::clone(&shared);
        let (thread, host, local_addr) = match role {
            LinkRole::Host { listen } => {
                let listener = TcpListener::bind(listen)
                    .map_err(|e| Error::Device(format!("link: cannot listen on {listen}: {e}")))?;
                listener
                    .set_nonblocking(true)
                    .map_err(|e| Error::Device(format!("link: {e}")))?;
                let addr = listener.local_addr().ok();
                let thread = std::thread::Builder::new()
                    .name("pokebot-link".into())
                    .spawn(move || run_host(&listener, &worker, &events_tx, &hello));
                (thread, true, addr)
            }
            LinkRole::Join { host } => {
                let host = host.clone();
                let thread = std::thread::Builder::new()
                    .name("pokebot-link".into())
                    .spawn(move || run_join(&host, &worker, &events_tx, &hello));
                (thread, false, None)
            }
        };
        let thread =
            thread.map_err(|e| Error::Device(format!("link: cannot spawn thread: {e}")))?;
        Ok(Self {
            shared,
            events,
            thread: Some(thread),
            host,
            local_addr,
        })
    }

    pub fn is_host(&self) -> bool {
        self.host
    }

    /// The address a host listens on.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }

    pub fn try_event(&self) -> Option<LinkEvent> {
        self.events.try_recv().ok()
    }

    pub fn wait_event(&self, timeout: Duration) -> Option<LinkEvent> {
        self.events.recv_timeout(timeout).ok()
    }

    /// Sends a core packet to the peer; dropped when none is connected (like
    /// a transfer with no cable plugged in).
    pub fn send(&self, payload: &[u8]) {
        self.write(KIND_PACKET, payload);
    }

    /// Tells the peer how many frames this side has finished.
    pub fn frames_done(&self, frames: u32) {
        self.write(KIND_FRAMES, &frames.to_be_bytes());
    }

    fn write(&self, kind: u8, payload: &[u8]) {
        let mut record = Vec::with_capacity(1 + payload.len());
        record.push(kind);
        record.extend_from_slice(payload);
        let mut writer = self.shared.writer.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(stream) = writer.as_mut() {
            if write_packet(stream, &record).is_err() {
                // The reader sees the closed socket and reports the loss.
                let _ = stream.shutdown(Shutdown::Both);
                *writer = None;
            }
        }
    }

    /// Hangs up on the current peer, if any.
    pub fn hang_up(&self) {
        let mut writer = self.shared.writer.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(stream) = writer.take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

impl Drop for LinkPort {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.hang_up();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_host(listener: &TcpListener, shared: &PortShared, events: &Sender<LinkEvent>, hello: &[u8]) {
    while !shared.stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                eprintln!("link: guest {peer} connected");
                session(stream, shared, events, hello);
                eprintln!("link: guest {peer} disconnected");
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(ACCEPT_POLL),
            Err(e) => {
                eprintln!("link: accept failed: {e}");
                std::thread::sleep(RETRY);
            }
        }
    }
}

fn run_join(host: &str, shared: &PortShared, events: &Sender<LinkEvent>, hello: &[u8]) {
    let mut reported = false;
    while !shared.stop.load(Ordering::SeqCst) {
        match TcpStream::connect(host) {
            Ok(stream) => {
                eprintln!("link: connected to host {host}");
                session(stream, shared, events, hello);
                eprintln!("link: host {host} disconnected");
                reported = false;
            }
            Err(e) if !reported => {
                eprintln!("link: waiting for host {host} ({e})");
                reported = true;
            }
            Err(_) => {}
        }
        std::thread::sleep(RETRY);
    }
}

/// Runs one connection until either side hangs up.
fn session(stream: TcpStream, shared: &PortShared, events: &Sender<LinkEvent>, hello: &[u8]) {
    let mut reader = stream;
    let greeted = (|| -> io::Result<Vec<u8>> {
        reader.set_nonblocking(false)?;
        reader.set_nodelay(true)?;
        write_packet(&mut reader, hello)?;
        reader.set_read_timeout(Some(HELLO_TIMEOUT))?;
        let theirs = read_packet(&mut reader)?;
        reader.set_read_timeout(None)?;
        Ok(theirs)
    })();
    match greeted {
        Ok(theirs) if theirs == hello => {}
        Ok(theirs) => {
            eprintln!(
                "link: refusing peer: it speaks {:?}, we speak {:?}",
                String::from_utf8_lossy(&theirs),
                String::from_utf8_lossy(hello)
            );
            return;
        }
        Err(e) => {
            eprintln!("link: handshake failed: {e}");
            return;
        }
    }
    let Ok(writer) = reader.try_clone() else {
        return;
    };
    *shared.writer.lock().unwrap_or_else(|e| e.into_inner()) = Some(writer);
    if shared.stop.load(Ordering::SeqCst) || events.send(LinkEvent::Connected).is_err() {
        return;
    }
    while let Ok(mut record) = read_packet(&mut reader) {
        let event = match record.first() {
            Some(&KIND_PACKET) => LinkEvent::Packet(record.split_off(1)),
            Some(&KIND_FRAMES) if record.len() == 5 => LinkEvent::PeerFrames(u32::from_be_bytes([
                record[1], record[2], record[3], record[4],
            ])),
            _ => {
                eprintln!("link: malformed record from peer; hanging up");
                break;
            }
        };
        if events.send(event).is_err() {
            break;
        }
    }
    let _ = reader.shutdown(Shutdown::Both);
    shared
        .writer
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let _ = events.send(LinkEvent::Disconnected);
}

fn write_packet(stream: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let len = u16::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "link packet too large"))?;
    let mut frame = Vec::with_capacity(2 + payload.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(payload);
    stream.write_all(&frame)
}

fn read_packet(stream: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    stream.read_exact(&mut len)?;
    let mut payload = vec![0u8; usize::from(u16::from_be_bytes(len))];
    stream.read_exact(&mut payload)?;
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn wait_event(port: &LinkPort) -> LinkEvent {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(event) = port.try_event() {
                return event;
            }
            assert!(Instant::now() < deadline, "no link event");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn host() -> LinkPort {
        let listen = "127.0.0.1:0".parse().unwrap();
        LinkPort::open(&LinkRole::Host { listen }, "test v1").unwrap()
    }

    fn join(host: &LinkPort, protocol: &str) -> LinkPort {
        let host = host.local_addr().unwrap().to_string();
        LinkPort::open(&LinkRole::Join { host }, protocol).unwrap()
    }

    #[test]
    fn packets_cross_both_ways() {
        let host = host();
        let guest = join(&host, "test v1");
        assert!(matches!(wait_event(&host), LinkEvent::Connected));
        assert!(matches!(wait_event(&guest), LinkEvent::Connected));
        host.send(b"from host");
        guest.send(&[7; 104]);
        guest.send(b"");
        guest.frames_done(70_000);
        match wait_event(&guest) {
            LinkEvent::Packet(p) => assert_eq!(p, b"from host"),
            _ => panic!("expected a packet"),
        }
        match (wait_event(&host), wait_event(&host), wait_event(&host)) {
            (LinkEvent::Packet(a), LinkEvent::Packet(b), LinkEvent::PeerFrames(n)) => {
                assert_eq!(a, vec![7; 104]);
                assert!(b.is_empty());
                assert_eq!(n, 70_000);
            }
            _ => panic!("expected two packets and a frame count"),
        }
    }

    #[test]
    fn host_sees_the_guest_leave_and_come_back() {
        let host = host();
        let guest = join(&host, "test v1");
        assert!(matches!(wait_event(&host), LinkEvent::Connected));
        drop(guest);
        assert!(matches!(wait_event(&host), LinkEvent::Disconnected));
        host.send(b"nobody listens"); // dropped, no error
        let _guest = join(&host, "test v1");
        assert!(matches!(wait_event(&host), LinkEvent::Connected));
    }

    #[test]
    fn different_protocols_refuse_each_other() {
        let host = host();
        let _guest = join(&host, "other core");
        std::thread::sleep(Duration::from_millis(300));
        assert!(host.try_event().is_none());
    }

    #[test]
    fn frames_round_trip() {
        let mut wire = Vec::new();
        write_packet(&mut wire, b"abc").unwrap();
        assert_eq!(wire, [0, 3, b'a', b'b', b'c']);
        assert_eq!(read_packet(&mut wire.as_slice()).unwrap(), b"abc");
        assert!(write_packet(&mut Vec::new(), &vec![0; 70_000]).is_err());
    }
}
