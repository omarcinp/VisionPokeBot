//! Shiny check: the opponent's front sprite matched against its species'
//! normal and shiny palettes (16 colours each, mGBA output, index 0
//! transparent). The rule counts pixels that match only one of the two
//! palettes and asks for a clear majority; anything else is `Unclear`.

use std::collections::BTreeMap;

use pokebot_core::RgbImage;
use pokebot_state::{Region, ShinyReading};

use crate::color::{near, Rgb};

/// Species display name (`RATTATA`, as the HUD prints it) → (normal, shiny)
/// palettes.
pub type SpritePalettes = BTreeMap<String, ([Rgb; 16], [Rgb; 16])>;

/// Per-channel tolerance for matching a sprite pixel to a palette colour.
/// Tight on purpose: normal and shiny palettes can differ by a few levels
/// per colour. Switch captures come within about 4 levels of mGBA after
/// calibration; the tests cover ±6 noise.
pub const PALETTE_TOLERANCE: u8 = 8;
/// Pixels that must match only the winning palette.
pub const MIN_PIXELS: u32 = 40;
/// The winning palette must match this many times more pixels than the other.
pub const DOMINANCE: u32 = 4;

/// Classifies the opaque pixels of `region` (colours not equal to the
/// background sampled at the region's corners; "equal" within
/// [`PALETTE_TOLERANCE`], since capture cards blur exact values).
///
/// A pixel counts for a palette when it is within [`PALETTE_TOLERANCE`] of
/// one of that palette's colours (index 0, transparent, excluded) and of
/// none of the other's; colours both palettes share count for neither.
/// `Shiny` iff `s >= MIN_PIXELS && s >= DOMINANCE * n`, `Normal` likewise,
/// else `Unclear`.
pub fn classify(
    image: &RgbImage,
    region: Region,
    normal: &[Rgb; 16],
    shiny: &[Rgb; 16],
) -> ShinyReading {
    let (x1, y1) = (
        (region.x + region.width).min(image.width()),
        (region.y + region.height).min(image.height()),
    );
    if region.x >= x1 || region.y >= y1 {
        return ShinyReading::Unclear;
    }
    let background = [
        image.pixel(region.x, region.y),
        image.pixel(x1 - 1, region.y),
        image.pixel(region.x, y1 - 1),
        image.pixel(x1 - 1, y1 - 1),
    ];
    let matches =
        |palette: &[Rgb; 16], p: Rgb| palette[1..].iter().any(|c| near(p, *c, PALETTE_TOLERANCE));
    let (mut n, mut s) = (0u32, 0u32);
    for y in region.y..y1 {
        for x in region.x..x1 {
            let p = image.pixel(x, y);
            if background.iter().any(|b| near(p, *b, PALETTE_TOLERANCE)) {
                continue;
            }
            match (matches(normal, p), matches(shiny, p)) {
                (true, false) => n += 1,
                (false, true) => s += 1,
                _ => {}
            }
        }
    }
    if s >= MIN_PIXELS && s >= DOMINANCE * n {
        ShinyReading::Shiny
    } else if n >= MIN_PIXELS && n >= DOMINANCE * s {
        ShinyReading::Normal
    } else {
        ShinyReading::Unclear
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const BACKGROUND: Rgb = [231, 251, 231];
    const REGION: Region = Region {
        x: 144,
        y: 8,
        width: 64,
        height: 64,
    };

    fn zubat() -> Option<([Rgb; 16], [Rgb; 16])> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let data = pokebot_gamedata::GameData::load(root.join("data/world/gamedata.json")).ok()?;
        let p = data.species.get("SPECIES_ZUBAT")?.palettes.clone()?;
        Some((p.normal.try_into().ok()?, p.shiny.try_into().ok()?))
    }

    /// A 64×64 sprite in `REGION`: a pattern of palette indices 1..15 with a
    /// transparent 4-px margin (so the corners show the background). Columns
    /// left of `split` use `left`, the rest `right`.
    fn render(left: &[Rgb; 16], right: &[Rgb; 16], split: u32, noise: u8) -> RgbImage {
        let mut image = RgbImage::filled(240, 160, BACKGROUND);
        let mut seed: u32 = 0x1234_5678;
        for dy in 4..60 {
            for dx in 4..60 {
                let index = 1 + ((dx * 7 + dy * 3) % 15) as usize;
                let palette = if dx < split { left } else { right };
                let mut c = palette[index];
                if noise > 0 {
                    for ch in &mut c {
                        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        let span = u32::from(noise) * 2 + 1;
                        let delta = (seed >> 16) % span;
                        *ch =
                            (i32::from(*ch) + delta as i32 - i32::from(noise)).clamp(0, 255) as u8;
                    }
                }
                image.put_pixel(REGION.x + dx, REGION.y + dy, c);
            }
        }
        image
    }

    #[test]
    fn normal_palette_reads_normal() {
        let Some((normal, shiny)) = zubat() else {
            return;
        };
        let image = render(&normal, &normal, 64, 0);
        assert_eq!(
            classify(&image, REGION, &normal, &shiny),
            ShinyReading::Normal
        );
    }

    #[test]
    fn shiny_palette_reads_shiny() {
        let Some((normal, shiny)) = zubat() else {
            return;
        };
        let image = render(&shiny, &shiny, 64, 0);
        assert_eq!(
            classify(&image, REGION, &normal, &shiny),
            ShinyReading::Shiny
        );
    }

    #[test]
    fn half_and_half_is_unclear() {
        let Some((normal, shiny)) = zubat() else {
            return;
        };
        let image = render(&normal, &shiny, 32, 0);
        assert_eq!(
            classify(&image, REGION, &normal, &shiny),
            ShinyReading::Unclear
        );
    }

    #[test]
    fn background_only_is_unclear() {
        let Some((normal, shiny)) = zubat() else {
            return;
        };
        let image = RgbImage::filled(240, 160, BACKGROUND);
        assert_eq!(
            classify(&image, REGION, &normal, &shiny),
            ShinyReading::Unclear
        );
    }

    #[test]
    fn capture_noise_keeps_the_reading() {
        // Capture-card colours land within a few levels of mGBA; ±6 per
        // channel is a margin over the ~4 measured on the Switch.
        let Some((normal, shiny)) = zubat() else {
            return;
        };
        let image = render(&normal, &normal, 64, 6);
        assert_eq!(
            classify(&image, REGION, &normal, &shiny),
            ShinyReading::Normal
        );
        let image = render(&shiny, &shiny, 64, 6);
        assert_eq!(
            classify(&image, REGION, &normal, &shiny),
            ShinyReading::Shiny
        );
    }
}
