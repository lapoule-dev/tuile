// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What one moment's sun and sky give a scene, and how a camera meets it:
//! the `ReferenceIllumination` of `docs/18-color-harmonization.md` §6, as
//! amended by `docs/19-sky-atmosphere.md` §5.2 — the numbers a renderer is
//! lit by, frame by frame, whatever model made them.
//!
//! # Units
//!
//! Linear Rec.709 RGB on the renderer's own scale, the one its look's lights
//! are written in: today, a white Lambert surface facing the sun comes out
//! at `sun_rgb`. Exposure is in EV100, and the renderer's look is exposed
//! for [`DAYLIGHT_EV100`]: an illumination at that EV100 with no
//! compensation leaves the look's exposure as it is.
//!
//! # Physical or provisional
//!
//! [`crate::LutAtmosphere::light`] works these numbers out from the air
//! (Hillaire 2020). [`ReferenceIllumination::provisional`] stands in where
//! no atmosphere is drawn: the colour and strength of the light are then the
//! look's own constants, and only the sun's direction follows the hour.
//!
//! # How the camera meets the light
//!
//! Like a camera on automatic, but not all the way, and the way an eye
//! adapts at night: a light brighter than daylight is exposed down by
//! [`EXPOSURE_FOLLOWS`] of its difference; a dimmer one is shown darker by
//! a share that shrinks as the light falls ([`shown_ev`]) — a sunset a
//! stop and a half under noon, a moonlit night three, a moonless one four.
//! One smooth curve from noon to the darkest night: no cap to hit, no
//! step, nothing for the exposure to pump about. Its white balance goes
//! [`ReferenceIllumination::WHITE_ADAPTATION`] of the way to the light's
//! white: dawn and dusk stay warm (`docs/19-sky-atmosphere.md` §4.6).
//!
//! # The Moon
//!
//! [`Moonlight`] is the second light of a frame, beside the sun: where the
//! Moon stands, how much of it is lit and how large it is — what a host
//! stores frame by frame, as it stores the sun — and what it gives a
//! surface facing it, through the air.

use glam::{DVec3, Vec3};

/// The EV100 a renderer's look is exposed for: full daylight, the "sunny
/// 16" rule's 15. An illumination brighter than that is exposed down by the
/// difference, a dimmer one up.
pub const DAYLIGHT_EV100: f32 = 15.0;

/// How much of a light's difference from daylight, in stops, the camera's
/// exposure makes up.
pub const EXPOSURE_FOLLOWS: f32 = 0.75;

/// How dark the darkest night is shown: the scale, in stops, of
/// [`shown_ev`]'s logarithm.
pub const NIGHT_DEPTH_EV: f32 = 3.0;

/// How much darker than daylight, in stops, a light `ev` stops from
/// daylight's is shown (`ev` negative for a dimmer light): `(1 − F)·ev`
/// above daylight, with `F` [`EXPOSURE_FOLLOWS`]; below, `−D·log₂(1 +
/// |ev|/b)`, `D` [`NIGHT_DEPTH_EV`], `b` chosen so that the two meet with
/// the same slope. Smooth and increasing: a sunset (−8) is shown 1.7 stops
/// under noon, the end of civil twilight (−15) 2.7, a full Moon (−19) 3.1,
/// a moonless night (−26) 4.
pub fn shown_ev(ev: f32) -> f32 {
    let kept = 1.0 - EXPOSURE_FOLLOWS;
    if ev >= 0.0 {
        return kept * ev;
    }
    let b = NIGHT_DEPTH_EV / (kept * std::f32::consts::LN_2);
    -NIGHT_DEPTH_EV * (1.0 - ev / b).log2()
}

/// What a moment's sun and sky give a scene, and how the camera meets it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReferenceIllumination {
    /// Unit vector towards the sun, ECEF.
    pub to_sun_ecef: DVec3,
    /// The sun's irradiance on a surface facing it, through the air.
    pub sun_rgb: [f32; 3],
    /// The sky's irradiance as an L1 spherical harmonic, its constants
    /// folded in: a surface of normal `n` (ECEF axes) receives
    /// `sky_sh[0] + sky_sh[1]·n.x + sky_sh[2]·n.y + sky_sh[3]·n.z`.
    /// `sky_sh[0]` alone is the sky averaged over the facings of a wall all
    /// round — what a renderer with one ambient term uses; level ground
    /// receives `sky_sh[0] + sky_sh[·]·up`.
    pub sky_sh: [[f32; 3]; 4],
    /// The white the camera balances towards: the colour a white surface
    /// takes under this light. Equal channels: no cast.
    pub white: [f32; 3],
    /// How far the camera adapts to `white`: 0 not at all, 1 fully. Kept
    /// below 1 so that dawn and dusk stay warmer than noon.
    pub adaptation: f32,
    /// The exposure the light calls for, EV100, physical.
    pub ev100: f32,
    /// The exposure chosen on top, in stops: a shot's choice, not the
    /// light's.
    pub compensation_ev: f32,
    /// The least share of daylight's irradiance the ground is let fall to,
    /// already applied to `sun_rgb` and `sky_sh` by whatever made them:
    /// night is dim, never black.
    pub night_floor: f32,
    /// The Moon, when it is placed: `None` lights by the sun alone.
    pub moon: Option<Moonlight>,
}

/// The Moon of one frame: where it is, its phase and its size — what a
/// host stores — and the light it gives.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Moonlight {
    /// Unit vector towards the Moon from the place the light is for, ECEF.
    pub to_moon_ecef: DVec3,
    /// The angle at the Moon between the Sun and the place, radians: 0
    /// full, π new.
    pub phase_angle: f32,
    /// The disc's angular radius, radians.
    pub angular_radius: f32,
    /// The Moon's irradiance on a surface facing it, through the air, in
    /// the renderer's units — as `sun_rgb`. Zero until a model works it out
    /// ([`crate::LutAtmosphere::light`]).
    pub moon_rgb: [f32; 3],
}

impl Moonlight {
    /// The Moon `seen` from a place, its light not yet worked out.
    pub fn of(seen: &crate::MoonSeen) -> Self {
        Self {
            to_moon_ecef: seen.direction_ecef,
            phase_angle: seen.phase_angle as f32,
            angular_radius: seen.angular_radius as f32,
            moon_rgb: [0.0; 3],
        }
    }

    /// The share of the disc lit.
    pub fn illuminated_fraction(&self) -> f32 {
        0.5 * (1.0 + self.phase_angle.cos())
    }

    /// The Moon's irradiance outside the air, against the Sun's
    /// ([`crate::moon_irradiance`]), its distance read from its size.
    pub fn outside_air(&self) -> f64 {
        let distance = crate::moon::MOON_RADIUS / f64::from(self.angular_radius).sin().max(1e-9);
        crate::moon_irradiance(f64::from(self.phase_angle), distance)
    }
}

impl ReferenceIllumination {
    /// The night floor: the light of a clear moonless night — starlight
    /// and airglow, about 0.002 lx — over the reference daylight on level
    /// ground, about 100 klx. Under [`shown_ev`], ground lit by it alone is
    /// shown four stops under noon: dark, never black.
    pub const NIGHT_FLOOR: f32 = 2e-8;

    /// The least brightness, on the picture, of ground lit by the floor,
    /// against noon's: what the floor is raised to where the camera is
    /// exposed for brighter ground in the same frame — a night side seen
    /// beside a day side: dark blue-grey, a sixteenth of noon.
    pub const SHOWN_FLOOR: f32 = 1.0 / 16.0;

    /// How far the camera's white balance goes towards a light's white.
    pub const WHITE_ADAPTATION: f32 = 0.6;

    /// **Provisional**, until an atmosphere model computes the light: the
    /// sun at `to_sun_ecef`, and the light the renderer's look already
    /// has — `sun_rgb` and `sky_rgb` as the look states them, the camera
    /// exposed for daylight and not balanced. Night is never black under
    /// it: the sky's constant stays whatever the sun's elevation, and the
    /// ground lit by it alone is the floor.
    pub fn provisional(to_sun_ecef: DVec3, sun_rgb: [f32; 3], sky_rgb: [f32; 3]) -> Self {
        Self {
            to_sun_ecef,
            sun_rgb,
            sky_sh: [sky_rgb, [0.0; 3], [0.0; 3], [0.0; 3]],
            white: [1.0; 3],
            adaptation: 0.0,
            ev100: DAYLIGHT_EV100,
            compensation_ev: 0.0,
            night_floor: Self::NIGHT_FLOOR,
            moon: None,
        }
    }

    /// The light of the instant `utc` (Unix seconds) at the place `at`,
    /// provisional as [`Self::provisional`]: the sun and the Moon placed
    /// from their ephemerides, the strength of the light the look's.
    /// `None` for an instant or a place the ephemerides refuse.
    pub fn of_instant(
        utc: f64,
        at: tuile_core::geo::Geodetic,
        sun_rgb: [f32; 3],
        sky_rgb: [f32; 3],
    ) -> Option<Self> {
        let sun = crate::Sun::seen_from(utc, at)?;
        let moon = crate::Moon::seen_from(utc, at)?;
        Some(Self {
            moon: Some(Moonlight::of(&moon)),
            ..Self::provisional(sun.direction_ecef, sun_rgb, sky_rgb)
        })
    }

    /// The sky averaged over every facing: [`Self::sky_sh`]'s first term.
    pub fn sky_ambient(&self) -> [f32; 3] {
        self.sky_sh[0]
    }

    /// Stops to add to a look exposed for daylight: the compensation, and
    /// what brings the light from its own brightness to the one
    /// [`shown_ev`] shows it at.
    pub fn exposure_offset_ev(&self) -> f32 {
        let metered = self.ev100 - DAYLIGHT_EV100;
        self.compensation_ev + shown_ev(metered) - metered
    }

    /// The camera's white balance, as gains on the picture's light: none
    /// at no adaptation; at full, what makes `white` grey at its own
    /// luminance (a von Kries scaling in RGB).
    pub fn white_balance(&self) -> [f32; 3] {
        let white = Vec3::from_array(self.white).max(Vec3::splat(1e-6));
        let luma = white.dot(Vec3::new(0.2126, 0.7152, 0.0722));
        let full = Vec3::splat(luma) / white;
        let a = self.adaptation.clamp(0.0, 1.0);
        (Vec3::ONE + (full - Vec3::ONE) * a).to_array()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_provisional_light_is_the_looks_and_changes_no_exposure_or_colour() {
        let light =
            ReferenceIllumination::provisional(DVec3::Z, [2.5, 2.45, 2.3], [0.7, 0.76, 0.8]);
        assert_eq!(light.exposure_offset_ev(), 0.0);
        assert_eq!(light.white_balance(), [1.0; 3]);
        assert_eq!(light.sky_ambient(), [0.7, 0.76, 0.8]);
        assert!(light.night_floor > 0.0);
    }

    #[test]
    fn a_brighter_light_is_exposed_down_and_a_compensation_added() {
        let mut light = ReferenceIllumination::provisional(DVec3::Z, [1.0; 3], [1.0; 3]);
        light.ev100 = DAYLIGHT_EV100 + 2.0;
        light.compensation_ev = 0.5;
        assert!((light.exposure_offset_ev() - (0.5 - 2.0 * EXPOSURE_FOLLOWS)).abs() < 1e-6);
    }

    #[test]
    fn a_dim_light_is_shown_darker_and_darker_on_one_smooth_curve() {
        let mut light = ReferenceIllumination::provisional(DVec3::Z, [1.0; 3], [1.0; 3]);
        let shown = |light: &ReferenceIllumination| {
            light.ev100 - DAYLIGHT_EV100 + light.exposure_offset_ev()
        };
        // Near daylight, as before: three quarters of the difference made up.
        light.ev100 = DAYLIGHT_EV100 - 0.1;
        assert!((light.exposure_offset_ev() - 0.1 * EXPOSURE_FOLLOWS).abs() < 2e-3);
        // Darker on the picture as the light falls, never by a step, and
        // never anything like the light's own fall: a moonless night is
        // shown about four stops under noon.
        let mut last = (0.25f32, f32::INFINITY);
        for tenth in 0..=320 {
            light.ev100 = DAYLIGHT_EV100 + 2.0 - tenth as f32 / 10.0;
            let s = shown(&light);
            assert!(s < last.1, "{}: {s}", light.ev100);
            if tenth > 0 {
                let slope = (last.1 - s) / 0.1;
                assert!(
                    slope <= 0.2502 && (slope - last.0).abs() < 0.01,
                    "{}: {slope}",
                    light.ev100
                );
                last.0 = slope;
            }
            last.1 = s;
        }
        light.ev100 = DAYLIGHT_EV100 - 26.0;
        assert!((-4.3..=-3.6).contains(&shown(&light)), "{}", shown(&light));
        light.ev100 = DAYLIGHT_EV100 - 8.0;
        assert!((-2.0..=-1.3).contains(&shown(&light)), "{}", shown(&light));
    }

    #[test]
    fn a_full_moon_gives_a_quarter_of_a_lux_and_a_quarter_moon_a_tenth_of_it() {
        let full = Moonlight {
            to_moon_ecef: DVec3::Z,
            phase_angle: 0.0,
            angular_radius: (crate::moon::MOON_RADIUS / crate::moon::MOON_MEAN_DISTANCE) as f32,
            moon_rgb: [0.0; 3],
        };
        let lux = full.outside_air() * 128_000.0;
        assert!((0.1..=0.35).contains(&lux), "{lux}");
        assert_eq!(full.illuminated_fraction(), 1.0);
        let quarter = Moonlight {
            phase_angle: std::f32::consts::FRAC_PI_2,
            ..full
        };
        assert!((quarter.illuminated_fraction() - 0.5).abs() < 1e-6);
        assert!(quarter.outside_air() < 0.15 * full.outside_air());
    }

    #[test]
    fn a_warm_white_is_balanced_as_far_as_the_camera_adapts() {
        let mut light = ReferenceIllumination::provisional(DVec3::Z, [1.0; 3], [1.0; 3]);
        light.white = [1.2, 1.0, 0.6];
        light.adaptation = 1.0;
        let full = Vec3::from_array(light.white_balance()) * Vec3::from_array(light.white);
        assert!(
            (full.x - full.y).abs() < 1e-5 && (full.y - full.z).abs() < 1e-5,
            "{full}"
        );
        light.adaptation = 0.5;
        let half = light.white_balance();
        assert!(half[2] > 1.0 && half[2] < 1.0 / 0.6 * 0.95, "{half:?}");
        assert!(half[0] < 1.0);
    }
}
