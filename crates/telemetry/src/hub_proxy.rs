//! The always-on hub on port 8080: one address for every bot instance.
//!
//! Each instance (the physical Switch run, an emulator run) serves its own
//! web UI on a loopback port. The hub maps `/<name>/…` onto that port with
//! the prefix stripped, so the page, which only uses relative URLs, works the
//! same behind the hub and standalone. `/` redirects to the first route
//! (`/switch/`) and `/api/instances` lists what is running, from the JSON
//! files `tools/live-run.sh` writes.
//!
//! This is a plain TCP proxy, not an HTTP client: it reads the request head,
//! rewrites the request line, and then pipes bytes both ways until either
//! side closes. That keeps long-lived responses (MJPEG, server-sent events)
//! streaming as they are produced. Only the first request on a connection is
//! parsed, so the forwarded head always says `Connection: close`: every
//! request then gets its own connection and its own path rewrite, and a
//! keep-alive client can never slip a second, unrewritten request through.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pokebot_core::{Error, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Where `tools/live-run.sh` describes the instances it started.
pub const DEFAULT_INSTANCES_DIR: &str = "/tmp/pokebot-instances";
/// Overrides the instances directory when `--instances-dir` is not given.
pub const INSTANCES_DIR_ENV: &str = "POKEBOT_INSTANCES_DIR";
/// Larger request heads are refused.
const MAX_HEAD: usize = 64 * 1024;
/// A client that doesn't finish its request head in this long is dropped.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Headers that describe the client's connection, not the request.
const HOP_BY_HOP: [&str; 3] = ["connection", "keep-alive", "proxy-connection"];

/// `/<name>/…` is served by the instance listening on `backend`.
#[derive(Debug, Clone)]
pub struct Route {
    pub name: String,
    pub label: String,
    pub backend: SocketAddr,
}

impl Route {
    fn new(name: &str, label: &str, port: u16) -> Self {
        Self {
            name: name.into(),
            label: label.into(),
            backend: SocketAddr::from(([127, 0, 0, 1], port)),
        }
    }

    /// The URL prefix, with both slashes: `/switch/`.
    pub fn path(&self) -> String {
        format!("/{}/", self.name)
    }
}

/// The Switch first: `/` redirects to it and the listing starts with it.
pub fn default_routes() -> Vec<Route> {
    vec![
        Route::new("switch", "Switch", 18080),
        Route::new("emu", "Emulator", 18081),
    ]
}

/// `--instances-dir`, else `$POKEBOT_INSTANCES_DIR`, else
/// `/tmp/pokebot-instances`.
pub fn resolve_instances_dir(flag: Option<PathBuf>) -> PathBuf {
    flag.or_else(|| std::env::var_os(INSTANCES_DIR_ENV).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_INSTANCES_DIR))
}

/// Every default route with what its instance file says, see
/// [`list_instances_for`].
pub fn list_instances(dir: &Path) -> Vec<Value> {
    list_instances_for(&default_routes(), dir)
}

/// [`list_instances_in`] against the real `/proc`.
pub fn list_instances_for(routes: &[Route], dir: &Path) -> Vec<Value> {
    list_instances_in(routes, dir, Path::new("/proc"))
}

/// One entry per route, in route order: the fields of `<dir>/<name>.json`
/// plus `alive` and `path`. `alive` means `<proc_root>/<pid>/comm` is
/// `pokebot-<name>`, the process name `tools/live-run.sh` gives each
/// instance, so a pid reused by another program doesn't count. A route
/// without a readable file is `{ name, label, path, alive: false }`. Files
/// that match no route are ignored: the hub could not reach them anyway.
pub fn list_instances_in(routes: &[Route], dir: &Path, proc_root: &Path) -> Vec<Value> {
    routes
        .iter()
        .map(|route| {
            let file = std::fs::read_to_string(dir.join(format!("{}.json", route.name)))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .filter(Value::is_object);
            let Some(mut entry) = file else {
                return json!({
                    "name": route.name,
                    "label": route.label,
                    "path": route.path(),
                    "alive": false,
                });
            };
            let expected = format!("pokebot-{}", route.name);
            let alive = entry["pid"].as_u64().is_some_and(|pid| {
                std::fs::read_to_string(proc_root.join(pid.to_string()).join("comm"))
                    .is_ok_and(|comm| comm.trim_end() == expected)
            });
            let obj = entry.as_object_mut().expect("checked above");
            obj.entry("name").or_insert_with(|| json!(route.name));
            obj.entry("label").or_insert_with(|| json!(route.label));
            obj.insert("path".into(), json!(route.path()));
            obj.insert("alive".into(), json!(alive));
            entry
        })
        .collect()
}

/// The route serving a request target and the target with the route's
/// prefix removed: `/emu/api/stream?x=1` → (emu, `/api/stream?x=1`).
pub fn strip_route<'a>(routes: &'a [Route], target: &str) -> Option<(&'a Route, String)> {
    routes.iter().find_map(|route| {
        let rest = target.strip_prefix(&route.path())?;
        Some((route, format!("/{rest}")))
    })
}

/// The request head to send to the backend: `target` replaces the request
/// line's path, and the client's connection headers give way to
/// `Connection: close` (see the module docs for why).
pub fn rewrite_head(head: &str, target: &str) -> String {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.splitn(3, ' ');
    let (method, _, version) = (
        parts.next().unwrap_or_default(),
        parts.next(),
        parts.next().unwrap_or("HTTP/1.1"),
    );
    let mut out = format!("{method} {target} {version}\r\n");
    for line in lines.filter(|line| !line.is_empty()) {
        let name = line.split(':').next().unwrap_or_default().trim();
        if HOP_BY_HOP.iter().any(|h| name.eq_ignore_ascii_case(h)) {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("Connection: close\r\n\r\n");
    out
}

/// Binds `addr` and serves the default routes until `stop` is set.
pub fn run_blocking(addr: SocketAddr, instances_dir: PathBuf, stop: Arc<AtomicBool>) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| Error::Device(format!("tokio runtime: {e}")))?;
    runtime.block_on(async move {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| Error::Device(format!("cannot bind {addr}: {e}")))?;
        eprintln!(
            "hub on http://{} (instances in {})",
            listener.local_addr().unwrap_or(addr),
            instances_dir.display()
        );
        for route in default_routes() {
            eprintln!("  {} -> {}", route.path(), route.backend);
        }
        tokio::select! {
            () = run(listener, default_routes(), instances_dir) => {}
            () = async {
                while !stop.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            } => {}
        }
        Ok(())
    })
}

/// Serves connections from `listener` forever.
pub async fn run(listener: TcpListener, routes: Vec<Route>, instances_dir: PathBuf) {
    let routes = Arc::new(routes);
    let instances_dir = Arc::new(instances_dir);
    loop {
        match listener.accept().await {
            Ok((client, _)) => {
                let (routes, dir) = (Arc::clone(&routes), Arc::clone(&instances_dir));
                tokio::spawn(async move {
                    let _ = handle(client, &routes, &dir).await;
                });
            }
            // Out of file descriptors and the like: wait instead of spinning.
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

async fn handle(mut client: TcpStream, routes: &[Route], dir: &Path) -> std::io::Result<()> {
    let _ = client.set_nodelay(true);
    let Ok(Ok(Some((head, body)))) =
        tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut client)).await
    else {
        return Ok(()); // timed out, closed early or too large
    };
    let Ok(head) = String::from_utf8(head) else {
        return respond(
            &mut client,
            false,
            "400 Bad Request",
            "",
            "text/plain",
            "bad request",
        )
        .await;
    };
    let mut request_line = head.split("\r\n").next().unwrap_or_default().split(' ');
    let (method, target) = (
        request_line.next().unwrap_or_default(),
        request_line.next().unwrap_or_default(),
    );
    let head_only = method == "HEAD";
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, format!("?{query}")),
        None => (target, String::new()),
    };

    if path == "/" {
        let home = routes.first().map_or_else(|| "/".to_owned(), Route::path);
        let location = format!("Location: {home}\r\n");
        return respond(
            &mut client,
            head_only,
            "302 Found",
            &location,
            "text/plain",
            "",
        )
        .await;
    }
    if routes.iter().any(|r| path == format!("/{}", r.name)) {
        let location = format!("Location: {path}/{query}\r\n");
        return respond(
            &mut client,
            head_only,
            "301 Moved Permanently",
            &location,
            "text/plain",
            "",
        )
        .await;
    }
    if path == "/api/instances" {
        let list = Value::Array(list_instances_for(routes, dir)).to_string();
        return respond(
            &mut client,
            head_only,
            "200 OK",
            "",
            "application/json",
            &list,
        )
        .await;
    }
    let Some((route, stripped)) = strip_route(routes, target) else {
        return respond(
            &mut client,
            head_only,
            "404 Not Found",
            "",
            "text/plain",
            "not found",
        )
        .await;
    };

    let backend = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(route.backend)).await;
    let Ok(Ok(mut backend)) = backend else {
        let page = not_running_page(route, routes);
        return respond(
            &mut client,
            head_only,
            "503 Service Unavailable",
            "",
            "text/html; charset=utf-8",
            &page,
        )
        .await;
    };
    let _ = backend.set_nodelay(true);
    backend
        .write_all(rewrite_head(&head, &stripped).as_bytes())
        .await?;
    backend.write_all(&body).await?;
    tokio::io::copy_bidirectional(&mut client, &mut backend).await?;
    Ok(())
}

/// The request head (through the blank line) and whatever body bytes came
/// with it; `None` if the client closed first or the head is too large.
async fn read_head(client: &mut TcpStream) -> std::io::Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let body = buf.split_off(end + 4);
            return Ok(Some((buf, body)));
        }
        if buf.len() > MAX_HEAD {
            return Ok(None);
        }
        let n = client.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

async fn respond(
    client: &mut TcpStream,
    head_only: bool,
    status: &str,
    extra_headers: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let mut reply = format!(
        "HTTP/1.1 {status}\r\n{extra_headers}Content-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    if !head_only {
        reply.push_str(body);
    }
    client.write_all(reply.as_bytes()).await?;
    client.shutdown().await
}

/// Reloads itself every few seconds, so it turns into the page once the
/// instance starts.
fn not_running_page(route: &Route, routes: &[Route]) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let links: String = routes
        .iter()
        .filter(|r| r.name != route.name)
        .map(|r| {
            format!(
                "<li><a href=\"{}\">{}</a></li>",
                esc(&r.path()),
                esc(&r.label)
            )
        })
        .collect();
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"refresh\" content=\"5\">\
         <title>{label} not running</title>\
         <style>body{{background:#0d1014;color:#d8dee6;font:15px system-ui,sans-serif;padding:24px}}a{{color:#7fb3ff}}</style>\
         </head><body><h1>{label} instance is not running</h1>\
         <p>Start it with <code>tools/live-run.sh --instance {name} …</code>; this page reloads every 5 s.</p>\
         <p>Other instances:</p><ul>{links}</ul></body></html>",
        label = esc(&route.label),
        name = esc(&route.name),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pokebot-hub-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn instances_list_every_route_in_order_with_liveness() {
        let dir = temp_dir("list");
        // Fake /proc: pid 101 runs pokebot-switch, pid 102 was reused by
        // another program, pid 103 is gone.
        let proc_root = dir.join("proc");
        for (pid, comm) in [(101, "pokebot-switch\n"), (102, "bash\n")] {
            std::fs::create_dir_all(proc_root.join(pid.to_string())).unwrap();
            std::fs::write(proc_root.join(format!("{pid}/comm")), comm).unwrap();
        }
        let entry = |name: &str, label: &str, port: u16, pid: u32| {
            serde_json::json!({
                "name": name, "label": label, "port": port, "pid": pid,
                "command": "pokebot run", "log": "/tmp/x.log", "started_at": "2026-09-24T10:00:00Z",
            })
        };
        let write = |name: &str, label: &str, port: u16, pid: u32| {
            std::fs::write(
                dir.join(format!("{name}.json")),
                entry(name, label, port, pid).to_string(),
            )
            .unwrap();
        };
        // Written emu first: the listing must still put switch first.
        write("emu", "Emulator", 18081, 103);
        write("switch", "Switch", 18080, 101);
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();

        let list = list_instances_in(&default_routes(), &dir, &proc_root);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["name"], "switch");
        assert_eq!(list[0]["alive"], true);
        assert_eq!(list[0]["path"], "/switch/");
        assert_eq!(list[0]["port"], 18080);
        assert_eq!(list[0]["log"], "/tmp/x.log");
        assert_eq!(list[1]["name"], "emu");
        assert_eq!(list[1]["label"], "Emulator");
        assert_eq!(list[1]["alive"], false);
        assert_eq!(list[1]["path"], "/emu/");

        // A reused pid (another program's comm) is not alive.
        write("emu", "Emulator", 18081, 102);
        let list = list_instances_in(&default_routes(), &dir, &proc_root);
        assert_eq!(list[1]["alive"], false);
        // Nor is a live pid that runs a different instance.
        write("emu", "Emulator", 18081, 101);
        let list = list_instances_in(&default_routes(), &dir, &proc_root);
        assert_eq!(list[1]["alive"], false);

        // A route without a file is still listed, as stopped.
        std::fs::remove_file(dir.join("emu.json")).unwrap();
        let list = list_instances_in(&default_routes(), &dir, &proc_root);
        assert_eq!(list.len(), 2);
        assert_eq!(
            list[1],
            serde_json::json!({ "name": "emu", "label": "Emulator", "path": "/emu/", "alive": false })
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn this_test_process_is_not_an_instance() {
        let dir = temp_dir("self");
        let pid = std::process::id();
        std::fs::write(
            dir.join("switch.json"),
            serde_json::json!({ "name": "switch", "pid": pid }).to_string(),
        )
        .unwrap();
        // /proc/<pid> exists, but its comm is the test binary's.
        assert!(Path::new(&format!("/proc/{pid}")).exists());
        assert_eq!(list_instances(&dir)[0]["alive"], false);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_instances_dir_lists_routes_as_stopped() {
        let list = list_instances(Path::new("/nonexistent/pokebot-instances"));
        let names: Vec<_> = list.iter().map(|e| e["name"].clone()).collect();
        assert_eq!(names, ["switch", "emu"]);
        assert!(list.iter().all(|e| e["alive"] == false));
        assert_eq!(list[0]["label"], "Switch");
    }

    #[test]
    fn route_prefix_is_stripped_from_the_path() {
        let routes = default_routes();
        let strip = |t: &str| strip_route(&routes, t).map(|(r, p)| (r.name.clone(), p));
        assert_eq!(
            strip("/emu/api/stream?x=1"),
            Some(("emu".into(), "/api/stream?x=1".into()))
        );
        assert_eq!(strip("/switch/"), Some(("switch".into(), "/".into())));
        assert_eq!(
            strip("/switch/stream.mjpg"),
            Some(("switch".into(), "/stream.mjpg".into()))
        );
        assert_eq!(strip("/other/api/stream"), None);
        assert_eq!(strip("/switch"), None);
        assert_eq!(strip("/emulator/"), None);
        assert_eq!(strip("/"), None);
    }

    #[test]
    fn forwarded_head_gets_new_path_and_connection_close() {
        let head = "GET /emu/api/stream?x=1 HTTP/1.1\r\nHost: pi:8080\r\nconnection: keep-alive\r\nKeep-Alive: timeout=5\r\nAccept: text/event-stream\r\n\r\n";
        assert_eq!(
            rewrite_head(head, "/api/stream?x=1"),
            "GET /api/stream?x=1 HTTP/1.1\r\nHost: pi:8080\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n"
        );
        let head = "POST /switch/x HTTP/1.0\r\nContent-Length: 3\r\n\r\n";
        assert_eq!(
            rewrite_head(head, "/x"),
            "POST /x HTTP/1.0\r\nContent-Length: 3\r\nConnection: close\r\n\r\n"
        );
    }
}
