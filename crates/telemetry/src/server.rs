use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::Stream;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};
use pokebot_core::{Error, Result, RgbImage};
use serde_json::json;
use tokio_stream::wrappers::{BroadcastStream, IntervalStream, WatchStream};
use tokio_stream::StreamExt;

use crate::hub::FrameSnapshot;
use crate::Telemetry;

const INDEX_HTML: &str = include_str!("../web/index.html");
const STATUS_INTERVAL: Duration = Duration::from_millis(100);
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// Default `/stream.mjpg` upscale and frame interval; `?scale=1..3` and
/// `?fps=1..30` lower them (the hub's thumbnails use `scale=1&fps=10`).
const MJPEG_SCALE: u32 = 3;
const MJPEG_MAX_FPS: u32 = 30;
/// The page reconnects when it hears nothing for a few of these.
const PING_INTERVAL: Duration = Duration::from_secs(2);
/// Species folders (`front.png`, `shiny.pal`) from the decompilation.
const SPRITE_DIR: &str = "data/pret-pokefirered/graphics/pokemon";
/// Map views built by `tools/world/extract_region_map.py`.
const WORLD_DIR: &str = "data/world";
const WORLD_FILES: [(&str, &str); 3] = [
    ("region_map.png", "image/png"),
    ("overworld.png", "image/png"),
    ("region_map.json", "application/json"),
];

/// A running web UI. Dropping it does not stop the server; it runs until the
/// process exits.
pub struct WebServer {
    pub addr: SocketAddr,
}

/// Starts the web UI on its own thread and returns once the port is bound.
/// `instance_label` names this run in `/api/snapshot` (`Switch`, `Emulator`,
/// `Local`); the page uses it to highlight its tab behind the hub.
pub fn serve(telemetry: Telemetry, addr: SocketAddr, instance_label: &str) -> Result<WebServer> {
    let listener =
        TcpListener::bind(addr).map_err(|e| Error::Device(format!("cannot bind {addr}: {e}")))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| Error::Device(e.to_string()))?;
    let addr = listener
        .local_addr()
        .map_err(|e| Error::Device(e.to_string()))?;
    let app = Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/frame.png", get(frame_png))
        .route("/stream.mjpg", get(mjpeg))
        .route("/sprite/{species}", get(sprite))
        .route("/world/{file}", get(world_file))
        .route("/api/snapshot", get(snapshot))
        .route("/api/summary", get(summary))
        .route("/api/stream", get(sse))
        .with_state(AppState {
            telemetry,
            png_cache: Arc::new(Mutex::new(None)),
            sprites: Arc::new(Mutex::new(HashMap::new())),
            instance_label: Arc::from(instance_label),
        });
    std::thread::Builder::new()
        .name("pokebot-web".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
                if let Err(e) = axum::serve(listener, app).await {
                    eprintln!("web UI stopped: {e}");
                }
            });
        })
        .map_err(|e| Error::Device(format!("cannot spawn web thread: {e}")))?;
    Ok(WebServer { addr })
}

/// (folder, shiny) → PNG, or `None` if the species has no sprite.
type SpriteCache = HashMap<(String, bool), Option<Bytes>>;

#[derive(Clone)]
struct AppState {
    telemetry: Telemetry,
    png_cache: Arc<Mutex<Option<(u64, Bytes)>>>,
    sprites: Arc<Mutex<SpriteCache>>,
    instance_label: Arc<str>,
}

async fn frame_png(State(app): State<AppState>) -> Response {
    let Some(frame) = app.telemetry.inner.frame.borrow().clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no frame yet").into_response();
    };
    let cached = app
        .png_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .filter(|(id, _)| *id == frame.frame_id);
    let bytes = match cached {
        Some((_, bytes)) => bytes,
        None => {
            let bytes = Bytes::from(encode_png(&frame.image));
            *app.png_cache.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((frame.frame_id, bytes.clone()));
            bytes
        }
    };
    (
        [
            (header::CONTENT_TYPE, "image/png".to_owned()),
            (header::CACHE_CONTROL, "no-store".to_owned()),
            (
                header::HeaderName::from_static("x-frame-id"),
                frame.frame_id.to_string(),
            ),
        ],
        bytes,
    )
        .into_response()
}

#[derive(serde::Deserialize)]
struct MjpegQuery {
    scale: Option<String>,
    fps: Option<String>,
}

/// (upscale, frame interval) for `/stream.mjpg`: values clamp to 1–3 and
/// 1–30 fps; missing or unparsable ones keep the full-quality defaults.
fn mjpeg_params(scale: Option<&str>, fps: Option<&str>) -> (u32, Duration) {
    let scale = scale
        .and_then(|s| s.parse::<u32>().ok())
        .map_or(MJPEG_SCALE, |s| s.clamp(1, MJPEG_SCALE));
    let interval = fps
        .and_then(|s| s.parse::<u32>().ok())
        .map(|fps| fps.clamp(1, MJPEG_MAX_FPS))
        .filter(|&fps| fps < MJPEG_MAX_FPS)
        .map_or(FRAME_INTERVAL, |fps| {
            Duration::from_millis(1000 / u64::from(fps))
        });
    (scale, interval)
}

async fn mjpeg(State(app): State<AppState>, Query(query): Query<MjpegQuery>) -> Response {
    let (scale, interval) = mjpeg_params(query.scale.as_deref(), query.fps.as_deref());
    let frames = WatchStream::new(app.telemetry.inner.frame.subscribe())
        .throttle(interval)
        .filter_map(|frame: Option<FrameSnapshot>| frame)
        .map(move |frame| {
            let jpeg = encode_jpeg(&frame.image, scale);
            let mut part = format!(
                "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                jpeg.len()
            )
            .into_bytes();
            part.extend_from_slice(&jpeg);
            part.extend_from_slice(b"\r\n");
            Ok::<_, Infallible>(Bytes::from(part))
        });
    (
        [
            (
                header::CONTENT_TYPE,
                "multipart/x-mixed-replace; boundary=frame",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::from_stream(frames),
    )
        .into_response()
}

async fn snapshot(State(app): State<AppState>) -> Json<serde_json::Value> {
    let status = app.telemetry.inner.status.borrow().clone();
    Json(json!({
        "status": status,
        "log": app.telemetry.recent_log(),
        "instance_label": &*app.instance_label,
    }))
}

/// Fleet polling avoids copying the full state and 2,000-entry log per tile.
async fn summary(State(app): State<AppState>) -> Json<serde_json::Value> {
    let status = app.telemetry.inner.status.borrow();
    Json(json!({
        "stats": status.stats,
        "screen": status.state.get("screen"),
        "player": status.state.get("player"),
        "latest": app.telemetry.latest_log(),
    }))
}

async fn sse(State(app): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let inner = &app.telemetry.inner;
    let status = WatchStream::new(inner.status.subscribe())
        .throttle(STATUS_INTERVAL)
        .map(|status| Event::default().event("status").json_data(status));
    let frames = WatchStream::new(inner.frame.subscribe())
        .throttle(FRAME_INTERVAL)
        .filter_map(|frame: Option<FrameSnapshot>| frame)
        .map(|frame| {
            Ok(Event::default()
                .event("frame")
                .data(frame.frame_id.to_string()))
        });
    let log = BroadcastStream::new(inner.log_tx.subscribe())
        .filter_map(|entry| entry.ok()) // lagging clients skip entries; they can reload the snapshot
        .map(|entry| Event::default().event("log").json_data(entry));
    let ping = IntervalStream::new(tokio::time::interval(PING_INTERVAL))
        .map(|_| Ok(Event::default().event("ping").data("")));
    let events = status
        .merge(frames)
        .merge(log)
        .merge(ping)
        .filter_map(|event: std::result::Result<Event, axum::Error>| event.ok())
        .map(Ok);
    Sse::new(events).keep_alive(KeepAlive::default())
}

#[derive(serde::Deserialize)]
struct SpriteQuery {
    /// `1` or `true`.
    shiny: Option<String>,
}

/// Front sprite of a species (`SPECIES_PIDGEY` or `pidgey`), transparent
/// background, optionally in its shiny palette.
async fn sprite(
    State(app): State<AppState>,
    UrlPath(species): UrlPath<String>,
    Query(query): Query<SpriteQuery>,
) -> Response {
    let folder = species
        .trim_end_matches(".png")
        .trim_start_matches("SPECIES_")
        .to_ascii_lowercase();
    if folder.is_empty()
        || !folder
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return (StatusCode::BAD_REQUEST, "bad species").into_response();
    }
    let shiny = matches!(query.shiny.as_deref(), Some("1" | "true"));
    let key = (folder, shiny);
    let cached = app
        .sprites
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned();
    let png = match cached {
        Some(png) => png,
        None => {
            let png = load_sprite(&key.0, key.1).map(Bytes::from);
            app.sprites
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key, png.clone());
            png
        }
    };
    match png {
        Some(bytes) => (
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "public, max-age=86400"),
            ],
            bytes,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "no sprite").into_response(),
    }
}

/// The region map, the stitched overworld and where each map lies on them.
async fn world_file(UrlPath(file): UrlPath<String>) -> Response {
    let Some((name, mime)) = WORLD_FILES.iter().find(|(name, _)| *name == file) else {
        return (StatusCode::NOT_FOUND, "unknown file").into_response();
    };
    match data_dir(WORLD_DIR).and_then(|dir| std::fs::read(dir.join(name)).ok()) {
        Some(bytes) => (
            [
                (header::CONTENT_TYPE, *mime),
                (header::CACHE_CONTROL, "public, max-age=3600"),
            ],
            bytes,
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            "run tools/world/build.sh to build the map views",
        )
            .into_response(),
    }
}

/// A repository data directory, from the working directory or the source
/// tree.
fn data_dir(relative: &str) -> Option<PathBuf> {
    [
        PathBuf::from(relative),
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(relative),
    ]
    .into_iter()
    .find(|p| p.is_dir())
}

fn load_sprite(folder: &str, shiny: bool) -> Option<Vec<u8>> {
    let mut dir = data_dir(SPRITE_DIR)?.join(folder);
    // Forms (Unown) keep their sprites one level down.
    if !dir.join("front.png").is_file() {
        dir = dir.join("a");
    }
    let png = std::fs::read(dir.join("front.png")).ok()?;
    if !shiny {
        return Some(png);
    }
    let palette = std::fs::read_to_string(dir.join("shiny.pal")).ok()?;
    Some(replace_palette(&png, &parse_jasc(&palette)).unwrap_or(png))
}

/// RGB triples of a JASC-PAL file.
fn parse_jasc(text: &str) -> Vec<u8> {
    text.lines()
        .skip(3)
        .flat_map(|line| line.split_whitespace().filter_map(|v| v.parse::<u8>().ok()))
        .collect()
}

/// Swaps the colours of an indexed PNG's `PLTE` chunk, keeping its size.
fn replace_palette(png: &[u8], rgb: &[u8]) -> Option<Vec<u8>> {
    let mut out = png.get(..8)?.to_vec();
    let mut at = 8;
    while at + 12 <= png.len() {
        let len = u32::from_be_bytes(png[at..at + 4].try_into().ok()?) as usize;
        let end = at + 12 + len;
        let chunk = png.get(at..end)?;
        if &chunk[4..8] == b"PLTE" {
            let mut body = chunk[4..8 + len].to_vec();
            let n = rgb.len().min(len);
            body[4..4 + n].copy_from_slice(&rgb[..n]);
            out.extend_from_slice(&chunk[..4]);
            out.extend_from_slice(&body);
            out.extend_from_slice(&crc32(&body).to_be_bytes());
        } else {
            out.extend_from_slice(chunk);
        }
        at = end;
    }
    Some(out)
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

fn encode_png(image: &RgbImage) -> Vec<u8> {
    let mut out = Vec::new();
    let _ = PngEncoder::new(&mut out).write_image(
        image.as_bytes(),
        image.width(),
        image.height(),
        ExtendedColorType::Rgb8,
    );
    out
}

/// Nearest-neighbour upscale first so JPEG artifacts don't smear pixel art.
fn encode_jpeg(image: &RgbImage, scale: u32) -> Vec<u8> {
    let (w, h) = (image.width() * scale, image.height() * scale);
    let src = image.as_bytes();
    let mut scaled = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        let row = ((y / scale) * image.width()) as usize;
        for x in 0..w {
            let i = (row + (x / scale) as usize) * 3;
            scaled.extend_from_slice(&src[i..i + 3]);
        }
    }
    let mut out = Vec::new();
    let _ = JpegEncoder::new_with_quality(&mut out, 90).write_image(
        &scaled,
        w,
        h,
        ExtendedColorType::Rgb8,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_names_the_instance() {
        use std::io::{Read, Write};
        let telemetry = Telemetry::new("video", "controller");
        let server = serve(telemetry, "127.0.0.1:0".parse().unwrap(), "Emulator").unwrap();
        let mut sock = std::net::TcpStream::connect(server.addr).unwrap();
        sock.write_all(b"GET /api/snapshot HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut reply = String::new();
        sock.read_to_string(&mut reply).unwrap();
        let body = reply.split("\r\n\r\n").nth(1).unwrap();
        let snap: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(snap["instance_label"], "Emulator");
    }

    #[test]
    fn mjpeg_params_default_and_clamp() {
        let full = (MJPEG_SCALE, FRAME_INTERVAL);
        assert_eq!(mjpeg_params(None, None), full);
        assert_eq!(
            mjpeg_params(Some("1"), Some("10")),
            (1, Duration::from_millis(100))
        );
        // Out of range values clamp; garbage falls back to the default.
        assert_eq!(mjpeg_params(Some("0"), Some("0")).0, 1);
        assert_eq!(mjpeg_params(Some("0"), Some("0")).1, Duration::from_secs(1));
        assert_eq!(mjpeg_params(Some("9"), Some("500")), full);
        assert_eq!(mjpeg_params(Some("x"), Some("")), full);
        assert_eq!(mjpeg_params(Some("2"), None), (2, FRAME_INTERVAL));
    }

    #[test]
    fn crc32_matches_png_reference() {
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
    }

    #[test]
    fn shiny_sprite_is_a_valid_png_with_new_colours() {
        let (Some(normal), Some(shiny)) =
            (load_sprite("pidgey", false), load_sprite("pidgey", true))
        else {
            return; // decompilation graphics not checked out
        };
        assert_ne!(normal, shiny);
        let decoded = image::load_from_memory(&shiny).expect("valid png");
        assert_eq!((decoded.width(), decoded.height()), (64, 64));
        assert!(load_sprite("unown", false).is_some());
        assert!(load_sprite("nidoran_f", false).is_some());
    }
}
