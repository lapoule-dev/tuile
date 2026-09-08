// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Writes a camera path that orbits a point on the ground.
//!
//! The neutral demo trajectory: a full circle at constant altitude and
//! distance, always looking at the same spot. Where the zoom tape exercises
//! every LOD level and the border tape flies a real product path, the orbit
//! shows one place from every azimuth — the shape that makes missing tiles,
//! seams and texture swimming easiest to see.
//!
//! ```text
//! cargo run -p tuile-tape --bin orbit-tape -- orbit.mcap \
//!     [frames] [lon] [lat] [radius-m] [altitude-m]
//! ```

use tuile_tape::{Frame, Tape};

/// Where the orbit looks by default: the same lake as the zoom tape, so the
/// two trajectories are comparable over identical ground.
const LON: f64 = 2.17;
const LAT: f64 = 42.52;
const FRAMES: usize = 96;
/// Horizontal distance and height of the circle, metres. A ~30° look-down
/// angle: high enough to see terrain relief, low enough that imagery detail
/// matters.
const RADIUS: f64 = 8_000.0;
const ALTITUDE: f64 = 5_000.0;

/// WGS84 equatorial radius. Sphere rather than ellipsoid: this positions a
/// camera, and the difference never reaches a pixel.
const EARTH: f64 = 6_378_137.0;

fn normalize(v: [f64; 3]) -> [f64; 3] {
    let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-12);
    [v[0] / n, v[1] / n, v[2] / n]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arg = |n: usize| std::env::args().nth(n);
    let path = arg(1).unwrap_or_else(|| "orbit.mcap".to_owned());
    let parse = |n: usize, or: f64| -> Result<f64, String> {
        arg(n).map_or(Ok(or), |a| {
            a.parse()
                .map_err(|_| format!("argument {n} is not a number: {a}"))
        })
    };
    let frames = parse(2, FRAMES as f64)? as usize;
    let lon = parse(3, LON)?.to_radians();
    let lat = parse(4, LAT)?.to_radians();
    let radius = parse(5, RADIUS)?;
    let altitude = parse(6, ALTITUDE)?;

    // The local frame at the target: zenith, then east/north on the tangent
    // plane. Everything the orbit does is a rotation in that plane.
    let target = [
        EARTH * lat.cos() * lon.cos(),
        EARTH * lat.cos() * lon.sin(),
        EARTH * lat.sin(),
    ];
    let zenith = normalize(target);
    let east = normalize(cross([0.0, 0.0, 1.0], zenith));
    let north = cross(zenith, east);

    let mut tape = Tape::recording(&path)?;
    for i in 0..frames {
        let theta = std::f64::consts::TAU * i as f64 / frames as f64;
        let (sin, cos) = theta.sin_cos();
        let position = [
            target[0] + radius * (cos * east[0] + sin * north[0]) + altitude * zenith[0],
            target[1] + radius * (cos * east[1] + sin * north[1]) + altitude * zenith[1],
            target[2] + radius * (cos * east[2] + sin * north[2]) + altitude * zenith[2],
        ];
        let direction = normalize([
            target[0] - position[0],
            target[1] - position[1],
            target[2] - position[2],
        ]);
        tape.push(Frame {
            position,
            direction,
            // The zenith is never parallel to a look direction that keeps a
            // horizontal offset, so the basis downstream stays well-formed.
            up: zenith,
            fovy: 45f64.to_radians(),
        });
    }

    let written = tape.finish()?;
    println!(
        "{path}: {written} frames orbiting ({:.4}, {:.4}) at r={radius} m, h={altitude} m",
        lon.to_degrees(),
        lat.to_degrees()
    );
    Ok(())
}
