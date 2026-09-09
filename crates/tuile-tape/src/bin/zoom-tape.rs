// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Writes a camera path that dives and climbs, for reproducing a zoom.
//!
//! A hand cannot make the same zoom twice, and the bug being chased only shows
//! up in fast movement — so the movement has to come from a file. Replay this
//! with `TUILE_REPLAY` and the viewer flies it frame for frame; add
//! `TUILE_TRACE` and it writes the camera and every frame it drew into one
//! MCAP, which is the thing to open afterwards.
//!
//! ```text
//! cargo run -p tuile-tape --bin zoom-tape -- zoom.mcap
//! TUILE_REPLAY=zoom.mcap TUILE_TRACE=trace.mcap cargo run --release -p wgpu-viewer
//! ```

use tuile_tape::{Frame, Tape};

/// Where the camera looks: the lake this bug was first reported over.
const LON: f64 = 2.17;
const LAT: f64 = 42.52;

/// Metres. Two hundred metres above the ground to geostationary orbit — the
/// whole usable range of the viewer in one gesture.
///
/// Geostationary is 35 786 km, the altitude at which an orbit takes a sidereal
/// day. It is the top of what anyone looks at a globe from, and starting there
/// means the path crosses **every** level the traversal has, from the pinned
/// coarse pyramid to the deepest terrain the source serves.
const LOW: f64 = 200.0;
const HIGH: f64 = 35_786_000.0;

/// Frames each way, and the number matters more than it looks.
///
/// The path is geometric, so each frame multiplies the altitude by
/// `(HIGH/LOW)^(1/FRAMES)`. At ninety frames that is **+8.6 % per frame**, which
/// plays back as a slideshow rather than a zoom — the first trace was
/// unwatchable for exactly this reason, and a jerky trace hides the transient it
/// was recorded to show: a hole that lasts three frames is invisible when three
/// frames span two orders of magnitude.
///
/// Frames each way, unless the second argument says otherwise.
///
/// This is **the** knob, because the bug is provoked by the frustum's footprint
/// growing faster than the selection describing it — a property of the step
/// *per frame*, not of how long the path runs. A gentle path cannot reproduce
/// it however many frames it takes.
///
/// The path is geometric over a factor of ~179 000 in altitude, so the step per
/// frame is `179000^(1/frames)`:
///
/// | frames each way | step per frame | wheel time at 60 Hz |
/// |---|---|---|
/// | 32  | +46 %  | 0.5 s |
/// | 60  | +22 %  | 1.0 s |
/// | 120 | +10 %  | 2.0 s |
/// | 240 | +5.2 % | 4.0 s |
///
/// Thirty-two by default: a flick, and three times the rate of the path before
/// it — which produced nothing at any speed over a range a fifth as wide.
/// Gentler has been measured and provokes nothing.
const FRAMES: usize = 32;

/// Geodetic → ECEF on the WGS84 ellipsoid.
///
/// The comment that stood here said a sphere was enough because this only
/// positions a camera and "the difference never reaches a pixel". Measured at
/// 42.52° N, the difference is **14.8 km of altitude and 21 km of ground**:
/// the equatorial radius is 9.7 km larger than the ellipsoid's radius at that
/// latitude, and using a geodetic latitude as a geocentric one moves the point
/// 0.19° north. It reaches rather more than a pixel.
fn geodetic_to_ecef(lon: f64, lat: f64, height: f64) -> [f64; 3] {
    let p = tuile_core::geo::geodetic_to_ecef(tuile_core::geo::Geodetic {
        lon: lon.to_radians(),
        lat: lat.to_radians(),
        height,
    });
    [p.x, p.y, p.z]
}

fn looking_down(altitude: f64) -> Frame {
    let eye = geodetic_to_ecef(LON, LAT, altitude);
    let n = (eye[0] * eye[0] + eye[1] * eye[1] + eye[2] * eye[2]).sqrt();
    let up = [eye[0] / n, eye[1] / n, eye[2] / n];
    // Screen-up must not be parallel to where the camera looks, or the view
    // matrix is degenerate and nothing lands on screen.
    let east = [-up[1], up[0], 0.0];
    let e = (east[0] * east[0] + east[1] * east[1]).sqrt().max(1e-12);
    Frame {
        position: eye,
        direction: [-up[0], -up[1], -up[2]],
        up: [east[0] / e, east[1] / e, 0.0],
        fovy: 45f64.to_radians(),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "zoom.mcap".to_owned());
    // Second argument overrides the aggressiveness, so dialling it in does not
    // mean editing and rebuilding.
    let frames: usize = std::env::args()
        .nth(2)
        .map_or(Ok(FRAMES), |a| a.parse())
        .map_err(|_| "usage: zoom-tape <path> [frames-each-way]")?;
    let mut tape = Tape::recording(&path)?;

    // Settle at the top so the session has something loaded before it moves —
    // a cold start is not a zoom, and mixing the two makes a trace unreadable.
    for _ in 0..90 {
        tape.push(looking_down(HIGH));
    }
    // Geometric, so every frame multiplies the altitude by the same factor,
    // which is what a wheel does.
    for i in 0..=frames {
        let t = i as f64 / frames as f64;
        tape.push(looking_down(HIGH * (LOW / HIGH).powf(t)));
    }
    for _ in 0..60 {
        tape.push(looking_down(LOW));
    }
    for i in 0..=frames {
        let t = i as f64 / frames as f64;
        tape.push(looking_down(LOW * (HIGH / LOW).powf(t)));
    }
    for _ in 0..60 {
        tape.push(looking_down(HIGH));
    }

    let written = tape.finish()?;
    let step = (HIGH / LOW).powf(1.0 / frames as f64);
    println!(
        "{path}: {written} frames — {LOW:.0} m to {HIGH:.0} m at {:+.1}% per frame",
        (step - 1.0) * 100.0
    );
    Ok(())
}
