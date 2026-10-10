// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use glam::{DVec3, EulerRot, Quat, Vec3};

/// How imagery's stored values are read before they are lit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Imagery {
    /// Decoded from sRGB: what a photograph asks for, and the default.
    Decoded,
    /// Taken for linear as they lie. Brighter, flatter and duller than the
    /// photograph — a gamma applied twice — and what one Cycles film this
    /// was once calibrated on shows: see [`Look::cycles_film`].
    AsStored,
}

/// The air between the eye and what it looks at: thinning with height, it
/// takes light away along a ray and puts its own in its place, so distance
/// fades to the colour of the horizon — and the sky, which is that air seen
/// through, runs from that colour at the horizon to another straight up.
///
/// Extinction falls off exponentially with height over the ellipsoid, so
/// what a ray crosses has a closed form: [`Haze::transmittance`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Haze {
    /// Extinction at the ellipsoid, per metre.
    pub density: f32,
    /// The height over which the air thins by a factor of e, in metres.
    pub scale_height: f32,
    /// What the air sends towards the eye — the colour distance fades to,
    /// and the sky's at the horizon. Linear radiance, as [`Look::world`].
    pub horizon: Vec3,
    /// The sky's colour straight up, as `horizon`.
    pub zenith: Vec3,
}

impl Default for Haze {
    /// A very clear day: ground seen through 20 km of air at the ellipsoid
    /// keeps nine tenths of its light. Set by eye on one film, against no
    /// measurement — and thinner than the air of a photograph, because the
    /// imagery is far darker than the sky: a hundredth of the horizon's
    /// light already lifts dark ground by a quarter.
    fn default() -> Self {
        Self {
            density: 6.0e-6,
            scale_height: 1200.0,
            horizon: Vec3::new(0.86, 0.92, 1.0) * 0.9,
            zenith: Vec3::new(0.42, 0.62, 1.0) * 0.75,
        }
    }
}

/// A [`Haze`] as one frame's eye meets it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HazeAt {
    /// Extinction at the eye's height, per metre.
    pub density: f32,
    /// One over the scale height.
    pub per_height: f32,
    /// Up at the eye, in the stage frame.
    pub up: Vec3,
    /// One over twice the eye's distance from the Earth's centre: what a
    /// point's height gains, for each square metre of its horizontal
    /// distance, from the ground curving away.
    pub curve: f32,
}

impl Haze {
    /// The share of light left after `length` metres of a straight path
    /// from height `from` to height `to`, in metres over the ellipsoid.
    ///
    /// Extinction at height h is `density · e^(−h / scale_height)`; along a
    /// path whose height runs evenly from one end to the other its mean is
    /// that at `from` times `(1 − e^(−d)) / d`, `d` being the climb in
    /// scale heights.
    pub fn transmittance(&self, from: f32, to: f32, length: f32) -> f32 {
        let at_start = self.density * (-from / self.scale_height).exp();
        (-at_start * length * mean_thinning((to - from) / self.scale_height)).exp()
    }

    /// What a frame's renderer needs of the haze, for an eye at `eye_ecef`.
    pub fn at(&self, eye_ecef: DVec3) -> HazeAt {
        let geodetic = tuile_core::geo::ecef_to_geodetic(eye_ecef);
        let height = geodetic.height as f32;
        HazeAt {
            density: self.density * (-height / self.scale_height).exp(),
            per_height: 1.0 / self.scale_height,
            up: tuile_core::geo::enu_frame(geodetic).z_axis.as_vec3(),
            curve: (0.5 / eye_ecef.length()) as f32,
        }
    }
}

/// `(1 − e^(−d)) / d`, by its series where `d` is small: there the
/// difference above is of two numbers too near one another for an f32 to
/// tell apart well. The shaders' `mean_thinning`, to the letter.
fn mean_thinning(d: f32) -> f32 {
    if d.abs() < 2e-2 {
        1.0 - d * (0.5 - d / 6.0)
    } else {
        (1.0 - (-d).exp()) / d
    }
}

/// How a film looks, as numbers.
///
/// The default lights the imagery as the photograph it is: its stored
/// values are sRGB, and are decoded before anything is done to them. The
/// lights are a dome and a distant light; a Lambert surface under an
/// unoccluded dome and sun is the whole model, with no shadow and no
/// occlusion — unless [`Look::shadow`] or [`Look::haze`] ask for either,
/// which the default does not.
///
/// Exposure, contrast and saturation are the picture's own, and are aimed
/// at what photographs of country taken from the air measure. Over 66 such
/// photographs of green country, their ground alone: mean lightness L* 47
/// (quartiles 38 to 53), lightness between the 5th and 95th centiles 58
/// apart (42 to 67), mean chroma C* 21 (14 to 27).
///
/// The exposure default puts both films it was set on at L* 45 to 46, with
/// their imagery brought to its coarse levels' tone (`tuile-radiometry`).
/// **The contrast default is none**, and not for want of trying: under one
/// look, moorland came out 25 apart and a coast 56. A power that lifts the
/// first to what photographs show (1.5) burnt a twentieth of the second
/// white. Contrast is the ground's as much as the picture's, so one number
/// for every film is the wrong tool; what a film should aim at is to be
/// derived for that film. Highlights are rolled off whatever it is set to.
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
    /// The picture's contrast: a power on its luminance about middle grey
    /// (0.18), applied after exposure. One is none.
    pub contrast: f32,
    /// The picture's saturation, applied last. One is none.
    pub saturation: f32,
    /// How much of the sun's light the ground loses where other ground
    /// stands between it and the sun: none at 0 — no shadow is computed at
    /// all — and all of it at 1. Only the sun is ever taken away: the dome
    /// lights shadowed ground as it lights any other, so no shadow is
    /// black.
    pub shadow: f32,
    /// The air, or none: the ground as clear at any distance, and the sky
    /// the dome's one colour.
    pub haze: Option<Haze>,
}

impl Default for Look {
    fn default() -> Self {
        Self {
            world: Vec3::new(0.9, 0.95, 1.0) * 0.8,
            sun: Vec3::new(1.0, 0.98, 0.92) * 2.5,
            // An unoriented distant light shines along −Z.
            to_sun: Vec3::Z,
            exposure_ev: 1.0,
            imagery: Imagery::Decoded,
            contrast: 1.0,
            saturation: 1.0,
            shadow: 0.0,
            haze: None,
        }
    }
}

impl Look {
    /// The look of one Cycles film of the same stage, as a comparison over
    /// a grid covering one frame of one pack established it — a
    /// calibration, not a law, kept to compare against such films.
    ///
    /// - **Imagery is lit as stored** ([`Imagery::AsStored`]), its sRGB
    ///   values taken for linear ones. Read that way, that film is this
    ///   render times a constant — a gain of 4.5 to 5.1 per channel on land
    ///   and on sea alike, with a residual under two thousandths. Decoded,
    ///   the ratio runs from 14 on land to 38 on the sea: not a constant, so
    ///   not the same reading. It is why those films look washed.
    /// - **The lights are the stage's own**, in shape: a dome at 0.8 of
    ///   (0.9, 0.95, 1.0) and a distant light at 2.5 of (1.0, 0.98, 0.92),
    ///   left unoriented — so it shines down the stage's −Z, the Earth's
    ///   axis.
    /// - **Their absolute scale is not derived.** The measured gain is
    ///   carried by the exposure: the driver's −1.5 stops plus the 2.3 the
    ///   comparison found.
    pub fn cycles_film() -> Self {
        Self {
            exposure_ev: -1.5 + 2.3,
            imagery: Imagery::AsStored,
            contrast: 1.0,
            saturation: 1.0,
            ..Self::default()
        }
    }

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
    fn the_default_look_has_no_shadow_and_no_air() {
        let look = Look::default();
        assert_eq!((look.shadow, look.haze), (0.0, None));
        assert_eq!(Look::cycles_film().haze, None);
    }

    #[test]
    fn level_air_takes_light_as_its_density_says() {
        let haze = Haze::default();
        // Along the ground, the plain exponential…
        let level = haze.transmittance(0.0, 0.0, 20_000.0);
        assert!((level - (-haze.density * 20_000.0).exp()).abs() < 1e-6);
        // …and a scale height up, e times less air to cross.
        let higher = haze.transmittance(haze.scale_height, haze.scale_height, 20_000.0);
        assert!((higher.ln() * std::f32::consts::E - level.ln()).abs() < 1e-5);
    }

    #[test]
    fn a_climbing_path_crosses_the_air_between_its_ends() {
        let haze = Haze::default();
        let (low, high) = (200.0, 3000.0);
        let length = 12_000.0;
        // The closed form against the sum it stands for.
        let steps = 10_000;
        let mut depth = 0.0f64;
        for i in 0..steps {
            let h = low + (high - low) * (i as f32 + 0.5) / steps as f32;
            depth += f64::from(haze.density * (-h / haze.scale_height).exp())
                * f64::from(length / steps as f32);
        }
        let summed = (-depth).exp() as f32;
        let closed = haze.transmittance(low, high, length);
        assert!((closed - summed).abs() < 1e-4, "{closed} against {summed}");
        // The same air either way, and less of it than staying low.
        assert!((haze.transmittance(high, low, length) - closed).abs() < 1e-5);
        assert!(closed > haze.transmittance(low, low, length));
        // Where the series hands over to the closed form, no step.
        let turn = 0.02 * haze.scale_height;
        let (a, b) = (
            haze.transmittance(low, low + turn - 0.01, length),
            haze.transmittance(low, low + turn + 0.01, length),
        );
        assert!((a - b).abs() < 1e-5, "{a} then {b}");
    }

    #[test]
    fn the_eye_meets_the_air_at_its_own_height() {
        let haze = Haze::default();
        let eye = DVec3::new(
            tuile_core::geo::WGS84_A + f64::from(haze.scale_height),
            0.0,
            0.0,
        );
        let at = haze.at(eye);
        assert!((at.density * std::f32::consts::E - haze.density).abs() < 1e-9);
        assert!((at.up - Vec3::X).length() < 1e-6);
        // A point 10 km away on the level has dropped 7.8 m with the ground.
        assert!(
            (at.curve * 1.0e8 - 7.84).abs() < 0.01,
            "{}",
            at.curve * 1.0e8
        );
    }

    #[test]
    fn the_sky_alone_lights_a_surface_facing_away() {
        let look = Look::default();
        let lit = look.shade(Vec3::ONE, -look.to_sun);
        assert!((lit - look.world).length() < 1e-6);
    }
}
