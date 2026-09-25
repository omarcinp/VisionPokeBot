//! The white-out screens after the party's last Pokémon fainted: white
//! text on a black screen without a box ("RED scurried back home,
//! protecting the exhausted and fainted POKéMON from further harm…"), a
//! red ▼ once the page is printed. Fixture
//! `captures/fixtures/switch-whiteout-scurried-home.png` (Switch, Route 1).

use pokebot_core::RgbImage;
use pokebot_state::Region;

use crate::color::luma;

/// Rows the text occupies (the fixture's text sits in rows 51–86).
const TEXT_BAND: Region = Region {
    x: 0,
    y: 40,
    width: 240,
    height: 64,
};
/// Share (per mille) of near-black pixels a white-out screen has at least.
const DARK_MIN: u32 = 850;
/// Share (per mille) of bright pixels: text, not a fade (the fixture has 25).
const BRIGHT_MIN: u32 = 8;
const BRIGHT_MAX: u32 = 120;
const DARK_LUMA: u8 = 48;
const BRIGHT_LUMA: u8 = 200;

/// Whether `image` is a white-out screen: almost all black, a little
/// bright text, all of it inside the text band.
pub fn is_whiteout(image: &RgbImage) -> bool {
    let (w, h) = (image.width(), image.height());
    let total = w * h;
    let (mut dark, mut bright, mut bright_outside) = (0u32, 0u32, 0u32);
    for y in 0..h {
        for x in 0..w {
            let l = luma(image.pixel(x, y));
            if l <= DARK_LUMA {
                dark += 1;
            } else if l >= BRIGHT_LUMA {
                bright += 1;
                if !TEXT_BAND.contains(x, y) {
                    bright_outside += 1;
                }
            }
        }
    }
    dark * 1000 >= total * DARK_MIN
        && bright * 1000 >= total * BRIGHT_MIN
        && bright * 1000 <= total * BRIGHT_MAX
        && bright_outside * 50 <= bright
}
