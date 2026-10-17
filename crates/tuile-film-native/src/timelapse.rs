// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A time-lapse's tape, made where the ephemerides are: the cameras of
//! [`tuile_tape::timelapse::TimeLapse`], a tracking camera turned to the
//! sun and the Moon of `tuile-atmosphere`, and the frames' instants in the
//! tape itself ([`tuile_tape::Tape::set_clock`]).

use std::path::Path;

use tuile_core::geo::Geodetic;
use tuile_tape::timelapse::{Seen, Sky, TimeLapse};
use tuile_tape::Tape;

/// The sun and the Moon of `tuile-atmosphere`: SPA, and Meeus through
/// `astro`, topocentric.
pub struct Ephemerides;

impl Sky for Ephemerides {
    fn sun(&self, utc: f64, at: Geodetic) -> Option<Seen> {
        tuile_atmosphere::Sun::seen_from(utc, at).map(|s| Seen {
            azimuth_deg: s.azimuth_deg,
            elevation_deg: s.elevation_deg,
        })
    }

    fn moon(&self, utc: f64, at: Geodetic) -> Option<Seen> {
        tuile_atmosphere::Moon::seen_from(utc, at).map(|m| Seen {
            azimuth_deg: m.azimuth_deg,
            elevation_deg: m.elevation_deg,
        })
    }
}

/// Writes `lapse`'s tape at `path`, its clock in it. Returns the frames.
pub fn write_tape(lapse: &TimeLapse, path: &Path) -> Result<u64, String> {
    let cameras = lapse.cameras(Some(&Ephemerides))?;
    let mut tape = Tape::recording(path).map_err(|e| e.to_string())?;
    tape.set_clock(lapse.clock()).map_err(|e| e.to_string())?;
    for camera in cameras {
        tape.push(camera);
    }
    tape.finish().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;
    use tuile_core::geo::enu_frame;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn a_tape_states_its_frames_instants() {
        let lapse =
            TimeLapse::parse(&words("start=2026-10-19T00:00Z hours=1 length=1 fps=10")).unwrap();
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("t.mcap");
        assert_eq!(write_tape(&lapse, &path).expect("written"), 10);
        let clock = tuile_tape::Clock::of_tape(&path)
            .expect("read")
            .expect("a clock");
        assert_eq!(clock, lapse.clock());
        assert_eq!(clock.seconds_per_frame, 360.0);
    }

    #[test]
    fn a_tracking_camera_keeps_the_sun_by_day_and_the_moon_by_night_in_frame() {
        let t = TimeLapse::parse(&words(
            "place=0.1423,42.9364 height=4000 start=2026-10-19T00:00Z days=14 length=120 fps=60 \
             track=sun-moon pitch=-4 fov=80",
        ))
        .unwrap();
        assert!(t.cameras(None).is_err(), "no ephemerides, no tracking");
        let cameras = t.cameras(Some(&Ephemerides)).expect("cameras");
        let place = Geodetic {
            lon: t.lon_deg.to_radians(),
            lat: t.lat_deg.to_radians(),
            height: t.height_m,
        };
        let half_h = ((t.fovy_deg.to_radians() / 2.0).tan() * 16.0 / 9.0).atan();
        let (mut sun_days, mut sun_seen, mut moon_nights, mut moon_seen, mut ground) =
            (0, 0, 0, 0, 0);
        for (i, c) in cameras.iter().enumerate().step_by(7) {
            let time = t.clock().instant(i as u32 + 1);
            let d = DVec3::from_array(c.direction);
            let up = DVec3::from_array(c.up);
            let right = d.cross(up);
            let inside = |to: DVec3| {
                let (x, y, z) = (to.dot(right), to.dot(up), to.dot(d));
                z > 0.0 && (x / z).atan().abs() < half_h && (y / z).atan().abs() < c.fovy / 2.0
            };
            let sun = tuile_atmosphere::Sun::seen_from(time, place).unwrap();
            let moon = tuile_atmosphere::Moon::seen_from(time, place).unwrap();
            if sun.elevation_deg > 1.0 {
                sun_days += 1;
                sun_seen += usize::from(inside(sun.direction_ecef));
            } else if sun.elevation_deg < -12.0 && (2.0..50.0).contains(&moon.elevation_deg) {
                moon_nights += 1;
                moon_seen += usize::from(inside(moon.direction_ecef));
            }
            let bottom = (d * (c.fovy / 2.0).cos() - up * (c.fovy / 2.0).sin()).normalize();
            ground += usize::from(bottom.dot(enu_frame(place).z_axis) < -0.05);
        }
        assert!(
            sun_seen as f64 > 0.95 * sun_days as f64,
            "sun {sun_seen} of {sun_days}"
        );
        assert!(
            moon_seen as f64 > 0.9 * moon_nights as f64,
            "moon {moon_seen} of {moon_nights}"
        );
        assert_eq!(ground, cameras.len().div_ceil(7));
    }
}
