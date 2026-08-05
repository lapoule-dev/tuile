// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Aerial perspective: what the air between the eye and the ground does to the
//! ground's colour.
//!
//! This is the single strongest depth cue a globe has. Without it a ridge twenty
//! kilometres away is drawn at the same contrast and saturation as one at two
//! hundred metres, and the eye — which has spent a lifetime reading haze as
//! distance — takes the whole scene as flat and close. It is most of the
//! difference between a textured sphere and a landscape.
//!
//! # What is modelled
//!
//! Single scattering through an exponential atmosphere, integrated **in closed
//! form** rather than sampled. The usual reference implementation ray-marches
//! sixteen primary steps and four light steps per fragment, which buys correct
//! self-shadowing of the air near the terminator. Over the ground, at the
//! distances that matter, the closed form agrees closely and costs nothing.
//!
//! Not modelled here: multiple scattering (which is why a real horizon never
//! goes fully black), ozone absorption, and the atmosphere seen from outside
//! against space. The last is a different problem — a shell rather than a
//! surface effect — and lives in [`crate::sky`].

use crate::sun::Sun;
use glam::DVec3;
use tuile_core::geo::WGS84_A;

/// Rayleigh scattering per metre at sea level, for R, G and B.
///
/// Blue scatters about six times as strongly as red. That ratio is the whole of
/// why distance looks blue and why sunsets do not.
pub const RAYLEIGH_SCATTERING: [f32; 3] = [5.802e-6, 13.558e-6, 33.100e-6];

/// The height over which Rayleigh density falls by `e` — the bulk of the air.
pub const RAYLEIGH_SCALE_HEIGHT: f32 = 8_000.0;

/// Mie scattering per metre at sea level: aerosols, haze, dust. Grey rather than
/// coloured, and strongly forward-biased, which is why looking toward the sun
/// through haze is bright and looking away from it is not.
pub const MIE_SCATTERING: f32 = 21.0e-6;

/// Aerosols sit far lower than the air itself; above a couple of kilometres
/// there is essentially none, which is why mountain air looks so clear.
pub const MIE_SCALE_HEIGHT: f32 = 1_200.0;

/// How forward-biased Mie scattering is. 0 would be isotropic; 0.76 is the usual
/// figure for terrestrial haze.
pub const MIE_ANISOTROPY: f32 = 0.76;

/// Aerosols have to thin out well faster than the air they sit in, or a summit
/// is as hazy as a valley floor and altitude stops reading as clarity. A build
/// error rather than a test, because the two heights only mean anything relative
/// to each other and nothing at run time would notice them converging.
const _: () = assert!(
    MIE_SCALE_HEIGHT * 4.0 < RAYLEIGH_SCALE_HEIGHT,
    "aerosols must thin out far faster than the air they sit in"
);

/// Everything a shader needs to apply aerial perspective, packed as four `vec4`s
/// so a uniform buffer takes it unchanged.
///
/// Positions are in **render space** — relative to the render origin the tiles
/// were rebased onto — because that is the only frame in which f32 can say
/// anything about planetary distances without losing metres to rounding. A haze
/// that jitters by metres between frames reads as a crawling shimmer over the
/// whole scene.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct AerialPerspective {
    /// Eye in render space (xyz); its height above the surface (w).
    pub eye: [f32; 4],
    /// The planet's centre in render space (xyz); its radius (w).
    pub earth: [f32; 4],
    /// [`RAYLEIGH_SCATTERING`] (xyz); [`RAYLEIGH_SCALE_HEIGHT`] (w).
    pub rayleigh: [f32; 4],
    /// [`MIE_SCATTERING`], [`MIE_SCALE_HEIGHT`], [`MIE_ANISOTROPY`], and an
    /// overall strength — **0 turns the atmosphere off**, which is what a
    /// renderer with no planet in it wants.
    pub mie: [f32; 4],
    /// Direction sunlight travels (xyz), and how bright the haze it lights is
    /// (w). Held here rather than read from the renderer's own light so that a
    /// scene can be lit for looks and hazed for a stated time without the two
    /// silently disagreeing.
    pub sun: [f32; 4],
}

impl Default for AerialPerspective {
    /// Off. A scene that is not a planet has no air in it, and a default that
    /// quietly tinted every render would be worse than one that does nothing.
    fn default() -> Self {
        Self {
            eye: [0.0; 4],
            earth: [0.0, 0.0, 0.0, WGS84_A as f32],
            rayleigh: [
                RAYLEIGH_SCATTERING[0],
                RAYLEIGH_SCATTERING[1],
                RAYLEIGH_SCATTERING[2],
                RAYLEIGH_SCALE_HEIGHT,
            ],
            mie: [MIE_SCATTERING, MIE_SCALE_HEIGHT, MIE_ANISOTROPY, 0.0],
            sun: [0.0, 0.0, -1.0, 1.0],
        }
    }
}

impl AerialPerspective {
    /// For an eye at `eye_ecef`, with tiles rebased onto `render_origin`, lit by
    /// `sun`.
    ///
    /// `strength` scales the optical depth: 1.0 is the physical atmosphere, less
    /// is a clearer day than Earth ever has, more makes the cue readable on a
    /// small screen. 0 disables it.
    ///
    /// The subtraction happens in f64 and only its result narrows — the same
    /// protocol the geometry uses, for the same reason.
    pub fn new(eye_ecef: DVec3, render_origin: DVec3, sun: &Sun, strength: f32) -> Self {
        let eye_local = eye_ecef - render_origin;
        let centre_local = -render_origin;
        // Height above a *sphere*, not the ellipsoid: the two radii differ by
        // 21 km, under 0.3% of either, and nothing here reads at that level.
        let eye_height = (eye_ecef.length() - WGS84_A).max(0.0);
        let travel = sun.light_travel_direction();
        Self {
            eye: [
                eye_local.x as f32,
                eye_local.y as f32,
                eye_local.z as f32,
                eye_height as f32,
            ],
            earth: [
                centre_local.x as f32,
                centre_local.y as f32,
                centre_local.z as f32,
                WGS84_A as f32,
            ],
            mie: [MIE_SCATTERING, MIE_SCALE_HEIGHT, MIE_ANISOTROPY, strength],
            sun: [travel.x as f32, travel.y as f32, travel.z as f32, 1.0],
            ..Self::default()
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.mie[3] > 0.0
    }

    /// Transmittance through the air between two points, per channel — how much
    /// of a distant surface's own colour survives the trip.
    ///
    /// The shader computes this itself; this exists so the model can be
    /// *asserted* rather than only looked at. A haze whose depth cue is wrong is
    /// very hard to see and very easy to test.
    pub fn transmittance(&self, from_height: f64, to_height: f64, distance: f64) -> [f64; 3] {
        let strength = f64::from(self.mie[3]);
        if strength <= 0.0 {
            return [1.0; 3];
        }
        let mean = |scale_height: f64| {
            0.5 * ((-from_height.max(0.0) / scale_height).exp()
                + (-to_height.max(0.0) / scale_height).exp())
        };
        let rayleigh_density = mean(f64::from(self.rayleigh[3]));
        let mie_density = mean(f64::from(self.mie[1]));
        let mie_depth = f64::from(self.mie[0]) * mie_density * distance;
        let mut out = [0.0; 3];
        for (channel, slot) in out.iter_mut().enumerate() {
            let rayleigh_depth = f64::from(self.rayleigh[channel]) * rayleigh_density * distance;
            *slot = (-(rayleigh_depth + mie_depth) * strength).exp();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::geo::{geodetic_to_ecef, Geodetic};

    fn eye_at(lon: f64, lat: f64, height: f64) -> DVec3 {
        geodetic_to_ecef(Geodetic { lon, lat, height })
    }

    fn noon() -> Sun {
        Sun::from_direction(DVec3::X)
    }

    /// Off unless asked for. A renderer with no planet in it must not have its
    /// colours quietly changed by a default.
    #[test]
    fn the_default_is_no_atmosphere() {
        assert!(!AerialPerspective::default().is_enabled());
        let eye = eye_at(0.1, 0.8, 5_000.0);
        assert!(!AerialPerspective::new(eye, DVec3::ZERO, &noon(), 0.0).is_enabled());
        assert!(AerialPerspective::new(eye, DVec3::ZERO, &noon(), 1.0).is_enabled());
        // And disabled means *nothing changes*, not merely "a little".
        assert_eq!(
            AerialPerspective::default().transmittance(0.0, 0.0, 500_000.0),
            [1.0; 3]
        );
    }

    /// The whole point of render space: with the origin at the eye, the eye is
    /// at zero and the planet's centre is one radius away, both exactly
    /// representable. ECEF positions are what f32 cannot hold.
    #[test]
    fn render_space_puts_the_eye_at_the_origin_it_was_rebased_onto() {
        let eye = eye_at(0.11, 0.81, 5_000.0);
        let a = AerialPerspective::new(eye, eye, &noon(), 1.0);
        assert_eq!([a.eye[0], a.eye[1], a.eye[2]], [0.0, 0.0, 0.0]);
        let to_centre = f64::from(glam::Vec3::from_slice(&a.earth[0..3]).length());
        assert!(
            (to_centre - eye.length()).abs() < 1.0,
            "the centre should sit one eye-radius away, got {to_centre}"
        );
    }

    /// Density along the ray is set by height above the *surface*, which is the
    /// distance from the centre minus six thousand kilometres. Confusing the two
    /// makes the air uniformly thin everywhere.
    #[test]
    fn the_eye_carries_its_height_above_the_ground_not_above_the_centre() {
        for height in [0.0, 5_000.0, 400_000.0] {
            let a = AerialPerspective::new(eye_at(0.0, 0.0, height), DVec3::ZERO, &noon(), 1.0);
            assert!(
                (f64::from(a.eye[3]) - height).abs() < 1.0,
                "at {height} m the uniform says {}",
                a.eye[3]
            );
        }
    }

    /// Blue scatters several times more strongly than red. If that stops being
    /// true the haze goes grey, and every distance cue it carries goes with it.
    #[test]
    fn blue_scatters_far_more_than_red() {
        let [r, g, b] = RAYLEIGH_SCATTERING;
        assert!(b > g && g > r, "{RAYLEIGH_SCATTERING:?}");
        assert!(b / r > 4.0, "blue is only {}× red", b / r);
    }

    /// The cue itself: farther must mean hazier, monotonically, or the eye reads
    /// distance wrongly rather than merely weakly.
    #[test]
    fn transmittance_falls_with_distance_and_never_rises() {
        let air = AerialPerspective::new(eye_at(0.0, 0.0, 0.0), DVec3::ZERO, &noon(), 1.0);
        let mut previous = [1.0; 3];
        for km in [1.0, 5.0, 20.0, 50.0, 200.0] {
            let t = air.transmittance(0.0, 0.0, km * 1000.0);
            for c in 0..3 {
                assert!(
                    t[c] <= previous[c],
                    "channel {c} got clearer between {previous:?} and {t:?} at {km} km"
                );
            }
            previous = t;
        }
        // At 50 km of sea-level air, blue is mostly gone and red mostly is not.
        // That gap *is* the blue of distance; without it haze is only a fog.
        let far = air.transmittance(0.0, 0.0, 50_000.0);
        assert!(far[2] < far[0] * 0.5, "not blue enough at 50 km: {far:?}");
    }

    /// Air thins with height, so the same distance costs less of it higher up —
    /// which is why a summit sees two hundred kilometres and a valley floor does
    /// not.
    #[test]
    fn the_same_distance_is_clearer_higher_up() {
        let air = AerialPerspective::new(eye_at(0.0, 0.0, 0.0), DVec3::ZERO, &noon(), 1.0);
        let low = air.transmittance(0.0, 0.0, 30_000.0);
        let high = air.transmittance(9_000.0, 9_000.0, 30_000.0);
        for c in 0..3 {
            assert!(
                high[c] > low[c],
                "channel {c}: {high:?} at altitude is no clearer than {low:?} at sea level"
            );
        }
    }
}
