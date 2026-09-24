use std::io;
use std::path::Path;

use pokebot_core::{Error, Result, RgbImage};
use v4l::video::Output;
use v4l::{Device, Format, FourCC};

/// Writes frames into a V4L2 output device (e.g. v4l2loopback) as RGB24,
/// optionally integer-upscaled like a console's HDMI output.
pub struct V4l2Sink {
    device: Device,
    width: u32,
    height: u32,
    stride: usize,
    scale: u32,
    buffer: Vec<u8>,
}

impl V4l2Sink {
    pub fn open(path: &Path, source_width: u32, source_height: u32, scale: u32) -> Result<Self> {
        let err =
            |what: &str, e: io::Error| Error::Device(format!("{}: {what}: {e}", path.display()));
        let scale = scale.max(1);
        let (width, height) = (source_width * scale, source_height * scale);
        let device = Device::with_path(path).map_err(|e| err("open", e))?;
        let wanted = Format::new(width, height, FourCC::new(b"RGB3"));
        let actual =
            Output::set_format(&device, &wanted).map_err(|e| err("set output format", e))?;
        if actual.width != width || actual.height != height || actual.fourcc != wanted.fourcc {
            return Err(Error::Device(format!(
                "{}: device chose {}x{} {} instead of {width}x{height} RGB3",
                path.display(),
                actual.width,
                actual.height,
                actual.fourcc
            )));
        }
        let stride = (actual.stride as usize).max(width as usize * 3);
        Ok(Self {
            device,
            width,
            height,
            stride,
            scale,
            buffer: vec![0; stride * height as usize],
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn write(&mut self, image: &RgbImage) -> Result<()> {
        if image.width() * self.scale != self.width || image.height() * self.scale != self.height {
            return Err(Error::InvalidImage(format!(
                "sink expects {}x{} source frames, got {}x{}",
                self.width / self.scale,
                self.height / self.scale,
                image.width(),
                image.height()
            )));
        }
        let (scale, src) = (self.scale as usize, image.as_bytes());
        let src_line = image.width() as usize * 3;
        for (y, line) in self.buffer.chunks_exact_mut(self.stride).enumerate() {
            let row = &src[(y / scale) * src_line..][..src_line];
            for (x, px) in line[..self.width as usize * 3]
                .chunks_exact_mut(3)
                .enumerate()
            {
                px.copy_from_slice(&row[(x / scale) * 3..][..3]);
            }
        }
        // SAFETY: writing an initialised buffer to the device's fd.
        let written = unsafe {
            libc::write(
                self.device.handle().fd(),
                self.buffer.as_ptr().cast(),
                self.buffer.len(),
            )
        };
        if written < 0 {
            return Err(Error::Device(format!(
                "V4L2 write failed: {}",
                io::Error::last_os_error()
            )));
        }
        Ok(())
    }
}
