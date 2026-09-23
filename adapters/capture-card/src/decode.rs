use pokebot_core::{Error, Result, RgbImage};

/// Pixel formats the capture adapter understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Rgb24,
    Bgr24,
    /// YUV 4:2:2 packed, BT.601 limited range (typical of UVC capture cards).
    Yuyv,
    Mjpeg,
}

impl PixelFormat {
    pub fn from_fourcc(fourcc: [u8; 4]) -> Option<Self> {
        match &fourcc {
            b"RGB3" => Some(Self::Rgb24),
            b"BGR3" => Some(Self::Bgr24),
            b"YUYV" => Some(Self::Yuyv),
            b"MJPG" => Some(Self::Mjpeg),
            _ => None,
        }
    }

    /// `stride` is bytes per line as reported by the driver (0 = packed).
    pub fn decode(self, bytes: &[u8], width: u32, height: u32, stride: u32) -> Result<RgbImage> {
        let (w, h) = (width as usize, height as usize);
        let packed_line = match self {
            Self::Rgb24 | Self::Bgr24 => w * 3,
            Self::Yuyv => w * 2,
            Self::Mjpeg => return decode_mjpeg(bytes),
        };
        let line = if stride == 0 {
            packed_line
        } else {
            stride as usize
        };
        if line < packed_line || bytes.len() < line * (h.max(1) - 1) + packed_line {
            return Err(Error::InvalidImage(format!(
                "{self:?} buffer of {} bytes is too small for {width}x{height}",
                bytes.len()
            )));
        }
        let mut rgb = Vec::with_capacity(w * h * 3);
        for row in bytes.chunks(line).take(h) {
            let row = &row[..packed_line];
            match self {
                Self::Rgb24 => rgb.extend_from_slice(row),
                Self::Bgr24 => row
                    .chunks_exact(3)
                    .for_each(|p| rgb.extend_from_slice(&[p[2], p[1], p[0]])),
                Self::Yuyv => row.chunks_exact(4).for_each(|p| {
                    rgb.extend_from_slice(&yuv_to_rgb(p[0], p[1], p[3]));
                    rgb.extend_from_slice(&yuv_to_rgb(p[2], p[1], p[3]));
                }),
                Self::Mjpeg => unreachable!(),
            }
        }
        RgbImage::from_raw(width, height, rgb)
    }
}

/// BT.601 limited range, integer arithmetic.
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let c = 298 * (i32::from(y) - 16);
    let d = i32::from(u) - 128;
    let e = i32::from(v) - 128;
    let clamp = |x: i32| ((x + 128) >> 8).clamp(0, 255) as u8;
    [
        clamp(c + 409 * e),
        clamp(c - 100 * d - 208 * e),
        clamp(c + 516 * d),
    ]
}

fn decode_mjpeg(bytes: &[u8]) -> Result<RgbImage> {
    let decoded = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)
        .map_err(|e| Error::InvalidImage(format!("MJPEG frame: {e}")))?
        .into_rgb8();
    let (width, height) = decoded.dimensions();
    RgbImage::from_raw(width, height, decoded.into_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_packed_rgb_with_stride_padding() {
        // 2x2 image, 8-byte lines (6 used + 2 padding).
        let bytes = [1, 2, 3, 4, 5, 6, 0, 0, 7, 8, 9, 10, 11, 12];
        let img = PixelFormat::Rgb24.decode(&bytes, 2, 2, 8).unwrap();
        assert_eq!(img.as_bytes(), &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        let bgr = PixelFormat::Bgr24.decode(&bytes, 2, 2, 8).unwrap();
        assert_eq!(bgr.pixel(0, 0), [3, 2, 1]);
    }

    #[test]
    fn decodes_yuyv_primaries() {
        // Two pixels: white (Y=235) and black (Y=16), neutral chroma.
        let img = PixelFormat::Yuyv
            .decode(&[235, 128, 16, 128], 2, 1, 0)
            .unwrap();
        assert_eq!(img.pixel(0, 0), [255, 255, 255]);
        assert_eq!(img.pixel(1, 0), [0, 0, 0]);
        assert!(PixelFormat::Yuyv.decode(&[0; 3], 2, 1, 0).is_err());
    }

    #[test]
    fn knows_supported_fourccs() {
        assert_eq!(PixelFormat::from_fourcc(*b"YUYV"), Some(PixelFormat::Yuyv));
        assert_eq!(PixelFormat::from_fourcc(*b"NV12"), None);
    }
}
