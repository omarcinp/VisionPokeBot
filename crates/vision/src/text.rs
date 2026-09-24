//! Reading the game's text font (dialogue, battle messages, menus).
//!
//! The glyph bitmaps and advance widths come from the decompilation
//! (`tools/gamedata/extract_font.py` → `data/world/font_normal.json`, built
//! locally). Text colour varies by speaker and window, so the reader tries
//! each prominent non-background colour as ink and keeps the reading that
//! explains the most ink. Matching is exact on the ink mask, except that a
//! glyph one pixel off (compression noise on a capture) is accepted when no
//! glyph matches exactly and no other is as close; columns no glyph explains
//! read as `?`, which dictionary resolution treats as a wildcard.

use std::collections::HashMap;
use std::path::Path;

use pokebot_core::{Error, Result, RgbImage};
use pokebot_state::Region;
use serde::Deserialize;

use crate::color::{near, Rgb};

/// Rows in a glyph cell.
const CELL: u32 = 16;
/// Blank columns between glyphs that read as a word break (a space is 6 px;
/// glyphs whose ink starts one column in shorten the visible gap).
const SPACE_GAP: u32 = 4;
/// Colour candidates tried as ink, most common first.
const MAX_INKS: usize = 4;
/// Colours closer than this (per channel) are one palette colour.
const CLUSTER_SPREAD: u8 = 40;
/// Pixels a colour needs to be considered as ink.
const MIN_INK_PIXELS: u32 = 4;
/// Ink a glyph needs before a one-pixel-off match is accepted: one pixel is
/// then at most an eighth of it (not a comma turning into a full stop).
const NEAR_MATCH_MIN_INK: u32 = 8;

#[derive(Debug, Clone)]
pub struct Glyph {
    pub text: String,
    pub width: u32,
    /// Ink per column, bit `r` = row `r` of the cell.
    columns: Vec<u16>,
    /// Shadow per column (for rendering test images).
    shadow: Vec<u16>,
    ink: u32,
}

/// One text font: glyphs indexed by their first ink column.
#[derive(Debug, Clone)]
pub struct Font {
    glyphs: Vec<Glyph>,
    by_first_column: HashMap<u16, Vec<usize>>,
}

#[derive(Deserialize)]
struct FontFile {
    glyphs: Vec<GlyphFile>,
}

#[derive(Deserialize)]
struct GlyphFile {
    text: String,
    width: u32,
    rows: Vec<String>,
}

/// Whether `text` is a single ASCII digit (used to break bitmap ties in
/// favour of letters; see `Font::from_json`).
fn is_digit(text: &str) -> bool {
    matches!(text.as_bytes(), [b] if b.is_ascii_digit())
}

impl Font {
    pub fn load(path: impl AsRef<Path>) -> Result<Font> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        Font::from_json(&text).map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))
    }

    pub fn from_json(text: &str) -> std::result::Result<Font, String> {
        let file: FontFile = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let mut glyphs: Vec<Glyph> = Vec::new();
        for g in file.glyphs {
            if g.rows.len() != CELL as usize || g.width == 0 || g.width > 16 {
                return Err(format!("glyph {:?}: bad size", g.text));
            }
            let column = |c: char, x: usize| {
                g.rows.iter().enumerate().fold(0u16, |bits, (r, row)| {
                    if row.chars().nth(x) == Some(c) {
                        bits | 1 << r
                    } else {
                        bits
                    }
                })
            };
            let columns: Vec<u16> = (0..g.width as usize).map(|x| column('#', x)).collect();
            let shadow = (0..g.width as usize).map(|x| column('s', x)).collect();
            let ink = columns.iter().map(|c| c.count_ones()).sum();
            if ink == 0 {
                continue;
            }
            // Duplicate bitmap (the game reuses one glyph for two codes,
            // e.g. `0`/`O` in the small font): the lowest character code
            // wins, unless it's a digit standing in for a letter — digits
            // are rare in menu text, so keep the letter reading.
            if let Some(existing) = glyphs
                .iter_mut()
                .find(|o| o.columns == columns && o.width == g.width)
            {
                if is_digit(&existing.text) && !is_digit(&g.text) {
                    existing.text = g.text;
                }
            } else {
                glyphs.push(Glyph {
                    text: g.text,
                    width: g.width,
                    columns,
                    shadow,
                    ink,
                });
            }
        }
        let mut by_first_column: HashMap<u16, Vec<usize>> = HashMap::new();
        for (i, g) in glyphs.iter().enumerate() {
            by_first_column.entry(g.columns[0]).or_default().push(i);
        }
        Ok(Font {
            glyphs,
            by_first_column,
        })
    }

    pub fn glyph(&self, text: &str) -> Option<&Glyph> {
        self.glyphs.iter().find(|g| g.text == text)
    }

    /// Reads the text lines inside `region` (a text window's interior),
    /// ignoring pixels in `exclude` (the ▼ arrow, cursors).
    pub fn read(&self, image: &RgbImage, region: Region, exclude: &[Region]) -> Vec<String> {
        let mut best: Option<(i64, Vec<String>)> = None;
        let palette = colour_clusters(image, region, exclude);
        for &ink in palette.iter().skip(1).take(MAX_INKS) {
            let mask = Mask::new(image, region, exclude, ink, &palette);
            let (score, lines) = self.read_mask(&mask);
            if best.as_ref().is_none_or(|(s, _)| score > *s) {
                best = Some((score, lines));
            }
        }
        best.map(|(_, lines)| lines).unwrap_or_default()
    }

    fn read_mask(&self, mask: &Mask) -> (i64, Vec<String>) {
        let mut score = 0;
        let mut lines = Vec::new();
        for (top, bottom) in mask.bands() {
            // Glyph ink starts between cell rows 0 and 8 (accents … lower case),
            // so try every cell top that keeps the band inside the cell. Specks
            // above the text (a window corner smeared into the ink colour on
            // a capture) can make the band start early: tops below the band's
            // top row are tried too, down to where the band bottom is a
            // glyph's baseline.
            let lowest = bottom.saturating_sub(CELL - 1);
            let mut line_best: Option<(i64, String)> = None;
            for y in lowest.max(top.saturating_sub(8))..=bottom.saturating_sub(8).max(top) {
                let (s, text) = self.read_line(mask, i64::from(y));
                if line_best.as_ref().is_none_or(|(b, _)| s > *b) {
                    line_best = Some((s, text));
                }
            }
            // Lines that glyphs explain worse than they leave unexplained are
            // window frames or graphics, not text.
            if let Some((s, text)) = line_best.filter(|(s, _)| *s > 0) {
                score += s;
                lines.push(text);
            }
        }
        (score, lines)
    }

    /// Reads one line whose glyph cells start at mask row `y`. Score: ink
    /// explained by glyphs minus twice the ink left unexplained.
    fn read_line(&self, mask: &Mask, y: i64) -> (i64, String) {
        let columns: Vec<u16> = (0..mask.width).map(|x| mask.column(x, y)).collect();
        let mut text = String::new();
        let mut score: i64 = 0;
        let mut gap = 0u32;
        let mut unknown = false;
        let mut x = 0usize;
        while x < columns.len() {
            let found = self
                .match_at(&columns[x..])
                .map(|g| (g, 0))
                .or_else(|| self.near_match_at(&columns[x..]).map(|g| (g, 1)));
            match found {
                Some((g, off)) => {
                    if !text.is_empty() && gap >= SPACE_GAP {
                        text.push(' ');
                    }
                    text.push_str(&g.text);
                    // The off pixel is ink left unexplained (or explained
                    // wrongly): it costs what unexplained ink costs.
                    score += i64::from(g.ink) - 2 * off;
                    x += g.width as usize;
                    gap = 0;
                    unknown = false;
                }
                None if columns[x] == 0 => {
                    gap += 1;
                    x += 1;
                    unknown = false;
                }
                None => {
                    if !unknown {
                        if !text.is_empty() && gap >= SPACE_GAP {
                            text.push(' ');
                        }
                        text.push('?');
                    }
                    score -= 2 * i64::from(columns[x].count_ones());
                    unknown = true;
                    gap = 0;
                    x += 1;
                }
            }
        }
        (score, text)
    }

    /// The glyph whose columns match exactly at the start of `columns`: most
    /// ink, then widest, then lowest character code.
    fn match_at(&self, columns: &[u16]) -> Option<&Glyph> {
        if columns[0] == 0 {
            // Only glyphs with a blank first column can start here; they
            // must still put ink somewhere (spaces are gaps, not glyphs).
            if columns.iter().take(3).all(|c| *c == 0) {
                return None;
            }
        }
        let candidates = self.by_first_column.get(&columns[0])?;
        let mut best: Option<&Glyph> = None;
        for &i in candidates {
            let g = &self.glyphs[i];
            let w = g.width as usize;
            let fits = if w <= columns.len() {
                columns[..w] == g.columns[..]
            } else {
                // Clipped at the window edge: the visible part must match
                // and the rest must be blank.
                columns[..] == g.columns[..columns.len()]
                    && g.columns[columns.len()..].iter().all(|c| *c == 0)
            };
            if fits && best.is_none_or(|b| (g.ink, g.width) > (b.ink, b.width)) {
                best = Some(g);
            }
        }
        best
    }

    /// The glyph exactly one pixel off at the start of `columns` (a flipped
    /// pixel from compression), if it is the only one that close and big
    /// enough for one pixel not to change what it is. Only tried where no
    /// glyph matches exactly, and never on a blank column.
    fn near_match_at(&self, columns: &[u16]) -> Option<&Glyph> {
        if columns[0] == 0 {
            return None;
        }
        let mut found: Option<&Glyph> = None;
        for g in &self.glyphs {
            let w = g.width as usize;
            if w > columns.len() {
                continue;
            }
            let off: u32 = columns[..w]
                .iter()
                .zip(&g.columns)
                .map(|(a, b)| (a ^ b).count_ones())
                .sum();
            if off != 1 {
                continue;
            }
            if g.ink < NEAR_MATCH_MIN_INK || found.is_some() {
                return None;
            }
            found = Some(g);
        }
        found
    }

    /// Draws `text` with its glyph cell's top-left at (x, y) (test images).
    pub fn render(&self, image: &mut RgbImage, x: u32, y: u32, text: &str, ink: Rgb, shadow: Rgb) {
        let mut cx = x;
        for ch in text.chars() {
            let key = ch.to_string();
            if ch == ' ' {
                cx += 6;
                continue;
            }
            let Some(g) = self.glyph(&key) else { continue };
            for (col, (bits, sbits)) in g.columns.iter().zip(&g.shadow).enumerate() {
                for row in 0..CELL {
                    let (px, py) = (cx + col as u32, y + row);
                    if bits & (1 << row) != 0 {
                        image.put_pixel(px, py, ink);
                    } else if sbits & (1 << row) != 0 {
                        image.put_pixel(px, py, shadow);
                    }
                }
            }
            cx += g.width;
        }
    }
}

/// Non-background colours in `region`, most common first (clustered within
/// the capture tolerance).
/// The region's colour clusters, most common first (the first is the
/// background): the candidates for ink, and the palette pixels are assigned
/// to.
fn colour_clusters(image: &RgbImage, region: Region, exclude: &[Region]) -> Vec<Rgb> {
    let mut counts: HashMap<Rgb, u32> = HashMap::new();
    for y in region.y..region.y + region.height {
        for x in region.x..region.x + region.width {
            if !exclude.iter().any(|r| r.contains(x, y)) {
                *counts.entry(image.pixel(x, y)).or_default() += 1;
            }
        }
    }
    let mut colours: Vec<(Rgb, u32)> = counts.into_iter().collect();
    colours.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    // Text, shadow and background colours are far apart (48+ per channel);
    // compression spreads each over a few dozen levels.
    let mut clusters: Vec<(Rgb, u32)> = Vec::new();
    for (c, n) in colours {
        match clusters
            .iter_mut()
            .find(|(k, _)| near(*k, c, CLUSTER_SPREAD))
        {
            Some(cluster) => cluster.1 += n,
            None => clusters.push((c, n)),
        }
    }
    clusters.sort_by_key(|a| std::cmp::Reverse(a.1));
    clusters
        .into_iter()
        .enumerate()
        .filter(|(i, (_, n))| *i == 0 || *n >= MIN_INK_PIXELS)
        .map(|(_, (c, _))| c)
        .collect()
}

fn distance(a: Rgb, b: Rgb) -> u32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| u32::from(x.abs_diff(y)).pow(2))
        .sum()
}

/// Whether `pixel` belongs to `ink`: that is the nearest palette colour
/// (compressed captures smear colours past a fixed tolerance, but stay
/// nearest to their own) and not implausibly far from it.
fn is_ink(pixel: Rgb, ink: Rgb, palette: &[Rgb]) -> bool {
    let d = distance(pixel, ink);
    d <= 3 * 64 * 64 && palette.iter().all(|&c| c == ink || distance(pixel, c) > d)
}

/// Ink pixels of one colour inside a region.
struct Mask {
    width: usize,
    height: usize,
    bits: Vec<bool>,
}

impl Mask {
    fn new(
        image: &RgbImage,
        region: Region,
        exclude: &[Region],
        ink: Rgb,
        palette: &[Rgb],
    ) -> Mask {
        let (width, height) = (region.width as usize, region.height as usize);
        let mut bits = vec![false; width * height];
        for dy in 0..region.height {
            for dx in 0..region.width {
                let (x, y) = (region.x + dx, region.y + dy);
                bits[dy as usize * width + dx as usize] = !exclude.iter().any(|r| r.contains(x, y))
                    && is_ink(image.pixel(x, y), ink, palette);
            }
        }
        Mask {
            width,
            height,
            bits,
        }
    }

    fn at(&self, x: usize, y: i64) -> bool {
        y >= 0 && (y as usize) < self.height && self.bits[y as usize * self.width + x]
    }

    /// Cell column at `x` for a cell starting at row `y`.
    fn column(&self, x: usize, y: i64) -> u16 {
        (0..CELL as i64).fold(0, |bits, r| {
            if self.at(x, y + r) {
                bits | 1 << r
            } else {
                bits
            }
        })
    }

    /// Row ranges (inclusive) containing ink, merging gaps of up to 2 rows
    /// (the dot of an i, the colon) into one text line.
    fn bands(&self) -> Vec<(u32, u32)> {
        let inked: Vec<bool> = (0..self.height)
            .map(|y| (0..self.width).any(|x| self.bits[y * self.width + x]))
            .collect();
        let mut bands: Vec<(u32, u32)> = Vec::new();
        for (y, _) in inked.iter().enumerate().filter(|(_, i)| **i) {
            let y = y as u32;
            match bands.last_mut() {
                Some(last) if y <= last.1 + 3 => last.1 = y,
                _ => bands.push((y, y)),
            }
        }
        bands
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny font: `I` (3 wide), `o` (4 wide), `!` (2 wide).
    fn font() -> Font {
        let blank = || vec![".".repeat(4); 16];
        let mut i = vec!["...".to_owned(); 16];
        for row in i.iter_mut().take(11).skip(3) {
            *row = "##s".into();
        }
        let mut o = blank();
        o[7] = ".##.".into();
        o[8] = "#..#".into();
        o[9] = "#..#".into();
        o[10] = ".##.".into();
        let mut bang = vec!["..".to_owned(); 16];
        for row in bang.iter_mut().take(9).skip(3) {
            *row = "#.".into();
        }
        bang[10] = "#.".into();
        let json = serde_json::json!({"height": 16, "glyphs": [
            {"text": "I", "width": 3, "rows": i},
            {"text": "o", "width": 4, "rows": o},
            {"text": "!", "width": 2, "rows": bang},
            {"text": " ", "width": 6, "rows": vec!["......"; 16]},
        ]});
        Font::from_json(&json.to_string()).unwrap()
    }

    const INK: Rgb = [49, 81, 206];
    const SHADOW: Rgb = [214, 211, 206];

    #[test]
    fn reads_rendered_lines_in_any_ink_colour() {
        let font = font();
        for (ink, shadow, bg) in [
            (INK, SHADOW, [255, 251, 255]),
            ([255, 251, 255], [107, 89, 115], [41, 81, 107]),
        ] {
            let mut image = RgbImage::filled(240, 160, bg);
            font.render(&mut image, 16, 121, "Io! oI", ink, shadow);
            font.render(&mut image, 16, 137, "oo", ink, shadow);
            let lines = font.read(&image, Region::new(8, 118, 224, 36), &[]);
            assert_eq!(lines, vec!["Io! oI", "oo"]);
        }
    }

    #[test]
    fn unknown_ink_reads_as_one_wildcard() {
        let font = font();
        let mut image = RgbImage::filled(240, 160, [255, 251, 255]);
        font.render(&mut image, 16, 121, "I", INK, SHADOW);
        for y in 126..129 {
            for x in 30..32 {
                image.put_pixel(x, y, INK);
            }
        }
        font.render(&mut image, 40, 121, "o", INK, SHADOW);
        let lines = font.read(&image, Region::new(8, 118, 224, 36), &[]);
        assert_eq!(lines, vec!["I ? o"]);
    }

    #[test]
    fn a_glyph_one_pixel_off_is_still_read() {
        let font = font();
        let mut image = RgbImage::filled(240, 160, [255, 251, 255]);
        font.render(&mut image, 16, 121, "Io", INK, SHADOW);
        // Compression flips a pixel inside the I (ink 16) and one of the o
        // (ink 8): both are still the only glyph that close.
        image.put_pixel(17, 127, SHADOW);
        image.put_pixel(19, 128, INK);
        let lines = font.read(&image, Region::new(8, 118, 224, 36), &[]);
        assert_eq!(lines, vec!["Io"]);
    }

    #[test]
    fn a_small_glyph_one_pixel_off_is_unknown() {
        let font = font();
        let mut image = RgbImage::filled(240, 160, [255, 251, 255]);
        font.render(&mut image, 16, 121, "I!", INK, SHADOW);
        // The ! (ink 7) with its dot missing could be anything.
        image.put_pixel(19, 131, [255, 251, 255]);
        let lines = font.read(&image, Region::new(8, 118, 224, 36), &[]);
        assert_eq!(lines, vec!["I?"]);
    }

    #[test]
    fn ink_that_is_not_text_reads_as_nothing() {
        let font = font();
        let mut image = RgbImage::filled(240, 160, [41, 81, 107]);
        for x in 8..232 {
            image.put_pixel(x, 119, [214, 170, 66]);
            image.put_pixel(x, 150, [214, 170, 66]);
        }
        assert!(font
            .read(&image, Region::new(8, 118, 224, 36), &[])
            .is_empty());
    }

    #[test]
    fn excluded_regions_are_ignored() {
        let font = font();
        let mut image = RgbImage::filled(240, 160, [255, 251, 255]);
        font.render(&mut image, 16, 121, "oI", INK, SHADOW);
        for x in 200..209 {
            image.put_pixel(x, 140, INK);
        }
        let exclude = [Region::new(198, 138, 12, 6)];
        assert_eq!(
            font.read(&image, Region::new(8, 118, 224, 36), &exclude),
            vec!["oI"]
        );
    }
}
