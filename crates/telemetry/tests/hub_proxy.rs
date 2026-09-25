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
    assert!(reply.contains("\r\nLocation: /games/\r\n"), "{reply}");

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

#[tokio::test]
async fn discovers_new_worker_after_start_and_checks_process_identity() {
    let dir = std::env::temp_dir().join(format!("hub-discovery-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(hub_proxy::run(
        listener,
        hub_proxy::default_routes(),
        dir.clone(),
    ));
    assert!(get(addr, "/emu-test/anything")
        .await
        .starts_with("HTTP/1.1 404"));
    let (backend, mut heads, release) = streaming_backend().await;
    hub_proxy::register_worker(&dir.join("emu-test.json"), "Worker test", backend.port()).unwrap();
    let listing = get(addr, "/api/instances").await;
    let list: serde_json::Value =
        serde_json::from_str(listing.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    let worker = list
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "emu-test")
        .unwrap();
    assert_eq!(worker["alive"], true);
    let pending = tokio::spawn(get(addr, "/emu-test/api/stream"));
    assert!(heads
        .recv()
        .await
        .unwrap()
        .starts_with("GET /api/stream HTTP/1.1"));
    release.send(()).unwrap();
    assert!(pending.await.unwrap().contains("second"));
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("emu-test.json")).unwrap()).unwrap();
    manifest["process_start"] = serde_json::json!("not-this-process");
    // A different service now owns a port recorded in a stale manifest.
    let unrelated = TcpListener::bind("127.0.0.1:0").await.unwrap();
    manifest["port"] = serde_json::json!(unrelated.local_addr().unwrap().port());
    std::fs::write(dir.join("emu-test.json"), manifest.to_string()).unwrap();
    assert_eq!(
        hub_proxy::list_instances(&dir).last().unwrap()["alive"],
        false
    );
    assert!(get(addr, "/emu-test/api/snapshot")
        .await
        .starts_with("HTTP/1.1 503"));
    assert!(hub_proxy::register_worker(&dir.join("switch.json"), "bad", 1234).is_err());
    assert!(get(addr, "/emulators/").await.contains("Start emulators"));
    task.abort();
    std::fs::remove_dir_all(dir).unwrap();
}

struct EchoControl;
impl hub_proxy::FleetControl for EchoControl {
    fn request(
        &self,
        method: &str,
        path: &str,
        body: serde_json::Value,
    ) -> (u16, serde_json::Value) {
        (
            200,
            serde_json::json!({"method":method,"path":path,"body":body}),
        )
    }
}

#[tokio::test]
async fn control_api_requires_json_header_and_reads_fragmented_body() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(hub_proxy::run_with_fleet(
        listener,
        vec![],
        PathBuf::from("/nonexistent"),
        Some(std::sync::Arc::new(EchoControl)),
    ));
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(b"POST /api/emulators HTTP/1.1\r\nHost: hub\r\nContent-Length: 2\r\n\r\n{}")
        .await
        .unwrap();
    let mut out = String::new();
    sock.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 403"));
    let body = r#"{"count":2,"task":"new-game"}"#;
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(format!("POST /api/emulators HTTP/1.1\r\nHost: hub\r\nX-Pokebot-Control: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",body.len()).as_bytes()).await.unwrap();
    sock.write_all(&body.as_bytes()[..8]).await.unwrap();
    tokio::task::yield_now().await;
    sock.write_all(&body.as_bytes()[8..]).await.unwrap();
    let mut out = String::new();
    timeout(WAIT, sock.read_to_string(&mut out))
        .await
        .unwrap()
        .unwrap();
    assert!(out.starts_with("HTTP/1.1 200"));
    let reply: serde_json::Value =
        serde_json::from_str(out.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(reply["body"]["count"], 2);
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(b"POST /api/emulators HTTP/1.1\r\nHost: hub\r\nX-Pokebot-Control: 1\r\nContent-Type: application/json\r\nContent-Length: 4097\r\n\r\n").await.unwrap();
    let mut out = String::new();
    sock.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 400"));
    task.abort();
}

#[tokio::test]
async fn hub_owns_dashboard_navigation_while_old_switch_keeps_running() {
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub = start_hub(vec![route(
        "switch",
        "Switch",
        backend.local_addr().unwrap(),
    )])
    .await;
    // Backend is reachable but needn't serve a new page (or restart its bot).
    let reply = get(hub, "/switch/").await;
    assert!(reply.starts_with("HTTP/1.1 200"));
    assert!(reply.contains("['← Game instances', '/games/'"));
    assert!(!reply.contains("id=\"thumbs\""));
}

#[tokio::test]
async fn switch_preview_and_detail_survive_without_a_bot() {
    let dir = std::env::temp_dir().join(format!("hub-switch-device-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (backend, mut heads, release) = streaming_backend().await;
    let manifest = dir.join("switch-device.json");
    hub_proxy::register_worker(&manifest, "Switch", backend.port()).unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    value["connected"] = serde_json::json!(true);
    value["video_ready"] = serde_json::json!(true);
    std::fs::write(&manifest, value.to_string()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(hub_proxy::run(
        listener,
        hub_proxy::default_routes(),
        dir.clone(),
    ));
    let pending = tokio::spawn(get(addr, "/switch/frame.png"));
    let head = timeout(WAIT, heads.recv()).await.unwrap().unwrap();
    assert!(head.starts_with("GET /frame.png HTTP/1.1"));
    release.send(()).unwrap();
    assert!(pending.await.unwrap().contains("second"));
    assert!(get(addr, "/games/").await.contains("Game instances"));
    assert!(get(addr, "/emulators/").await.contains("Game instances"));
    task.abort();
    let _ = std::fs::remove_dir_all(dir);
}
