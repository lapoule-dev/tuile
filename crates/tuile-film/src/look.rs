// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use glam::{EulerRot, Quat, Vec3};

/// How imagery's stored values are read before they are lit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Imagery {
    /// Decoded from sRGB: what a photograph asks for.
    Decoded,
    /// Taken for linear as they lie. Brighter and flatter than the
    /// photograph, and what the farm's Cycles films show — see [`Look`].
    AsStored,
}

/// How a film looks, as numbers.
///
/// The defaults are a calibration against one Cycles film the farm rendered,
/// compared with a render of the same pack over a grid covering the frame.
/// What that comparison established, and what it did not:
///
/// - **Imagery is lit as stored** ([`Imagery::AsStored`]), its sRGB values
///   taken for linear ones. Read that way, the film is this render times a
///   constant — a gain of 4.5 to 5.1 per channel on land and on sea alike,
///   with a residual under two thousandths. Decoded, the ratio runs from 14
///   on land to 38 on the sea: not a constant, so not the same reading. It
///   is why those films look washed, and it is not what a photograph asks
///   for; it is what they do.
/// - **The lights are the stage's own**, in shape: a dome at 0.8 of
///   (0.9, 0.95, 1.0) and a distant light at 2.5 of (1.0, 0.98, 0.92), left
///   unoriented — so it shines down the stage's −Z, the Earth's axis
///   (`tuile-usd`'s manifest).
/// - **Their absolute scale is not derived.** How the Cycles delegate turns
///   a USD intensity into radiance was not established; the measured gain
///   is carried by `exposure_ev` instead — the driver's −1.5 stops plus the
///   2.3 the comparison found. One frame of one pack: a calibration, not a
///   law.
/// - A small constant the film has and this model does not (about 0.015 in
///   linear light) is left out.
///
/// Whether a film *should* look like that is a colour decision, kept apart
/// from this one: every number here is a field.
///
/// Not matched, and not matchable without a path tracer: shadows and the
/// dome's occlusion. A Lambert surface under an unoccluded dome and sun is
/// the whole model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Look {
    /// The dome's radiance, linear RGB (colour × intensity).
    pub world: Vec3,
    /// The sun's strength, linear RGB: what a white Lambert surface facing
    /// it comes out at.
    pub sun: Vec3,
    /// Unit vector towards the sun, in the stage frame (ECEF axes).
    pub to_sun: Vec3,
    /// Exposure in stops.
    pub exposure_ev: f32,
    pub imagery: Imagery,
}

impl Default for Look {
    fn default() -> Self {
        Self {
            world: Vec3::new(0.9, 0.95, 1.0) * 0.8,
            sun: Vec3::new(1.0, 0.98, 0.92) * 2.5,
            // An unoriented distant light shines along −Z.
            to_sun: Vec3::Z,
            exposure_ev: -1.5 + 2.3,
            imagery: Imagery::AsStored,
        }
    }
}

impl Look {
    /// The linear multiplier the exposure stands for.
    pub fn exposure_scale(&self) -> f32 {
        self.exposure_ev.exp2()
    }

    /// Outgoing radiance of a Lambert surface, before exposure: the world
    /// seen by the whole hemisphere, plus the sun at `cos θ`.
    pub fn shade(&self, albedo: Vec3, normal: Vec3) -> Vec3 {
        let cos = normal.dot(self.to_sun).max(0.0);
        albedo * (self.world + self.sun * cos)
    }
}

/// The direction *towards* a Blender sun lamp with this `rotation_euler`.
///
/// Blender's default order is XYZ, applied X first; a lamp shines along its
/// local −Z, so the light comes from its local +Z.
pub fn blender_sun(euler_xyz: Vec3) -> Vec3 {
    let r = Quat::from_euler(EulerRot::ZYX, euler_xyz.z, euler_xyz.y, euler_xyz.x);
    (r * Vec3::Z).normalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unrotated_sun_is_overhead() {
        assert!((blender_sun(Vec3::ZERO) - Vec3::Z).length() < 1e-6);
    }

    #[test]
    fn x_first_then_z() {
        // X by 90° tips +Z to −Y; Z by 90° then turns −Y to +X.
        let s = blender_sun(Vec3::new(
            std::f32::consts::FRAC_PI_2,
            0.0,
            std::f32::consts::FRAC_PI_2,
        ));
        assert!((s - Vec3::X).length() < 1e-5, "{s}");
    }

    #[test]
    fn a_surface_facing_the_sun_is_lit_by_the_dome_and_the_sun() {
        let look = Look::default();
        let lit = look.shade(Vec3::ONE, look.to_sun);
        assert!((lit - (look.world + look.sun)).length() < 1e-4);
        // Facing away, only the dome is left.
        assert!((look.shade(Vec3::ONE, -look.to_sun) - look.world).length() < 1e-6);
    }

    #[test]
    fn the_sky_alone_lights_a_surface_facing_away() {
        let look = Look::default();
        let lit = look.shade(Vec3::ONE, -look.to_sun);
        assert!((lit - look.world).length() < 1e-6);
    }
}
