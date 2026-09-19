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

use tuile_tape::path::{geodetic_to_ecef, ClosedPath};
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

const BORDER: [(f64, f64); 27] = [
    (2.38, 51.03),  // Dunkerque
    (1.85, 50.95),  // Calais
    (1.08, 49.93),  // Dieppe
    (0.11, 49.49),  // Le Havre
    (-1.61, 49.64), // Cherbourg
    (-4.49, 48.39), // Brest
    (-3.12, 47.48), // Quiberon
    (-2.20, 47.28), // Saint-Nazaire
    (-1.78, 46.50), // Les Sables-d'Olonne
    (-1.15, 46.16), // La Rochelle
    (-1.16, 44.66), // Arcachon
    (-1.56, 43.48), // Biarritz
    (-0.37, 42.80), // the western Pyrenees
    (1.00, 42.70),  // Andorra
    (3.03, 42.50),  // Perpignan
    (3.88, 43.60),  // Montpellier
    (5.37, 43.30),  // Marseille
    (7.27, 43.70),  // Nice
    (6.63, 44.90),  // Briançon
    (6.87, 45.92),  // Chamonix
    (6.14, 46.20),  // the Geneva border
    (7.59, 47.56),  // Basel
    (7.75, 48.58),  // Strasbourg
    (8.17, 48.97),  // Lauterbourg
    (6.13, 49.46),  // the Luxembourg border
    (4.72, 49.77),  // Charleville-Mézières
    (3.06, 50.63),  // Lille
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "france.mcap".to_owned());
    let minutes: f64 = std::env::args()
        .nth(2)
        .map_or(Ok(MINUTES), |a| a.parse())
        .map_err(|_| "usage: border-tape <path> [minutes]")?;

    let path_around = ClosedPath::from_degrees(&BORDER);
    let total = (minutes * 60.0 * FPS) as usize;

    let mut tape = Tape::recording(&path)?;
    let pitch = PITCH.to_radians();
    for n in 0..total {
        let step = path_around.at(n as f64 / total as f64);
        tape.push(Frame {
            position: geodetic_to_ecef(step.here.0, step.here.1, ALTITUDE),
            direction: step.look(pitch),
            // The local vertical as screen-up: it is never parallel to a
            // direction tipped 50° off it, and it keeps the horizon level.
            up: step.up,
            fovy: 45f64.to_radians(),
        });
    }

    let written = tape.finish()?;
    println!(
        "{path}: {written} frames — {minutes:.0} min around {:.0} km of border at {:.0} km, \
         pitched {PITCH:.0}° below horizontal",
        path_around.length_m() / 1000.0,
        ALTITUDE / 1000.0
    );
    Ok(())
}
