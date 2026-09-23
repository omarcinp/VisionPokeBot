use std::convert::Infallible;
use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
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
use tokio_stream::wrappers::{BroadcastStream, WatchStream};
use tokio_stream::StreamExt;

use crate::hub::FrameSnapshot;
use crate::Telemetry;

const INDEX_HTML: &str = include_str!("../web/index.html");
const STATUS_INTERVAL: Duration = Duration::from_millis(100);
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
const MJPEG_SCALE: u32 = 3;

/// A running web UI. Dropping it does not stop the server; it runs until the
/// process exits.
pub struct WebServer {
    pub addr: SocketAddr,
}

/// Starts the web UI on its own thread and returns once the port is bound.
pub fn serve(telemetry: Telemetry, addr: SocketAddr) -> Result<WebServer> {
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
        .route("/api/snapshot", get(snapshot))
        .route("/api/stream", get(sse))
        .with_state(AppState {
            telemetry,
            png_cache: Arc::new(Mutex::new(None)),
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

#[derive(Clone)]
struct AppState {
    telemetry: Telemetry,
    png_cache: Arc<Mutex<Option<(u64, Bytes)>>>,
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

async fn mjpeg(State(app): State<AppState>) -> Response {
    let frames = WatchStream::new(app.telemetry.inner.frame.subscribe())
        .throttle(FRAME_INTERVAL)
        .filter_map(|frame: Option<FrameSnapshot>| frame)
        .map(|frame| {
            let jpeg = encode_jpeg(&frame.image, MJPEG_SCALE);
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
    Json(json!({ "status": status, "log": app.telemetry.recent_log() }))
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
    let events = status
        .merge(frames)
        .merge(log)
        .filter_map(|event: std::result::Result<Event, axum::Error>| event.ok())
        .map(Ok);
    Sse::new(events).keep_alive(KeepAlive::default())
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
