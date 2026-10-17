// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Prints the level-ground illuminance the model gives as the sun goes
//! down, in lux: the twilight curve, to hold against measured ones.
use glam::DVec3;
use tuile_atmosphere::hillaire::LUT_RADIUS;
use tuile_atmosphere::{AtmosphereParams, LutAtmosphere};

fn main() {
    let air = LutAtmosphere::new(AtmosphereParams::default());
    for e in [20.0f64, 6.0, 2.0, 0.0, -2.0, -4.0, -6.0, -8.0, -12.0, -18.0] {
        let s = e.to_radians();
        let sun = DVec3::new(s.cos(), 0.0, s.sin());
        let sky = air.sky_irradiance(500.0, sun, &[DVec3::Z])[0];
        let table = air.level_irradiance(500.0, s.sin());
        let beam = air.sunlight(LUT_RADIUS + 500.0, s.sin());
        let lum = |v: [f64; 3]| 128_000.0 * (0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2]);
        println!(
            "{e:6.1}°  sky {:10.3} lx  table {:10.3} lx  beam {:10.3} lx",
            lum(sky),
            lum(table),
            lum(beam) * s.sin().max(0.0)
        );
    }
}
