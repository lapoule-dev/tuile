// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use glam::{EulerRot, Quat, Vec3};

/// How a film looks, as numbers.
///
/// The defaults are the Cycles render's, read off the Blender driver
/// (`integrations/blender/render_usd.py`): a uniform world, one sun, an
/// exposure in stops and the "Standard" view transform — that is, the sRGB
/// curve and nothing else. Matching them is what makes a wgpu film comparable
/// with a Cycles one frame for frame.
///
/// What is *not* matched, and cannot be without a path tracer: the sun's
/// shadows and the world's occlusion. A Lambert surface under an unoccluded
/// world and sun is the whole model here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Look {
    /// The world's radiance, linear RGB (colour × strength).
    pub world: Vec3,
    /// The sun's irradiance at normal incidence, W/m², linear RGB.
    pub sun: Vec3,
    /// Unit vector towards the sun, in the stage frame (ECEF axes).
    pub to_sun: Vec3,
    /// Exposure in stops.
    pub exposure_ev: f32,
}

impl Default for Look {
    fn default() -> Self {
        Self {
            world: Vec3::new(0.45, 0.58, 0.78) * 0.6,
            sun: Vec3::splat(4.0),
            to_sun: blender_sun(Vec3::new(0.7, 0.2, 0.3)),
            exposure_ev: -1.5,
        }
    }
}

impl Look {
    /// The linear multiplier the exposure stands for.
    pub fn exposure_scale(&self) -> f32 {
        self.exposure_ev.exp2()
    }

    /// Outgoing radiance of a Lambert surface, before exposure: the world
    /// seen by the whole hemisphere, plus the sun at `cos θ`, over π.
    pub fn shade(&self, albedo: Vec3, normal: Vec3) -> Vec3 {
        let cos = normal.dot(self.to_sun).max(0.0);
        albedo * (self.world + self.sun * cos / std::f32::consts::PI)
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
        let s = blender_sun(Vec3::new(std::f32::consts::FRAC_PI_2, 0.0, std::f32::consts::FRAC_PI_2));
        assert!((s - Vec3::X).length() < 1e-5, "{s}");
    }

    #[test]
    fn the_sky_alone_lights_a_surface_facing_away() {
        let look = Look::default();
        let lit = look.shade(Vec3::ONE, -look.to_sun);
        assert!((lit - look.world).length() < 1e-6);
    }
}
