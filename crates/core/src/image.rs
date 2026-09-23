use std::fmt;

use crate::{Error, Result};

/// Packed 8-bit RGB image, row-major, no padding.
#[derive(Clone, PartialEq, Eq)]
pub struct RgbImage {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl RgbImage {
    pub fn from_raw(width: u32, height: u32, data: Vec<u8>) -> Result<Self> {
        let expected = width as usize * height as usize * 3;
        if data.len() != expected {
            return Err(Error::InvalidImage(format!(
                "{width}x{height} RGB image needs {expected} bytes, got {}",
                data.len()
            )));
        }
        Ok(Self {
            width,
            height,
            data,
        })
    }

    pub fn filled(width: u32, height: u32, rgb: [u8; 3]) -> Self {
        let data = rgb
            .iter()
            .copied()
            .cycle()
            .take(width as usize * height as usize * 3)
            .collect();
        Self {
            width,
            height,
            data,
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }

    /// Panics if `(x, y)` is out of bounds.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 3] {
        let i = self.index(x, y);
        [self.data[i], self.data[i + 1], self.data[i + 2]]
    }

    /// Panics if `(x, y)` is out of bounds.
    pub fn put_pixel(&mut self, x: u32, y: u32, rgb: [u8; 3]) {
        let i = self.index(x, y);
        self.data[i..i + 3].copy_from_slice(&rgb);
    }

    /// Stable 64-bit FNV-1a hash of dimensions and pixels. Used for
    /// determinism checks and replay diffing, not for security.
    pub fn fingerprint(&self) -> u64 {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut hash = OFFSET;
        let dims = [self.width.to_le_bytes(), self.height.to_le_bytes()];
        for byte in dims.iter().flatten().chain(self.data.iter()) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
        hash
    }

    fn index(&self, x: u32, y: u32) -> usize {
        assert!(
            x < self.width && y < self.height,
            "pixel ({x}, {y}) outside {}x{}",
            self.width,
            self.height
        );
        (y as usize * self.width as usize + x as usize) * 3
    }
}

impl fmt::Debug for RgbImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RgbImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("fingerprint", &format_args!("{:016x}", self.fingerprint()))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_wrong_buffer_length() {
        assert!(RgbImage::from_raw(2, 2, vec![0; 11]).is_err());
        assert!(RgbImage::from_raw(2, 2, vec![0; 12]).is_ok());
    }

    #[test]
    fn fingerprint_depends_on_pixels_and_shape() {
        let a = RgbImage::filled(4, 2, [1, 2, 3]);
        let b = RgbImage::filled(2, 4, [1, 2, 3]);
        let mut c = a.clone();
        c.put_pixel(3, 1, [1, 2, 4]);
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_ne!(a.fingerprint(), c.fingerprint());
        assert_eq!(a.fingerprint(), a.clone().fingerprint());
    }
}
