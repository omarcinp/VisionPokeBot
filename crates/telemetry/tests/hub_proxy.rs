//! The hub forwards `/<name>/…` to a backend with the prefix stripped and
//! streams the response through as it arrives.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use pokebot_telemetry::hub_proxy::{self, Route};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(5);

/// A backend that reports each request head it gets, answers with a chunked
/// body, sends a first chunk and holds the second until `release` fires.
async fn streaming_backend() -> (SocketAddr, mpsc::Receiver<String>, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (heads_tx, heads) = mpsc::channel(4);
    let (release, released) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut chunk).await.unwrap();
            assert!(n > 0, "client closed before the head ended");
            buf.extend_from_slice(&chunk[..n]);
        }
        heads_tx
            .send(String::from_utf8(buf).unwrap())
            .await
            .unwrap();
        sock.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
        )
        .await
        .unwrap();
        sock.write_all(b"6\r\nfirst\n\r\n").await.unwrap();
        sock.flush().await.unwrap();
        let _ = released.await;
        sock.write_all(b"7\r\nsecond\n\r\n0\r\n\r\n").await.unwrap();
    });
    (addr, heads, release)
}

async fn start_hub(routes: Vec<Route>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let dir = PathBuf::from("/nonexistent/pokebot-hub-test");
    tokio::spawn(hub_proxy::run(listener, routes, dir));
    addr
}

fn route(name: &str, label: &str, backend: SocketAddr) -> Route {
    Route {
        name: name.into(),
        label: label.into(),
        backend,
    }
}

/// Reads until `needle` shows up; returns everything read so far.
async fn read_until(sock: &mut TcpStream, got: &mut Vec<u8>, needle: &str) {
    let mut chunk = [0u8; 1024];
    while !String::from_utf8_lossy(got).contains(needle) {
        let n = timeout(WAIT, sock.read(&mut chunk))
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "timed out waiting for {needle:?}; got {:?}",
                    String::from_utf8_lossy(got)
                )
            })
            .unwrap();
        assert!(
            n > 0,
            "closed before {needle:?}; got {:?}",
            String::from_utf8_lossy(got)
        );
        got.extend_from_slice(&chunk[..n]);
    }
}

async fn get(hub: SocketAddr, path: &str) -> String {
    let mut sock = TcpStream::connect(hub).await.unwrap();
    sock.write_all(format!("GET {path} HTTP/1.1\r\nHost: hub\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut out = Vec::new();
    timeout(WAIT, sock.read_to_end(&mut out))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(out).unwrap()
}

#[tokio::test]
async fn proxied_request_has_stripped_path_and_streams_incrementally() {
    let (backend, mut heads, release) = streaming_backend().await;
    let hub = start_hub(vec![route("emu", "Emulator", backend)]).await;

    let mut sock = TcpStream::connect(hub).await.unwrap();
    sock.write_all(
        b"GET /emu/api/stream?x=1 HTTP/1.1\r\nHost: hub\r\nConnection: keep-alive\r\n\r\n",
    )
    .await
    .unwrap();

    let head = timeout(WAIT, heads.recv()).await.unwrap().unwrap();
    assert!(
        head.starts_with("GET /api/stream?x=1 HTTP/1.1\r\n"),
        "{head}"
    );
    assert!(head.contains("\r\nConnection: close\r\n"), "{head}");
    assert!(!head.contains("keep-alive"), "{head}");

    // The first chunk arrives while the backend still holds the second.
    let mut got = Vec::new();
    read_until(&mut sock, &mut got, "first").await;
    assert!(String::from_utf8_lossy(&got).starts_with("HTTP/1.1 200 OK"));
    assert!(!String::from_utf8_lossy(&got).contains("second"));

    release.send(()).unwrap();
    read_until(&mut sock, &mut got, "second").await;
}

#[tokio::test]
async fn stopped_backend_gives_503_with_links() {
    // Bind and drop to get a port nobody listens on.
    let dead = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let (live, _heads, _release) = streaming_backend().await;
    let hub = start_hub(vec![
        route("switch", "Switch", live),
        route("emu", "Emulator", dead),
    ])
    .await;

    let reply = get(hub, "/emu/stream.mjpg").await;
    assert!(reply.starts_with("HTTP/1.1 503"), "{reply}");
    assert!(
        reply.contains("Emulator instance is not running"),
        "{reply}"
    );
    assert!(reply.contains("href=\"/switch/\""), "{reply}");
}

#[tokio::test]
async fn hub_redirects_lists_and_rejects() {
    let dead = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let hub = start_hub(vec![
        route("switch", "Switch", dead),
        route("emu", "Emulator", dead),
    ])
    .await;

    let reply = get(hub, "/").await;
    assert!(reply.starts_with("HTTP/1.1 302"), "{reply}");
    assert!(reply.contains("\r\nLocation: /switch/\r\n"), "{reply}");

    let reply = get(hub, "/emu?a=1").await;
    assert!(reply.starts_with("HTTP/1.1 301"), "{reply}");
    assert!(reply.contains("\r\nLocation: /emu/?a=1\r\n"), "{reply}");

    let reply = get(hub, "/api/instances").await;
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(reply.contains("application/json"), "{reply}");
    let body = reply.split("\r\n\r\n").nth(1).unwrap();
    let list: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(list[0]["path"], "/switch/");
    assert_eq!(list[1]["path"], "/emu/");

    let reply = get(hub, "/nope/x").await;
    assert!(reply.starts_with("HTTP/1.1 404"), "{reply}");
}
