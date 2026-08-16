// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A ten-minute flight around the border of mainland France, at 50 km.
//!
//! Long, slow and continuous — the opposite of `zoom-tape`, and it looks for a
//! different failure. A zoom stresses the *depth* of the tree: the traversal
//! crosses twenty levels in a second and the question is whether coverage keeps
//! up. A traverse stresses its *breadth*: ground enters the frustum on one side
//! and leaves on the other for ten minutes without pause, so the session is
//! loading, drawing and evicting continuously, and nothing is ever settled.
//!
//! Which is where the numbers pointed. A session churned 19 326 evictions
//! against 26 915 uploads, and a stationary camera cannot produce that.
//!
//! ```text
//! cargo run -p tuile-tape --bin border-tape -- france.mcap [minutes]
//! TUILE_REPLAY=france.mcap cargo run --release -p wgpu-viewer
//! ```
//!
//! **Do not trace this at full rate.** Ten minutes of frames is thirty-six
//! thousand pictures — tens of gigabytes. Replay it, watch it, and trace only
//! the stretch that misbehaves.

use tuile_tape::{Frame, Tape};

/// Metres **above the ellipsoid**, which is what the viewer reads back.
///
/// The first version placed the camera on a sphere of the equatorial radius and
/// asked for 50 km; the viewer showed 59.7 km over Biarritz. Not a display bug:
/// at 43° the ellipsoid's surface is some ten kilometres below a sphere of
/// radius `a`, so a camera put at `a + 50 km` really is sixty kilometres up.
/// The conversion below is the proper one, and the number in the menu now
/// matches the number here.
const ALTITUDE: f64 = 30_000.0;

/// Degrees below horizontal. Not straight down, deliberately: an oblique view
/// is what a person actually flies with, it puts the horizon in frame, and
/// tilt is the posture the reports single out.
const PITCH: f64 = 50.0;

/// Frames per second the path is written for. The viewer replays one frame per
/// rendered frame, so this sets how far the camera moves between two pictures.
const FPS: f64 = 60.0;

const MINUTES: f64 = 6.0;

/// WGS84. `A` is the equatorial radius; `E2` the first eccentricity squared,
/// which is the flattening the sphere above ignored.
const A: f64 = 6_378_137.0;
const E2: f64 = 6.694_379_990_141_32e-3;

/// The border of mainland France, clockwise from Dunkerque: the Channel, the
/// Atlantic, the Pyrenees, the Mediterranean, the Alps, the Rhine, the north.
///
/// Coarse on purpose — a few dozen points, interpolated along great circles.
/// The path has to *cover* the country's edge, not survey it, and a denser
/// outline would buy nothing a tile is large enough to notice.
const BORDER: [(f64, f64); 27] = [
    (2.38, 51.03),   // Dunkerque
    (1.85, 50.95),   // Calais
    (1.08, 49.93),   // Dieppe
    (0.11, 49.49),   // Le Havre
    (-1.61, 49.64),  // Cherbourg
    (-4.49, 48.39),  // Brest
    (-3.12, 47.48),  // Quiberon
    (-2.20, 47.28),  // Saint-Nazaire
    (-1.78, 46.50),  // Les Sables-d'Olonne
    (-1.15, 46.16),  // La Rochelle
    (-1.16, 44.66),  // Arcachon
    (-1.56, 43.48),  // Biarritz
    (-0.37, 42.80),  // the western Pyrenees
    (1.00, 42.70),   // Andorra
    (3.03, 42.50),   // Perpignan
    (3.88, 43.60),   // Montpellier
    (5.37, 43.30),   // Marseille
    (7.27, 43.70),   // Nice
    (6.63, 44.90),   // Briançon
    (6.87, 45.92),   // Chamonix
    (6.14, 46.20),   // the Geneva border
    (7.59, 47.56),   // Basel
    (7.75, 48.58),   // Strasbourg
    (8.17, 48.97),   // Lauterbourg
    (6.13, 49.46),   // the Luxembourg border
    (4.72, 49.77),   // Charleville-Mézières
    (3.06, 50.63),   // Lille
];

/// Geodetic to ECEF on the WGS84 ellipsoid.
///
/// The prime vertical radius `n` is what a sphere leaves out, and leaving it
/// out costs ten kilometres of altitude at French latitudes — which is a fifth
/// of the height this path is flown at.
fn geodetic_to_ecef(lon: f64, lat: f64, height: f64) -> [f64; 3] {
    let (s, c) = (lat.sin(), lat.cos());
    let n = A / (1.0 - E2 * s * s).sqrt();
    [
        (n + height) * c * lon.cos(),
        (n + height) * c * lon.sin(),
        (n * (1.0 - E2) + height) * s,
    ]
}

/// Great-circle distance in radians between two points given in radians.
fn arc(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lon1, lat1) = a;
    let (lon2, lat2) = b;
    let d = (lat1.sin() * lat2.sin() + lat1.cos() * lat2.cos() * (lon2 - lon1).cos()).clamp(-1.0, 1.0);
    d.acos()
}

/// A point `t` of the way along the great circle from `a` to `b`.
///
/// Spherical interpolation rather than linear on longitude and latitude: linear
/// drifts off the shortest path and, near the poles, moves at a wildly
/// different speed for the same `t`. France is not near a pole, and doing it
/// properly costs four trigonometric calls.
fn along(a: (f64, f64), b: (f64, f64), t: f64) -> (f64, f64) {
    let d = arc(a, b);
    if d < 1e-12 {
        return a;
    }
    let (sa, sb) = (((1.0 - t) * d).sin() / d.sin(), (t * d).sin() / d.sin());
    let x = sa * a.1.cos() * a.0.cos() + sb * b.1.cos() * b.0.cos();
    let y = sa * a.1.cos() * a.0.sin() + sb * b.1.cos() * b.0.sin();
    let z = sa * a.1.sin() + sb * b.1.sin();
    (y.atan2(x), z.atan2((x * x + y * y).sqrt()))
}

/// The local east-north-up frame at a point.
fn frame_at(lon: f64, lat: f64) -> ([f64; 3], [f64; 3], [f64; 3]) {
    let east = [-lon.sin(), lon.cos(), 0.0];
    let up = [lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()];
    let north = [
        up[1] * east[2] - up[2] * east[1],
        up[2] * east[0] - up[0] * east[2],
        up[0] * east[1] - up[1] * east[0],
    ];
    (east, north, up)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "france.mcap".to_owned());
    let minutes: f64 = std::env::args()
        .nth(2)
        .map_or(Ok(MINUTES), |a| a.parse())
        .map_err(|_| "usage: border-tape <path> [minutes]")?;

    let points: Vec<(f64, f64)> = BORDER
        .iter()
        .map(|(lon, lat)| (lon.to_radians(), lat.to_radians()))
        .collect();
    // Closed loop, and each leg gets frames in proportion to its length so the
    // camera flies at a constant speed rather than sprinting across the short
    // legs and crawling along the Atlantic.
    let legs: Vec<f64> = (0..points.len())
        .map(|i| arc(points[i], points[(i + 1) % points.len()]))
        .collect();
    let perimeter: f64 = legs.iter().sum();
    let total = (minutes * 60.0 * FPS) as usize;

    let mut tape = Tape::recording(&path)?;
    let pitch = PITCH.to_radians();
    for n in 0..total {
        // How far around the loop, in radians of arc.
        let travelled = perimeter * (n as f64 / total as f64);
        let mut remaining = travelled;
        let mut leg = 0;
        while leg < legs.len() && remaining > legs[leg] {
            remaining -= legs[leg];
            leg += 1;
        }
        let leg = leg.min(legs.len() - 1);
        let t = if legs[leg] > 1e-12 {
            remaining / legs[leg]
        } else {
            0.0
        };
        let here = along(points[leg], points[(leg + 1) % points.len()], t);
        // A little further on, to take the bearing from — the camera faces the
        // way it is going, which is what makes this a flight rather than a
        // sequence of stills.
        let ahead = along(points[leg], points[(leg + 1) % points.len()], (t + 0.01).min(1.0));

        let (east, north, up) = frame_at(here.0, here.1);
        let bearing = {
            let (dlon, dlat) = (ahead.0 - here.0, ahead.1 - here.1);
            (dlon * here.1.cos()).atan2(dlat)
        };
        // Forward along the bearing, tipped down by the pitch.
        let (cb, sb) = (bearing.cos(), bearing.sin());
        let (cp, sp) = (pitch.cos(), pitch.sin());
        let direction = [
            (north[0] * cb + east[0] * sb) * cp - up[0] * sp,
            (north[1] * cb + east[1] * sb) * cp - up[1] * sp,
            (north[2] * cb + east[2] * sb) * cp - up[2] * sp,
        ];
        tape.push(Frame {
            position: geodetic_to_ecef(here.0, here.1, ALTITUDE),
            direction,
            // The local vertical as screen-up: it is never parallel to a
            // direction tipped 50° off it, and it keeps the horizon level.
            up,
            fovy: 45f64.to_radians(),
        });
    }

    let written = tape.finish()?;
    println!(
        "{path}: {written} frames — {minutes:.0} min around {:.0} km of border at {:.0} km, \
         pitched {PITCH:.0}° below horizontal",
        perimeter * A / 1000.0,
        ALTITUDE / 1000.0
    );
    Ok(())
}
