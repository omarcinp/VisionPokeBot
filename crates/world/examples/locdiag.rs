// Scratch diagnostic (not part of the change): best scores at every pixel
// offset around a tile. locdiag <world> <png> <map> <x> <y> [out.png]
use pokebot_core::RgbImage;
use pokebot_state::Region;
use pokebot_world::localize::{score, SampleGrid, PLAYER_SPRITE};
use pokebot_world::{World, BLOCK};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let world = World::load(&args[1]).unwrap();
    let frame = pokebot_video::png::load(&args[2]).unwrap();
    if args[3] == "-" {
        let grid = SampleGrid::new(&[PLAYER_SPRITE], 4);
        let mut all = Vec::new();
        for m in world.maps() {
            let Ok(r) = m.render() else { continue };
            for y in 0..m.height {
                for x in 0..m.width {
                    if let Some(s) = score(&frame, r, m, x, y, &grid, 0) {
                        all.push((s, m.name.clone(), x, y));
                    }
                }
            }
        }
        all.sort_by_key(|a| std::cmp::Reverse(a.0));
        for a in all.iter().take(8) {
            println!("{:4} {} ({}, {})", a.0, a.1, a.2, a.3);
        }
        return;
    }
    let m = world.map(&args[3]).unwrap();
    let (tx, ty): (i32, i32) = (args[4].parse().unwrap(), args[5].parse().unwrap());
    let extra: Vec<Region> = std::env::var("EXCL")
        .ok()
        .map(|s| {
            s.split(';')
                .map(|r| {
                    let v: Vec<u32> = r.split(',').map(|x| x.parse().unwrap()).collect();
                    Region::new(v[0], v[1], v[2], v[3])
                })
                .collect()
        })
        .unwrap_or_default();
    let exclude: Vec<Region> = std::iter::once(PLAYER_SPRITE).chain(extra).collect();
    let grid = SampleGrid::new(&exclude, 4);
    let render = m.render().unwrap();
    // Score at pixel offsets: shift the frame instead of the render by
    // building a shifted render view.
    let mut results = Vec::new();
    for dy in 0..BLOCK {
        for dx in 0..BLOCK {
            let shifted = shift(render, dx, dy);
            for y in ty - 3..=ty + 3 {
                for x in tx - 3..=tx + 3 {
                    if let Some(s) = score(&frame, &shifted, m, x, y, &grid, 0) {
                        results.push((s, x, y, dx, dy));
                    }
                }
            }
        }
    }
    results.sort_by_key(|r| std::cmp::Reverse(r.0));
    for r in results.iter().take(6) {
        println!("{:4} ({}, {}) +({}, {}) px", r.0, r.1, r.2, r.3, r.4);
    }
    if let (Some(out), Some(best)) = (args.get(6), results.first()) {
        let shifted = shift(render, best.3, best.4);
        let ox = (best.1 + m.pad) * BLOCK - 112;
        let oy = (best.2 + m.pad) * BLOCK - 72;
        let mut img = RgbImage::filled(720, 160, [0, 0, 0]);
        for y in 0..160 {
            for x in 0..240 {
                let f = frame.pixel(x, y);
                img.put_pixel(x, y, f);
                let (rx, ry) = (ox + x as i32, oy + y as i32);
                let r = if rx >= 0
                    && ry >= 0
                    && rx < shifted.width() as i32
                    && ry < shifted.height() as i32
                {
                    shifted.pixel(rx as u32, ry as u32)
                } else {
                    [255, 0, 255]
                };
                img.put_pixel(240 + x, y, r);
                let d = (0..3).map(|c| f[c].abs_diff(r[c])).max().unwrap();
                let v = if d > 24 {
                    [255, 0, 0]
                } else {
                    [d * 4, d * 4, d * 4]
                };
                img.put_pixel(480 + x, y, v);
            }
        }
        pokebot_video::png::save(&img, out).unwrap();
    }
}

/// The render moved so that its pixel (x + dx, y + dy) shows at (x, y).
fn shift(render: &RgbImage, dx: i32, dy: i32) -> RgbImage {
    let (w, h) = (render.width(), render.height());
    let mut out = RgbImage::filled(w, h, [255, 0, 255]);
    for y in 0..h {
        for x in 0..w {
            let (sx, sy) = (x as i32 + dx, y as i32 + dy);
            if sx < w as i32 && sy < h as i32 {
                out.put_pixel(x, y, render.pixel(sx as u32, sy as u32));
            }
        }
    }
    out
}
