// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A small pack made of nothing but arithmetic, for exercising a film renderer
//! without a bake: a 16 × 16 km grid of rolling, checkered tiles under a camera
//! that circles it. Tiles enter and leave with distance, and every tile is
//! re-draped half-way through, so each diff path is taken.
//!
//! ```bash
//! cargo run -p tuile-film --example synthetic_pack -- out.tuilepack [frames]
//! ```

use std::io::Cursor;

use glam::DVec3;
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};
use tuile_film::BakedView;
use tuile_pack::{BakedTile, PackWriter, TextureFormat};

const GRID: i32 = 16;
const TILE_M: f64 = 1000.0;
const STEPS: usize = 16;

fn height(x: f64, y: f64) -> f64 {
    120.0 * (x / 700.0).sin() * (y / 900.0).cos() + 60.0 * ((x + y) / 350.0).sin()
}

fn texture(i: i32, j: i32, drape: u64) -> Vec<u8> {
    let size = 128u32;
    let hue = ((i * 7 + j * 13) % 12) as f32 / 12.0;
    let (r, g, b) = hsv(hue, if drape == 0 { 0.55 } else { 0.25 }, 0.85);
    let img = image::RgbaImage::from_fn(size, size, |x, y| {
        let border = x < 3 || y < 3;
        let check = ((x / 16) + (y / 16)) % 2 == 0;
        let k = if border {
            0.25
        } else if check {
            1.0
        } else {
            0.7
        };
        image::Rgba([
            (r * k * 255.0) as u8,
            (g * k * 255.0) as u8,
            (b * k * 255.0) as u8,
            255,
        ])
    });
    let mut png = Vec::new();
    img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("png");
    png
}

fn hsv(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let k = |n: f32| (n + h * 6.0) % 6.0;
    let f = |n: f32| v - v * s * k(n).min(4.0 - k(n)).clamp(0.0, 1.0);
    (f(5.0), f(3.0), f(1.0))
}

fn tile(centre: DVec3, enu: glam::DMat3, i: i32, j: i32, drape: u64) -> BakedTile {
    let (x0, y0) = (f64::from(i) * TILE_M, f64::from(j) * TILE_M);
    let at = |x: f64, y: f64| centre + enu.x_axis * x + enu.y_axis * y + enu.z_axis * height(x, y);
    let origin = at(x0 + TILE_M / 2.0, y0 + TILE_M / 2.0);
    let (mut p, mut n, mut uv, mut idx) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for b in 0..=STEPS {
        for a in 0..=STEPS {
            let (u, v) = (a as f64 / STEPS as f64, b as f64 / STEPS as f64);
            let (x, y) = (x0 + u * TILE_M, y0 + v * TILE_M);
            let q = at(x, y) - origin;
            let e = 1.0;
            let normal = (at(x + e, y) - at(x - e, y))
                .cross(at(x, y + e) - at(x, y - e))
                .normalize();
            p.extend([q.x as f32, q.y as f32, q.z as f32]);
            n.extend([normal.x as f32, normal.y as f32, normal.z as f32]);
            uv.extend([u as f32, 1.0 - v as f32]);
        }
    }
    let row = STEPS as u32 + 1;
    for b in 0..STEPS as u32 {
        for a in 0..STEPS as u32 {
            let k = b * row + a;
            idx.extend([k, k + 1, k + row + 1, k, k + row + 1, k + row]);
        }
    }
    let le = |v: &[f32]| v.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>();
    BakedTile {
        id: ((i + GRID) * 1000 + (j + GRID)) as u64,
        drape,
        origin_ecef: origin.to_array(),
        positions: le(&p),
        normals: le(&n),
        uvs: le(&uv),
        indices: idx.iter().flat_map(|v| v.to_le_bytes()).collect(),
        vertex_count: (p.len() / 3) as u32,
        index_count: idx.len() as u32,
        base_color_factor: [1.0; 4],
        texture: Some(texture(i, j, drape)),
        texture_format: TextureFormat::Png,
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| "synthetic.tuilepack".into());
    let frames: u32 = args.next().and_then(|f| f.parse().ok()).unwrap_or(240);

    let g = Geodetic {
        lon: 0.5f64.to_radians(),
        lat: 42.8f64.to_radians(),
        height: 1000.0,
    };
    let centre = geodetic_to_ecef(g);
    let enu = enu_frame(g);
    let dir = tempfile_dir();
    let mut writer =
        PackWriter::new("synthetic", centre.to_array(), dir.join("blob")).expect("writer");

    for f in 1..=frames {
        let t = f64::from(f - 1) / f64::from(frames) * std::f64::consts::TAU;
        let ground = enu.x_axis * (5000.0 * t.cos()) + enu.y_axis * (5000.0 * t.sin());
        let eye = centre + ground + enu.z_axis * 2500.0;
        let target = centre + ground * 0.2 + enu.z_axis * 0.0;
        let view = BakedView {
            position: eye.to_array(),
            direction: (target - eye).normalize().to_array(),
            up: enu.z_axis.to_array(),
            viewport_px: [1280.0, 720.0],
            fovy_rad: 0.9,
        };
        let drape = u64::from(f > frames / 2);
        let mut open = writer.begin_frame(f, view);
        for i in -GRID / 2..GRID / 2 {
            for j in -GRID / 2..GRID / 2 {
                let c = DVec3::new(
                    (f64::from(i) + 0.5) * TILE_M,
                    (f64::from(j) + 0.5) * TILE_M,
                    0.0,
                );
                let cam = DVec3::new(5000.0 * t.cos(), 5000.0 * t.sin(), 0.0);
                if c.distance(cam) > 9000.0 {
                    continue;
                }
                let id = ((i + GRID) * 1000 + (j + GRID)) as u64;
                if !open.push_known(id, drape) {
                    open.push(tile(centre, enu, i, j, drape));
                }
            }
        }
        open.end();
    }
    let bytes = writer.finish_to(&out).expect("finish");
    std::fs::remove_dir_all(&dir).ok();
    println!("{out}: {frames} frames, {:.1} MB", bytes as f64 / 1e6);
}

fn tempfile_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("synthetic-pack-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}
