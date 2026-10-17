// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The moments of a sky to check a render by, at a place over a window:
//! the first instant the sun stands at each regime's elevation (setting,
//! then rising), and a night with the Moon up and one without.
//!
//! ```text
//! cargo run -p tuile-atmosphere --example sky_moments -- <lon> <lat> <height m> <start unix s> <days>
//! ```
//!
//! One line a moment: its name, the instant (Unix seconds), and the sun's
//! and the Moon's azimuth and elevation, degrees.
use tuile_atmosphere::{Moon, Sun};
use tuile_core::geo::Geodetic;

fn main() {
    let a: Vec<f64> = std::env::args()
        .skip(1)
        .map(|s| s.parse().expect("a number"))
        .collect();
    let [lon, lat, height, start, days] = a[..] else {
        panic!("<lon> <lat> <height m> <start unix s> <days>");
    };
    let at = Geodetic {
        lon: lon.to_radians(),
        lat: lat.to_radians(),
        height,
    };
    let seen = |t: f64| {
        let s = Sun::seen_from(t, at).expect("sun");
        let m = Moon::seen_from(t, at).expect("moon");
        (s, m)
    };
    let end = start + days * 86_400.0;
    let step = 60.0;
    let print = |name: &str, t: f64| {
        let (s, m) = seen(t);
        println!(
            "{name} {t:.0} sun {:.1} {:.2} moon {:.1} {:.2} lit {:.2}",
            s.azimuth_deg, s.elevation_deg, m.azimuth_deg, m.elevation_deg, m.illuminated_fraction
        );
    };
    // Noon: the sun's highest on the first day.
    let mut best = (start, -90.0);
    let mut t = start;
    while t < start + 86_400.0 {
        let e = seen(t).0.elevation_deg;
        if e > best.1 {
            best = (t, e);
        }
        t += step;
    }
    print("noon", best.0);
    // Setting through each regime after that noon, then rising.
    let crossing = |from: f64, elevation: f64, falling: bool| {
        let mut t = from;
        let mut last = seen(t).0.elevation_deg;
        while t < end {
            t += step;
            let e = seen(t).0.elevation_deg;
            if (falling && last >= elevation && e < elevation)
                || (!falling && last <= elevation && e > elevation)
            {
                return t;
            }
            last = e;
        }
        end
    };
    for (name, e) in [
        ("golden", 6.0),
        ("sunset", 0.0),
        ("civil", -3.0),
        ("nautical", -9.0),
        ("astronomical", -15.0),
    ] {
        print(name, crossing(best.0, e, true));
    }
    print("sunrise", crossing(best.0, 0.0, false));
    // Nights: the sun under −18°, the Moon highest and the Moon down.
    let (mut moonlit, mut moonless) = ((0.0, -90.0), None);
    let mut t = start;
    while t < end {
        let (s, m) = seen(t);
        if s.elevation_deg < -18.0 {
            if m.elevation_deg * m.illuminated_fraction > moonlit.1 {
                moonlit = (t, m.elevation_deg * m.illuminated_fraction);
            }
            if moonless.is_none() && m.elevation_deg < -10.0 {
                moonless = Some(t);
            }
        }
        t += 600.0;
    }
    print("moonlit", moonlit.0);
    if let Some(t) = moonless {
        print("moonless", t);
    }
}
