// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The acceptance scenes of the sun's and the Moon's discs: four places and
//! instants where what a disc shows can be checked by eye and by measure —
//! a sunset from a summit, a full Moon rising, a thin crescent at dusk and
//! the sun going behind a ridge (`docs/explorations/sun-and-moon.md` §6.1).
//!
//! Every instant is the ephemeris's own — [`Sun`] and [`Moon`], worked out
//! here, never written down from memory — and the camera's aim follows from
//! it where the scene says so (the moonrise's heading is the Moon's
//! azimuth). The same scene is a film's plan (a tape of cameras, its clock)
//! and a viewer's script (a place, an aim and instants): one description
//! for both renderers.

use tuile_core::geo::Geodetic;

use crate::moon::Moon;
use crate::sun::Sun;

/// One acceptance scene: where the eye is, where it looks, and when.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiscScene {
    /// A short name, the scene's key in commands and file names.
    pub name: &'static str,
    /// The place, degrees, and the eye's height over the ellipsoid, metres.
    pub lon_deg: f64,
    pub lat_deg: f64,
    pub height_m: f64,
    /// The aim: degrees clockwise from north, and over the horizon
    /// (negative: down).
    pub heading_deg: f64,
    pub pitch_deg: f64,
    /// The vertical field of view, degrees.
    pub fovy_deg: f64,
    /// The first instant shown, Unix seconds UTC.
    pub start_utc: f64,
    /// Seconds of sky from the first instant shown to the last; 0 for one.
    pub span_s: f64,
    /// How many instants are shown, evenly over the span.
    pub instants: u32,
}

impl DiscScene {
    /// The place, as a geodetic position (radians).
    pub fn place(&self) -> Geodetic {
        Geodetic {
            lon: self.lon_deg.to_radians(),
            lat: self.lat_deg.to_radians(),
            height: self.height_m,
        }
    }

    /// The instants shown, Unix seconds UTC, first to last.
    pub fn times(&self) -> Vec<f64> {
        let n = self.instants.max(1);
        if n == 1 {
            return vec![self.start_utc];
        }
        (0..n)
            .map(|i| self.start_utc + self.span_s * f64::from(i) / f64::from(n - 1))
            .collect()
    }
}

/// The Pic du Midi de Bigorre's observatory: the place of three scenes.
const PIC_DU_MIDI: (f64, f64, f64) = (0.1418, 42.9365, 2_900.0);

/// 2026-06-21T12:00:00Z, 2026-10-26T00:00:00Z, 2026-11-07T00:00:00Z and
/// 2026-12-15T00:00:00Z, Unix seconds: where each search starts.
const MIDSUMMER_NOON: f64 = 1_782_043_200.0;
const FULL_MOON_DAY: f64 = 1_792_972_800.0;
const BEFORE_NEW_MOON: f64 = 1_794_009_600.0;
const MID_DECEMBER: f64 = 1_797_292_800.0;

/// The four scenes, in the study's order: `pic-du-midi-sunset`,
/// `full-moon-rise`, `crescent-at-dusk`, `sun-behind-ridge`.
pub fn disc_scenes() -> Vec<DiscScene> {
    [
        pic_du_midi_sunset(),
        full_moon_rise(),
        crescent_at_dusk(),
        sun_behind_ridge(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// The scene called `name`, if there is one.
pub fn disc_scene(name: &str) -> Option<DiscScene> {
    disc_scenes().into_iter().find(|s| s.name == name)
}

fn pic_du_midi() -> Geodetic {
    Geodetic {
        lon: PIC_DU_MIDI.0.to_radians(),
        lat: PIC_DU_MIDI.1.to_radians(),
        height: PIC_DU_MIDI.2,
    }
}

/// Midsummer's sunset from the Pic du Midi, facing west-north-west: the
/// almanac's sunset of the ephemeris ([`Sun::next_sunset`]), the sun's
/// centre then under the eye's horizontal and over the sea-level horizon's
/// dip — its lower limb on the far plain, reddened through the most air.
pub fn pic_du_midi_sunset() -> Option<DiscScene> {
    let at = Sun::next_sunset(MIDSUMMER_NOON, pic_du_midi())?;
    Some(DiscScene {
        name: "pic-du-midi-sunset",
        lon_deg: PIC_DU_MIDI.0,
        lat_deg: PIC_DU_MIDI.1,
        height_m: PIC_DU_MIDI.2,
        heading_deg: 290.0,
        pitch_deg: -3.0,
        fovy_deg: 35.0,
        start_utc: at.round(),
        span_s: 0.0,
        instants: 1,
    })
}

/// The full Moon of 2026-10-26 rising over the Pic du Midi: the instant
/// its centre crosses the eye's horizontal, east of the summit, and the
/// eye turned to it with a long lens — the Moon low, through the most air,
/// the ground under it.
pub fn full_moon_rise() -> Option<DiscScene> {
    let place = pic_du_midi();
    let elevation = |t: f64| Moon::seen_from(t, place).map(|m| m.elevation_deg);
    let at = rising(FULL_MOON_DAY, 86_400.0, 0.0, &elevation)?;
    let moon = Moon::seen_from(at, place)?;
    Some(DiscScene {
        name: "full-moon-rise",
        lon_deg: PIC_DU_MIDI.0,
        lat_deg: PIC_DU_MIDI.1,
        height_m: PIC_DU_MIDI.2,
        heading_deg: moon.azimuth_deg,
        pitch_deg: -2.0,
        fovy_deg: 10.0,
        start_utc: at.round(),
        span_s: 0.0,
        instants: 1,
    })
}

/// A thin crescent at dusk, west-south-west, after the new Moon of
/// 2026-11-09 (the ephemeris's: the Moon's phase angle at its largest): the
/// first evening on which, at civil dusk — the sun going down through 6°
/// under the horizon — the Moon stands 3° or more over the eye's
/// horizontal, and the eye turned to it with a long lens, the far plain's
/// horizon inside the frame's foot.
pub fn crescent_at_dusk() -> Option<DiscScene> {
    let place = pic_du_midi();
    let new = new_moon(BEFORE_NEW_MOON, 7.0 * 86_400.0)?;
    let sun = |t: f64| Sun::seen_from(t, place).map(|s| -s.elevation_deg);
    // Local solar noon of the new Moon's day, then of each day after.
    let noon = (new / 86_400.0).floor() * 86_400.0 + 43_200.0 - PIC_DU_MIDI.0 / 15.0 * 3_600.0;
    let (at, moon) = (0..4).find_map(|day| {
        let from = noon + f64::from(day) * 86_400.0;
        let at = rising(from.max(new), 86_400.0, 6.0, &sun)?;
        let moon = Moon::seen_from(at, place)?;
        (moon.elevation_deg >= 3.0).then_some((at, moon))
    })?;
    Some(DiscScene {
        name: "crescent-at-dusk",
        lon_deg: PIC_DU_MIDI.0,
        lat_deg: PIC_DU_MIDI.1,
        height_m: PIC_DU_MIDI.2,
        heading_deg: moon.azimuth_deg,
        // The Moon over the frame's middle, the far plain's horizon — 1.7°
        // under the eye's — inside its foot.
        pitch_deg: (moon.elevation_deg - 3.5).min(2.0),
        fovy_deg: 10.0,
        start_utc: at.round(),
        span_s: 0.0,
        instants: 1,
    })
}

/// The sun going down behind a ridge: from the Pic du Midi in December,
/// facing the Vignemale's massif to the south-west, the sun crossing the
/// ridge line over the quarter of an hour its centre takes from 1.5° over
/// the eye's horizontal to 1° under it — what the glare and the disc's
/// edge do as the ground covers it.
pub fn sun_behind_ridge() -> Option<DiscScene> {
    let place = pic_du_midi();
    let sun = |t: f64| Sun::seen_from(t, place).map(|s| -s.elevation_deg);
    // The sun's centre going down through 1.5° over the horizontal.
    let at = rising(MID_DECEMBER + 43_200.0, 86_400.0, -1.5, &sun)?;
    let seen = Sun::seen_from(at, place)?;
    Some(DiscScene {
        name: "sun-behind-ridge",
        lon_deg: PIC_DU_MIDI.0,
        lat_deg: PIC_DU_MIDI.1,
        height_m: PIC_DU_MIDI.2,
        heading_deg: seen.azimuth_deg,
        pitch_deg: -1.0,
        fovy_deg: 10.0,
        start_utc: at.round(),
        span_s: 900.0,
        instants: 16,
    })
}

/// The first instant within `within` seconds of `from` at which `f` goes up
/// through `level`, to half a second: ten-minute steps, then halved.
fn rising(from: f64, within: f64, level: f64, f: &dyn Fn(f64) -> Option<f64>) -> Option<f64> {
    const STEP: f64 = 600.0;
    let mut t = from;
    let mut before = f(t)? - level;
    while t < from + within {
        let next = t + STEP;
        let now = f(next)? - level;
        if before < 0.0 && now >= 0.0 {
            let (mut lo, mut hi) = (t, next);
            while hi - lo > 0.5 {
                let mid = 0.5 * (lo + hi);
                if f(mid)? - level < 0.0 {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            return Some(0.5 * (lo + hi));
        }
        t = next;
        before = now;
    }
    None
}

/// The new Moon within `within` seconds of `from`: the instant the Moon's
/// phase angle, seen from the Earth's centre, is largest — hourly, then by
/// golden-section to a minute.
fn new_moon(from: f64, within: f64) -> Option<f64> {
    let phase = |t: f64| -> Option<f64> {
        let moon = Moon::at_unix_seconds(t)?.position_ecef;
        let sun = Sun::at_unix_seconds(t).direction_ecef * crate::moon::ASTRONOMICAL_UNIT;
        Some((sun - moon).angle_between(-moon))
    };
    let mut best = (from, phase(from)?);
    let mut t = from;
    while t <= from + within {
        let p = phase(t)?;
        if p > best.1 {
            best = (t, p);
        }
        t += 3_600.0;
    }
    let (mut lo, mut hi) = (best.0 - 3_600.0, best.0 + 3_600.0);
    let g = 0.5 * (5f64.sqrt() - 1.0);
    while hi - lo > 60.0 {
        let a = hi - g * (hi - lo);
        let b = lo + g * (hi - lo);
        if phase(a)? > phase(b)? {
            hi = b;
        } else {
            lo = a;
        }
    }
    Some(0.5 * (lo + hi))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::format_rfc3339;

    fn named(name: &str) -> DiscScene {
        disc_scene(name).expect("a scene of that name")
    }

    /// The instants are the ephemeris's, and stay what they were when the
    /// scenes' renders were made: a change here makes every one of them
    /// again.
    #[test]
    fn the_scenes_instants_are_the_ephemeris_s() {
        let at = |name: &str| format_rfc3339(named(name).start_utc);
        assert_eq!(at("pic-du-midi-sunset"), "2026-06-21T19:41:54Z");
        assert_eq!(at("full-moon-rise"), "2026-10-26T16:53:46Z");
        assert_eq!(at("crescent-at-dusk"), "2026-11-11T17:10:12Z");
        assert_eq!(at("sun-behind-ridge"), "2026-12-15T16:10:22Z");
        assert_eq!(
            format_rfc3339(new_moon(BEFORE_NEW_MOON, 7.0 * 86_400.0).expect("a new Moon")),
            "2026-11-09T06:52:38Z"
        );
    }

    /// Each scene shows what it is for.
    #[test]
    fn each_scene_shows_its_body_where_it_says() {
        let sunset = named("pic-du-midi-sunset");
        let sun = Sun::seen_from(sunset.start_utc, sunset.place()).expect("sun");
        assert!((sun.elevation_deg + 0.833).abs() < 0.01, "{sun:?}");

        let rise = named("full-moon-rise");
        let moon = Moon::seen_from(rise.start_utc, rise.place()).expect("moon");
        assert!(moon.elevation_deg.abs() < 0.01, "{moon:?}");
        assert!(
            moon.phase_angle.to_degrees() < 10.0,
            "a full Moon: {moon:?}"
        );
        // The instant is rounded to the second: the Moon a hundredth of a
        // degree from the aim at most.
        assert!((moon.azimuth_deg - rise.heading_deg).abs() < 0.01);

        let crescent = named("crescent-at-dusk");
        let place = crescent.place();
        let sun = Sun::seen_from(crescent.start_utc, place).expect("sun");
        let moon = Moon::seen_from(crescent.start_utc, place).expect("moon");
        assert!(
            (sun.elevation_deg + 6.0).abs() < 0.01,
            "civil dusk: {sun:?}"
        );
        assert!(moon.elevation_deg >= 3.0, "{moon:?}");
        assert!(moon.illuminated_fraction < 0.1, "a thin crescent: {moon:?}");
        // In the frame, over its middle and under its top.
        let over = moon.elevation_deg - crescent.pitch_deg;
        assert!(over > 0.0 && over < 0.5 * crescent.fovy_deg, "{over}");

        let ridge = named("sun-behind-ridge");
        let times = ridge.times();
        assert_eq!(times.len(), 16);
        let first = Sun::seen_from(times[0], ridge.place()).expect("sun");
        let last = Sun::seen_from(times[15], ridge.place()).expect("sun");
        assert!((first.elevation_deg - 1.5).abs() < 0.01, "{first:?}");
        assert!(last.elevation_deg < -0.5, "{last:?}");
    }
}
