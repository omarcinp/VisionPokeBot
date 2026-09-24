//! The mart screen: the MONEY window, the item list, the ▶ cursor and the
//! quantity box. The mart's item list shares its frame, colours and row
//! geometry with the field bag (see the probe note); colour alone can't
//! separate the two, so the gate below keys on the MONEY window's frame
//! (never shown in the bag) plus either the active list cursor (the list has
//! focus) or the quantity box's ▲/▼ arrows (the quantity box has focus).
//! The BUY/SELL/SEE YA! menu and the "OK?" YES/NO confirm are deliberately
//! excluded: their list cursor is the inactive ▷ and no quantity arrows show,
//! so they fall through to the existing menu-over-dialogue reading.

use pokebot_core::RgbImage;
use pokebot_state::{Region, ShopObservation};

use super::share;
use crate::color::Rgb;
use crate::text::Font;

/// MONEY (and, while it's open, the quantity box's IN BAG) window frame.
/// Never shown in the bag, whose windows use the orange frame instead.
const MONEY_FRAME: Rgb = [206, 211, 214];
const MONEY_FRAME_PROBE: Region = Region {
    x: 5,
    y: 4,
    width: 70,
    height: 1,
};
const MONEY_LABEL: Region = Region {
    x: 5,
    y: 5,
    width: 70,
    height: 15,
};
const MONEY_AMOUNT: Region = Region {
    x: 5,
    y: 20,
    width: 70,
    height: 15,
};

/// Item list: 6 visible rows, pitch 16, cell top at `8 + 16r` (same geometry
/// as the bag).
const ROWS: u32 = 6;
const ROW_TOP: u32 = 8;
const ROW_PITCH: u32 = 16;
const ROW_HEIGHT: u32 = 15;
const NAME_X: u32 = 96;
const NAME_WIDTH: u32 = 94;
const PRICE_X: u32 = 190;
const PRICE_WIDTH: u32 = 42;

/// List cursor column: the active ▶ (the inactive ▷, shown while the
/// quantity box or a dialogue has focus, does not match this gray).
const LIST_CURSOR_X: std::ops::Range<u32> = 88..96;
const LIST_CURSOR_Y0: u32 = 12;

/// Quantity box: one line, `×NN ¥NNNN`.
const QUANTITY_TEXT: Region = Region {
    x: 134,
    y: 80,
    width: 100,
    height: 16,
};
/// The ▲ arrow sits on the quantity box's top frame; present only while it's
/// open (not during the list or the confirm YES/NO).
const QUANTITY_ARROW: Rgb = [255, 81, 0];
const QUANTITY_ARROW_PROBE: Region = Region {
    x: 146,
    y: 67,
    width: 11,
    height: 6,
};

pub fn detect(image: &RgbImage, font: &Font, small_font: &Font) -> Option<ShopObservation> {
    if !has_money_frame(image) {
        return None;
    }
    let cursor = list_cursor(image);
    let quantity_box_open = has_quantity_arrow(image);
    if cursor.is_none() && !quantity_box_open {
        // Neither the list nor the quantity box has focus: the BUY/SELL/SEE
        // YA! menu or the "OK?" confirm is showing instead.
        return None;
    }
    let money = {
        let label = font.read(image, MONEY_LABEL, &[]).join(" ");
        if label == "MONEY" {
            parse_amount(&small_font.read(image, MONEY_AMOUNT, &[]).join(" "))
        } else {
            None
        }
    };
    let mut items = Vec::new();
    for r in 0..ROWS {
        let name_region = Region::new(NAME_X, ROW_TOP + ROW_PITCH * r, NAME_WIDTH, ROW_HEIGHT);
        let name = font.read(image, name_region, &[]).join(" ");
        if name.is_empty() {
            break;
        }
        let price_region = Region::new(PRICE_X, ROW_TOP + ROW_PITCH * r, PRICE_WIDTH, ROW_HEIGHT);
        let price_text = small_font.read(image, price_region, &[]).join(" ");
        items.push((name, parse_amount(&price_text)));
    }
    let quantity = quantity_box_open
        .then(|| small_font.read(image, QUANTITY_TEXT, &[]).join(" "))
        .and_then(|text| parse_quantity(&text));
    Some(ShopObservation {
        money,
        items,
        cursor,
        quantity,
    })
}

/// The MONEY window's distinctive frame colour, at the top border strip.
/// Present on every mart screen except the BUY/SELL/SEE YA! menu; absent
/// from the bag (orange frame instead).
fn has_money_frame(image: &RgbImage) -> bool {
    share(image, MONEY_FRAME_PROBE, MONEY_FRAME, 2) >= 900
}

/// The quantity box's ▲ arrow, present only while it's open.
fn has_quantity_arrow(image: &RgbImage) -> bool {
    share(image, QUANTITY_ARROW_PROBE, QUANTITY_ARROW, 1) >= 200
}

/// The active list ▶, turned into a row index. `None` while the list lacks
/// focus (the inactive ▷ doesn't match, by colour).
fn list_cursor(image: &RgbImage) -> Option<u8> {
    let (x, y) = super::menu::find_cursor(image)?;
    if !LIST_CURSOR_X.contains(&x) || y < LIST_CURSOR_Y0 {
        return None;
    }
    let row = (y - LIST_CURSOR_Y0) / ROW_PITCH;
    (row < ROWS).then_some(row as u8)
}

/// Digits, mapping the small font's shared `0`/`O` bitmap back to `0` and
/// dropping `¥`/`,` (see the probe note's digit caveat). `None` when no
/// digit is found.
fn parse_amount<T: std::str::FromStr>(text: &str) -> Option<T> {
    let digits: String = text
        .chars()
        .map(|c| if c == 'O' { '0' } else { c })
        .filter(char::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// `×NN ¥NNNN` → (count, total price).
fn parse_quantity(text: &str) -> Option<(u16, u32)> {
    let mut parts = text.split_whitespace();
    let count = parse_amount(parts.next()?)?;
    let total = parse_amount(parts.next()?)?;
    Some((count, total))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn fixture(name: &str) -> Option<(RgbImage, Font, Font)> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let image = pokebot_video::png::load(root.join("captures/fixtures").join(name)).ok()?;
        let font = Font::load(root.join("data/world/font_normal.json")).ok()?;
        let small_font = Font::load(root.join("data/world/font_small.json")).ok()?;
        Some((image, font, small_font))
    }

    #[test]
    fn reads_the_item_list_and_money() {
        let Some((image, font, small_font)) = fixture("mart-list.png") else {
            return;
        };
        let shop = detect(&image, &font, &small_font).expect("shop");
        assert_eq!(shop.money, Some(4880));
        assert_eq!(
            shop.items.first(),
            Some(&("POKé BALL".to_owned(), Some(200)))
        );
        assert_eq!(shop.cursor, Some(0));
        assert_eq!(shop.quantity, None);
    }

    #[test]
    fn reads_the_quantity_box() {
        let Some((image, font, small_font)) = fixture("mart-quantity-1.png") else {
            return;
        };
        let shop = detect(&image, &font, &small_font).expect("shop");
        assert_eq!(shop.quantity, Some((1, 200)));

        let Some((image, font, small_font)) = fixture("mart-quantity-3.png") else {
            return;
        };
        let shop = detect(&image, &font, &small_font).expect("shop");
        assert_eq!(shop.quantity, Some((3, 600)));
    }

    #[test]
    fn other_screens_are_not_shops() {
        for name in [
            "bag-pokeballs.png",
            "bag-items.png",
            "bag-pokeballs-cursor1.png",
            "bag-use-prompt.png",
            "move-select.png",
            "mart-menu.png",
            "mart-confirm.png",
        ] {
            let Some((image, font, small_font)) = fixture(name) else {
                continue;
            };
            assert!(detect(&image, &font, &small_font).is_none(), "{name}");
        }
    }
}
