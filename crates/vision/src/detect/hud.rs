//! Battle HUD text: Pokémon names, levels and our HP numbers.
//!
//! The HUD font is a fixed 7-pixel-tall bitmap font in dark gray: the
//! small font's ink rows (`data/world/font_small.json`, rows 4–10) without
//! its shadow. Glyph shapes were measured from recorded battles and the
//! missing letters taken from that font; characters not in the table read as `?`, and names are resolved
//! against a caller-supplied dictionary, so partial knowledge still works.

use pokebot_core::RgbImage;

use super::px;
use crate::color::{near, Rgb, TOLERANCE};

const INK: Rgb = [66, 65, 66];
const ROWS: u32 = 7;

/// Glyph shapes, rows top to bottom (`#` = ink).
const GLYPHS: &[(char, [&str; 7])] = &[
    (
        '0',
        [".##.", "#..#", "#..#", "#..#", "#..#", "#..#", ".##."],
    ),
    ('1', [".#.", "##.", ".#.", ".#.", ".#.", ".#.", "###"]),
    (
        '2',
        [".##.", "#..#", "...#", "..#.", ".#..", "#...", "####"],
    ),
    (
        '3',
        [".##.", "#..#", "...#", ".##.", "...#", "#..#", ".##."],
    ),
    (
        '4',
        [".##.", "#.#.", "#.#.", "#.#.", "#.#.", "####", "..#."],
    ),
    (
        '5',
        ["####", "#...", "#...", "###.", "...#", "#..#", ".##."],
    ),
    (
        '6',
        [".##.", "#..#", "#...", "###.", "#..#", "#..#", ".##."],
    ),
    (
        '7',
        ["####", "...#", "...#", "..#.", "..#.", ".#..", ".#.."],
    ),
    (
        '8',
        [".##.", "#..#", "#..#", ".##.", "#..#", "#..#", ".##."],
    ),
    // Not yet seen on screen; drawn in the same style as 6 (mirrored).
    (
        '9',
        [".##.", "#..#", "#..#", ".###", "...#", "#..#", ".##."],
    ),
    (
        '/',
        ["...#", "..#.", "..#.", ".#..", ".#..", "#...", "#..."],
    ),
    ('v', ["...", "...", "...", "#.#", "#.#", "#.#", ".#."]),
    (
        'A',
        [".##.", "#..#", "#..#", "####", "#..#", "#..#", "#..#"],
    ),
    (
        'B',
        ["###.", "#..#", "#..#", "###.", "#..#", "#..#", "###."],
    ),
    (
        'C',
        [".##.", "#..#", "#...", "#...", "#...", "#..#", ".##."],
    ),
    (
        'D',
        ["###.", "#..#", "#..#", "#..#", "#..#", "#..#", "###."],
    ),
    (
        'E',
        ["####", "#...", "#...", "###.", "#...", "#...", "####"],
    ),
    (
        'F',
        ["####", "#...", "#...", "###.", "#...", "#...", "#..."],
    ),
    (
        'G',
        [".##.", "#..#", "#...", "#.##", "#..#", "#..#", ".##."],
    ),
    (
        'H',
        ["#..#", "#..#", "#..#", "####", "#..#", "#..#", "#..#"],
    ),
    ('I', ["###", ".#.", ".#.", ".#.", ".#.", ".#.", "###"]),
    (
        'J',
        ["...#", "...#", "...#", "...#", "#..#", "#..#", ".##."],
    ),
    (
        'K',
        ["#..#", "#..#", "#.#.", "##..", "#.#.", "#..#", "#..#"],
    ),
    (
        'L',
        ["#...", "#...", "#...", "#...", "#...", "#...", "####"],
    ),
    // The L of "Lv" is narrower than the letter L in names.
    ('L', ["#..", "#..", "#..", "#..", "#..", "#..", "###"]),
    (
        'M',
        ["#..#", "####", "#..#", "#..#", "#..#", "#..#", "#..#"],
    ),
    (
        'N',
        ["#..#", "##.#", "##.#", "#.##", "#.##", "#..#", "#..#"],
    ),
    (
        'P',
        ["###.", "#..#", "#..#", "###.", "#...", "#...", "#..."],
    ),
    (
        'R',
        ["###.", "#..#", "#..#", "###.", "#..#", "#..#", "#..#"],
    ),
    (
        'S',
        [".##.", "#..#", "#...", ".##.", "...#", "#..#", ".##."],
    ),
    ('T', ["###", ".#.", ".#.", ".#.", ".#.", ".#.", ".#."]),
    (
        'U',
        ["#..#", "#..#", "#..#", "#..#", "#..#", "#..#", ".##."],
    ),
    (
        'V',
        ["#..#", "#..#", "#..#", "#..#", "#..#", "#.#.", ".#.."],
    ),
    (
        'W',
        ["#..#", "#..#", "#..#", "#..#", "#..#", "####", "#..#"],
    ),
    (
        'X',
        ["#..#", "#..#", "#..#", ".##.", "#..#", "#..#", "#..#"],
    ),
    ('Y', ["#.#", "#.#", "#.#", "#.#", ".#.", ".#.", ".#."]),
    (
        'Z',
        ["####", "...#", "..#.", "..#.", ".#..", ".#..", "####"],
    ),
];

fn ink(image: &RgbImage, x: u32, y: u32) -> bool {
    x < image.width() && y < image.height() && near(px(image, x, y), INK, TOLERANCE)
}

/// First row in `y0..y1` with ink between `x0..x1` (the text's top).
/// A row needs `min` ink pixels: stray ink-coloured pixels (sprite edges,
/// compression noise) don't start a line.
fn text_top(image: &RgbImage, y0: u32, y1: u32, x0: u32, x1: u32, min: usize) -> Option<u32> {
    (y0..y1).find(|&y| (x0..x1).filter(|&x| ink(image, x, y)).count() >= min)
}

/// Reads one line of HUD text starting at row `top`.
pub fn read_line(image: &RgbImage, top: u32, x0: u32, x1: u32) -> String {
    let column = |x: u32| (0..ROWS).any(|r| ink(image, x, top + r));
    let mut out = String::new();
    let mut x = x0;
    let mut last_end: Option<u32> = None;
    while x < x1 {
        if !column(x) {
            x += 1;
            continue;
        }
        let start = x;
        while x < x1 && column(x) {
            x += 1;
        }
        if last_end.is_some_and(|e| start - e >= 4) {
            out.push(' ');
        }
        last_end = Some(x);
        let width = (x - start) as usize;
        // Pixels that differ from each glyph of this width; an exact match,
        // else a unique glyph one pixel off (compressed captures).
        let mut scored: Vec<(usize, char)> = GLYPHS
            .iter()
            .filter(|(_, rows)| rows[0].len() == width)
            .map(|(c, rows)| {
                let off = rows
                    .iter()
                    .enumerate()
                    .flat_map(|(r, row)| row.chars().enumerate().map(move |(x, v)| (r, x, v)))
                    .filter(|&(r, x, v)| (v == '#') != ink(image, start + x as u32, top + r as u32))
                    .count();
                (off, *c)
            })
            .collect();
        scored.sort();
        let glyph = match scored.as_slice() {
            [(0, c), ..] => Some(*c),
            [(1, c)] => Some(*c),
            [(1, c), (next, _), ..] if *next > 1 => Some(*c),
            _ => None,
        };
        out.push(glyph.unwrap_or('?'));
    }
    out
}

/// Name and level from a HUD name line such as `BULBASAUR Lv6`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HudLine {
    /// Name as read (`?` for unknown letters).
    pub name: String,
    pub level: Option<u8>,
}

fn parse_name_line(text: &str) -> HudLine {
    let (name, level) = match text.rfind("Lv") {
        Some(i) => (&text[..i], text[i + 2..].trim().parse().ok()),
        None => (text, None),
    };
    // The gender sign is coloured, not ink; any stray `?` at the end is it.
    // O and 0 are the same glyph; names have no digits.
    HudLine {
        name: name.trim().trim_end_matches('?').trim().replace('0', "O"),
        level,
    }
}

/// Our Pokémon's name line (the box bobs a pixel, so the top is searched).
pub fn player_line(image: &RgbImage) -> Option<HudLine> {
    let top = text_top(image, 76, 83, 138, 228, 6)?;
    Some(parse_name_line(&read_line(image, top, 138, 228)))
}

pub fn opponent_line(image: &RgbImage) -> Option<HudLine> {
    let top = text_top(image, 18, 25, 16, 112, 6)?;
    Some(parse_name_line(&read_line(image, top, 16, 112)))
}

/// Our current and maximum HP (`20/ 20`).
pub fn player_hp(image: &RgbImage) -> Option<(u16, u16)> {
    // The slash is a pixel taller than the digits: try both alignments.
    let top = text_top(image, 94, 101, 184, 226, 3)?;
    (top..top + 2).find_map(|t| {
        let text: String = read_line(image, t, 184, 226)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        // Specks at the box edge read as unknown glyphs around the numbers.
        let (cur, max) = text.trim_matches('?').split_once('/')?;
        Some((cur.parse().ok()?, max.parse().ok()?))
    })
}

/// The dictionary entry matching a read name: `?` matches any letter, and
/// the length must match (unique match only).
pub fn resolve<'a>(read: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let read: Vec<char> = read.chars().collect();
    let mut found = None;
    for candidate in candidates {
        let c: Vec<char> = candidate.chars().collect();
        if c.len() == read.len() && c.iter().zip(&read).all(|(a, b)| *b == '?' || a == b) {
            if found.is_some() {
                return None;
            }
            found = Some(candidate);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hud_lines() {
        assert_eq!(
            parse_name_line("BULBASAUR? Lv6"),
            HudLine {
                name: "BULBASAUR".into(),
                level: Some(6)
            }
        );
        assert_eq!(parse_name_line("PIDGEY? Lv12").level, Some(12));
    }

    #[test]
    fn resolves_partial_names() {
        let names = ["PIDGEY", "RATTATA", "SPEAROW", "MANKEY"];
        assert_eq!(resolve("SPEAR??", names), Some("SPEAROW"));
        assert_eq!(resolve("MA?KEY", names), Some("MANKEY"));
        assert_eq!(resolve("??????", ["PIDGEY", "MANKEY"]), None);
    }
}
