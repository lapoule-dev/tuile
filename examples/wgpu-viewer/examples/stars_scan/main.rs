// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The stars the viewer shows, all the way round: the viewer's own frame,
//! headless, at one instant and one size, turned through every heading at
//! a few pitches and places. For each view, the point sources it shows
//! (pixels over their ring of neighbours by 4 code values, the on-screen
//! controls left out), printed as a table; each picture saved.
//!
//! ```text
//! TUILE_ION_TOKEN=… cargo run --release -p tuile-wgpu-viewer --example stars_scan -- \
//!     --at 2026-06-21T23:30:00Z --width 2880 --height 1620 --scale 2 --out /tmp/scan
//! ```

#[allow(dead_code)]
#[path = "../headless/catalog.rs"]
mod catalog;

use std::path::PathBuf;

use tuile_atmosphere::clock::parse_rfc3339;
use tuile_viewer::control::{Action, Headless, Start, View};

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// Point sources: pixels whose brightest channel is the greatest of their
/// 3×3 and over the brightest of their ring at distance 3 by `over`.
fn points(rgba: &[u8], (w, h): (u32, u32), over: i32, skip_right: u32) -> usize {
    let at = |x: u32, y: u32| -> i32 {
        let i = ((y * w + x) * 4) as usize;
        i32::from(rgba[i].max(rgba[i + 1]).max(rgba[i + 2]))
    };
    let mut n = 0;
    for y in 3..h - 3 {
        for x in 3..w.saturating_sub(skip_right + 3) {
            let v = at(x, y);
            let mut top = true;
            for dy in 0..3 {
                for dx in 0..3 {
                    if at(x + dx - 1, y + dy - 1) > v {
                        top = false;
                    }
                }
            }
            if !top {
                continue;
            }
            let mut ring = 0;
            for k in 0..7 {
                ring = ring
                    .max(at(x + k - 3, y - 3))
                    .max(at(x + k - 3, y + 3))
                    .max(at(x - 3, y + k - 3))
                    .max(at(x + 3, y + k - 3));
            }
            if v >= ring + over {
                n += 1;
            }
        }
    }
    n
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let number = |name: &str, or: f64| flag(&args, name).map_or(or, |v| v.parse().expect(name));
    let instants: Vec<f64> = flag(&args, "--at")
        .unwrap_or("2026-06-21T23:30:00Z")
        .split(',')
        .map(|t| parse_rfc3339(t).expect("--at"))
        .collect();
    let at = instants[0];
    let size = (
        number("--width", 2880.0) as u32,
        number("--height", 1620.0) as u32,
    );
    let scale = number("--scale", 2.0);
    let out = PathBuf::from(flag(&args, "--out").unwrap_or("scan"));
    let step = number("--step", 15.0);
    let pitches: Vec<f64> = flag(&args, "--pitches")
        .unwrap_or("0,-30,-60")
        .split(',')
        .map(|p| p.parse().expect("--pitches"))
        .collect();
    // The Pic, and two other places: the plain north of it, and far away.
    let places: Vec<(f64, f64, f64)> = flag(&args, "--places")
        .unwrap_or("0.1418:42.9365:3100,0.10:43.30:400,2.35:48.85:1200")
        .split(',')
        .map(|p| {
            let v: Vec<f64> = p.split(':').map(|x| x.parse().expect("--places")).collect();
            (v[0], v[1], v[2])
        })
        .collect();
    std::fs::create_dir_all(&out).expect("out");
    let proxy = catalog::Proxy::serve().expect("proxy");
    let dir = tempfile::Builder::new()
        .prefix("tuile-scan-")
        .tempdir()
        .expect("tmp");
    let first = places[0];
    let mut run = Headless::start(Start {
        view: View {
            lon: first.0,
            lat: first.1,
            altitude: first.2,
            heading: 0.0,
            pitch: 0.0,
        },
        size,
        scale,
        lit_at: at,
        ..Start::new(
            proxy.host(dir.path().join("http")),
            dir.path().join("tiles"),
        )
    })
    .expect("start");
    let skip_right = (size.0 as f64 * 0.12) as u32;
    println!("instant\tplace\tpitch\theading\tpoints");
    for (t, &instant) in instants.iter().enumerate() {
        run.act(&Action::Time { instant, rate: 0.0 });
        for (p, place) in places.iter().enumerate() {
            for &pitch in &pitches {
                let mut heading = 0.0;
                while heading < 360.0 - 1e-6 {
                    run.act(&Action::Go(View {
                        lon: place.0,
                        lat: place.1,
                        altitude: place.2,
                        heading,
                        pitch,
                    }));
                    // A view asked for is drawn a frame or two later: the
                    // picture is read once it is.
                    run.steps(6);
                    run.steps(6);
                    let (pixels, (w, h)) = run.picture(0).expect("a picture");
                    let n = points(pixels, (w, h), 4, skip_right);
                    println!("{t}\t{p}\t{pitch}\t{heading}\t{n}");
                    if flag(&args, "--save").is_some() {
                        let path = out.join(format!("t{t}-p{p}-pitch{pitch}-h{heading:03}.png"));
                        image::save_buffer(&path, pixels, w, h, image::ExtendedColorType::Rgba8)
                            .expect("save");
                    }
                    heading += step;
                }
            }
        }
    }
    run.finish().expect("finish");
}
