// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The air's light between an eye and the ground against the ground's own,
//! as the sun goes down: what the veil over twilight ground is made of.
use glam::DVec3;
use tuile_atmosphere::hillaire::LUT_RADIUS;
use tuile_atmosphere::{AtmosphereParams, LutAtmosphere};

fn main() {
    let air = LutAtmosphere::new(AtmosphereParams::default());
    let lum = |v: [f64; 3]| 0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2];
    let height = 2000.0;
    for e in [30.0f64, 6.0, 0.0, -3.0, -6.0, -9.0, -12.0] {
        let s = e.to_radians();
        // Looking across the sun's azimuth, 10° down: the ground ~11 km off.
        let sun = DVec3::new(s.cos(), 0.0, s.sin());
        let d = (-10f64).to_radians();
        let dir = DVec3::new(0.0, d.cos(), d.sin());
        let ray = air.march(height, dir, sun, Some(height / -d.sin()), 12, false);
        let ground_e = {
            let b = air.sunlight(LUT_RADIUS, s.sin());
            let sky = air.level_irradiance(0.0, s.sin());
            [0, 1, 2].map(|c| b[c] * s.sin().max(0.0) + sky[c])
        };
        let ground =
            [0, 1, 2].map(|c| 0.1 * ground_e[c] / std::f64::consts::PI * ray.transmittance[c]);
        println!(
            "{e:6.1}°  ground {:.3e}  air {:.3e}  air/ground {:.2}  air rgb {:?}",
            lum(ground),
            lum(ray.inscattered),
            lum(ray.inscattered) / lum(ground),
            ray.inscattered
                .map(|v| (v / lum(ray.inscattered) * 100.0).round() / 100.0)
        );
    }
}
