// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Where the Moon is, how much of it is lit, and how much light it sends.
//!
//! # Where the numbers come from
//!
//! The ephemeris is not written here. The Moon's geocentric ecliptic place
//! is the `astro` crate's evaluation of the ELP-2000/82 series as Meeus
//! truncates it (*Astronomical Algorithms*, ch. 47: 10″ in longitude, 4″ in
//! latitude, a few kilometres in distance), with its nutation (ch. 22) and
//! sidereal time (ch. 12). Ten arcseconds is a hundred-and-eightieth of the
//! Moon's disc: far finer than any picture here can show.
//!
//! What this module adds is geometry, not astronomy: the apparent place of
//! date turned into the Earth's own frame by the apparent sidereal time,
//! then seen from a place — the topocentric direction is the difference of
//! two positions, so the Moon's parallax (up to a degree near the horizon)
//! comes out of it exactly. The phase is the angle at the Moon between the
//! Sun and the eye, the Sun from [`crate::Sun`] at one astronomical unit.
//!
//! # Why `astro`
//!
//! Of the crates that compute the Moon (`astro`, `lunar-position`,
//! `solar-ephemeris`, `practical-astronomy-rust`, `astronav`, bindings to
//! C libraries), `astro` is the one in wide use — some two hundred thousand
//! downloads, MIT, pure Rust with no dependency, and it builds for wasm32.
//! It has not changed since 2016; neither has the series it evaluates. The
//! younger crates are a few months old with a handful of users; the C
//! bindings do not build for the browser. One thing it does not give: the
//! bit-identical arithmetic `solar-positioning` has through `libm` — `astro`
//! calls the platform's `sin` and `cos`, which may differ in the last bit
//! between targets. A picture cannot show a difference of 10⁻¹⁶ rad.
//!
//! The crate is taken from lapoule-dev's fork, pinned: the release's
//! `mn_oblq_IAU` evaluated the IAU 1980 polynomial in ten thousand years
//! instead of centuries (2.5″ of declination in Meeus's example). The mean
//! obliquity used here is Laskar's all the same — good to 0.01″ over a
//! thousand years where the IAU 1980 formula drifts to 1″ over two thousand,
//! and the one `astro`'s own apparent sidereal time is written with — so the
//! Moon's place and the stars' turn agree on one obliquity.
//!
//! # Light
//!
//! [`moon_irradiance`] is the Moon's illuminance outside the air against the
//! Sun's, by Allen's magnitude law (*Astrophysical Quantities*: −12.73 at
//! full, and `0.026·α + 4·10⁻⁹·α⁴` magnitudes fainter at a phase angle of α
//! degrees, the opposition surge included), scaled by the square of the
//! mean distance over the distance: 2.5·10⁻⁶ of the Sun at full, about a
//! quarter of a lux.

use glam::{DMat3, DVec3};
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};

use crate::sun::Sun;

const UNIX_EPOCH_JD: f64 = 2_440_587.5;
const SECONDS_PER_DAY: f64 = 86_400.0;
const FALLBACK_DELTA_T: f64 = 69.0;
/// The Moon's mean radius, metres (IAU).
pub const MOON_RADIUS: f64 = 1_737_400.0;
/// The Moon's mean distance from the Earth's centre, metres.
pub const MOON_MEAN_DISTANCE: f64 = 384_400_000.0;
/// The astronomical unit, metres.
pub const ASTRONOMICAL_UNIT: f64 = 149_597_870_700.0;
/// The Sun's apparent magnitude, and the full Moon's at its mean distance.
const SUN_MAGNITUDE: f64 = -26.74;
const FULL_MOON_MAGNITUDE: f64 = -12.73;

/// The Moon from the Earth's centre at one instant, in the Earth's frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Moon {
    /// Its centre, ECEF metres (apparent place of date, true equator).
    pub position_ecef: DVec3,
}

impl Moon {
    /// The Moon at a UTC instant, seconds since the Unix epoch. `None` for
    /// an instant that is not finite.
    pub fn at_unix_seconds(utc: f64) -> Option<Self> {
        if !utc.is_finite() {
            return None;
        }
        let jd = UNIX_EPOCH_JD + utc / SECONDS_PER_DAY;
        let jde = jd + delta_t(jd) / SECONDS_PER_DAY;
        let (ra, dec, distance) = apparent_equatorial(jde);
        let gast = apparent_sidereal_time(jd, jde);
        let h = ra - gast;
        Some(Self {
            position_ecef: DVec3::new(dec.cos() * h.cos(), dec.cos() * h.sin(), dec.sin())
                * distance,
        })
    }

    /// The Moon at a UTC instant as it stands over the place `at`.
    pub fn seen_from(utc: f64, at: Geodetic) -> Option<MoonSeen> {
        let moon = Self::at_unix_seconds(utc)?;
        let sun = Sun::at_unix_seconds(utc).direction_ecef * ASTRONOMICAL_UNIT;
        if !sun.is_finite() {
            return None;
        }
        let eye = geodetic_to_ecef(at);
        let to_moon = moon.position_ecef - eye;
        let distance = to_moon.length();
        let direction = to_moon / distance;
        let local = enu_frame(at);
        let (east, north, up) = (
            direction.dot(local.x_axis),
            direction.dot(local.y_axis),
            direction.dot(local.z_axis),
        );
        let phase_angle = (sun - moon.position_ecef).angle_between(eye - moon.position_ecef);
        Some(MoonSeen {
            azimuth_deg: east.atan2(north).to_degrees().rem_euclid(360.0),
            elevation_deg: up.clamp(-1.0, 1.0).asin().to_degrees(),
            direction_ecef: direction,
            distance_m: distance,
            angular_radius: (MOON_RADIUS / distance).asin(),
            phase_angle,
            illuminated_fraction: 0.5 * (1.0 + phase_angle.cos()),
        })
    }
}

/// The Moon from one place.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoonSeen {
    /// Degrees from north, clockwise through east.
    pub azimuth_deg: f64,
    /// Degrees above the horizon, geometric (no refraction), topocentric.
    pub elevation_deg: f64,
    /// Unit vector towards the Moon's centre from the place, ECEF.
    pub direction_ecef: DVec3,
    /// From the place to the Moon's centre, metres.
    pub distance_m: f64,
    /// The disc's angular radius, radians: about a quarter of a degree.
    pub angular_radius: f64,
    /// The angle at the Moon between the Sun and the place, radians: 0 at
    /// full, π at new.
    pub phase_angle: f64,
    /// The share of the disc lit, 0 to 1.
    pub illuminated_fraction: f64,
}

/// The Moon's illuminance outside the air on a surface facing it, against
/// the Sun's, at `phase_angle` (radians) and `distance` metres: Allen's
/// magnitude law, see the module.
pub fn moon_irradiance(phase_angle: f64, distance: f64) -> f64 {
    let a = phase_angle.abs().to_degrees().min(180.0);
    let magnitude = FULL_MOON_MAGNITUDE + 0.026 * a + 4e-9 * a.powi(4);
    let mean = 10f64.powf(-0.4 * (magnitude - SUN_MAGNITUDE));
    mean * (MOON_MEAN_DISTANCE / distance.max(1.0)).powi(2)
}

/// The mean over the Moon's disc, seen at `phase_angle` (radians), of the
/// Lommel-Seeliger shading `μ₀/(μ₀ + μ)` a renderer draws it with: what
/// that shading is divided by so that the disc sends [`moon_irradiance`]
/// in all. Summed over a grid of the disc.
pub fn disc_shading_mean(phase_angle: f64) -> f64 {
    // The eye along −z; the sun at the phase angle from it.
    let sun = DVec3::new(phase_angle.sin(), 0.0, phase_angle.cos());
    let n = 96;
    let (mut sum, mut count) = (0.0, 0usize);
    for j in 0..n {
        for i in 0..n {
            let x = (i as f64 + 0.5) / n as f64 * 2.0 - 1.0;
            let y = (j as f64 + 0.5) / n as f64 * 2.0 - 1.0;
            let r2 = x * x + y * y;
            if r2 >= 1.0 {
                continue;
            }
            let mu = (1.0 - r2).sqrt();
            let mu0 = DVec3::new(x, y, mu).dot(sun);
            if mu0 > 0.0 {
                sum += mu0 / (mu0 + mu);
            }
            count += 1;
        }
    }
    (sum / count.max(1) as f64).max(1e-6)
}

/// The rotation from the celestial frame of date (true equator, equinox)
/// to the Earth's at a UTC instant: what turns the stars once a sidereal
/// day.
pub fn celestial_to_ecef(utc: f64) -> DMat3 {
    let jd = UNIX_EPOCH_JD + utc / SECONDS_PER_DAY;
    let jde = jd + delta_t(jd) / SECONDS_PER_DAY;
    DMat3::from_rotation_z(-apparent_sidereal_time(jd, jde))
}

fn delta_t(jd: f64) -> f64 {
    let year = 2000.0 + (jd - 2_451_545.0) / 365.25;
    solar_positioning::delta_t::estimate(year).unwrap_or(FALLBACK_DELTA_T)
}

/// Apparent right ascension and declination of date (radians) and the
/// distance from the Earth's centre (metres), at Julian Ephemeris Day
/// `jde`: Meeus ch. 47, by `astro`.
fn apparent_equatorial(jde: f64) -> (f64, f64, f64) {
    let (ecliptic, distance_km) = astro::lunar::geocent_ecl_pos(jde);
    let (nutation_longitude, nutation_obliquity) = astro::nutation::nutation(jde);
    let obliquity = astro::ecliptic::mn_oblq_laskar(jde) + nutation_obliquity;
    let longitude = ecliptic.long + nutation_longitude;
    let ra = astro::coords::asc_frm_ecl(longitude, ecliptic.lat, obliquity);
    let dec = astro::coords::dec_frm_ecl(longitude, ecliptic.lat, obliquity);
    (ra, dec, distance_km * 1000.0)
}

/// Greenwich apparent sidereal time (radians) at Julian Day `jd` (UT),
/// its nutation at `jde`.
fn apparent_sidereal_time(jd: f64, jde: f64) -> f64 {
    let (nutation_longitude, nutation_obliquity) = astro::nutation::nutation(jde);
    let obliquity = astro::ecliptic::mn_oblq_laskar(jde) + nutation_obliquity;
    astro::time::apprnt_sidr(astro::time::mn_sidr(jd), nutation_longitude, obliquity)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deg(lon: f64, lat: f64, height: f64) -> Geodetic {
        Geodetic {
            lon: lon.to_radians(),
            lat: lat.to_radians(),
            height,
        }
    }

    /// Unix seconds of a UTC instant (civil-from-days, as in `sun`).
    fn utc(year: i64, month: i64, day: i64, hour: i64, minute: i64) -> f64 {
        let y = if month <= 2 { year - 1 } else { year };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = (month + 9) % 12;
        let doy = (153 * mp + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        (days * 86_400 + hour * 3_600 + minute * 60) as f64
    }

    /// Meeus's worked example 47.a: 1992 April 12, 0h TD. Apparent
    /// α = 134.688470°, δ = 13.768368°, Δ = 368 409.7 km.
    #[test]
    fn the_apparent_place_is_meeuss_worked_example() {
        let (ra, dec, distance) = apparent_equatorial(2_448_724.5);
        assert!(
            (ra.to_degrees() - 134.688_470).abs() < 2e-4,
            "α {}",
            ra.to_degrees()
        );
        assert!(
            (dec.to_degrees() - 13.768_368).abs() < 2e-4,
            "δ {}",
            dec.to_degrees()
        );
        assert!((distance - 368_409_700.0).abs() < 200.0, "Δ {distance}");
    }

    /// The full Moon of 2026-10-26 04:11 UTC (published by timeanddate.com
    /// and the Royal Observatory Greenwich): the Moon's ecliptic longitude
    /// is then the Sun's plus 180°, the disc all but wholly lit; a week
    /// either side, half; at the new Moon of 2026-11-09 07:02, nothing.
    #[test]
    fn the_full_moon_of_october_2026_is_full_when_the_almanacs_say() {
        let place = deg(0.5, 43.0, 0.0);
        let full = utc(2026, 10, 26, 4, 11);
        let elongation = |t: f64| {
            // The ecliptic's pole, of date, in the Earth's frame.
            let e = astro::ecliptic::mn_oblq_laskar(UNIX_EPOCH_JD + t / SECONDS_PER_DAY);
            let pole = celestial_to_ecef(t) * DVec3::new(0.0, -e.sin(), e.cos());
            let flat = |v: DVec3| (v - pole * v.dot(pole)).normalize();
            let moon = flat(Moon::at_unix_seconds(t).expect("moon").position_ecef);
            let sun = flat(Sun::at_unix_seconds(t).direction_ecef);
            moon.angle_between(sun).to_degrees()
        };
        // The Moon moves half a degree an hour: 0.05° is six minutes.
        assert!(
            (elongation(full) - 180.0).abs() < 0.05,
            "{}",
            elongation(full)
        );
        assert!(elongation(full - 3_600.0) < elongation(full));
        let at = |t: f64| Moon::seen_from(t, place).expect("moon");
        assert!(
            at(full).illuminated_fraction > 0.99,
            "{}",
            at(full).illuminated_fraction
        );
        for days in [-7.4, 7.4] {
            let f = at(full + days * SECONDS_PER_DAY).illuminated_fraction;
            assert!((f - 0.5).abs() < 0.08, "{days} d: {f}");
        }
        assert!(at(utc(2026, 11, 9, 7, 2)).illuminated_fraction < 0.01);
    }

    /// Seen from a place, the Moon is lower than from the Earth's centre by
    /// its parallax — near the horizon about the planet's radius over its
    /// distance, close to a degree.
    #[test]
    fn the_moon_seen_from_a_place_shows_its_parallax() {
        let place = deg(0.5, 43.0, 0.0);
        for hour in 0..48 {
            let t = utc(2026, 10, 20, 0, 0) + f64::from(hour) * 1800.0;
            let seen = Moon::seen_from(t, place).expect("moon");
            let centre = Moon::at_unix_seconds(t).expect("moon").position_ecef;
            let up = enu_frame(place).z_axis;
            let geocentric = centre.normalize().dot(up).asin().to_degrees();
            let shift = geocentric - seen.elevation_deg;
            let horizontal = (6_371_000.0 / seen.distance_m).asin().to_degrees();
            let expected = horizontal * seen.elevation_deg.to_radians().cos();
            assert!(
                shift > 0.0 && (shift - expected).abs() < 0.1,
                "{hour}: {shift} vs {expected}"
            );
            assert!((seen.angular_radius.to_degrees() - 0.26).abs() < 0.02);
        }
    }

    #[test]
    fn the_full_moon_gives_a_quarter_of_a_lux_and_less_as_it_wanes() {
        let full = moon_irradiance(0.0, MOON_MEAN_DISTANCE) * 128_000.0;
        assert!((0.1..=0.35).contains(&full), "{full} lx");
        let quarter = moon_irradiance(90f64.to_radians(), MOON_MEAN_DISTANCE);
        let ratio = quarter / moon_irradiance(0.0, MOON_MEAN_DISTANCE);
        // A quarter Moon is about a tenth as bright as a full one.
        assert!((0.06..=0.12).contains(&ratio), "{ratio}");
        assert!(moon_irradiance(170f64.to_radians(), MOON_MEAN_DISTANCE) < 1e-3 * full);
    }

    /// The fork's fix, held: the IAU 1980 mean obliquity of Meeus's example
    /// 22.a (1987 April 10, 0h TD) is 23°26′27.407″. The crates.io release
    /// of `astro` gives 23°26′21.508″.
    #[test]
    fn the_iau_obliquity_is_meeuss_worked_example() {
        let iau = astro::ecliptic::mn_oblq_IAU(2_446_895.5).to_degrees();
        let seconds = (iau - 23.0 - 26.0 / 60.0) * 3600.0;
        assert!((seconds - 27.407).abs() < 0.001, "{seconds}″");
        // And Laskar's, the one used, agrees there.
        let laskar = astro::ecliptic::mn_oblq_laskar(2_446_895.5).to_degrees();
        assert!((laskar - iau).abs() * 3600.0 < 0.01);
    }

    #[test]
    fn the_disc_is_shaded_half_at_full_and_less_as_it_wanes() {
        // Lommel-Seeliger at full: μ/(2μ), a half everywhere.
        assert!((disc_shading_mean(0.0) - 0.5).abs() < 1e-9);
        let quarter = disc_shading_mean(std::f64::consts::FRAC_PI_2);
        assert!(quarter < 0.25 && quarter > 0.1, "{quarter}");
        assert!(disc_shading_mean(3.1) < 0.01);
    }

    #[test]
    fn the_stars_turn_once_a_sidereal_day() {
        let t = utc(2026, 10, 20, 0, 0);
        let a = celestial_to_ecef(t) * DVec3::X;
        let b = celestial_to_ecef(t + 86_164.090_5) * DVec3::X;
        assert!(a.angle_between(b).to_degrees() < 0.01);
        // And turn west to east: the vernal point moves to lower longitude.
        let c = celestial_to_ecef(t + 3_600.0) * DVec3::X;
        assert!(a.y.atan2(a.x) - c.y.atan2(c.x) > 0.0 || (a.y.atan2(a.x) - c.y.atan2(c.x)) < -6.0);
    }
}
