//! PNG load/save for [`RgbImage`] (fixtures, screenshots, replay frames).

use std::path::Path;

use pokebot_core::{Error, Result, RgbImage};

pub fn load(path: impl AsRef<Path>) -> Result<RgbImage> {
    let path = path.as_ref();
    let decoded = image::open(path)
        .map_err(|e| Error::InvalidImage(format!("{}: {e}", path.display())))?
        .into_rgb8();
    let (width, height) = decoded.dimensions();
    RgbImage::from_raw(width, height, decoded.into_raw())
}

pub fn save(image: &RgbImage, path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    image::save_buffer(
        path,
        image.as_bytes(),
        image.width(),
        image.height(),
        image::ExtendedColorType::Rgb8,
    )
    .map_err(|e| Error::InvalidImage(format!("{}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_disk() {
        let mut img = RgbImage::filled(3, 2, [10, 20, 30]);
        img.put_pixel(2, 1, [255, 0, 128]);
        let path = std::env::temp_dir().join(format!("pokebot-png-{}.png", std::process::id()));
        save(&img, &path).unwrap();
        let loaded = load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(loaded, img);
    }
}
