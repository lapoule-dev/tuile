// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Prints the exposure the viewer's frame-metered camera chooses at the Pic
//! du Midi through the evening of 2026-06-21, for every heading at pitch 0:
//! the same metering rays (`hillaire::metering`) and the same calibration
//! the viewer's sky uses, in stops against the frame looking east
//! (heading 110), the darkest sky.
//!
//! ```text
//! cargo run --release -p tuile-atmosphere --example metered_exposure
//! ```

use tuile_atmosphere::hillaire::{metering, Moment};
use tuile_atmosphere::{AtmosphereFrame, LutAtmosphere, Moon, Moonlight, SkyLook, Sun};
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};

fn main() {
    let air = LutAtmosphere::today();
    let look = SkyLook::default();
    let deg = std::f64::consts::PI / 180.0;
    let eye_at = Geodetic {
        lon: 0.1418 * deg,
        lat: 42.9365 * deg,
        height: 3_100.0,
    };
    let ground = Geodetic {
        height: 2_877.0,
        ..eye_at
    };
    let eye = geodetic_to_ecef(eye_at);
    let enu = enu_frame(eye_at);
    let sunset = 1_782_070_914.0; // 2026-06-21T19:41:54Z
    let (fov_y, aspect) = (45.0 * deg, 16.0 / 9.0);
    let exposure = |instant: f64, heading: f64| -> f64 {
        let h = heading * deg;
        let fwd = enu.x_axis * h.sin() + enu.y_axis * h.cos();
        let right = fwd.cross(enu.z_axis).normalize();
        let up = right.cross(fwd);
        let half = (fov_y * 0.5).tan();
        let metered = metering(
            eye,
            fwd,
            right * half * aspect,
            up * half,
            &AtmosphereFrame::at(ground),
            ground.height,
        );
        let light = air.light(
            &air,
            ground,
            Moment {
                to_sun_ecef: Sun::at_unix_seconds(instant).direction_ecef,
                moon: Moon::seen_from(instant, eye_at).map(|m| Moonlight::of(&m)),
                compensation_ev: 0.0,
            },
            &metered,
            look.calibration,
        );
        f64::from(look.exposure_scale(&light)).log2()
    };
    let headings: Vec<f64> = (0..12).map(|k| f64::from(k) * 30.0).collect();
    print!("min after sunset ");
    for h in &headings {
        print!("{h:>6}");
    }
    println!("   (exposure, stops over the heading-110 frame's)");
    for minutes in [15.0, 30.0, 45.0, 60.0, 90.0, 120.0, 228.0] {
        let t = sunset + minutes * 60.0;
        let east = exposure(t, 110.0);
        print!("{minutes:>16} ");
        for &h in &headings {
            print!("{:>+6.2}", exposure(t, h) - east);
        }
        println!("   east {east:+.2}");
    }
}
