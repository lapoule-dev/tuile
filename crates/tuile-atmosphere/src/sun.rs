// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Where the sun is, for a given instant.
//!
//! Everything else in this crate is downstream of this. The colour of the air,
//! where the terminator falls, whether a limb glows orange or not at all — all
//! of it is a function of the sun's direction, so a globe that wants to be lit
//! at a *stated* time needs the sun placed from that time rather than from a
//! direction someone picked by eye.
//!
//! # Accuracy, and what it is for
//!
//! A low-precision solar position: better than 0.01° over the years around
//! J2000, which is a few hundredths of a degree of terminator placement. The
//! high-precision algorithms exist for pointing instruments; nothing here is
//! doing that. What this has to get right is *which side of the planet is lit
//! at 18:00 UTC*, and it does that to well under a pixel.
//!
//! Not modelled: refraction near the horizon (which lifts the apparent sun by
//! about half a degree at sunset), nutation, and the equation of the equinoxes.
//! Each is smaller than the eye can read on a horizon, and each would need a
//! term nobody could check.

use glam::DVec3;
use std::f64::consts::{PI, TAU};

/// Julian date of the J2000.0 epoch, 2000-01-01 12:00 TT.
const J2000: f64 = 2_451_545.0;
/// Julian date of the Unix epoch, 1970-01-01 00:00 UTC.
const UNIX_EPOCH_JD: f64 = 2_440_587.5;
const SECONDS_PER_DAY: f64 = 86_400.0;

/// The sun, seen from Earth at one instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sun {
    /// Unit vector from the planet's centre **toward** the sun, ECEF.
    ///
    /// Toward, not along — the direction light *travels* is its negation, and
    /// mixing the two is the classic way to light a globe inside out. Renderers
    /// that want a travel direction should say so and negate here.
    pub direction_ecef: DVec3,
    /// Latitude of the point directly under the sun, radians. Between roughly
    /// ±23.44° over a year; this is what the seasons are.
    pub subsolar_lat: f64,
    /// Longitude of the same point, radians, east positive. Sweeps a full turn
    /// westward each day.
    pub subsolar_lon: f64,
}

impl Sun {
    /// The sun at a UTC instant given as seconds since the Unix epoch.
    ///
    /// Seconds rather than a date type on purpose: this crate has no business
    /// pulling in a calendar, and every caller already has one instant in some
    /// form it can turn into a number.
    pub fn at_unix_seconds(utc: f64) -> Self {
        Self::at_julian_date(UNIX_EPOCH_JD + utc / SECONDS_PER_DAY)
    }

    /// The sun at a Julian date (UTC).
    pub fn at_julian_date(jd: f64) -> Self {
        // Days since J2000. UTC is used where the algorithm asks for TT: they
        // differ by about a minute this century, which moves the sun by under
        // 0.005° — an order of magnitude below the model's own error.
        let n = jd - J2000;

        // The sun's position along the ecliptic, as a mean motion plus the two
        // largest terms of the equation of centre. Earth's orbit is very nearly
        // circular, so those two carry almost all of the departure from uniform.
        let mean_longitude = (280.460 + 0.985_647_4 * n).to_radians();
        let mean_anomaly = (357.528 + 0.985_600_3 * n).to_radians();
        let ecliptic_longitude = mean_longitude
            + (1.915_f64).to_radians() * mean_anomaly.sin()
            + (0.020_f64).to_radians() * (2.0 * mean_anomaly).sin();

        // The tilt of the axis: the only reason the subsolar point leaves the
        // equator at all, and therefore the only reason there are seasons.
        let obliquity = (23.439 - 4.0e-7 * n).to_radians();

        let declination = (obliquity.sin() * ecliptic_longitude.sin()).asin();
        let right_ascension =
            (obliquity.cos() * ecliptic_longitude.sin()).atan2(ecliptic_longitude.cos());

        // Which meridian faces the sun is a question about the planet's own
        // rotation, so it needs sidereal time — the solar day is four minutes
        // longer than a rotation, and using it instead would drift the
        // terminator by a degree a day.
        let gmst = (280.460_618_378 + 360.985_647_366_29 * n).to_radians();
        let subsolar_lon = wrap_to_pi(right_ascension - gmst);
        let subsolar_lat = declination;

        Self {
            direction_ecef: DVec3::new(
                subsolar_lat.cos() * subsolar_lon.cos(),
                subsolar_lat.cos() * subsolar_lon.sin(),
                subsolar_lat.sin(),
            ),
            subsolar_lat,
            subsolar_lon,
        }
    }

    /// A sun placed by hand, from a direction that already points at it.
    ///
    /// For scenes that are not meant to be a real moment — a look someone chose
    /// rather than a time someone stated. Kept distinct from the ephemeris so
    /// that a render's lighting is never ambiguous about which it was.
    pub fn from_direction(direction_ecef: DVec3) -> Self {
        let d = direction_ecef.normalize_or(DVec3::X);
        Self {
            direction_ecef: d,
            subsolar_lat: d.z.clamp(-1.0, 1.0).asin(),
            subsolar_lon: d.y.atan2(d.x),
        }
    }

    /// How high the sun stands above the horizon at a point on the ground,
    /// radians. Negative is night; near zero is the terminator, where every
    /// interesting atmospheric effect lives.
    pub fn elevation_at(&self, up_ecef: DVec3) -> f64 {
        self.direction_ecef
            .dot(up_ecef.normalize_or(DVec3::Z))
            .clamp(-1.0, 1.0)
            .asin()
    }

    /// The direction sunlight **travels** — what a renderer's light vector
    /// usually wants.
    pub fn light_travel_direction(&self) -> DVec3 {
        -self.direction_ecef
    }
}

fn wrap_to_pi(radians: f64) -> f64 {
    let wrapped = radians.rem_euclid(TAU);
    if wrapped > PI {
        wrapped - TAU
    } else {
        wrapped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seconds since the Unix epoch for a UTC calendar instant. Written out
    /// rather than pulled from a date crate so the test states its own inputs.
    fn utc(year: i64, month: i64, day: i64, hour: i64, minute: i64) -> f64 {
        // Days from 1970-01-01 to the given date, by the civil-from-days
        // algorithm (Howard Hinnant's), which is exact and has no leap-year
        // special cases to get wrong.
        let y = if month <= 2 { year - 1 } else { year };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = (month + 9) % 12;
        let doy = (153 * mp + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        (days * 86_400 + hour * 3_600 + minute * 60) as f64
    }

    /// The June solstice is *defined* by the subsolar point reaching its
    /// northern limit, one axial tilt above the equator. If this drifts, the
    /// seasons are wrong and so is every terminator in June.
    #[test]
    fn the_june_solstice_puts_the_sun_over_the_tropic_of_cancer() {
        // 2024-06-20 20:51 UTC.
        let sun = Sun::at_unix_seconds(utc(2024, 6, 20, 20, 51));
        let lat = sun.subsolar_lat.to_degrees();
        assert!(
            (lat - 23.44).abs() < 0.05,
            "subsolar latitude {lat}°, expected the tropic of Cancer"
        );
    }

    /// And the December solstice mirrors it. Testing both catches a sign error
    /// in the obliquity that a single solstice would let through.
    #[test]
    fn the_december_solstice_mirrors_it() {
        // 2024-12-21 09:20 UTC.
        let sun = Sun::at_unix_seconds(utc(2024, 12, 21, 9, 20));
        let lat = sun.subsolar_lat.to_degrees();
        assert!(
            (lat + 23.44).abs() < 0.05,
            "subsolar latitude {lat}°, expected the tropic of Capricorn"
        );
    }

    /// At an equinox the sun stands over the equator, whatever the time of day.
    #[test]
    fn at_an_equinox_the_sun_is_over_the_equator_all_day() {
        // 2024-03-20 03:06 UTC is the March equinox.
        for hour in 0..24 {
            let sun = Sun::at_unix_seconds(utc(2024, 3, 20, 3, 6) + f64::from(hour) * 3600.0);
            let lat = sun.subsolar_lat.to_degrees();
            assert!(
                lat.abs() < 0.45,
                "{hour} h after the equinox the subsolar latitude is {lat}°"
            );
        }
    }

    /// Local noon at Greenwich puts the sun near the prime meridian. Not exactly
    /// on it — the equation of time swings solar noon by up to a quarter of an
    /// hour across the year, which is about 4° of longitude, and reproducing
    /// that swing is most of what the equation of centre above is for.
    #[test]
    fn the_sun_is_near_the_prime_meridian_at_noon_utc() {
        for (month, day) in [(1, 15), (4, 15), (7, 15), (10, 15)] {
            let sun = Sun::at_unix_seconds(utc(2024, month, day, 12, 0));
            let lon = sun.subsolar_lon.to_degrees();
            assert!(
                lon.abs() < 5.0,
                "{month}/{day} noon UTC puts the sun at {lon}° instead of near 0°"
            );
        }
    }

    /// One solar day returns the sun to the same meridian. The point of using
    /// sidereal time is that this stays true; mean solar time would drift the
    /// subsolar longitude by about a degree a day.
    #[test]
    fn a_solar_day_brings_the_sun_back_to_the_same_meridian() {
        let start = utc(2024, 5, 1, 12, 0);
        let a = Sun::at_unix_seconds(start);
        let b = Sun::at_unix_seconds(start + 86_400.0);
        let drift = wrap_to_pi(b.subsolar_lon - a.subsolar_lon).to_degrees();
        assert!(drift.abs() < 0.5, "a day moved the meridian by {drift}°");
    }

    /// The direction light travels is the negation of the direction to the sun.
    /// Getting this backwards lights a globe on its night side, and the render
    /// looks plausible until you notice the terminator runs the wrong way.
    #[test]
    fn the_travel_direction_is_the_negation_of_the_direction_to_the_sun() {
        let sun = Sun::at_unix_seconds(utc(2024, 7, 1, 15, 0));
        assert!((sun.light_travel_direction() + sun.direction_ecef).length() < 1e-12);
        // The subsolar point has the sun overhead by construction.
        let overhead = sun.elevation_at(sun.direction_ecef).to_degrees();
        assert!(
            (overhead - 90.0).abs() < 1e-6,
            "{overhead}° at the subsolar point"
        );
        // And its antipode has it exactly underfoot.
        let midnight = sun.elevation_at(-sun.direction_ecef).to_degrees();
        assert!(
            (midnight + 90.0).abs() < 1e-6,
            "{midnight}° at the antipode"
        );
    }
}
