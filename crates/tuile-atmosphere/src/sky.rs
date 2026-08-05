// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The distant atmosphere: the shell of air seen against space, and how its
//! colour turns over with the hour.
//!
//! Distinct from [`crate::aerial`], and the distinction is not cosmetic. Aerial
//! perspective is what air does to something you can *see through it*; this is
//! what air does when there is nothing behind it but space. One is a correction
//! applied to a surface's colour, the other is a colour of its own, and a
//! renderer draws them in different passes: the ground shades with the first,
//! then a shell around the planet contributes the second.
//!
//! # What the hour does
//!
//! Everything here is a function of one number — how high the sun stands over
//! the piece of atmosphere being looked at ([`Sun::elevation_at`]). That single
//! angle is what separates a blue midday limb, an orange one at the terminator,
//! and a dark one on the night side, and driving all three from it is what makes
//! the turnover *continuous* rather than three looks with seams between them.
//!
//! The reason the terminator is orange is the same reason a sunset is: light
//! reaching grazing air has already crossed far more of the atmosphere, and the
//! blue was scattered out of it on the way. So it is not a tint applied near the
//! terminator — it is the transmittance of a long path, and computing it that
//! way is what keeps it from looking painted on.

use crate::aerial::{MIE_SCATTERING, RAYLEIGH_SCALE_HEIGHT, RAYLEIGH_SCATTERING};
use crate::sun::Sun;
use glam::DVec3;
use tuile_core::geo::WGS84_A;

/// How far up the atmosphere is worth drawing, metres.
///
/// Not where the air stops — there is no such height — but where what is left
/// stops contributing a colour anyone can see. Ten Rayleigh scale heights leaves
/// under `e^-10` of the density, and the reference implementation uses much the
/// same figure for the same reason.
pub const ATMOSPHERE_THICKNESS: f32 = 80_000.0;

/// The sun elevation below which a point is fully in night, radians.
///
/// Not zero. The sun is still lighting the air above a point for a good while
/// after it has set from that point's own horizon, which is exactly what
/// twilight is; -18° is the astronomical end of it.
pub const NIGHT_ELEVATION: f64 = -18.0 * std::f64::consts::PI / 180.0;

/// How much longer than the vertical path grazing sunlight travels before the
/// planet's own curvature bounds it.
///
/// The flat-slab air mass is a secant, and a secant runs to infinity at the
/// horizon. Real light does not: it leaves the atmosphere sideways. Uncapped,
/// the terminator goes black instead of orange — which is the one thing the
/// model exists to get right — and forty is roughly where the two curves part.
const MAX_AIR_MASS: f64 = 40.0;

/// How much further again the path lengthens between sunset and the end of
/// twilight.
///
/// Light still reaching the air above a point that has itself lost the sun has
/// come further than grazing. Without this the sky would switch off at the
/// horizon, and the terminator would read as a drawn line rather than as dusk.
const TWILIGHT_PATH_GROWTH: f64 = 1.5;

/// How much colour the unlit limb keeps.
///
/// The one number here that is a choice rather than a measurement. Single
/// scattering says the night limb is black; it is not, because light gets there
/// after more than one bounce, and a model that stops at one has to put that
/// back by hand or the planet acquires a hard black edge no photograph of Earth
/// has.
pub const NIGHT_LIMB_FLOOR: f32 = 0.04;

/// The shell a renderer draws to get sky and limb, and the state that colours it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct SkyShell {
    /// Planet centre in render space (xyz); the surface radius (w).
    pub earth: [f32; 4],
    /// The radius the shell is drawn out to (x), the Rayleigh scale height (y),
    /// the Mie scattering coefficient (z), and an overall strength — **0 turns
    /// the sky off** (w).
    pub shell: [f32; 4],
    /// Direction sunlight travels (xyz); how bright the lit sky is (w).
    pub sun: [f32; 4],
    /// [`RAYLEIGH_SCATTERING`] (xyz); [`NIGHT_LIMB_FLOOR`] (w).
    pub rayleigh: [f32; 4],
}

impl Default for SkyShell {
    fn default() -> Self {
        Self {
            earth: [0.0, 0.0, 0.0, WGS84_A as f32],
            shell: [
                WGS84_A as f32 + ATMOSPHERE_THICKNESS,
                RAYLEIGH_SCALE_HEIGHT,
                MIE_SCATTERING,
                0.0,
            ],
            sun: [0.0, 0.0, -1.0, 1.0],
            rayleigh: [
                RAYLEIGH_SCATTERING[0],
                RAYLEIGH_SCATTERING[1],
                RAYLEIGH_SCATTERING[2],
                NIGHT_LIMB_FLOOR,
            ],
        }
    }
}

impl SkyShell {
    /// For tiles rebased onto `render_origin`, lit by `sun`.
    pub fn new(render_origin: DVec3, sun: &Sun, strength: f32) -> Self {
        let centre = -render_origin;
        let travel = sun.light_travel_direction();
        Self {
            earth: [
                centre.x as f32,
                centre.y as f32,
                centre.z as f32,
                WGS84_A as f32,
            ],
            shell: [
                WGS84_A as f32 + ATMOSPHERE_THICKNESS,
                RAYLEIGH_SCALE_HEIGHT,
                MIE_SCATTERING,
                strength,
            ],
            sun: [travel.x as f32, travel.y as f32, travel.z as f32, 1.0],
            ..Self::default()
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.shell[3] > 0.0
    }

    /// How much of the sun's light reaches air standing at `sun_elevation`, per
    /// channel.
    ///
    /// A flat-slab approximation of the air mass: straight up crosses one scale
    /// height's worth, and the path lengthens as the secant of the zenith angle.
    /// The secant runs away at the horizon, where the real path is bounded by the
    /// planet's curvature, so it is capped — uncapped it turns the terminator
    /// black instead of orange, which is the one thing this function exists to
    /// get right.
    ///
    /// Below [`NIGHT_ELEVATION`] nothing is lit and this is zero.
    pub fn sunlight_reaching(&self, sun_elevation: f64) -> [f64; 3] {
        if sun_elevation <= NIGHT_ELEVATION {
            return [0.0; 3];
        }
        let air_mass = if sun_elevation > 0.0 {
            (1.0 / sun_elevation.sin()).min(MAX_AIR_MASS)
        } else {
            // Below the horizon the path keeps lengthening toward the night
            // floor, rather than a different rule taking over at zero — a
            // discontinuity there is exactly where a drawn-looking terminator
            // comes from.
            let through_twilight = sun_elevation / NIGHT_ELEVATION;
            MAX_AIR_MASS * (1.0 + through_twilight * TWILIGHT_PATH_GROWTH)
        };
        let scale_height = f64::from(self.shell[1]);
        let mut out = [0.0; 3];
        for (channel, slot) in out.iter_mut().enumerate() {
            let vertical_depth = f64::from(self.rayleigh[channel]) * scale_height;
            *slot = (-vertical_depth * air_mass).exp();
        }
        out
    }

    /// The colour the limb takes at a given sun elevation, before the shell's own
    /// density weights it: the transmitted sunlight times what Rayleigh
    /// scattering does to it, floored so the night side keeps an edge.
    pub fn limb_colour(&self, sun_elevation: f64) -> [f64; 3] {
        let reaching = self.sunlight_reaching(sun_elevation);
        let floor = f64::from(self.rayleigh[3]);
        let peak = RAYLEIGH_SCATTERING.iter().fold(0.0f32, |a, b| a.max(*b));
        let mut out = [0.0; 3];
        for (channel, slot) in out.iter_mut().enumerate() {
            let scatter = f64::from(RAYLEIGH_SCATTERING[channel] / peak);
            *slot = reaching[channel] * scatter + floor * scatter;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell() -> SkyShell {
        SkyShell::new(DVec3::ZERO, &Sun::from_direction(DVec3::X), 1.0)
    }

    #[test]
    fn the_default_is_no_sky() {
        assert!(!SkyShell::default().is_enabled());
        assert!(shell().is_enabled());
    }

    /// The whole reason a sunset is orange: light arriving along a grazing path
    /// has had its blue scattered out of it, so what is left is red. If the
    /// channels do not separate as the sun drops, the terminator is grey.
    #[test]
    fn light_reddens_as_the_sun_drops_toward_the_horizon() {
        let sky = shell();
        // Reddening is stated as a ratio and asserted to grow all the way down,
        // rather than as two thresholds at two elevations. Thresholds would let
        // a model through that reddened in the wrong places in between, and the
        // in-between is the whole of a sunset.
        let redness = |elevation_deg: f64| {
            let l = sky.sunlight_reaching(elevation_deg.to_radians());
            l[0] / l[2].max(1e-30)
        };

        // Even straight up, a quarter of the blue is gone — which is why the sky
        // is blue and why the midday sun reads as yellow-white rather than
        // blue-white. Small, but it must not be zero.
        assert!(
            (1.15..1.45).contains(&redness(90.0)),
            "overhead light should be slightly warm, ratio {}",
            redness(90.0)
        );
        // Grazing, the blue is essentially all gone. This is a sunset.
        assert!(redness(0.5) > 10.0, "grazing ratio only {}", redness(0.5));

        let mut previous = redness(90.0);
        let mut elevation = 89.0f64;
        while elevation > 0.0 {
            let current = redness(elevation);
            assert!(
                current >= previous - 1e-9,
                "light got bluer between {}° and {elevation}°: {previous} then {current}",
                elevation + 1.0
            );
            previous = current;
            elevation -= 1.0;
        }

        // And it must be dimmer as well as redder, in every channel.
        let overhead = sky.sunlight_reaching(90f64.to_radians());
        let grazing = sky.sunlight_reaching(0.5f64.to_radians());
        for c in 0..3 {
            assert!(
                grazing[c] < overhead[c],
                "channel {c}: {grazing:?} vs {overhead:?}"
            );
        }
    }

    /// Twilight is why the elevation floor is not zero: the sky stays lit for a
    /// while after the sun has set, and cutting at the horizon gives a hard
    /// terminator that reads as a rendering error.
    #[test]
    fn twilight_persists_below_the_horizon_and_ends_at_the_night_floor() {
        let sky = shell();
        let just_below: f64 = sky.sunlight_reaching(-2f64.to_radians()).iter().sum();
        assert!(
            just_below > 0.0,
            "the sky went black two degrees past sunset"
        );
        assert_eq!(sky.sunlight_reaching(NIGHT_ELEVATION), [0.0; 3]);
        assert_eq!(sky.sunlight_reaching(-1.0), [0.0; 3], "well past the floor");
    }

    /// Light must never get brighter as the sun sets — an easy thing to break
    /// with a secant that runs away, and invisible until the terminator flares.
    #[test]
    fn transmitted_light_never_rises_as_the_sun_sets() {
        let sky = shell();
        let mut previous = [f64::INFINITY; 3];
        let mut elevation = 90.0f64;
        while elevation > -18.0 {
            let reaching = sky.sunlight_reaching(elevation.to_radians());
            for c in 0..3 {
                assert!(
                    reaching[c] <= previous[c] + 1e-12,
                    "channel {c} brightened at {elevation}°: {reaching:?} after {previous:?}"
                );
            }
            previous = reaching;
            elevation -= 0.25;
        }
    }

    /// The night limb keeps an edge. Single scattering says it is black; every
    /// photograph of Earth says otherwise, because light gets there after more
    /// than one bounce.
    #[test]
    fn the_night_limb_is_dark_but_not_black() {
        let sky = shell();
        let night = sky.limb_colour(-1.0);
        assert!(
            night.iter().all(|c| *c > 0.0),
            "the night limb is black: {night:?}"
        );
        let day = sky.limb_colour(90f64.to_radians());
        assert!(
            night.iter().sum::<f64>() < day.iter().sum::<f64>() * 0.2,
            "night {night:?} is not much darker than day {day:?}"
        );
    }
}
