// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Every setting of the air — what it is made of, how a camera meets its
//! light, how finely it is drawn, how the picture is graded over it — as a
//! **tuning value**: read from the environment once, by a native host, each
//! with today's constant for its default, each checked against the range in
//! which it still means something physically.
//!
//! The one place these values are read and refused: [`AirTuning::from_env`]
//! (a native host) or [`AirTuning::from_lookup`] (anything that has its own
//! store of settings, a test). Nothing here is a switch: every value is a
//! number, and unset it is today's. A shader never reads the environment;
//! a host reads it here and hands the values on through its uniforms.
//!
//! # Units of the aerosols
//!
//! The aerosols can be stated three ways, which are one quantity:
//!
//! - `TUILE_AIR_AOD_550`, an **aerosol optical depth** straight up from sea
//!   level at 550 nm, as a sun photometer (AERONET) reports it;
//! - `TUILE_AIR_TURBIDITY`, the air's whole depth over its molecules' alone,
//!   `T = 1 + τ_a / τ_R`, `τ_R` the molecules' depth in the green
//!   (`β_R,550 · H_R` = 13.558e-6 × 8 000 = 0.1085);
//! - the extinction at sea level, `β_M = τ_a / H_M` per metre, `H_M` the
//!   aerosols' scale height — what the shaders read.
//!
//! Today's turbidity 2 is an AOD of 0.1085 and an extinction of 9.04e-5 /m.
//! At most one of the first two may be set.
//!
//! # Every value
//!
//! [`KNOBS`] lists them, with their defaults, ranges and meaning;
//! `docs/19-sky-atmosphere.md` §8 says the same in prose.

use std::fmt;

use crate::aerial::{self, AerialAir};
use crate::illumination::CameraResponse;
use crate::params::{self, AtmosphereParams};

/// One tuning value: its variable, its default as it is written, the range
/// it is refused outside of, and what it does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Knob {
    pub name: &'static str,
    pub default: &'static str,
    /// Inclusive bounds of each number.
    pub range: (f64, f64),
    /// How many numbers it takes: 1, or 3 for a colour (`r,g,b`).
    pub arity: usize,
    pub what: &'static str,
}

const fn knob(
    name: &'static str,
    default: &'static str,
    range: (f64, f64),
    arity: usize,
    what: &'static str,
) -> Knob {
    Knob {
        name,
        default,
        range,
        arity,
        what,
    }
}

pub const TURBIDITY: Knob = knob(
    "TUILE_AIR_TURBIDITY",
    "2.0",
    (1.0, 10.0),
    1,
    "the air's optical depth in the green over its molecules' alone (1: no aerosol, 2 a clear day, 4 hazy); not with TUILE_AIR_AOD_550",
);
pub const AOD_550: Knob = knob(
    "TUILE_AIR_AOD_550",
    "0.1085 (turbidity 2)",
    (0.0, 3.0),
    1,
    "the aerosols' optical depth straight up from sea level at 550 nm, as AERONET reports it; not with TUILE_AIR_TURBIDITY",
);
pub const MIE_SCALE_HEIGHT: Knob = knob(
    "TUILE_AIR_MIE_SCALE_HEIGHT",
    "1200",
    (100.0, 8_000.0),
    1,
    "metres over which the aerosols thin by e; the same depth spread lower is denser near the ground and clearer above; below the molecules' scale height",
);
pub const MIE_ALBEDO: Knob = knob(
    "TUILE_AIR_MIE_ALBEDO",
    "0.9 (3.996/4.440)",
    (0.5, 1.0),
    1,
    "the aerosols' single-scattering albedo: the share of what they take that they scatter (brightens the veil) rather than absorb (darkens it)",
);
pub const MIE_G: Knob = knob(
    "TUILE_AIR_MIE_G",
    "0.8",
    (0.0, 0.95),
    1,
    "the aerosols' anisotropy (Cornette-Shanks g): glow towards the sun against flat haze away from it",
);
pub const RAYLEIGH_SCATTERING: Knob = knob(
    "TUILE_AIR_RAYLEIGH_SCATTERING",
    "5.802e-6,13.558e-6,33.1e-6",
    (1e-7, 1e-4),
    3,
    "the molecules' scattering at sea level, per metre, red, green, blue: the blue of distance and of the sky (also the viewer's)",
);
pub const RAYLEIGH_SCALE_HEIGHT: Knob = knob(
    "TUILE_AIR_RAYLEIGH_SCALE_HEIGHT",
    "8000",
    (5_000.0, 12_000.0),
    1,
    "metres over which the molecules thin by e (also the viewer's)",
);
pub const OZONE_ABSORPTION: Knob = knob(
    "TUILE_AIR_OZONE_ABSORPTION",
    "0.650e-6,1.881e-6,0.085e-6",
    (0.0, 1e-5),
    3,
    "ozone's absorption at its densest (25 km), per metre, RGB: the zenith's and the twilight's colour",
);
pub const GROUND_ALBEDO: Knob = knob(
    "TUILE_AIR_GROUND_ALBEDO",
    "0.3",
    (0.0, 1.0),
    1,
    "the ground's mean albedo in the model: the light it sends back into the air, for the air to scatter again",
);
pub const EXPOSURE_FOLLOWS: Knob = knob(
    "TUILE_AIR_EXPOSURE_FOLLOWS",
    "0.75",
    (0.0, 0.95),
    1,
    "of a light brighter than daylight, the share of its difference in stops the auto-exposure makes up",
);
pub const NIGHT_DEPTH_EV: Knob = knob(
    "TUILE_AIR_NIGHT_DEPTH_EV",
    "3.0",
    (0.5, 8.0),
    1,
    "the scale, in stops, of how much darker a dim light is shown",
);
pub const WHITE_ADAPTATION: Knob = knob(
    "TUILE_AIR_WHITE_ADAPTATION",
    "0.35",
    (0.0, 1.0),
    1,
    "how far the white balance goes towards the light's white (0 none, 1 fully)",
);
pub const METER_REFLECTANCE: Knob = knob(
    "TUILE_AIR_METER_REFLECTANCE",
    "0.18",
    (0.05, 0.5),
    1,
    "the reflectance the meter takes the scene for; the air's own light is metered as ground that bright, so a higher value counts the haze for less",
);
pub const METER_FLOOR: Knob = knob(
    "TUILE_AIR_METER_FLOOR",
    "1.0",
    (1e-3, 1e6),
    1,
    "the floor of the meter's log-average luminance, in units of the night floor's light: under it every part of the frame counts as dark as any other",
);
pub const VEIL_BOUND: Knob = knob(
    "TUILE_AIR_VEIL_BOUND",
    "6.0",
    (1.0, 100.0),
    1,
    "how many times the light of ground of 0.1 reflectance under it the air's veil may reach (a soft cap)",
);
pub const MARCH_STEPS: Knob = knob(
    "TUILE_AIR_MARCH_STEPS",
    "12",
    (4.0, 64.0),
    1,
    "steps of the march through the air between the eye and the ground, under 10 km (accuracy, not look)",
);
pub const MARCH_STEPS_HIGH: Knob = knob(
    "TUILE_AIR_MARCH_STEPS_HIGH",
    "24",
    (4.0, 128.0),
    1,
    "the same, from over 10 km",
);
pub const REFLECTANCE: Knob = knob(
    "TUILE_AIR_REFLECTANCE",
    "6.5 (0.7 for imagery read as stored)",
    (0.1, 50.0),
    1,
    "the real ground's reflectance an imagery value of one stands for: the air's own light is drawn against ground that bright; higher, a lighter veil",
);
pub const VIEWER_MIE_SCATTERING: Knob = knob(
    "TUILE_AIR_VIEWER_MIE_SCATTERING",
    "21e-6",
    (0.0, 1e-3),
    1,
    "the interactive viewer's aerosol scattering at sea level, per metre (the film's: 8.14e-5 at turbidity 2)",
);
pub const VIEWER_MIE_G: Knob = knob(
    "TUILE_AIR_VIEWER_MIE_G",
    "0.76",
    (0.0, 0.95),
    1,
    "the interactive viewer's aerosol anisotropy (the film's: 0.8)",
);
pub const VIEWER_STRENGTH: Knob = knob(
    "TUILE_AIR_VIEWER_STRENGTH",
    "1.0",
    (0.0, 4.0),
    1,
    "what the viewer's optical depth is scaled by; 1 is the physical air",
);
pub const LOOK_EXPOSURE_EV: Knob = knob(
    "TUILE_LOOK_EXPOSURE_EV",
    "1.0 (0.8 for imagery read as stored)",
    (-10.0, 10.0),
    1,
    "the film's exposure, in stops, before the auto-exposure's",
);
pub const LOOK_CONTRAST: Knob = knob(
    "TUILE_LOOK_CONTRAST",
    "1.0",
    (0.25, 4.0),
    1,
    "a power on the picture's luminance about middle grey",
);
pub const LOOK_SATURATION: Knob = knob(
    "TUILE_LOOK_SATURATION",
    "1.0",
    (0.0, 4.0),
    1,
    "the picture's saturation, applied last",
);
pub const LOOK_SUN_LIGHT: Knob = knob(
    "TUILE_LOOK_SUN_LIGHT",
    "2.5,2.45,2.3",
    (0.0, 100.0),
    3,
    "the sun the imagery was calibrated under, linear RGB: under the physical air, what a clear day's sun 45° up gives level ground",
);
pub const LOOK_SKY_LIGHT: Knob = knob(
    "TUILE_LOOK_SKY_LIGHT",
    "0.72,0.76,0.8",
    (0.0, 100.0),
    3,
    "the dome the imagery was calibrated under, linear RGB, as the sun",
);
pub const LOOK_HIGHLIGHT_KNEE: Knob = knob(
    "TUILE_LOOK_HIGHLIGHT_KNEE",
    "0.5",
    (0.05, 0.95),
    1,
    "the linear luminance where the roll-off of highlights begins",
);

/// Every tuning value, in the order they are documented.
pub const KNOBS: [Knob; 28] = [
    TURBIDITY,
    AOD_550,
    MIE_SCALE_HEIGHT,
    MIE_ALBEDO,
    MIE_G,
    RAYLEIGH_SCATTERING,
    RAYLEIGH_SCALE_HEIGHT,
    OZONE_ABSORPTION,
    GROUND_ALBEDO,
    EXPOSURE_FOLLOWS,
    NIGHT_DEPTH_EV,
    WHITE_ADAPTATION,
    METER_REFLECTANCE,
    METER_FLOOR,
    VEIL_BOUND,
    MARCH_STEPS,
    MARCH_STEPS_HIGH,
    REFLECTANCE,
    VIEWER_MIE_SCATTERING,
    VIEWER_MIE_G,
    VIEWER_STRENGTH,
    LOOK_EXPOSURE_EV,
    LOOK_CONTRAST,
    LOOK_SATURATION,
    LOOK_SUN_LIGHT,
    LOOK_SKY_LIGHT,
    LOOK_HIGHLIGHT_KNEE,
    // Listed last: a constraint between two, not a value of its own.
    knob(
        "(TUILE_AIR_MIE_SCALE_HEIGHT < TUILE_AIR_RAYLEIGH_SCALE_HEIGHT)",
        "",
        (0.0, 0.0),
        0,
        "aerosols lie lower than the molecules they sit in",
    ),
];

/// A tuning value refused: which, what it was, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct TuningError {
    pub name: &'static str,
    pub value: String,
    pub why: String,
}

impl fmt::Display for TuningError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={:?}: {}", self.name, self.value, self.why)
    }
}

impl std::error::Error for TuningError {}

/// What the look over the air is graded by, when a value is set: `None`
/// is the look's own (a preset's, or the default's).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LookTuning {
    pub reflectance: Option<f32>,
    pub exposure_ev: Option<f32>,
    pub contrast: Option<f32>,
    pub saturation: Option<f32>,
    pub sun_light: Option<[f32; 3]>,
    pub sky_light: Option<[f32; 3]>,
}

/// Every setting of the air, as one host reads it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AirTuning {
    /// The atmosphere a host draws when it draws one, its aerosols as the
    /// turbidity or the AOD say.
    pub params: AtmosphereParams,
    pub camera: CameraResponse,
    /// [`VEIL_BOUND`].
    pub veil_bound: f32,
    /// [`MARCH_STEPS`] and [`MARCH_STEPS_HIGH`].
    pub march_steps: [u32; 2],
    /// [`LOOK_HIGHLIGHT_KNEE`].
    pub highlight_knee: f32,
    /// The interactive viewer's air.
    pub viewer: AerialAir,
    pub look: LookTuning,
}

/// The veil's bound, today's: `resolve.wgsl`'s constant before it was a
/// uniform.
pub const DEFAULT_VEIL_BOUND: f32 = 6.0;
/// The march's steps, today's: `hillaire.wgsl`'s constants.
pub const DEFAULT_MARCH_STEPS: [u32; 2] = [12, 24];
/// The knee, today's: `present.wgsl`'s constant.
pub const DEFAULT_HIGHLIGHT_KNEE: f32 = 0.5;

impl Default for AirTuning {
    /// Today's constants, every one.
    fn default() -> Self {
        Self {
            params: AtmosphereParams::default(),
            camera: CameraResponse::default(),
            veil_bound: DEFAULT_VEIL_BOUND,
            march_steps: DEFAULT_MARCH_STEPS,
            highlight_knee: DEFAULT_HIGHLIGHT_KNEE,
            viewer: AerialAir::default(),
            look: LookTuning::default(),
        }
    }
}

impl AirTuning {
    /// Read from the process's environment: what a native host calls,
    /// once.
    pub fn from_env() -> Result<Self, TuningError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Read from `lookup` — a variable's text, or `None` when unset. An
    /// empty text is unset. A value that is not a finite number in its
    /// range, or not as many numbers as it takes, is refused.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, TuningError> {
        let get = |k: &Knob| lookup(k.name).filter(|v| !v.trim().is_empty());
        let one = |k: &Knob| -> Result<Option<f64>, TuningError> {
            get(k).map(|v| numbers(k, &v).map(|n| n[0])).transpose()
        };
        let three = |k: &Knob| -> Result<Option<[f64; 3]>, TuningError> {
            get(k)
                .map(|v| numbers(k, &v).map(|n| [n[0], n[1], n[2]]))
                .transpose()
        };
        let d = AtmosphereParams::default();
        let mut p = AtmosphereParams {
            rayleigh_scattering: three(&RAYLEIGH_SCATTERING)?.unwrap_or(d.rayleigh_scattering),
            rayleigh_scale_height: one(&RAYLEIGH_SCALE_HEIGHT)?.unwrap_or(d.rayleigh_scale_height),
            mie_scale_height: one(&MIE_SCALE_HEIGHT)?.unwrap_or(d.mie_scale_height),
            mie_anisotropy: one(&MIE_G)?.unwrap_or(d.mie_anisotropy),
            mie_albedo: one(&MIE_ALBEDO)?.unwrap_or(d.mie_albedo),
            ozone_absorption: three(&OZONE_ABSORPTION)?.unwrap_or(d.ozone_absorption),
            ground_albedo: one(&GROUND_ALBEDO)?.unwrap_or(d.ground_albedo),
            ..d
        };
        if p.mie_scale_height >= p.rayleigh_scale_height {
            return Err(TuningError {
                name: MIE_SCALE_HEIGHT.name,
                value: p.mie_scale_height.to_string(),
                why: format!(
                    "aerosols lie lower than the molecules: under {}={}",
                    RAYLEIGH_SCALE_HEIGHT.name, p.rayleigh_scale_height
                ),
            });
        }
        p = match (one(&TURBIDITY)?, one(&AOD_550)?) {
            (Some(_), Some(_)) => {
                return Err(TuningError {
                    name: AOD_550.name,
                    value: get(&AOD_550).unwrap_or_default(),
                    why: format!(
                        "the same aerosols as {}: set one or the other",
                        TURBIDITY.name
                    ),
                })
            }
            (Some(t), None) => p.with_turbidity(t),
            (None, Some(aod)) => p.with_aerosol_depth(aod),
            (None, None) => p.with_turbidity(params::CLEAR_TURBIDITY),
        };

        let f = |v: Option<f64>, or: f32| v.map_or(or, |v| v as f32);
        let c = CameraResponse::default();
        let camera = CameraResponse {
            follows: f(one(&EXPOSURE_FOLLOWS)?, c.follows),
            night_depth_ev: f(one(&NIGHT_DEPTH_EV)?, c.night_depth_ev),
            white_adaptation: f(one(&WHITE_ADAPTATION)?, c.white_adaptation),
            meter_reflectance: one(&METER_REFLECTANCE)?.unwrap_or(c.meter_reflectance),
            meter_floor: one(&METER_FLOOR)?.unwrap_or(c.meter_floor),
        };
        let steps = |k: &Knob, or: u32| -> Result<u32, TuningError> {
            match one(k)? {
                None => Ok(or),
                Some(n) if n.fract() == 0.0 => Ok(n as u32),
                Some(n) => Err(TuningError {
                    name: k.name,
                    value: n.to_string(),
                    why: "a whole number of steps".into(),
                }),
            }
        };
        let v = AerialAir::default();
        let rgb32 = |a: [f64; 3]| a.map(|x| x as f32);
        let viewer = AerialAir {
            rayleigh_scattering: three(&RAYLEIGH_SCATTERING)?.map_or(v.rayleigh_scattering, rgb32),
            rayleigh_scale_height: f(one(&RAYLEIGH_SCALE_HEIGHT)?, v.rayleigh_scale_height),
            mie_scattering: f(one(&VIEWER_MIE_SCATTERING)?, v.mie_scattering),
            mie_scale_height: f(one(&MIE_SCALE_HEIGHT)?, v.mie_scale_height),
            mie_anisotropy: f(one(&VIEWER_MIE_G)?, v.mie_anisotropy),
            strength: f(one(&VIEWER_STRENGTH)?, v.strength),
        };
        debug_assert_eq!(aerial::MIE_SCALE_HEIGHT as f64, params::MIE_SCALE_HEIGHT);
        let look = LookTuning {
            reflectance: one(&REFLECTANCE)?.map(|v| v as f32),
            exposure_ev: one(&LOOK_EXPOSURE_EV)?.map(|v| v as f32),
            contrast: one(&LOOK_CONTRAST)?.map(|v| v as f32),
            saturation: one(&LOOK_SATURATION)?.map(|v| v as f32),
            sun_light: three(&LOOK_SUN_LIGHT)?.map(rgb32),
            sky_light: three(&LOOK_SKY_LIGHT)?.map(rgb32),
        };
        Ok(Self {
            params: p,
            camera,
            veil_bound: f(one(&VEIL_BOUND)?, DEFAULT_VEIL_BOUND),
            march_steps: [
                steps(&MARCH_STEPS, DEFAULT_MARCH_STEPS[0])?,
                steps(&MARCH_STEPS_HIGH, DEFAULT_MARCH_STEPS[1])?,
            ],
            highlight_knee: f(one(&LOOK_HIGHLIGHT_KNEE)?, DEFAULT_HIGHLIGHT_KNEE),
            viewer,
            look,
        })
    }
}

/// `text`'s numbers, as many as `knob` takes, each finite and in its range.
fn numbers(knob: &Knob, text: &str) -> Result<Vec<f64>, TuningError> {
    let refuse = |why: String| TuningError {
        name: knob.name,
        value: text.to_string(),
        why,
    };
    let parts: Vec<&str> = text.split(',').map(str::trim).collect();
    if parts.len() != knob.arity {
        return Err(refuse(if knob.arity == 1 {
            "one number".into()
        } else {
            format!("{} numbers, comma-separated", knob.arity)
        }));
    }
    parts
        .iter()
        .map(|p| {
            let n: f64 = p
                .parse()
                .map_err(|_| refuse(format!("{p:?} is not a number")))?;
            let (lo, hi) = knob.range;
            if !n.is_finite() || n < lo || n > hi {
                return Err(refuse(format!(
                    "{p} is outside [{lo}, {hi}]: {}",
                    knob.what
                )));
            }
            Ok(n)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn with(pairs: &[(&str, &str)]) -> Result<AirTuning, TuningError> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        AirTuning::from_lookup(move |k| map.get(k).cloned())
    }

    #[test]
    fn nothing_set_is_todays_constants_to_the_bit() {
        let t = with(&[]).expect("defaults");
        assert_eq!(t, AirTuning::default());
        assert_eq!(t.params, AtmosphereParams::default());
        assert_eq!(t.camera, CameraResponse::default());
        assert_eq!(t.viewer, AerialAir::default());
        assert_eq!(t.look, LookTuning::default());
        // Empty is unset.
        assert_eq!(with(&[("TUILE_AIR_MIE_G", " ")]).unwrap(), t);
    }

    #[test]
    fn each_value_lands_where_it_is_read() {
        let t = with(&[
            ("TUILE_AIR_MIE_SCALE_HEIGHT", "900"),
            ("TUILE_AIR_MIE_ALBEDO", "0.95"),
            ("TUILE_AIR_MIE_G", "0.7"),
            ("TUILE_AIR_RAYLEIGH_SCATTERING", "5e-6, 13e-6, 33e-6"),
            ("TUILE_AIR_RAYLEIGH_SCALE_HEIGHT", "8500"),
            ("TUILE_AIR_OZONE_ABSORPTION", "0,0,0"),
            ("TUILE_AIR_GROUND_ALBEDO", "0.12"),
            ("TUILE_AIR_AOD_550", "0.06"),
            ("TUILE_AIR_EXPOSURE_FOLLOWS", "0.6"),
            ("TUILE_AIR_NIGHT_DEPTH_EV", "4"),
            ("TUILE_AIR_WHITE_ADAPTATION", "0.5"),
            ("TUILE_AIR_METER_REFLECTANCE", "0.25"),
            ("TUILE_AIR_METER_FLOOR", "4"),
            ("TUILE_AIR_VEIL_BOUND", "3"),
            ("TUILE_AIR_MARCH_STEPS", "16"),
            ("TUILE_AIR_MARCH_STEPS_HIGH", "32"),
            ("TUILE_AIR_REFLECTANCE", "5"),
            ("TUILE_AIR_VIEWER_MIE_SCATTERING", "5e-5"),
            ("TUILE_AIR_VIEWER_MIE_G", "0.8"),
            ("TUILE_AIR_VIEWER_STRENGTH", "0.5"),
            ("TUILE_LOOK_EXPOSURE_EV", "0.5"),
            ("TUILE_LOOK_CONTRAST", "1.1"),
            ("TUILE_LOOK_SATURATION", "1.2"),
            ("TUILE_LOOK_SUN_LIGHT", "2,2,2"),
            ("TUILE_LOOK_SKY_LIGHT", "0.5,0.6,0.7"),
            ("TUILE_LOOK_HIGHLIGHT_KNEE", "0.6"),
        ])
        .expect("all in range");
        let p = t.params;
        assert_eq!(p.mie_scale_height, 900.0);
        assert_eq!(p.mie_albedo, 0.95);
        assert_eq!(p.mie_anisotropy, 0.7);
        assert_eq!(p.rayleigh_scattering, [5e-6, 13e-6, 33e-6]);
        assert_eq!(p.rayleigh_scale_height, 8500.0);
        assert_eq!(p.ozone_absorption, [0.0; 3]);
        assert_eq!(p.ground_albedo, 0.12);
        assert!((p.aerosol_depth() - 0.06).abs() < 1e-15);
        assert!((p.mie_scattering / (p.mie_scattering + p.mie_absorption) - 0.95).abs() < 1e-12);
        assert_eq!(
            t.camera,
            CameraResponse {
                follows: 0.6,
                night_depth_ev: 4.0,
                white_adaptation: 0.5,
                meter_reflectance: 0.25,
                meter_floor: 4.0,
            }
        );
        assert_eq!((t.veil_bound, t.march_steps), (3.0, [16, 32]));
        assert_eq!(t.highlight_knee, 0.6);
        assert_eq!(
            t.viewer,
            AerialAir {
                rayleigh_scattering: [5e-6, 13e-6, 33e-6],
                rayleigh_scale_height: 8500.0,
                mie_scattering: 5e-5,
                mie_scale_height: 900.0,
                mie_anisotropy: 0.8,
                strength: 0.5,
            }
        );
        assert_eq!(
            t.look,
            LookTuning {
                reflectance: Some(5.0),
                exposure_ev: Some(0.5),
                contrast: Some(1.1),
                saturation: Some(1.2),
                sun_light: Some([2.0; 3]),
                sky_light: Some([0.5, 0.6, 0.7]),
            }
        );
        // The turbidity, in its own units.
        let t = with(&[("TUILE_AIR_TURBIDITY", "1.5")]).unwrap();
        assert!((t.params.turbidity() - 1.5).abs() < 1e-12);
    }

    #[test]
    fn a_value_out_of_its_range_or_not_a_number_is_refused() {
        for (name, value) in [
            ("TUILE_AIR_TURBIDITY", "0.5"),
            ("TUILE_AIR_TURBIDITY", "NaN"),
            ("TUILE_AIR_TURBIDITY", "inf"),
            ("TUILE_AIR_TURBIDITY", "two"),
            ("TUILE_AIR_AOD_550", "-0.1"),
            ("TUILE_AIR_MIE_SCALE_HEIGHT", "50"),
            ("TUILE_AIR_MIE_ALBEDO", "1.1"),
            ("TUILE_AIR_MIE_G", "1"),
            ("TUILE_AIR_RAYLEIGH_SCATTERING", "5e-6,13e-6"),
            ("TUILE_AIR_RAYLEIGH_SCATTERING", "5e-6,13e-6,1"),
            ("TUILE_AIR_GROUND_ALBEDO", "1.5"),
            ("TUILE_AIR_EXPOSURE_FOLLOWS", "1"),
            ("TUILE_AIR_METER_REFLECTANCE", "0"),
            ("TUILE_AIR_VEIL_BOUND", "0.5"),
            ("TUILE_AIR_MARCH_STEPS", "12.5"),
            ("TUILE_AIR_MARCH_STEPS", "2"),
            ("TUILE_AIR_VIEWER_STRENGTH", "-1"),
            ("TUILE_LOOK_HIGHLIGHT_KNEE", "1"),
            ("TUILE_LOOK_SUN_LIGHT", "1,2"),
        ] {
            let e = with(&[(name, value)]).expect_err(&format!("{name}={value}"));
            assert_eq!(e.name, name, "{e}");
        }
        // The same aerosols twice.
        let e = with(&[("TUILE_AIR_TURBIDITY", "2"), ("TUILE_AIR_AOD_550", "0.1")]).unwrap_err();
        assert!(e.to_string().contains("one or the other"), "{e}");
        // Aerosols over the molecules.
        let e = with(&[
            ("TUILE_AIR_MIE_SCALE_HEIGHT", "6000"),
            ("TUILE_AIR_RAYLEIGH_SCALE_HEIGHT", "5500"),
        ])
        .unwrap_err();
        assert_eq!(e.name, "TUILE_AIR_MIE_SCALE_HEIGHT");
    }

    #[test]
    fn every_knob_is_documented() {
        let docs = include_str!("../../../docs/19-sky-atmosphere.md");
        for k in KNOBS.iter().filter(|k| k.arity > 0) {
            assert!(docs.contains(k.name), "{} is not in docs/19", k.name);
        }
    }
}
