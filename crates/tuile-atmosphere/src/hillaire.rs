// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The physical atmosphere: Hillaire's look-up tables, computed on the CPU,
//! in f64 — the reference every GPU path is held against.
//!
//! After S. Hillaire, *A Scalable and Production Ready Sky and Atmosphere
//! Rendering Technique* (EGSR 2020), §4–5: a **transmittance** table indexed
//! by height and the cosine of a ray's zenith angle (Bruneton's
//! parametrisation, *Precomputed Atmospheric Scattering*, 2017), a
//! **multiple-scattering** table — the paper's isotropic approximation of
//! every order past the first, `Ψ = L₂ / (1 − f_ms)` — indexed by height and
//! the sun's zenith cosine, and short ray marches over both for the sky seen
//! from a point and for the air between an eye and the ground. Written from
//! the paper; its MIT reference implementation and Bevy's were read for the
//! parametrisations, nothing of either is copied.
//!
//! `aerial.wgsl` and `hillaire.wgsl` are this module's twins on the GPU:
//! [`WGSL`] marches as [`LutAtmosphere::march`] does, over the same tables
//! uploaded.
//!
//! # Units
//!
//! The sun's irradiance outside the air is one, in every channel: a radiance
//! here is per unit of it. A renderer scales by what one is in its own units
//! ([`SkyLight::illuminance`]).

use std::f64::consts::PI;

use glam::{DMat3, DVec3};
use tuile_core::geo::{enu_frame, Geodetic};

use crate::illumination::{CameraResponse, Moonlight, ReferenceIllumination, DAYLIGHT_EV100};
use crate::params::{AtmosphereFrame, AtmosphereParams};

/// The GPU twin: the tables' parametrisations and the marches, as WGSL
/// functions over an `Atmosphere` uniform the including module binds.
pub const WGSL: &str = include_str!("hillaire.wgsl");

/// The bindings [`WGSL`] reads, at `group`: what any renderer — a film's or
/// a viewer's — declares before including it, its bind group laid out the
/// same. Binding 4 is a filtering sampler; 7, the night lights, a filterable
/// texture; every other a 32-bit float table, read by hand.
pub fn wgsl_bindings(group: u32) -> String {
    format!(
        "@group({group}) @binding(0) var<uniform> atmosphere: Atmosphere;\n\
         @group({group}) @binding(1) var transmittance_lut: texture_2d<f32>;\n\
         @group({group}) @binding(2) var multi_scattering_lut: texture_2d<f32>;\n\
         @group({group}) @binding(3) var sky_view_lut: texture_2d<f32>;\n\
         @group({group}) @binding(4) var lut_sampler: sampler;\n\
         @group({group}) @binding(5) var irradiance_lut: texture_2d<f32>;\n\
         @group({group}) @binding(6) var moon_view_lut: texture_2d<f32>;\n\
         @group({group}) @binding(7) var night_lights_lut: texture_2d<f32>;\n\
         @group({group}) @binding(8) var star_table: texture_2d<f32>;\n"
    )
}

/// Transmittance table: width (zenith cosine) by height.
pub const TRANSMITTANCE_SIZE: [usize; 2] = [256, 64];
/// Multiple-scattering table: sun zenith cosine by height.
pub const MULTI_SCATTERING_SIZE: [usize; 2] = [32, 32];
/// Sky-view table, made per frame: azimuth from the sun by zenith angle.
pub const SKY_VIEW_SIZE: [usize; 2] = [192, 108];
/// Level-ground irradiance table: sun zenith cosine by height. Fine across,
/// where the terminator is: a texel is under a degree of the sun's height.
pub const IRRADIANCE_SIZE: [usize; 2] = [256, 16];
/// Directions over which a texel of the irradiance table is summed.
const IRRADIANCE_TABLE_DIRECTIONS: usize = 384;
/// The planet radius the tables are laid out for. A height indexes them; the
/// radius shapes only where the horizon falls in them, and the Earth's
/// differ from this one by a few parts in a thousand.
pub const LUT_RADIUS: f64 = 6_360_000.0;
/// The sun's angular radius, in radians: half of 0.533°.
pub const SUN_ANGULAR_RADIUS: f64 = 0.004_652;

/// Steps of each ray of the multiple-scattering table, and its directions.
const MULTI_SCATTERING_STEPS: usize = 20;
const MULTI_SCATTERING_SQRT_DIRECTIONS: usize = 8;
/// Steps of a transmittance ray, to the top of the air.
const TRANSMITTANCE_STEPS: usize = 100;
/// Steps of a sky ray, in [`LutAtmosphere::sky_radiance`] and the sky-view.
pub const SKY_STEPS: usize = 30;
/// Directions over which the sky's irradiance is summed.
const IRRADIANCE_DIRECTIONS: usize = 1024;

/// A clear day's tables ([`LutAtmosphere::today`]), as
/// [`LutAtmosphere::table_bytes`] writes them.
pub const TODAY_TABLES: &[u8] = include_bytes!("today.tables");

/// The physical atmosphere of one [`AtmosphereParams`], its two constant
/// tables built.
#[derive(Debug, Clone)]
pub struct LutAtmosphere {
    params: AtmosphereParams,
    transmittance: Vec<[f64; 3]>,
    multi_scattering: Vec<[f64; 3]>,
    irradiance: Vec<[f64; 3]>,
}

/// Bruneton's half-texel remap: a table's texel `i` holds the unit value
/// `i / (n − 1)`, its first and last texels the ends of the range exactly.
fn uv_from_unit(x: f64, n: usize) -> f64 {
    0.5 / n as f64 + x * (1.0 - 1.0 / n as f64)
}

fn bilinear(table: &[[f64; 3]], size: [usize; 2], u: f64, v: f64) -> [f64; 3] {
    let [w, h] = size;
    let x = (u * w as f64 - 0.5).clamp(0.0, (w - 1) as f64);
    let y = (v * h as f64 - 0.5).clamp(0.0, (h - 1) as f64);
    let (x0, y0) = (x.floor() as usize, y.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = (x - x0 as f64, y - y0 as f64);
    let at = |x: usize, y: usize| table[y * w + x];
    let mut out = [0.0; 3];
    for (c, o) in out.iter_mut().enumerate() {
        let top = at(x0, y0)[c] * (1.0 - fx) + at(x1, y0)[c] * fx;
        let bottom = at(x0, y1)[c] * (1.0 - fx) + at(x1, y1)[c] * fx;
        *o = top * (1.0 - fy) + bottom * fy;
    }
    out
}

/// Where a ray from radius `r` at zenith cosine `mu` leaves a sphere of
/// radius `radius`, if it meets it: the far crossing for a sphere around
/// the point, the near one for a sphere under it.
fn sphere_exit(r: f64, mu: f64, radius: f64) -> Option<f64> {
    let disc = r * r * (mu * mu - 1.0) + radius * radius;
    (disc >= 0.0).then(|| -r * mu + disc.sqrt())
}

/// Where a ray from radius `r` at zenith cosine `mu` enters a sphere of
/// radius `top` from outside it: the near crossing, if it meets it at all
/// ahead. `Some(0)` for a ray that starts inside.
fn sphere_entry(r: f64, mu: f64, top: f64) -> Option<f64> {
    if r <= top {
        return Some(0.0);
    }
    let disc = r * r * (mu * mu - 1.0) + top * top;
    (mu < 0.0 && disc >= 0.0).then(|| -r * mu - disc.sqrt())
}

fn ground_hit(r: f64, mu: f64, bottom: f64) -> Option<f64> {
    let disc = r * r * (mu * mu - 1.0) + bottom * bottom;
    if mu >= 0.0 || disc < 0.0 {
        return None;
    }
    let t = -r * mu - disc.sqrt();
    (t >= 0.0).then_some(t)
}

/// How much of the sun's disc stands over the horizon of radius `r`, the
/// sun at zenith cosine `mu_s`: a smooth step across the disc's diameter, so
/// that a sun setting dims over the minutes it takes rather than at once.
pub fn sun_visible(r: f64, mu_s: f64) -> f64 {
    body_visible(r, mu_s, SUN_ANGULAR_RADIUS)
}

/// How much of a body's disc of angular radius `rho` stands over the
/// horizon of radius `r`, its centre at zenith cosine `mu`: as
/// [`sun_visible`], each body by its own size — the Moon's a little smaller
/// than the sun's, and changing with its distance.
pub fn body_visible(r: f64, mu: f64, rho: f64) -> f64 {
    let horizon = -(1.0 - (LUT_RADIUS / r).min(1.0).powi(2)).max(0.0).sqrt();
    let over = mu.clamp(-1.0, 1.0).asin() - horizon.asin();
    let rho = rho.max(1e-9);
    let x = ((over + rho) / (2.0 * rho)).clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// Rayleigh's phase function.
pub fn rayleigh_phase(cos: f64) -> f64 {
    3.0 / (16.0 * PI) * (1.0 + cos * cos)
}

/// Cornette and Shanks' phase function for aerosols of anisotropy `g`.
pub fn mie_phase(cos: f64, g: f64) -> f64 {
    let g2 = g * g;
    3.0 / (8.0 * PI) * (1.0 - g2) * (1.0 + cos * cos)
        / ((2.0 + g2) * (1.0 + g2 - 2.0 * g * cos).max(1e-6).powf(1.5))
}

/// The sky-view table's parametrisation (Hillaire 2020 §5.3), as
/// `hillaire.wgsl`'s `sky_view_direction`: for unit coordinates across and
/// down the table, seen from `height`, the cosine of the zenith angle and
/// the cosine of the azimuth from the sun. Across, `1 − 2u²`; down, the
/// horizon at the middle and the rows bunched towards it.
pub fn sky_view_direction(unit: [f64; 2], height: f64) -> (f64, f64) {
    let h = height.max(0.0);
    let r = LUT_RADIUS + h;
    let beta = ((h * (2.0 * LUT_RADIUS + h)).sqrt() / r)
        .clamp(-1.0, 1.0)
        .acos();
    let zenith_horizon = PI - beta;
    let theta = if unit[1] < 0.5 {
        let k = 1.0 - 2.0 * unit[1];
        zenith_horizon * (1.0 - k * k)
    } else {
        let k = 2.0 * unit[1] - 1.0;
        zenith_horizon + beta * k * k
    };
    (theta.cos(), 1.0 - 2.0 * unit[0] * unit[0])
}

/// The direction the sky-view table's texel `(i, j)` holds, seen from
/// `height`, in a frame whose `z` is up and whose `x` is towards the sun's
/// azimuth.
pub fn sky_view_texel_direction(i: usize, j: usize, height: f64) -> DVec3 {
    let [w, h] = SKY_VIEW_SIZE;
    let unit = [i as f64 / (w - 1) as f64, j as f64 / (h - 1) as f64];
    let (mu, c) = sky_view_direction(unit, height);
    let s = (1.0 - mu * mu).max(0.0).sqrt();
    DVec3::new(s * c, s * (1.0 - c * c).max(0.0).sqrt(), mu)
}

/// The light of one ray through the air: what it gathers on the way and
/// what it lets through.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ray {
    pub inscattered: [f64; 3],
    pub transmittance: [f64; 3],
}

impl LutAtmosphere {
    /// `params`' atmosphere, its transmittance and multiple-scattering
    /// tables computed.
    pub fn new(params: AtmosphereParams) -> Self {
        let mut atmosphere = Self {
            params,
            transmittance: Vec::new(),
            multi_scattering: Vec::new(),
            irradiance: Vec::new(),
        };
        atmosphere.transmittance = atmosphere.build_transmittance();
        atmosphere.multi_scattering = atmosphere.build_multi_scattering();
        atmosphere.irradiance = atmosphere.build_irradiance();
        atmosphere
    }

    /// A clear day's atmosphere ([`AtmosphereParams::default`]), its tables
    /// read rather than computed: [`TODAY_TABLES`], made by
    /// `examples/bake_today_tables.rs` from [`Self::new`] and held to it by a
    /// test. Building them takes seconds even optimised — a wait an
    /// interactive viewer cannot put before its first frame, least of all
    /// in a browser, where it would hold the page's only thread. Stored as
    /// 32-bit floats, which is what a GPU is handed anyway.
    pub fn today() -> Self {
        let mut texels = TODAY_TABLES
            .as_chunks::<12>()
            .0
            .iter()
            .map(|t| {
                let c =
                    |i: usize| f64::from(f32::from_le_bytes([t[i], t[i + 1], t[i + 2], t[i + 3]]));
                [c(0), c(4), c(8)]
            })
            .collect::<Vec<_>>();
        let count = |[w, h]: [usize; 2]| w * h;
        assert_eq!(
            texels.len(),
            count(TRANSMITTANCE_SIZE) + count(MULTI_SCATTERING_SIZE) + count(IRRADIANCE_SIZE),
            "TODAY_TABLES holds the three tables"
        );
        let irradiance = texels.split_off(texels.len() - count(IRRADIANCE_SIZE));
        let multi_scattering = texels.split_off(texels.len() - count(MULTI_SCATTERING_SIZE));
        Self {
            params: AtmosphereParams::default(),
            transmittance: texels,
            multi_scattering,
            irradiance,
        }
    }

    /// `params`' atmosphere: a clear day's read ([`Self::today`]), any
    /// other computed ([`Self::new`]).
    pub fn of(params: AtmosphereParams) -> Self {
        if params == AtmosphereParams::default() {
            Self::today()
        } else {
            Self::new(params)
        }
    }

    /// The three tables as [`TODAY_TABLES`] stores them: transmittance,
    /// multiple scattering, then irradiance, three little-endian 32-bit
    /// floats a texel.
    pub fn table_bytes(&self) -> Vec<u8> {
        self.transmittance
            .iter()
            .chain(&self.multi_scattering)
            .chain(&self.irradiance)
            .flat_map(|t| t.iter().flat_map(|&v| (v as f32).to_le_bytes()))
            .collect()
    }

    pub fn params(&self) -> &AtmosphereParams {
        &self.params
    }

    fn top(&self) -> f64 {
        LUT_RADIUS + self.params.thickness
    }

    /// The transmittance table, row by row (height), RGB.
    pub fn transmittance_table(&self) -> &[[f64; 3]] {
        &self.transmittance
    }

    /// The multiple-scattering table, row by row (height), RGB: Ψ, the
    /// radiance every order past the first adds, per unit of scattering.
    pub fn multi_scattering_table(&self) -> &[[f64; 3]] {
        &self.multi_scattering
    }

    /// The level-ground irradiance table, row by row (height), RGB: the
    /// sky's light alone on level ground, per unit of the sun's outside the
    /// air, the sun's zenith cosine across.
    pub fn irradiance_table(&self) -> &[[f64; 3]] {
        &self.irradiance
    }

    fn build_irradiance(&self) -> Vec<[f64; 3]> {
        let [w, h] = IRRADIANCE_SIZE;
        let mut table = Vec::with_capacity(w * h);
        for j in 0..h {
            let height = irradiance_height(j as f64 / (h - 1) as f64, self.params.thickness);
            for i in 0..w {
                let mu_s = 2.0 * (i as f64 / (w - 1) as f64) - 1.0;
                let sun = DVec3::new((1.0 - mu_s * mu_s).max(0.0).sqrt(), 0.0, mu_s);
                table.push(
                    self.sky_irradiance_with(
                        height,
                        sun,
                        SUN_ANGULAR_RADIUS,
                        &[DVec3::Z],
                        IRRADIANCE_TABLE_DIRECTIONS,
                    )[0],
                );
            }
        }
        table
    }

    /// The sky's light on level ground at `height`, the sun at zenith
    /// cosine `mu_s`, per unit of the sun's outside the air: from the
    /// table. What lights ground far from where a frame's light was worked
    /// out — the night side of a terminator gets its own.
    pub fn level_irradiance(&self, height: f64, mu_s: f64) -> [f64; 3] {
        let [w, h] = IRRADIANCE_SIZE;
        let x = (mu_s * 0.5 + 0.5).clamp(0.0, 1.0);
        let y = irradiance_unit(height, self.params.thickness);
        bilinear(
            &self.irradiance,
            IRRADIANCE_SIZE,
            uv_from_unit(x, w),
            uv_from_unit(y, h),
        )
    }

    /// Bruneton's parametrisation of the transmittance table: unit
    /// coordinates for radius `r` and zenith cosine `mu`.
    fn transmittance_unit(&self, r: f64, mu: f64) -> (f64, f64) {
        let (bottom, top) = (LUT_RADIUS, self.top());
        let h = (top * top - bottom * bottom).sqrt();
        let rho = (r * r - bottom * bottom).max(0.0).sqrt();
        let d = sphere_exit(r, mu, top).unwrap_or(0.0).max(0.0);
        let (d_min, d_max) = (top - r, rho + h);
        (((d - d_min) / (d_max - d_min)).clamp(0.0, 1.0), rho / h)
    }

    fn transmittance_r_mu(&self, x_mu: f64, x_r: f64) -> (f64, f64) {
        let (bottom, top) = (LUT_RADIUS, self.top());
        let h = (top * top - bottom * bottom).sqrt();
        let rho = h * x_r;
        let r = (rho * rho + bottom * bottom).sqrt();
        let (d_min, d_max) = (top - r, rho + h);
        let d = d_min + x_mu * (d_max - d_min);
        let mu = if d == 0.0 {
            1.0
        } else {
            ((h * h - rho * rho - d * d) / (2.0 * r * d)).clamp(-1.0, 1.0)
        };
        (r, mu)
    }

    fn build_transmittance(&self) -> Vec<[f64; 3]> {
        let [w, h] = TRANSMITTANCE_SIZE;
        let mut table = Vec::with_capacity(w * h);
        for j in 0..h {
            for i in 0..w {
                let (r, mu) =
                    self.transmittance_r_mu(i as f64 / (w - 1) as f64, j as f64 / (h - 1) as f64);
                let length = sphere_exit(r, mu, self.top()).unwrap_or(0.0).max(0.0);
                // Steps spread quadratically: dense where the ray starts,
                // which is where it is lowest unless it grazes, and a
                // grazing ray's height changes slowly about its lowest
                // point.
                let n = TRANSMITTANCE_STEPS as f64;
                let mut depth = [0.0; 3];
                for s in 0..TRANSMITTANCE_STEPS {
                    let s = s as f64;
                    let t = length * ((s + 0.5) / n).powi(2);
                    let dt = length * ((s + 1.0) * (s + 1.0) - s * s) / (n * n);
                    let height = (r * r + t * t + 2.0 * r * mu * t).sqrt() - LUT_RADIUS;
                    let e = self.params.medium(height).extinction;
                    for c in 0..3 {
                        depth[c] += e[c] * dt;
                    }
                }
                table.push(depth.map(|d| (-d).exp()));
            }
        }
        table
    }

    /// The share of light left along a ray from radius `r` at zenith
    /// cosine `mu` to the top of the air, from the table. A ray that meets
    /// the ground is read as the one that grazes it: what keeps the light
    /// out is [`sun_visible`].
    ///
    /// From outside the air, along the line to where it enters it: all of
    /// it for a line that misses it.
    pub fn transmittance(&self, r: f64, mu: f64) -> [f64; 3] {
        let top = self.top();
        let (r, mu) = if r > top {
            match sphere_entry(r, mu, top) {
                Some(t) => (top, ((r * mu + t) / top).clamp(-1.0, 1.0)),
                None => return [1.0; 3],
            }
        } else {
            (r, mu)
        };
        let (x_mu, x_r) = self.transmittance_unit(r, mu);
        let [w, h] = TRANSMITTANCE_SIZE;
        bilinear(
            &self.transmittance,
            TRANSMITTANCE_SIZE,
            uv_from_unit(x_mu, w),
            uv_from_unit(x_r, h),
        )
    }

    /// The sun's light reaching radius `r`, the sun at zenith cosine `mu_s`:
    /// the transmittance, the planet's shadow taken.
    pub fn sunlight(&self, r: f64, mu_s: f64) -> [f64; 3] {
        self.body_light(r, mu_s, SUN_ANGULAR_RADIUS)
    }

    /// A body's light reaching radius `r`, its centre at zenith cosine
    /// `mu` and its disc of angular radius `rho` cut by the planet: the
    /// sun's ([`Self::sunlight`]) or the Moon's, each by its own size.
    pub fn body_light(&self, r: f64, mu: f64, rho: f64) -> [f64; 3] {
        let v = body_visible(r, mu, rho);
        self.transmittance(r, mu).map(|t| t * v)
    }

    /// Ψ at `height`, the sun at zenith cosine `mu_s`.
    pub fn multi_scattering(&self, height: f64, mu_s: f64) -> [f64; 3] {
        let [w, h] = MULTI_SCATTERING_SIZE;
        let x = (mu_s * 0.5 + 0.5).clamp(0.0, 1.0);
        let y = (height / self.params.thickness).clamp(0.0, 1.0);
        bilinear(
            &self.multi_scattering,
            MULTI_SCATTERING_SIZE,
            uv_from_unit(x, w),
            uv_from_unit(y, h),
        )
    }

    fn build_multi_scattering(&self) -> Vec<[f64; 3]> {
        let [w, h] = MULTI_SCATTERING_SIZE;
        let n = MULTI_SCATTERING_SQRT_DIRECTIONS;
        let mut table = Vec::with_capacity(w * h);
        for j in 0..h {
            for i in 0..w {
                let mu_s = 2.0 * (i as f64 / (w - 1) as f64) - 1.0;
                let height = (j as f64 / (h - 1) as f64) * self.params.thickness;
                let r = LUT_RADIUS + height.max(1.0);
                let sun = DVec3::new((1.0 - mu_s * mu_s).max(0.0).sqrt(), 0.0, mu_s);
                let (mut second, mut f_ms) = ([0.0; 3], [0.0; 3]);
                for a in 0..n {
                    for b in 0..n {
                        let cos_theta = 1.0 - 2.0 * (a as f64 + 0.5) / n as f64;
                        let sin_theta = (1.0 - cos_theta * cos_theta).max(0.0).sqrt();
                        let phi = 2.0 * PI * (b as f64 + 0.5) / n as f64;
                        let dir =
                            DVec3::new(sin_theta * phi.cos(), sin_theta * phi.sin(), cos_theta);
                        let (l, f) = self.multi_scattering_ray(r, dir, sun);
                        for c in 0..3 {
                            second[c] += l[c] / (n * n) as f64;
                            f_ms[c] += f[c] / (n * n) as f64;
                        }
                    }
                }
                let mut psi = [0.0; 3];
                for c in 0..3 {
                    psi[c] = second[c] / (1.0 - f_ms[c]).max(1e-6);
                }
                table.push(psi);
            }
        }
        table
    }

    /// One direction of a multiple-scattering texel: the light scattered
    /// once towards the point, phase taken isotropic, and the share of light
    /// scattered towards it at all — both before the `1/4π` of an isotropic
    /// phase over the sphere's `4π`, which cancel.
    fn multi_scattering_ray(&self, r: f64, dir: DVec3, sun: DVec3) -> ([f64; 3], [f64; 3]) {
        let mu = dir.z;
        let ground = ground_hit(r, mu, LUT_RADIUS);
        let length = ground
            .or_else(|| sphere_exit(r, mu, self.top()))
            .unwrap_or(0.0);
        let dt = length / MULTI_SCATTERING_STEPS as f64;
        let origin = DVec3::new(0.0, 0.0, r);
        let (mut l, mut f, mut through) = ([0.0; 3], [0.0; 3], [1.0; 3]);
        let isotropic = 1.0 / (4.0 * PI);
        for s in 0..MULTI_SCATTERING_STEPS {
            let p = origin + dir * ((s as f64 + 0.5) * dt);
            let pr = p.length();
            let medium = self.params.medium(pr - LUT_RADIUS);
            let sunlight = self.sunlight(pr, p.dot(sun) / pr);
            for c in 0..3 {
                let scattering = medium.rayleigh[c] + medium.mie;
                let e = medium.extinction[c].max(1e-12);
                let step = (-e * dt).exp();
                let integral = (1.0 - step) / e;
                l[c] += through[c] * scattering * sunlight[c] * isotropic * integral;
                f[c] += through[c] * scattering * integral;
                through[c] *= step;
            }
        }
        if let Some(t) = ground {
            let p = origin + dir * t;
            let up = p.normalize();
            let lit = self.sunlight(p.length(), up.dot(sun));
            let cos = up.dot(sun).max(0.0);
            for c in 0..3 {
                l[c] += through[c] * lit[c] * cos * self.params.ground_albedo / PI;
            }
        }
        (l, f)
    }

    /// A ray from `height` along `dir`, the sun along `sun` (both unit, in
    /// a frame whose `z` is up at the start), for `length` metres or, if
    /// `None`, to the ground or out of the air — the ground, met, lit by the
    /// sun and sending its albedo's share back.
    ///
    /// `steps` samples, spread quadratically (denser near the start, where
    /// a sky ray's air is) when `quadratic`, evenly otherwise, each step
    /// integrated as Hillaire integrates it — the scattering over the step's
    /// own extinction, which stays exact however long the step.
    pub fn march(
        &self,
        height: f64,
        dir: DVec3,
        sun: DVec3,
        length: Option<f64>,
        steps: usize,
        quadratic: bool,
    ) -> Ray {
        self.march_under(
            height,
            dir,
            sun,
            SUN_ANGULAR_RADIUS,
            length,
            steps,
            quadratic,
        )
    }

    /// [`Self::march`] under a light whose disc's angular radius is
    /// `radius` — the Moon's, say: its horizon cut by its own size.
    #[allow(clippy::too_many_arguments)]
    pub fn march_under(
        &self,
        height: f64,
        dir: DVec3,
        sun: DVec3,
        radius: f64,
        length: Option<f64>,
        steps: usize,
        quadratic: bool,
    ) -> Ray {
        let r0 = LUT_RADIUS + height.max(0.0);
        // From outside the air, the march starts where the ray enters it,
        // and a ray that never does gathers nothing.
        let Some(entry) = sphere_entry(r0, dir.z, self.top()) else {
            return Ray {
                inscattered: [0.0; 3],
                transmittance: [1.0; 3],
            };
        };
        let start = DVec3::new(0.0, 0.0, r0) + dir * entry;
        let r = start.length();
        // The frame turned so that `z` is up where the march starts.
        let turn = glam::DQuat::from_rotation_arc(start / r, DVec3::Z);
        let (dir, sun) = (turn * dir, turn * sun);
        let length = length.map(|l| l - entry);
        let mu = dir.z;
        let ground = ground_hit(r, mu, LUT_RADIUS);
        let to_end = ground
            .or_else(|| sphere_exit(r, mu, self.top()))
            .unwrap_or(0.0);
        let (t_max, ends_on_ground) = match length {
            Some(l) => (l.min(to_end.max(0.0)).max(0.0), false),
            None => (to_end.max(0.0), ground.is_some()),
        };
        let cos = dir.dot(sun);
        let (p_r, p_m) = (
            rayleigh_phase(cos),
            mie_phase(cos, self.params.mie_anisotropy),
        );
        let origin = DVec3::new(0.0, 0.0, r);
        let (mut inscattered, mut through) = ([0.0; 3], [1.0; 3]);
        let mut t_prev = 0.0;
        for s in 0..steps {
            let (t, t_next) = if quadratic {
                let a = (s as f64 + 0.3) / steps as f64;
                let b = (s as f64 + 1.0) / steps as f64;
                (t_max * a * a, t_max * b * b)
            } else {
                (
                    t_max * (s as f64 + 0.5) / steps as f64,
                    t_max * (s as f64 + 1.0) / steps as f64,
                )
            };
            let dt = t_next - t_prev;
            t_prev = t_next;
            let p = origin + dir * t;
            let pr = p.length();
            let h = pr - LUT_RADIUS;
            let medium = self.params.medium(h);
            let mu_s = p.dot(sun) / pr;
            let sunlight = self.body_light(pr, mu_s, radius);
            let multi = self.multi_scattering(h, mu_s);
            for c in 0..3 {
                let scattering = medium.rayleigh[c] + medium.mie;
                let source = medium.rayleigh[c] * p_r * sunlight[c]
                    + medium.mie * p_m * sunlight[c]
                    + scattering * multi[c];
                let e = medium.extinction[c].max(1e-12);
                let step = (-e * dt).exp();
                inscattered[c] += through[c] * source * (1.0 - step) / e;
                through[c] *= step;
            }
        }
        if ends_on_ground {
            let p = origin + dir * t_max;
            let up = p.normalize();
            let mu_s = up.dot(sun);
            let lit = self.body_light(p.length(), mu_s, radius);
            for c in 0..3 {
                inscattered[c] +=
                    through[c] * lit[c] * mu_s.max(0.0) * self.params.ground_albedo / PI;
            }
        }
        Ray {
            inscattered,
            transmittance: through,
        }
    }

    /// The sky's radiance seen from `height` along `dir`, the sun along
    /// `sun` (unit vectors, `z` up): what the sky-view table holds, without
    /// the sun's disc.
    pub fn sky_radiance(&self, height: f64, dir: DVec3, sun: DVec3) -> [f64; 3] {
        self.sky_radiance_under(height, dir, sun, SUN_ANGULAR_RADIUS)
    }

    /// [`Self::sky_radiance`] under a light whose disc's angular radius is
    /// `radius`.
    pub fn sky_radiance_under(
        &self,
        height: f64,
        dir: DVec3,
        light: DVec3,
        radius: f64,
    ) -> [f64; 3] {
        self.march_under(height, dir, light, radius, None, SKY_STEPS, true)
            .inscattered
    }

    /// The sky's irradiance at `height` on surfaces facing each of
    /// `normals` (`z` up), the sun along `sun`: the radiance of every
    /// direction, the ground's below the horizon included, summed over the
    /// hemisphere each faces.
    pub fn sky_irradiance(&self, height: f64, sun: DVec3, normals: &[DVec3]) -> Vec<[f64; 3]> {
        self.sky_irradiance_with(
            height,
            sun,
            SUN_ANGULAR_RADIUS,
            normals,
            IRRADIANCE_DIRECTIONS,
        )
    }

    fn sky_irradiance_with(
        &self,
        height: f64,
        sun: DVec3,
        radius: f64,
        normals: &[DVec3],
        n: usize,
    ) -> Vec<[f64; 3]> {
        let mut out = vec![[0.0; 3]; normals.len()];
        let golden = PI * (3.0 - 5f64.sqrt());
        let solid_angle = 4.0 * PI / n as f64;
        for i in 0..n {
            let z = 1.0 - 2.0 * (i as f64 + 0.5) / n as f64;
            let ring = (1.0 - z * z).max(0.0).sqrt();
            let phi = golden * i as f64;
            let dir = DVec3::new(ring * phi.cos(), ring * phi.sin(), z);
            let l = self.sky_radiance_under(height, dir, sun, radius);
            for (o, normal) in out.iter_mut().zip(normals) {
                let cos = normal.dot(dir);
                if cos > 0.0 {
                    for c in 0..3 {
                        o[c] += l[c] * cos * solid_angle;
                    }
                }
            }
        }
        out
    }

    /// What one moment's sun, Moon and sky give the ground at `at`, in the
    /// renderer's units — those in which, under `reference`'s air (a clear
    /// day's), a sun 45° up gives level ground what `calibration` gives it.
    /// Imagery is a photograph taken by such a light: under it the picture
    /// is the renderer's as it was, and at every other hour it follows the
    /// physical ratio (`docs/18` §6).
    ///
    /// The camera is exposed for what `metering` sees of the frame: the
    /// light on its ground points, and its sky as bright as ground of
    /// [`METER_REFLECTANCE`] would have to be lit to match it — their mean,
    /// so that a frame half in daylight is exposed for its day and not for
    /// its night, and a sunset for its sky as much as for its dim ground.
    /// None: for the light at `at`.
    // Three channels, read and written side by side: an index says so best.
    #[allow(clippy::needless_range_loop)]
    pub fn light(
        &self,
        reference: &LutAtmosphere,
        at: Geodetic,
        moment: Moment,
        metering: &[Metered],
        calibration: Calibration,
    ) -> SkyLight {
        let height = at.height.max(0.0);
        let r = LUT_RADIUS + height;
        let enu: DMat3 = enu_frame(at);
        let to_sun = moment.to_sun_ecef.try_normalize().unwrap_or(enu.z_axis);
        let sun = enu.transpose() * to_sun;

        // The reference: a sun 45° up through a clear day's air, here.
        let up45 = std::f64::consts::FRAC_1_SQRT_2;
        let reference_sun = reference.sunlight(r, up45);
        let reference_sky =
            reference.sky_irradiance(height, DVec3::new(up45, 0.0, up45), &[DVec3::Z])[0];
        let mut scale = [0.0f64; 3];
        let mut calibrated = [0.0f64; 3];
        for c in 0..3 {
            calibrated[c] = f64::from(calibration.sun[c]) * up45 + f64::from(calibration.sky[c]);
            let physical = (reference_sun[c] * up45 + reference_sky[c]) / PI;
            scale[c] = calibrated[c] / physical.max(1e-12);
        }
        // What one unit of the model's irradiance is on the ground, in the
        // renderer's units.
        let ground = scale.map(|s| s / PI);

        // Each light's sky, as an L1 harmonic, and its beam.
        let sun_sky = self.sky_harmonic(height, sun, SUN_ANGULAR_RADIUS, enu);
        let transmitted = self.sunlight(r, sun.z);
        let moon = moment.moon.map(|m| {
            let to_moon = m.to_moon_ecef.try_normalize().unwrap_or(-enu.z_axis);
            let local = enu.transpose() * to_moon;
            let strength = m.outside_air();
            (
                m,
                to_moon,
                local,
                strength,
                self.sky_harmonic(height, local, f64::from(m.angular_radius), enu),
            )
        });

        let mut sun_rgb = [0.0f32; 3];
        let mut moon_rgb = [0.0f32; 3];
        let mut sky_sh = [[0.0f32; 3]; 4];
        let mut level = [0.0f64; 3];
        for c in 0..3 {
            sun_rgb[c] = (transmitted[c] * ground[c]) as f32;
            let mut a = sun_sky.mean[c];
            let mut gradient = sun_sky.gradient[c];
            level[c] = transmitted[c] * sun.z.max(0.0) + sun_sky.level[c];
            if let Some((m, _, local, strength, sky)) = &moon {
                let beam = self.body_light(r, local.z, f64::from(m.angular_radius))[c] * strength;
                moon_rgb[c] = (beam * ground[c]) as f32;
                a += sky.mean[c] * strength;
                gradient += sky.gradient[c] * strength;
                level[c] += beam * local.z.max(0.0) + sky.level[c] * strength;
            }
            sky_sh[0][c] = (a * ground[c]) as f32;
            let g = gradient * ground[c];
            sky_sh[1][c] = g.x as f32;
            sky_sh[2][c] = g.y as f32;
            sky_sh[3][c] = g.z as f32;
            level[c] *= ground[c];
        }

        // The level ground of anywhere, from the tables: here, and over the
        // metered ground — set to agree with the exact sum here.
        let moon_at = moon
            .map(|(m, to_moon, _, strength, _)| (to_moon, strength, f64::from(m.angular_radius)));
        let tabled = |height: f64, up: DVec3| -> [f64; 3] {
            let r = LUT_RADIUS + height.max(0.0);
            let mu_s = to_sun.dot(up);
            let beam = self.sunlight(r, mu_s);
            let sky = self.level_irradiance(height, mu_s);
            let mut out = [0.0; 3];
            for c in 0..3 {
                out[c] = beam[c] * mu_s.max(0.0) + sky[c];
            }
            if let Some((to_moon, strength, rho)) = moon_at {
                let mu_m = to_moon.dot(up);
                let beam = self.body_light(r, mu_m, rho);
                let sky = self.level_irradiance(height, mu_m);
                for c in 0..3 {
                    out[c] += (beam[c] * mu_m.max(0.0) + sky[c]) * strength;
                }
            }
            for c in 0..3 {
                out[c] *= ground[c];
            }
            out
        };
        let here = tabled(height, enu.z_axis);
        let agree = |c: usize| {
            if here[c] > 1e-30 {
                level[c] / here[c]
            } else {
                1.0
            }
        };
        let mut metered = level;
        let meter = calibration.camera.meter_reflectance;
        let frame_radius = AtmosphereFrame::at(at).radius;
        // One ground point seen from `eye`, its height and up `height`, `up`:
        // its light through the air between — what the air leaves of it and
        // what it adds, as bright as ground of the meter's grey would be lit.
        let ground_seen = |eye: DVec3, point: DVec3, height: f64, up: DVec3| -> [f64; 3] {
            let l = tabled(height, up);
            let from = tuile_core::geo::ecef_to_geodetic(eye);
            let local = enu_frame(from).transpose();
            let path = point - eye;
            let dir = (local * path).normalize_or(DVec3::NEG_Z);
            let length = Some(path.length());
            let air = self.march(from.height, dir, local * to_sun, length, 16, false);
            let mut through = air.transmittance;
            let mut added = air.inscattered;
            if let Some((to_moon, strength, rho)) = moon_at {
                let m = self.march_under(from.height, dir, local * to_moon, rho, length, 16, false);
                through = m.transmittance;
                for c in 0..3 {
                    added[c] += m.inscattered[c] * strength;
                }
            }
            [0, 1, 2].map(|c| l[c] * agree(c) * through[c] + added[c] * scale[c] / meter)
        };
        // The sky along `direction` from `eye`, and how much it counts: as
        // much as the air it crosses — a ray through the air whole, one past
        // it into space not at all, one between by its air — so that a frame
        // of the limb is exposed for the limb and the Earth, not for the
        // black around them, and an eye that climbs out of the air meets no
        // step. Grazing the air over the ground it grazes (`lut_direction`).
        let sky_seen = |eye: DVec3, direction: DVec3| -> ([f64; 3], f64) {
            let g = tuile_core::geo::ecef_to_geodetic(eye);
            let local = enu_frame(g).transpose();
            let dir = lut_direction(
                (local * direction).normalize_or(DVec3::Z),
                frame_radius,
                g.height,
            );
            let weight = self.meter_weight(g.height, dir.z);
            if weight <= 0.0 {
                return ([0.0; 3], 0.0);
            }
            let mut l = self.sky_radiance(g.height, dir, local * to_sun);
            if let Some((to_moon, strength, rho)) = moon_at {
                let m = self.sky_radiance_under(g.height, dir, local * to_moon, rho);
                for c in 0..3 {
                    l[c] += m[c] * strength;
                }
            }
            ([0, 1, 2].map(|c| l[c] * scale[c] / meter), weight)
        };
        let luminance = |v: [f64; 3]| 0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2];
        let wanted = luminance(calibrated);
        // The meter's floor ε: under it every part of a frame counts alike.
        let epsilon =
            calibration.camera.meter_floor * f64::from(ReferenceIllumination::NIGHT_FLOOR) * wanted;
        let ln = |v: [f64; 3]| (epsilon + luminance(v).max(0.0)).ln();
        if !metering.is_empty() {
            // The frame's light: its colour the mean's, its luminance the
            // log-average's — the geometric mean of the luminance over the
            // frame, so that a bright part of it weighs by its stops.
            let mut sum = [0.0; 3];
            let mut logs = 0.0;
            let mut weights = 0.0;
            let ground_radius = frame_radius + at.height.max(0.0);
            let top = frame_radius + self.params.thickness;
            for m in metering {
                let (l, log, weight) = match *m {
                    Metered::Ground { eye, point } => {
                        let g = tuile_core::geo::ecef_to_geodetic(point);
                        let v = ground_seen(eye, point, g.height, enu_frame(g).z_axis);
                        (v, ln(v), 1.0)
                    }
                    Metered::Sky { eye, direction } => {
                        let (v, weight) = sky_seen(eye, direction);
                        ([0, 1, 2].map(|c| v[c] * weight), ln(v) * weight, weight)
                    }
                    Metered::Cell(cell) => {
                        let mut l = [0.0; 3];
                        let mut log = 0.0;
                        let mut weight = 0.0;
                        for (theta, w, piece) in cell.nodes(ground_radius, top) {
                            let (v, w) = match piece {
                                Piece::Ground => {
                                    let point = cell.ground_point(theta, ground_radius);
                                    let up = (point - cell.centre()).normalize();
                                    (ground_seen(cell.eye, point, at.height.max(0.0), up), w)
                                }
                                Piece::Sky => {
                                    let (v, ww) = sky_seen(cell.eye, cell.direction(theta));
                                    (v, ww * w)
                                }
                            };
                            for c in 0..3 {
                                l[c] += v[c] * w;
                            }
                            log += ln(v) * w;
                            weight += w;
                        }
                        (l, log, weight)
                    }
                };
                weights += weight;
                logs += log;
                for c in 0..3 {
                    sum[c] += l[c];
                }
            }
            // A frame that sees nothing but space is exposed for the light
            // here, and one that sees a little air for that air: the two
            // joined continuously.
            for c in 0..3 {
                metered[c] = (sum[c] + level[c] * METER_FLOOR) / (weights + METER_FLOOR);
            }
            let geometric = ((logs + ln(level) * METER_FLOOR) / (weights + METER_FLOOR)).exp();
            let mean = luminance(metered).max(1e-300);
            metered = metered.map(|v| v * geometric / mean);
        }

        // The night floor: starlight and airglow, on all ground alike — the
        // light of a clear moonless night.
        let tint = NIGHT_TINT.map(f64::from);
        let floor_light = f64::from(ReferenceIllumination::NIGHT_FLOOR) * wanted;
        let added = tint.map(|t| t * floor_light / luminance(tint));
        for c in 0..3 {
            metered[c] += added[c];
            level[c] += added[c];
        }
        let x = (luminance(metered) / wanted).log2() as f32;
        let ev100 = DAYLIGHT_EV100 + x;
        // Where the camera is exposed for brighter ground than the floor's,
        // ground is never lit by less than what shows it at `SHOWN_FLOOR`
        // of noon: a night side beside a day side is dark, not black. A
        // least light, not an added one: it changes nothing lit by more.
        let least_light = f64::from(ReferenceIllumination::SHOWN_FLOOR)
            * wanted
            * f64::from(x - calibration.camera.shown_ev(x)).exp2();
        let least = tint.map(|t| t * least_light.max(floor_light) / luminance(tint));
        for c in 0..3 {
            sky_sh[0][c] += added[c] as f32;
        }

        // The white the camera balances to: the metered light's colour, and
        // as night falls a warm one — balanced towards, it leaves the
        // picture blue, as the dark-adapted eye sees a night.
        let white_lum = luminance(metered) / wanted;
        let night = smooth_step(-4.0, -20.0, f64::from(x));
        let mut white = [0.0f32; 3];
        for c in 0..3 {
            let physical = metered[c] / calibrated[c].max(1e-12) / white_lum.max(1e-300);
            white[c] = (physical * (1.0 - night) + f64::from(NIGHT_WHITE[c]) * night) as f32;
        }

        // The air's own light against the imagery's ground: a texel of one
        // is ground of this reflectance.
        let air = f64::from(calibration.reflectance).max(1e-6);
        let floor = added.map(|a| a * NIGHT_SKY_ALBEDO);
        // The sky's light alone on level ground here, from the tables, as a
        // fragment reads its own: what the frame's harmonic is set against.
        let mut level_reference = [0.0f32; 3];
        let sky_here = self.level_irradiance(height, to_sun.dot(enu.z_axis));
        let moon_sky = moon_at.map(|(to_moon, strength, _)| {
            self.level_irradiance(height, to_moon.dot(enu.z_axis))
                .map(|e| e * strength)
        });
        for c in 0..3 {
            let sky = sky_here[c] + moon_sky.map_or(0.0, |m| m[c]);
            level_reference[c] = (sky * ground[c] + added[c]) as f32;
        }

        SkyLight {
            illumination: ReferenceIllumination {
                to_sun_ecef: to_sun,
                sun_rgb,
                sky_sh,
                white,
                adaptation: calibration.camera.white_adaptation,
                ev100,
                compensation_ev: moment.compensation_ev,
                night_floor: ReferenceIllumination::NIGHT_FLOOR,
                moon: moon.map(|(m, to_moon, ..)| Moonlight {
                    to_moon_ecef: to_moon,
                    moon_rgb,
                    ..m
                }),
            },
            illuminance: scale.map(|s| (s / air) as f32),
            floor: floor.map(|f| (f / air) as f32),
            ground: ground.map(|g| g as f32),
            added: added.map(|a| a as f32),
            least: least.map(|l| l as f32),
            level_reference,
            moon_strength: moon.map_or(0.0, |m| m.3),
            frame: AtmosphereFrame::at(at),
            height,
        }
    }

    /// How much a meter's ray of the sky from `height` at zenith cosine
    /// `mu` counts: by the air it crosses, whole once its optical depth is
    /// [`METER_AIR`] of the air's from the ground straight up, nothing past
    /// the air — continuous in the eye and the ray.
    pub fn meter_weight(&self, height: f64, mu: f64) -> f64 {
        let depth = |r: f64, mu: f64| -self.transmittance(r, mu)[1].max(1e-30).ln();
        let column = depth(LUT_RADIUS, 1.0);
        let r = LUT_RADIUS + height.max(0.0);
        (depth(r, mu) / (METER_AIR * column)).clamp(0.0, 1.0)
    }

    /// The sky's light at `height` under one light along `light` (`z` up):
    /// as an L1 harmonic, its constant over the facings of a wall all round
    /// and its gradient turned to ECEF axes by `enu`, and level ground's.
    #[allow(clippy::needless_range_loop)]
    fn sky_harmonic(&self, height: f64, light: DVec3, radius: f64, enu: DMat3) -> SkyHarmonic {
        // Horizontal axes turned to the light's azimuth: the sky is
        // symmetric about the light's vertical plane, so only `x` has a
        // gradient.
        let toward = DVec3::new(light.x, light.y, 0.0)
            .try_normalize()
            .unwrap_or(DVec3::X);
        let aside = DVec3::Z.cross(toward);
        let facings = [DVec3::Z, toward, -toward, aside, -aside];
        let e = self.sky_irradiance_with(height, light, radius, &facings, IRRADIANCE_DIRECTIONS);
        let axes = [toward, aside, DVec3::Z].map(|axis| enu * axis);
        let mut out = SkyHarmonic {
            mean: [0.0; 3],
            gradient: [DVec3::ZERO; 3],
            level: [0.0; 3],
        };
        for c in 0..3 {
            let a = (e[1][c] + e[2][c] + e[3][c] + e[4][c]) / 4.0;
            let b = [
                (e[1][c] - e[2][c]) / 2.0,
                (e[3][c] - e[4][c]) / 2.0,
                e[0][c] - a,
            ];
            out.mean[c] = a;
            out.level[c] = e[0][c];
            for (axis, b) in axes.iter().zip(b) {
                out.gradient[c] += *axis * b;
            }
        }
        out
    }
}

#[derive(Clone, Copy)]
struct SkyHarmonic {
    mean: [f64; 3],
    gradient: [DVec3; 3],
    level: [f64; 3],
}

fn smooth_step(from: f64, to: f64, x: f64) -> f64 {
    let t = ((x - from) / (to - from)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The irradiance table's rows: heights bunched towards the ground, where
/// the light changes fastest with height.
fn irradiance_height(unit: f64, thickness: f64) -> f64 {
    unit * unit * thickness
}

fn irradiance_unit(height: f64, thickness: f64) -> f64 {
    (height.max(0.0) / thickness).min(1.0).sqrt()
}

/// The zenith cosine over the tables' planet ([`LUT_RADIUS`]) of a line of
/// sight from `height` over a sphere of `radius` — the ground drawn — at
/// zenith cosine `mu`: the one whose lowest point stands as high over the
/// tables' ground as the line's over the sphere's, so that the air a line
/// grazes is the air over the ground it grazes. The identity where the two
/// radii are one; `lut_mu` in `hillaire.wgsl`.
pub fn lut_mu(radius: f64, height: f64, mu: f64) -> f64 {
    let h = height.max(0.0);
    let s = (1.0 - mu * mu).max(0.0).sqrt();
    let drop = (radius + h) * mu * mu / (1.0 + s);
    let one_minus = (drop / (LUT_RADIUS + h)).min(1.0);
    let m = (one_minus * (2.0 - one_minus)).max(0.0).sqrt();
    if mu < 0.0 {
        -m
    } else {
        m
    }
}

/// The unit `dir` (`z` up) with its zenith cosine turned by [`lut_mu`],
/// its azimuth kept.
pub fn lut_direction(dir: DVec3, radius: f64, height: f64) -> DVec3 {
    let flat = DVec3::new(dir.x, dir.y, 0.0);
    let l = flat.length();
    if l < 1e-12 {
        return dir;
    }
    let m = lut_mu(radius, height, dir.z);
    flat / l * (1.0 - m * m).max(0.0).sqrt() + DVec3::new(0.0, 0.0, m)
}

/// What a camera meters: a ground point seen from an eye, the sky along a
/// direction from an eye, all ECEF — or, as [`metering`] makes them, one
/// cell of its grid, averaged over its extent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Metered {
    Ground { eye: DVec3, point: DVec3 },
    Sky { eye: DVec3, direction: DVec3 },
    Cell(MeterCell),
}

/// One cell of a camera's meter, as the planet's limb crosses it: the
/// lines of sight from `eye` in the plane of `toward` — the direction of
/// the sphere's centre, `distance` away — and `across`, at angles `from`
/// to `to` from `toward`: the cell's extent across the limb.
///
/// The meter reads a cell as the mean of its light over that extent — the
/// ground's where its lines meet the ground, the air's above it, nothing
/// past the air — integrated at nodes laid on each of the three pieces.
/// The pieces' ends are the limb's and the air's top, fixed on the globe,
/// and the cell's, fixed in the picture: as the camera moves, every node
/// moves continuously, and the meter with it. A limb far thinner than a
/// cell weighs what it covers of it, not all or nothing as a ray falls on
/// it or beside it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeterCell {
    pub eye: DVec3,
    pub toward: DVec3,
    pub across: DVec3,
    pub distance: f64,
    pub from: f64,
    pub to: f64,
}

/// Which piece of a cell a node is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece {
    Ground,
    Sky,
}

/// Gauss–Legendre's nodes and weights on [−1, 1]: four for the ground,
/// eight for the air, whose light is bunched towards the ground.
const GROUND_NODES: [(f64, f64); 4] = [
    (-0.861_136_311_594_052_6, 0.347_854_845_137_453_9),
    (-0.339_981_043_584_856_3, 0.652_145_154_862_546_1),
    (0.339_981_043_584_856_3, 0.652_145_154_862_546_1),
    (0.861_136_311_594_052_6, 0.347_854_845_137_453_9),
];
const SKY_NODES: [(f64, f64); 8] = [
    (-0.960_289_856_497_536_3, 0.101_228_536_290_376_3),
    (-0.796_666_477_413_626_7, 0.222_381_034_453_374_5),
    (-0.525_532_409_916_329, 0.313_706_645_877_887_3),
    (-0.183_434_642_495_649_8, 0.362_683_783_378_362),
    (0.183_434_642_495_649_8, 0.362_683_783_378_362),
    (0.525_532_409_916_329, 0.313_706_645_877_887_3),
    (0.796_666_477_413_626_7, 0.222_381_034_453_374_5),
    (0.960_289_856_497_536_3, 0.101_228_536_290_376_3),
];

impl MeterCell {
    /// The sphere's centre.
    pub fn centre(&self) -> DVec3 {
        self.eye + self.toward * self.distance
    }

    /// The line of sight at `theta` from `toward`.
    pub fn direction(&self, theta: f64) -> DVec3 {
        self.toward * theta.cos() + self.across * theta.sin()
    }

    /// Where the line at `theta` meets the sphere of `radius`, the near
    /// side: for a line on the ground piece.
    pub fn ground_point(&self, theta: f64, radius: f64) -> DVec3 {
        let (s, c) = theta.sin_cos();
        let b = self.distance * s;
        let t = self.distance * c - (radius * radius - b * b).max(0.0).sqrt();
        self.eye + self.direction(theta) * t.max(0.0)
    }

    /// The nodes the cell's light is summed at: each line's angle, its
    /// weight — the share of the cell's extent it stands for — and its
    /// piece. The ground's under the limb, of radius `ground`; the air's
    /// between it and its top, of radius `top`; nothing past it. A cell of
    /// no extent is its one line.
    pub fn nodes(&self, ground: f64, top: f64) -> Vec<(f64, f64, Piece)> {
        let limb = if self.distance > ground {
            (ground / self.distance).asin()
        } else {
            std::f64::consts::PI
        };
        let edge = if self.distance > top {
            (top / self.distance).asin()
        } else {
            f64::INFINITY
        };
        let width = self.to - self.from;
        if width <= 1e-12 {
            let piece = if self.from < limb {
                Piece::Ground
            } else {
                Piece::Sky
            };
            return if self.from < edge || piece == Piece::Ground {
                vec![(self.from, 1.0, piece)]
            } else {
                Vec::new()
            };
        }
        let mut out = Vec::with_capacity(12);
        let (a, b) = (self.from, self.to.min(limb));
        if b > a {
            for (x, w) in GROUND_NODES {
                out.push((
                    a + (b - a) * (x + 1.0) / 2.0,
                    w * (b - a) / 2.0 / width,
                    Piece::Ground,
                ));
            }
        }
        let (a, b) = (self.from.max(limb), self.to.min(edge));
        if b > a {
            // Bunched towards the piece's start, where the air is densest:
            // θ = a + (b − a)·u², u in [0, 1].
            for (x, w) in SKY_NODES {
                let u = (x + 1.0) / 2.0;
                let dtheta = 2.0 * (b - a) * u * (w / 2.0);
                out.push((a + (b - a) * u * u, dtheta / width, Piece::Sky));
            }
        }
        out
    }
}

/// What a camera at `eye` (ECEF) meters: a grid of twenty-four by fourteen
/// cells across its view — about `fwd + x·right + y·up` for `x`, `y` in
/// [−1, 1] — each read over its extent across the limb of `sphere` raised
/// by `ground_height` ([`MeterCell`]) — the light reads that height from
/// the place it is worked out for. The same for a film's frame and a
/// viewer's: [`LutAtmosphere::light`] takes it.
pub fn metering(
    eye: DVec3,
    fwd: DVec3,
    right: DVec3,
    up: DVec3,
    sphere: &AtmosphereFrame,
    ground_height: f64,
) -> Vec<Metered> {
    let _ = ground_height; // The light's own place holds it.
    let to_centre = sphere.centre - eye;
    let distance = to_centre.length();
    let toward = to_centre / distance.max(1e-9);
    let (columns, rows) = (24, 14);
    let mut out = Vec::with_capacity(columns * rows);
    for j in 0..rows {
        for i in 0..columns {
            let at = |di: f64, dj: f64| {
                let x = (i as f64 + 0.5 + di) / columns as f64 * 2.0 - 1.0;
                let y = (j as f64 + 0.5 + dj) / rows as f64 * 2.0 - 1.0;
                (fwd + right * x + up * y).normalize()
            };
            let d = at(0.0, 0.0);
            let cos = d.dot(toward).clamp(-1.0, 1.0);
            let across = (d - toward * cos)
                .try_normalize()
                .unwrap_or_else(|| toward.any_orthonormal_vector());
            let theta = cos.acos();
            // The cell's corners, as angles from `toward` along `across`.
            let mut lo = f64::INFINITY;
            let mut hi = f64::NEG_INFINITY;
            for (di, dj) in [(-0.5, -0.5), (0.5, -0.5), (-0.5, 0.5), (0.5, 0.5)] {
                let k = at(di, dj);
                let angle = k.dot(across).atan2(k.dot(toward));
                lo = lo.min(angle);
                hi = hi.max(angle);
            }
            out.push(Metered::Cell(MeterCell {
                eye,
                toward,
                across,
                distance,
                from: lo.min(theta),
                to: hi.max(theta),
            }));
        }
    }
    out
}

/// The reflectance a camera's meter takes a scene for: middle grey. Sky is
/// metered as the light that would make ground of it as bright.
pub const METER_REFLECTANCE: f64 = 0.18;

/// The share of the air's optical depth from the ground straight up a
/// meter's ray of the sky must cross to count whole
/// ([`LutAtmosphere::meter_weight`]): a hundredth, which every ray from
/// under some 35 km crosses — there, every ray of the sky counts whole.
pub const METER_AIR: f64 = 0.01;

/// How many rays' worth the light at the eye's ground weighs in the meter:
/// what exposes a frame of space alone, and nothing beside a frame that
/// holds any air or ground.
pub const METER_FLOOR: f64 = 1e-3;

/// What one frame's light is worked out from: where the sun is, the Moon
/// if it is placed, and the exposure chosen on top.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Moment {
    pub to_sun_ecef: DVec3,
    pub moon: Option<Moonlight>,
    pub compensation_ev: f32,
}

/// The white a camera at night balances towards: warm, so that the
/// picture it leaves is blue.
pub const NIGHT_WHITE: [f32; 3] = [1.5, 1.0, 0.55];

/// The colour of the light the night floor adds: a clear twilight's sky,
/// blue — normalised to unit luminance where it is used.
pub const NIGHT_TINT: [f32; 3] = [0.55, 0.7, 1.0];

/// The night sky is as bright as ground of this albedo under the night
/// floor's light: a clear moonlit night's, the sky about as dark as the
/// ground.
pub const NIGHT_SKY_ALBEDO: f64 = 0.1;

/// The renderer's own lights — a dome's radiance and a sun's, as a white
/// Lambert surface comes out under each — which a clear day's sun 45° up is
/// brought to on level ground: [`LutAtmosphere::light`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Calibration {
    pub sun: [f32; 3],
    pub sky: [f32; 3],
    /// The reflectance of real ground that a texel of one stands for: the
    /// air's own light — the sky, what it scatters towards the eye, the
    /// night floor's sky — is divided by it, so that the air stands to the
    /// imagery as it stands to the ground it shows. The ground's light is
    /// not: under the reference the imagery is lit as it always was.
    pub reflectance: f32,
    /// How the camera meets the light: its exposure's response, its white
    /// balance, its meter.
    pub camera: CameraResponse,
}

/// One moment's light, as [`LutAtmosphere::light`] works it out: the
/// [`ReferenceIllumination`] a renderer is lit by, and what its sky and air
/// passes need besides.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkyLight {
    pub illumination: ReferenceIllumination,
    /// What a radiance of the model, per unit of the sun outside the air, is
    /// in the renderer's units, RGB, set against the imagery's ground
    /// ([`Calibration::reflectance`]): what the sky-view and the air between
    /// eye and ground are scaled by.
    pub illuminance: [f32; 3],
    /// The radiance the night floor adds to the sky, in the renderer's
    /// units: what ground of [`NIGHT_SKY_ALBEDO`] shows under the light the
    /// floor adds. Far ground fades to it.
    pub floor: [f32; 3],
    /// What one unit of the model's irradiance is on the ground, in the
    /// renderer's units, RGB: what a ground fragment's own sun and sky,
    /// read from the tables where it is, are scaled by.
    pub ground: [f32; 3],
    /// The night floor's light on every surface, in the renderer's units.
    pub added: [f32; 3],
    /// The least light any ground is lit by, in the renderer's units: what
    /// shows it at [`ReferenceIllumination::SHOWN_FLOOR`] of noon, under
    /// this frame's exposure. Ground lit by more is lit as it is.
    pub least: [f32; 3],
    /// Level ground's sky light where the light was worked out, as the
    /// tables give it, the floor included: what a fragment's own is set
    /// against to scale the frame's harmonic.
    pub level_reference: [f32; 3],
    /// The Moon's irradiance outside the air against the sun's: what the
    /// sky it lights is scaled by.
    pub moon_strength: f64,
    /// The sphere the air is laid on, at the place the light was worked out.
    pub frame: AtmosphereFrame,
    /// The height the light was worked out at, over the ground, in metres.
    pub height: f64,
}

#[cfg(test)]
// Three channels, read and written side by side: an index says so best.
#[allow(clippy::needless_range_loop)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    fn clear() -> &'static LutAtmosphere {
        static CLEAR: OnceLock<LutAtmosphere> = OnceLock::new();
        CLEAR.get_or_init(|| LutAtmosphere::new(AtmosphereParams::default()))
    }

    /// The tables a viewer reads instead of computing them are the ones the
    /// model computes, to the 32-bit float they are stored in — on any
    /// machine: one ulp of a float either way, for the last bit of an f64
    /// a platform's `exp` rounds differently.
    #[test]
    fn todays_tables_are_what_the_model_computes() {
        let read = LutAtmosphere::today();
        let made = clear();
        assert_eq!(read.params(), made.params());
        for (name, a, b) in [
            (
                "transmittance",
                read.transmittance_table(),
                made.transmittance_table(),
            ),
            (
                "multiple scattering",
                read.multi_scattering_table(),
                made.multi_scattering_table(),
            ),
            (
                "irradiance",
                read.irradiance_table(),
                made.irradiance_table(),
            ),
        ] {
            assert_eq!(a.len(), b.len(), "{name}");
            for (i, (a, b)) in a.iter().zip(b).enumerate() {
                for c in 0..3 {
                    let off = (a[c] - b[c]).abs();
                    assert!(
                        off <= 3e-7 * b[c].abs() + 1e-30,
                        "{name} texel {i} channel {c}: stored {} computed {} — run \
                         `cargo run --release -p tuile-atmosphere --example bake_today_tables`",
                        a[c],
                        b[c]
                    );
                }
            }
        }
        // And a clear day asked for by its parameters is the one read.
        assert_eq!(
            LutAtmosphere::of(AtmosphereParams::default()).table_bytes(),
            TODAY_TABLES
        );
    }

    fn up_at(elevation_deg: f64) -> DVec3 {
        let e = elevation_deg.to_radians();
        DVec3::new(e.cos(), 0.0, e.sin())
    }

    #[test]
    fn the_molecules_straight_up_are_a_tenth_deep_in_the_green() {
        // Only the molecules: no aerosol, no ozone.
        let params = AtmosphereParams {
            ozone_absorption: [0.0; 3],
            ..AtmosphereParams::default().with_turbidity(1.0)
        };
        let air = LutAtmosphere::new(params);
        let depth = -air.transmittance(LUT_RADIUS, 1.0)[1].ln();
        assert!((depth - 0.108).abs() < 0.005, "τ = {depth}");
        // And through all the air, blue loses most.
        let t = air.transmittance(LUT_RADIUS, 1.0);
        assert!(t[0] > t[1] && t[1] > t[2], "{t:?}");
    }

    #[test]
    fn the_table_holds_what_a_direct_march_finds() {
        let air = clear();
        for (h, mu) in [(0.0, 1.0), (1500.0, 0.3), (8000.0, 0.05), (300.0, -0.01)] {
            let r = LUT_RADIUS + h;
            let length = sphere_exit(r, mu, air.top()).unwrap();
            let n = 4000;
            let mut depth = [0.0; 3];
            for s in 0..n {
                let t = (s as f64 + 0.5) * length / n as f64;
                let height = (r * r + t * t + 2.0 * r * mu * t).sqrt() - LUT_RADIUS;
                let e = air.params().medium(height).extinction;
                for c in 0..3 {
                    depth[c] += e[c] * length / n as f64;
                }
            }
            let table = air.transmittance(r, mu);
            for c in 0..3 {
                let direct = (-depth[c]).exp();
                assert!(
                    (table[c] - direct).abs() < 0.01 * direct.max(0.02),
                    "{h} m, μ {mu}, channel {c}: {} against {direct}",
                    table[c]
                );
            }
        }
    }

    #[test]
    fn the_zenith_sun_brings_a_clear_days_direct_light_to_sea_level() {
        // 128 klx outside; about 100 klx measured at sea level under a
        // clear sky, the sun overhead. (`docs/19` step 2 states it in W/m²,
        // 850 to 1 050: broadband, water vapour's infrared absorption
        // included, which three visible channels do not model.)
        let t = clear().sunlight(LUT_RADIUS, 1.0);
        let direct = 128_000.0 * (0.2126 * t[0] + 0.7152 * t[1] + 0.0722 * t[2]);
        assert!((90_000.0..=112_000.0).contains(&direct), "{direct} lx");
    }

    #[test]
    fn the_setting_sun_reddens_and_dims() {
        let air = clear();
        let mut last_ratio = 0.0;
        for elevation in [90.0, 60.0, 30.0, 15.0, 5.0, 2.0] {
            let t = air.sunlight(LUT_RADIUS + 500.0, up_at(elevation).z);
            assert!(t[0] > t[1] && t[1] > t[2], "{elevation}°: {t:?}");
            let ratio = t[0] / t[2];
            assert!(ratio > last_ratio, "{elevation}°: R/B {ratio}");
            last_ratio = ratio;
        }
        // Under the horizon by more than the disc, no sun at all.
        assert_eq!(air.sunlight(LUT_RADIUS + 500.0, up_at(-1.0).z), [0.0; 3]);
    }

    #[test]
    fn the_sky_is_blue_overhead_and_brighter_towards_the_sun() {
        let air = clear();
        let sun = up_at(40.0);
        let zenith = air.sky_radiance(500.0, DVec3::Z, sun);
        assert!(zenith[2] > zenith[1] && zenith[1] > zenith[0], "{zenith:?}");
        let lum = |l: [f64; 3]| 0.2126 * l[0] + 0.7152 * l[1] + 0.0722 * l[2];
        // At the sun's height, near it and opposite it.
        let near = air.sky_radiance(500.0, up_at(35.0), sun);
        let away = air.sky_radiance(500.0, DVec3::new(-up_at(35.0).x, 0.0, up_at(35.0).z), sun);
        assert!(lum(near) > 2.0 * lum(away), "{near:?} against {away:?}");
    }

    #[test]
    fn the_twilight_zenith_is_blue_because_of_ozone() {
        let sun = up_at(-4.0);
        let with = clear().sky_radiance(0.0, DVec3::Z, sun);
        assert!(with[2] > with[0], "with ozone: {with:?}");
        let without = LutAtmosphere::new(AtmosphereParams {
            ozone_absorption: [0.0; 3],
            ..AtmosphereParams::default()
        })
        .sky_radiance(0.0, DVec3::Z, sun);
        // Without it, the blue falls back towards the red.
        assert!(
            without[2] / without[0] < with[2] / with[0],
            "{without:?} against {with:?}"
        );
    }

    #[test]
    fn the_diffuse_share_of_daylight_grows_as_the_sun_sets() {
        let air = clear();
        let mut last = 0.0;
        for elevation in [60.0, 40.0, 20.0, 10.0, 5.0] {
            let sun = up_at(elevation);
            let sky = air.sky_irradiance(300.0, sun, &[DVec3::Z])[0][1];
            let direct = air.sunlight(LUT_RADIUS + 300.0, sun.z)[1] * sun.z;
            let share = sky / (sky + direct);
            if elevation == 60.0 {
                assert!((0.08..=0.22).contains(&share), "60°: {share}");
            }
            assert!(share > last, "{elevation}°: {share}");
            last = share;
        }
    }

    #[test]
    fn a_clear_skys_brightness_against_the_ground_is_what_is_measured() {
        // Measured under clear skies, the sun about 50° up: the zenith at
        // 3 to 7 kcd/m² while the sun's beam alone brings level ground 75 to
        // 85 klx — a white Lambert surface under it at E/π, 24 to 27 kcd/m².
        // The zenith is a tenth to three tenths of such ground; the horizon
        // a side from the sun, by the CIE clear sky's gradation and
        // indicatrix, about one and a half times the zenith. Against the
        // beam alone, not the whole daylight: a sky drawn too bright would
        // brighten its own share of the daylight too, and hide in it.
        let air = clear();
        let sun = up_at(50.0);
        let lum = |l: [f64; 3]| 0.2126 * l[0] + 0.7152 * l[1] + 0.0722 * l[2];
        let white = lum(air.sunlight(LUT_RADIUS, sun.z)) * sun.z / PI;
        let zenith = lum(air.sky_radiance(0.0, DVec3::Z, sun)) / white;
        assert!((0.1..=0.3).contains(&zenith), "zenith at {zenith} of white");
        // Just over the horizon, square to the sun.
        let side = DVec3::new(0.0, 3f64.to_radians().cos(), 3f64.to_radians().sin());
        let horizon = lum(air.sky_radiance(0.0, side, sun)) / white;
        let gradation = horizon / zenith;
        assert!(
            (1.0..=3.0).contains(&gradation),
            "horizon at {gradation} of the zenith"
        );
    }

    /// The renderer's own lights, as `tuile_film::Look::default` has them.
    const CALIBRATION: Calibration = Calibration {
        sun: [2.5, 2.45, 2.3],
        sky: [0.72, 0.76, 0.8],
        reflectance: 1.0,
        camera: CameraResponse::TODAY,
    };

    fn day(to_sun_ecef: DVec3) -> Moment {
        Moment {
            to_sun_ecef,
            moon: None,
            compensation_ev: 0.0,
        }
    }

    fn lit(elevation: f64, at_height: f64) -> SkyLight {
        let at = Geodetic {
            lon: 0.5f64.to_radians(),
            lat: 43.0f64.to_radians(),
            height: at_height,
        };
        // The sun at `elevation`, due south.
        let enu = enu_frame(at);
        let e = elevation.to_radians();
        let to_sun = enu * DVec3::new(0.0, -e.cos(), e.sin());
        clear().light(clear(), at, day(to_sun), &[], CALIBRATION)
    }

    fn level(light: &SkyLight, up: DVec3) -> [f64; 3] {
        let i = &light.illumination;
        let sun = i.to_sun_ecef.dot(up).max(0.0);
        let mut out = [0.0; 3];
        if let Some(moon) = i.moon {
            let cos = moon.to_moon_ecef.dot(up).max(0.0);
            for c in 0..3 {
                out[c] += f64::from(moon.moon_rgb[c]) * cos;
            }
        }
        for c in 0..3 {
            out[c] += f64::from(i.sun_rgb[c]) * sun
                + f64::from(i.sky_sh[0][c])
                + f64::from(i.sky_sh[1][c]) * up.x
                + f64::from(i.sky_sh[2][c]) * up.y
                + f64::from(i.sky_sh[3][c]) * up.z;
        }
        out
    }

    fn luminance(v: [f64; 3]) -> f64 {
        0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2]
    }

    fn up_ecef() -> DVec3 {
        enu_frame(Geodetic {
            lon: 0.5f64.to_radians(),
            lat: 43.0f64.to_radians(),
            height: 0.0,
        })
        .z_axis
    }

    #[test]
    fn under_the_reference_day_level_ground_is_lit_as_the_calibration_says() {
        for height in [0.0, 1_500.0] {
            let light = lit(45.0, height);
            let got = level(&light, up_ecef());
            let half = std::f64::consts::FRAC_1_SQRT_2;
            for c in 0..3 {
                let wanted = f64::from(CALIBRATION.sun[c]) * half + f64::from(CALIBRATION.sky[c]);
                assert!(
                    (got[c] - wanted).abs() < 1e-4 * wanted,
                    "{height} m, channel {c}: {} against {wanted}",
                    got[c]
                );
            }
            let i = light.illumination;
            assert!((i.ev100 - DAYLIGHT_EV100).abs() < 1e-4, "{}", i.ev100);
            assert!(
                i.white.iter().all(|w| (w - 1.0).abs() < 1e-4),
                "{:?}",
                i.white
            );
            // The floor, a moonless night's light, is nothing by day.
            assert!(light.floor.iter().all(|f| *f < 1e-6), "{:?}", light.floor);
            assert!(i.exposure_offset_ev().abs() < 1e-4);
        }
    }

    #[test]
    fn the_air_is_set_against_the_ground_the_imagery_stands_for() {
        // Imagery whose texel of one is ground four times as bright: the
        // air's light a quarter of what it is against a white texel, the
        // ground's light unchanged — night floor included.
        for elevation in [45.0, -18.0] {
            let at = Geodetic {
                lon: 0.5f64.to_radians(),
                lat: 43.0f64.to_radians(),
                height: 500.0,
            };
            let e = f64::to_radians(elevation);
            let to_sun = enu_frame(at) * DVec3::new(0.0, -e.cos(), e.sin());
            let white = clear().light(clear(), at, day(to_sun), &[], CALIBRATION);
            let dark = Calibration {
                reflectance: 4.0,
                ..CALIBRATION
            };
            let imagery = clear().light(clear(), at, day(to_sun), &[], dark);
            assert_eq!(imagery.illumination, white.illumination);
            for c in 0..3 {
                assert!((imagery.illuminance[c] * 4.0 - white.illuminance[c]).abs() < 1e-6);
                assert!((imagery.floor[c] * 4.0 - white.floor[c]).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn the_light_falls_and_warms_as_the_sun_sets_and_never_below_the_floor() {
        let mut last_ev = f32::INFINITY;
        let mut last_warmth = 0.0;
        for elevation in [60.0, 45.0, 20.0, 5.0, 0.0, -2.0, -6.0, -18.0, -60.0] {
            let light = lit(elevation, 500.0);
            let i = light.illumination;
            assert!(i.ev100 <= last_ev + 1e-4, "{elevation}°: EV {}", i.ev100);
            last_ev = i.ev100;
            let got = luminance(level(&light, up_ecef()));
            let reference = luminance(level(&lit(45.0, 500.0), up_ecef()));
            assert!(
                got >= f64::from(ReferenceIllumination::NIGHT_FLOOR) * reference * 0.97,
                "{elevation}°: {got} against {reference}"
            );
            // On the picture, after the camera's exposure: darker than noon
            // under the sun's dimming, a moonless night four stops under,
            // never black.
            let shown = got / reference * f64::from(i.exposure_offset_ev()).exp2();
            assert!(
                shown > 1.0 / 22.0,
                "{elevation}°: shown at {shown} of the reference"
            );
            if elevation > 0.0 {
                let warmth = f64::from(i.white[0] / i.white[2]);
                assert!(warmth >= last_warmth, "{elevation}°: R/B {warmth}");
                last_warmth = warmth;
            }
        }
        assert!(lit(-18.0, 500.0).floor.iter().all(|f| *f > 0.0));
        assert!(lit(5.0, 500.0).illumination.sun_rgb[0] > lit(5.0, 500.0).illumination.sun_rgb[2]);
    }

    #[test]
    fn the_light_moves_smoothly_through_sunset() {
        let mut last: Option<SkyLight> = None;
        for step in 0..=40 {
            let elevation = 2.0 - 0.1 * f64::from(step);
            let light = lit(elevation, 500.0);
            if let Some(last) = last {
                let (a, b) = (last.illumination, light.illumination);
                assert!(
                    (a.ev100 - b.ev100).abs() < 0.25,
                    "{elevation}°: {} to {}",
                    a.ev100,
                    b.ev100
                );
                for c in 0..3 {
                    assert!(
                        (a.sun_rgb[c] - b.sun_rgb[c]).abs() < 0.08,
                        "{elevation}°: the sun {:?} to {:?}",
                        a.sun_rgb,
                        b.sun_rgb
                    );
                    assert!((a.white[c] - b.white[c]).abs() < 0.05);
                }
            }
            last = Some(light);
        }
    }

    fn full_moon_at(elevation: f64, at: Geodetic) -> Moonlight {
        let e = elevation.to_radians();
        Moonlight {
            to_moon_ecef: enu_frame(at) * DVec3::new(0.0, e.cos(), e.sin()),
            phase_angle: 0.0,
            angular_radius: 0.0045,
            moon_rgb: [0.0; 3],
        }
    }

    fn place(height: f64) -> Geodetic {
        Geodetic {
            lon: 0.5f64.to_radians(),
            lat: 43.0f64.to_radians(),
            height,
        }
    }

    #[test]
    fn a_full_moon_lights_the_night_and_its_sky_more_than_a_moonless_one() {
        let at = place(500.0);
        let e = f64::to_radians(-30.0);
        let to_sun = enu_frame(at) * DVec3::new(0.0, -e.cos(), e.sin());
        let dark = clear().light(clear(), at, day(to_sun), &[], CALIBRATION);
        let moonlit = clear().light(
            clear(),
            at,
            Moment {
                moon: Some(full_moon_at(50.0, at)),
                ..day(to_sun)
            },
            &[],
            CALIBRATION,
        );
        let (d, m) = (dark.illumination, moonlit.illumination);
        // A full Moon 50° up: about 0.15 lx on level ground, two hundred
        // times the moonless night's 0.002 — some seven stops.
        assert!(m.ev100 - d.ev100 > 5.0, "{} against {}", m.ev100, d.ev100);
        let moon = m.moon.expect("placed");
        assert!(
            moon.moon_rgb.iter().all(|c| *c > 0.0),
            "{:?}",
            moon.moon_rgb
        );
        let lux = luminance(level(&moonlit, up_ecef()))
            / luminance(level(&lit(45.0, 500.0), up_ecef()))
            * 100_000.0;
        assert!((0.05..=0.4).contains(&lux), "{lux} lx");
        // Shown brighter than the moonless night, never as bright as day.
        let shown = |i: &ReferenceIllumination| i.ev100 - DAYLIGHT_EV100 + i.exposure_offset_ev();
        assert!(
            shown(&m) > shown(&d) + 0.5 && shown(&m) < -2.0,
            "{} {}",
            shown(&m),
            shown(&d)
        );
        // The moonlit sky is a dimmed sky: the sky's light, scaled.
        assert!(moonlit.moon_strength > 1e-6 && moonlit.moon_strength < 1e-5);
        assert!(m.sky_sh[0][2] > d.sky_sh[0][2]);
    }

    #[test]
    fn the_light_never_steps_from_day_to_night_and_back() {
        let at = place(500.0);
        let mut last: Option<ReferenceIllumination> = None;
        for step in 0..=600 {
            let elevation = 30.0 - 0.1 * f64::from(step);
            let e = elevation.to_radians();
            let to_sun = enu_frame(at) * DVec3::new(0.0, -e.cos(), e.sin());
            let i = clear()
                .light(
                    clear(),
                    at,
                    Moment {
                        moon: Some(full_moon_at(20.0, at)),
                        ..day(to_sun)
                    },
                    &[],
                    CALIBRATION,
                )
                .illumination;
            if let Some(last) = last {
                let shown = |i: &ReferenceIllumination| i.ev100 + i.exposure_offset_ev();
                assert!(
                    (shown(&last) - shown(&i)).abs() < 0.05,
                    "{elevation}°: shown {} to {}",
                    shown(&last),
                    shown(&i)
                );
                for c in 0..3 {
                    assert!(
                        (last.white[c] - i.white[c]).abs() < 0.06 * last.white[c].max(0.3),
                        "{elevation}°: white {:?} to {:?}",
                        last.white,
                        i.white
                    );
                }
            }
            last = Some(i);
        }
        // And night is blue: the white balanced towards is warm.
        let night = last.expect("lit");
        assert!(
            night.white_balance()[2] > night.white_balance()[0],
            "{:?}",
            night.white_balance()
        );
    }

    #[test]
    fn a_frame_half_in_daylight_is_exposed_for_its_day() {
        let at = place(70_000.0);
        let enu = enu_frame(at);
        // The sun on the horizon here; the metered ground on either side of
        // the terminator, 800 km off.
        let to_sun = enu * DVec3::new(1.0, 0.0, 0.0);
        let east = |km: f64| {
            let g = Geodetic {
                lon: at.lon + (km * 1000.0 / 6.37e6 / at.lat.cos()),
                height: 0.0,
                ..at
            };
            tuile_core::geo::geodetic_to_ecef(g)
        };
        let eye = tuile_core::geo::geodetic_to_ecef(at);
        let ground = |km: f64| Metered::Ground {
            eye,
            point: east(km),
        };
        let day_side = [ground(800.0)];
        let both = [ground(800.0), ground(-800.0)];
        let night_side = [ground(-800.0)];
        let ev = |m: &[Metered]| clear().light(clear(), at, day(to_sun), m, CALIBRATION);
        let (d, b, n) = (ev(&day_side), ev(&both), ev(&night_side));
        assert!(d.illumination.ev100 > 10.0, "{}", d.illumination.ev100);
        // Half the frame at night: the log-average of its light, halfway in
        // stops between its day and its night — not the mean's one stop
        // under its day, which the bright half would drive.
        let halfway = (d.illumination.ev100 + n.illumination.ev100) / 2.0;
        assert!(
            (b.illumination.ev100 - halfway).abs() < 0.1,
            "{} against {halfway}",
            b.illumination.ev100
        );
        // Space beside it, past the air: not metered, nothing changes.
        let with_space = [
            ground(800.0),
            // Straight up from over the top of the air.
            Metered::Sky {
                eye: tuile_core::geo::geodetic_to_ecef(Geodetic {
                    height: 300_000.0,
                    ..at
                }),
                direction: enu.z_axis,
            },
        ];
        assert_eq!(ev(&with_space).illumination.ev100, d.illumination.ev100);
        // The night side alone: darker — though not night-dark, seen from
        // 70 km through air the sun still lights.
        assert!(
            n.illumination.ev100 < d.illumination.ev100 - 1.0,
            "{}",
            n.illumination.ev100
        );
        // The night side under the day's exposure: lit at least to be shown
        // at a sixteenth of noon, not left black.
        let i = b.illumination;
        let shown = luminance(b.least.map(f64::from))
            / luminance(level(&lit(45.0, 0.0), up_ecef()))
            * f64::from(i.exposure_offset_ev()).exp2();
        assert!(
            (shown - f64::from(ReferenceIllumination::SHOWN_FLOOR)).abs() < 0.01,
            "{shown}"
        );
    }

    #[test]
    fn the_air_below_the_horizon_seen_from_high_up_is_never_black() {
        let air = clear();
        let lum = |l: [f64; 3]| 0.2126 * l[0] + 0.7152 * l[1] + 0.0722 * l[2];
        for height in [3_000.0, 70_000.0] {
            // The horizon's dip at this height, and 2° under it, and over.
            let dip = (LUT_RADIUS / (LUT_RADIUS + height)).acos();
            for sun_elevation in [30.0, 2.0, -4.0] {
                let sun = up_at(sun_elevation);
                for below in [-2.0f64, 0.5, 3.0] {
                    let e = -dip - below.to_radians();
                    let dir = DVec3::new(0.0, e.cos(), e.sin());
                    let l = air.sky_radiance(height, dir, sun);
                    assert!(
                        lum(l) > 1e-6,
                        "{height} m, sun {sun_elevation}°, {below}°: {l:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn from_outside_the_air_the_limb_glows_and_space_is_empty() {
        let air = clear();
        let height = 300_000.0;
        let r = LUT_RADIUS + height;
        let sun = up_at(10.0);
        // A ray grazing the air 20 km over the ground: its tangent height.
        let tangent = |h: f64| -((1.0 - ((LUT_RADIUS + h) / r).powi(2)).sqrt());
        let grazing = |h: f64| {
            let mu = tangent(h);
            DVec3::new(0.0, (1.0 - mu * mu).sqrt(), mu)
        };
        let limb = air.sky_radiance(height, grazing(20_000.0), sun);
        assert!(limb[2] > 1e-4, "{limb:?}");
        // Over the top of the air: nothing at all.
        assert_eq!(air.sky_radiance(height, grazing(150_000.0), sun), [0.0; 3]);
        assert_eq!(air.sky_radiance(height, DVec3::Z, sun), [0.0; 3]);
        // The light along a line is the same seen from outside the air as
        // from where it enters it.
        let mu = tangent(30_000.0);
        let outside = air.transmittance(r, mu);
        let top = LUT_RADIUS + air.params().thickness;
        let t = sphere_entry(r, mu, top).expect("enters");
        let inside = air.transmittance(top, (r * mu + t) / top);
        assert_eq!(outside, inside);
        assert!(outside[2] < outside[0] && outside[0] < 1.0);
        assert_eq!(air.transmittance(r, 0.5), [1.0; 3]);
    }

    /// `hillaire.wgsl`'s `ground_light`, on the CPU: a fragment's own sun
    /// and sky from the tables, the frame's harmonic scaled by its level
    /// sky against the frame's.
    fn fragment_light(
        light: &SkyLight,
        air: &LutAtmosphere,
        at: Geodetic,
        normal: DVec3,
    ) -> [f64; 3] {
        let i = &light.illumination;
        let up = enu_frame(at).z_axis;
        let r = LUT_RADIUS + at.height;
        let mu_s = i.to_sun_ecef.dot(up);
        let sky = air.level_irradiance(at.height, mu_s);
        let beam = air.sunlight(r, mu_s);
        let mut out = [0.0; 3];
        for c in 0..3 {
            let g = f64::from(light.ground[c]);
            let level = g * sky[c] + f64::from(light.added[c]);
            let ratio = level / f64::from(light.level_reference[c]).max(1e-30);
            let frame_sky = f64::from(i.sky_sh[0][c])
                + f64::from(i.sky_sh[1][c]) * normal.x
                + f64::from(i.sky_sh[2][c]) * normal.y
                + f64::from(i.sky_sh[3][c]) * normal.z;
            let direct = g * beam[c] * normal.dot(i.to_sun_ecef).max(0.0);
            out[c] = (frame_sky.max(0.0) * ratio + direct).max(f64::from(light.least[c]));
        }
        out
    }

    #[test]
    fn a_fragment_where_the_light_was_worked_out_is_lit_as_the_frame_is() {
        for elevation in [50.0, 20.0, 6.0, 1.0, -3.0, -20.0] {
            let light = lit(elevation, 500.0);
            let at = place(500.0);
            let up = enu_frame(at).z_axis;
            let frame = level(&light, up);
            let fragment = fragment_light(&light, clear(), at, up);
            for c in 0..3 {
                assert!(
                    (fragment[c] - frame[c]).abs() <= 0.02 * frame[c] + 1e-12,
                    "{elevation}°, channel {c}: fragment {} against frame {}",
                    fragment[c],
                    frame[c]
                );
            }
        }
    }

    #[test]
    fn a_fragment_past_the_terminator_is_lit_by_the_night() {
        // The light worked out where the sun is 20° up; a fragment 3 000 km
        // towards the night, where it is 7° under the horizon.
        let light = lit(20.0, 500.0);
        let at = place(500.0);
        let night = Geodetic {
            lat: at.lat + 27f64.to_radians(),
            ..at
        };
        let up = enu_frame(night).z_axis;
        assert!(light.illumination.to_sun_ecef.dot(up) < -0.05);
        let there = fragment_light(&light, clear(), night, up);
        let here = level(&light, enu_frame(at).z_axis);
        // Under the day's exposure the night is lit by the least light
        // alone: shown at a sixteenth of noon, dark and not black.
        assert!(
            luminance(there) < 0.08 * luminance(here),
            "{there:?} against {here:?}"
        );
        let least = light.least.map(f64::from);
        assert!((luminance(there) - luminance(least)).abs() < 1e-3 * luminance(least));
    }

    #[test]
    fn the_sky_view_parametrisation_turns_back_on_itself() {
        for height in [0.0, 2_000.0, 10_000.0] {
            for (x, y) in [
                (0.0f64, 0.0f64),
                (0.3, 0.2),
                (0.7, 0.49),
                (0.5, 0.51),
                (1.0, 0.9),
                (0.2, 1.0),
            ] {
                let (mu, c) = sky_view_direction([x, y], height);
                let h = height;
                let r = LUT_RADIUS + h;
                let beta = ((h * (2.0 * LUT_RADIUS + h)).sqrt() / r).acos();
                let zh = PI - beta;
                let theta = mu.clamp(-1.0, 1.0).acos();
                let back_y = if theta < zh {
                    0.5 * (1.0 - (1.0 - theta / zh).max(0.0).sqrt())
                } else {
                    0.5 + 0.5 * ((theta - zh) / beta).max(0.0).sqrt()
                };
                let back_x = (0.5 - 0.5 * c).clamp(0.0, 1.0).sqrt();
                assert!(
                    (back_x - x).abs() < 1e-9 && (back_y - y).abs() < 1e-6,
                    "{x},{y} at {h}"
                );
            }
        }
    }
}
