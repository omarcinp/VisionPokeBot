//! Live window: shows the normalized feed and turns the keyboard into
//! controller commands. Works with any device pair, so the same window will
//! drive a real Switch through the capture card + ESP32.
//!
//! Keys: arrows = D-pad, X = A, Z = B, A = L, S = R, Enter = Start,
//! Backspace = Select, P = screenshot to captures/, Esc = quit.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use minifb::{Key, KeyRepeat, Window, WindowOptions};
use pokebot_core::{Button, ButtonSet, ControllerCommand, CANONICAL_HEIGHT, CANONICAL_WIDTH};

use pokebot_runtime::Runtime;

const KEYMAP: [(Key, Button); 10] = [
    (Key::Up, Button::Up),
    (Key::Down, Button::Down),
    (Key::Left, Button::Left),
    (Key::Right, Button::Right),
    (Key::X, Button::A),
    (Key::Z, Button::B),
    (Key::A, Button::L),
    (Key::S, Button::R),
    (Key::Enter, Button::Start),
    (Key::Backspace, Button::Select),
];

/// Held keys are sent as short holds, re-issued whenever the controller
/// drains, so input latency stays around one frame.
const HOLD_SLICE: Duration = Duration::from_millis(50);

pub fn play(mut runtime: Runtime, scale: usize, stop: &AtomicBool) -> Result<()> {
    let (w, h) = (CANONICAL_WIDTH as usize, CANONICAL_HEIGHT as usize);
    let mut window = Window::new(
        "PokéBot — Esc to quit",
        w * scale,
        h * scale,
        WindowOptions::default(),
    )
    .context("opening window (is a display available?)")?;
    let mut pixels = vec![0u32; w * h];
    let mut held = ButtonSet::NONE;

    while window.is_open() && !window.is_key_down(Key::Escape) && !stop.load(Ordering::Relaxed) {
        let frame = runtime.observe()?;
        for (dst, px) in pixels
            .iter_mut()
            .zip(frame.image().as_bytes().chunks_exact(3))
        {
            *dst = (u32::from(px[0]) << 16) | (u32::from(px[1]) << 8) | u32::from(px[2]);
        }
        window.update_with_buffer(&pixels, w, h)?;

        if window.is_key_pressed(Key::P, KeyRepeat::No) {
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
            let path = PathBuf::from(format!("captures/screenshot-{stamp}.png"));
            pokebot_video::png::save(frame.image(), &path)?;
            runtime.info(format!("saved {}", path.display()));
        }

        let now: ButtonSet = KEYMAP
            .iter()
            .filter(|(key, _)| window.is_key_down(*key))
            .map(|(_, button)| *button)
            .collect();
        if now != held {
            runtime.execute(ControllerCommand::Neutral)?;
            held = now;
        }
        if !held.is_empty() && runtime.is_idle()? {
            runtime.execute(ControllerCommand::Hold {
                buttons: held,
                duration: HOLD_SLICE,
            })?;
        }
    }
    runtime.execute(ControllerCommand::Neutral)?;
    Ok(runtime.finish()?)
}
