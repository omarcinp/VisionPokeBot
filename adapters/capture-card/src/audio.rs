//! The capture card's own sound input (UVC cards such as the MS2109 expose a
//! USB audio interface next to the video one). `arecord` reads it, so no
//! ALSA headers are needed to build; the process needs the `audio` group.
//!
//! Output is mono signed 16-bit little-endian PCM at [`SAMPLE_RATE`], in
//! [`CHUNK_SAMPLES`] chunks. The Switch sends the GBA's sound as identical
//! left and right channels, so the channels are averaged.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub const SAMPLE_RATE: u32 = 48_000;
/// 20 ms: small enough for live playback, large enough to keep overhead low.
pub const CHUNK_SAMPLES: usize = 960;
const CARD_CHANNELS: usize = 2;
const RETRY: Duration = Duration::from_secs(2);

/// The ALSA capture device on the same USB device as a V4L2 node, e.g.
/// `hw:CARD=MS2109,DEV=0` for `/dev/video0`. `None` if the card has no sound
/// interface (or the node isn't a USB device).
pub fn alsa_device_for(video: &Path) -> Option<String> {
    alsa_device_in(Path::new("/sys/class"), video)
}

fn alsa_device_in(sys_class: &Path, video: &Path) -> Option<String> {
    let video = std::fs::canonicalize(video).unwrap_or_else(|_| video.to_path_buf());
    let node = video.file_name()?;
    let usb_device = |interface: PathBuf| -> Option<PathBuf> {
        Some(
            std::fs::canonicalize(interface)
                .ok()?
                .parent()?
                .to_path_buf(),
        )
    };
    let wanted = usb_device(sys_class.join("video4linux").join(node).join("device"))?;
    let mut cards: Vec<_> = std::fs::read_dir(sys_class.join("sound"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("card"))
        })
        .collect();
    cards.sort();
    cards.into_iter().find_map(|card| {
        if usb_device(card.join("device"))? != wanted {
            return None;
        }
        let id = std::fs::read_to_string(card.join("id")).ok()?;
        Some(format!("hw:CARD={},DEV=0", id.trim()))
    })
}

/// Averages interleaved stereo S16LE into mono S16LE.
fn downmix(stereo: &[u8], mono: &mut Vec<u8>) {
    mono.clear();
    for frame in stereo.chunks_exact(2 * CARD_CHANNELS) {
        let left = i32::from(i16::from_le_bytes([frame[0], frame[1]]));
        let right = i32::from(i16::from_le_bytes([frame[2], frame[3]]));
        mono.extend_from_slice(&(((left + right) / 2) as i16).to_le_bytes());
    }
}

/// Reads the card's sound on a background thread and hands each mono chunk
/// to a callback. Unplugs, a busy device and a missing `arecord` are retried
/// every few seconds until dropped.
pub struct AudioCapture {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    thread: Option<JoinHandle<()>>,
}

impl AudioCapture {
    /// `video` is the card's V4L2 node; its sound device is looked up again
    /// on every retry, since card numbers change on replug.
    /// `on_status(Some(device))` reports a started capture, `None` a stop.
    pub fn start(
        video: PathBuf,
        mut on_chunk: impl FnMut(&[u8]) + Send + 'static,
        mut on_status: impl FnMut(Option<&str>) + Send + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let thread = {
            let (stop, slot) = (stop.clone(), child.clone());
            std::thread::Builder::new()
                .name("pokebot-audio".into())
                .spawn(move || {
                    let mut stereo = vec![0u8; CHUNK_SAMPLES * CARD_CHANNELS * 2];
                    let mut mono = Vec::with_capacity(CHUNK_SAMPLES * 2);
                    while !stop.load(Ordering::Relaxed) {
                        let spawned = alsa_device_for(&video).and_then(|device| {
                            let child = Command::new("arecord")
                                .args(["-q", "-D", &device, "-f", "S16_LE", "-t", "raw"])
                                .args(["-r", &SAMPLE_RATE.to_string()])
                                .args(["-c", &CARD_CHANNELS.to_string()])
                                .stdin(Stdio::null())
                                .stdout(Stdio::piped())
                                .stderr(Stdio::null())
                                .spawn()
                                .ok()?;
                            Some((device, child))
                        });
                        if let Some((device, mut process)) = spawned {
                            let mut out = process.stdout.take().expect("piped stdout");
                            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(process);
                            let mut reported = false;
                            while !stop.load(Ordering::Relaxed)
                                && out.read_exact(&mut stereo).is_ok()
                            {
                                if !reported {
                                    reported = true;
                                    on_status(Some(&device));
                                }
                                downmix(&stereo, &mut mono);
                                on_chunk(&mono);
                            }
                            if let Some(mut process) =
                                slot.lock().unwrap_or_else(|e| e.into_inner()).take()
                            {
                                let _ = process.kill();
                                let _ = process.wait();
                            }
                            if reported {
                                on_status(None);
                            }
                        }
                        for _ in 0..RETRY.as_millis() / 100 {
                            if stop.load(Ordering::Relaxed) {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    }
                })
                .expect("spawn audio thread")
        };
        Self {
            stop,
            child,
            thread: Some(thread),
        }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Killing arecord ends the thread's blocking read.
        if let Some(child) = self
            .child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            let _ = child.kill();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn downmix_averages_the_channels() {
        let stereo: Vec<u8> = [100i16, 300, -7, -7, i16::MAX, i16::MAX]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let mut mono = Vec::new();
        downmix(&stereo, &mut mono);
        let samples: Vec<i16> = mono
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(samples, [200, -7, i16::MAX]);
    }

    /// Mirrors sysfs for an MS2109: video on interface 1.0, sound on 1.2 of
    /// the same USB device, and an unrelated sound card elsewhere.
    #[test]
    fn finds_the_sound_card_on_the_same_usb_device() {
        let root = std::env::temp_dir().join(format!("pokebot-audio-sys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let devices = root.join("devices");
        for dir in ["usb11/11-1/11-1:1.0", "usb11/11-1/11-1:1.2", "pci/hda"] {
            std::fs::create_dir_all(devices.join(dir)).unwrap();
        }
        let class = root.join("class");
        let video = class.join("video4linux/video0");
        std::fs::create_dir_all(&video).unwrap();
        symlink(devices.join("usb11/11-1/11-1:1.0"), video.join("device")).unwrap();
        for (card, device, id) in [
            ("card0", "pci/hda", "PCH"),
            ("card1", "usb11/11-1/11-1:1.2", "MS2109"),
        ] {
            let dir = class.join("sound").join(card);
            std::fs::create_dir_all(&dir).unwrap();
            symlink(devices.join(device), dir.join("device")).unwrap();
            std::fs::write(dir.join("id"), format!("{id}\n")).unwrap();
        }
        assert_eq!(
            alsa_device_in(&class, Path::new("/dev/video0")).as_deref(),
            Some("hw:CARD=MS2109,DEV=0")
        );
        assert_eq!(alsa_device_in(&class, Path::new("/dev/video7")), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
