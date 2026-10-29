// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The light of the sun's and the Moon's discs, measured on the pictures a
//! film makes ([`DiscEnergy`]).
//!
//! A disc is a light source of the atmosphere, seen: what its pixels add
//! up to, the sky behind them taken away, is the light it sends through
//! the air to the eye. That is measured on the frame's light before the
//! lens and the grade ([`FrameOut::linear`]) — a disc thousands of times
//! white, kept as it is — and held against what it should be: the body's
//! irradiance outside the air, each direction of its disc through the air
//! along that direction, cut by the planet's horizon, its limb or its phase
//! as the body's law has it — a quadrature on the CPU, over the model's own
//! tables in f64 ([`expected`]).
//!
//! A ratio of one is a disc that carries the light the ground is lit by.
//! Under one, the disc is dimmer than its light — or the ground covers part
//! of it, which the quadrature does not know (it knows the planet's
//! horizon, not the terrain): a ridge before the sun reads as a ratio
//! falling to nought, what a glare's occlusion should follow.

use std::path::{Path, PathBuf};

use glam::DVec3;
use tuile_atmosphere::gpu::AtmosphereUniform;
use tuile_atmosphere::hillaire::{LUT_RADIUS, SUN_ANGULAR_RADIUS};
use tuile_atmosphere::LutAtmosphere;
use tuile_film::FrameCamera;

use crate::observe::{FrameOut, Observer, SkyOut};

/// Which disc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
    Sun,
    Moon,
}

impl Body {
    pub fn name(self) -> &'static str {
        match self {
            Self::Sun => "sun",
            Self::Moon => "moon",
        }
    }
}

/// One disc of one frame, measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiscLight {
    pub frame: u32,
    pub body: Body,
    /// How many pixels across the disc is drawn.
    pub across_px: f64,
    /// Its light, the sky behind it taken away: the sum over its pixels of
    /// their radiance times their solid angle, in the model's units (the
    /// sun's irradiance outside the air is one).
    pub measured: [f64; 3],
    /// What it should be ([`expected`]), in the same units.
    pub expected: [f64; 3],
    /// The sky taken away, a radiance in the model's units: the median of
    /// a ring about the disc.
    pub background: [f64; 3],
    /// Some of the disc, or of the ring about it, falls off the picture: the
    /// measure is not whole.
    pub cut: bool,
}

impl DiscLight {
    /// Measured over expected, by luminance.
    pub fn ratio(&self) -> f64 {
        let y = |c: [f64; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        y(self.measured) / y(self.expected).max(1e-300)
    }
}

/// The renderer's pinhole: the ray through a pixel's centre and the solid
/// angle the pixel spans.
struct Pinhole {
    right: DVec3,
    up: DVec3,
    fwd: DVec3,
    axis: DVec3,
    focal: f64,
    width: u32,
    height: u32,
}

impl Pinhole {
    fn of(camera: &FrameCamera, width: u32, height: u32) -> Self {
        // As the film's frame uniform and its metering build them.
        let v = camera.view;
        let p = camera.projection;
        let right = (v.row(0).truncate() / p.x_axis.x).as_dvec3();
        let up = (v.row(1).truncate() / p.y_axis.y).as_dvec3();
        let fwd = (-v.row(2).truncate()).as_dvec3()
            + right * f64::from(p.z_axis.x)
            + up * f64::from(p.z_axis.y);
        // The optical axis: square to the picture, on the side it looks.
        let axis = right.cross(up).normalize();
        let axis = if axis.dot(fwd) < 0.0 { -axis } else { axis };
        Self {
            focal: fwd.dot(axis),
            right,
            up,
            fwd,
            axis,
            width,
            height,
        }
    }

    /// The ray through pixel `(x, y)`'s centre, not unit.
    fn ray(&self, x: u32, y: u32) -> DVec3 {
        let nx = (f64::from(x) + 0.5) / f64::from(self.width) * 2.0 - 1.0;
        let ny = 1.0 - (f64::from(y) + 0.5) / f64::from(self.height) * 2.0;
        self.fwd + self.right * nx + self.up * ny
    }

    /// The solid angle of the pixel whose ray is `d`.
    fn solid_angle(&self, d: DVec3) -> f64 {
        let area = 4.0 * self.right.length() * self.up.length()
            / (f64::from(self.width) * f64::from(self.height));
        let l = d.length();
        area * self.focal / (l * l * l)
    }

    /// Where the unit direction `s` falls, in pixels; `None` behind.
    fn pixel_of(&self, s: DVec3) -> Option<(f64, f64)> {
        let depth = s.dot(self.axis);
        if depth <= 1e-6 {
            return None;
        }
        let on = s * (self.focal / depth) - self.fwd;
        let nx = on.dot(self.right) / self.right.length_squared();
        let ny = on.dot(self.up) / self.up.length_squared();
        Some((
            (nx + 1.0) * 0.5 * f64::from(self.width),
            (1.0 - ny) * 0.5 * f64::from(self.height),
        ))
    }
}

/// The disc of `body` as the uniform draws it: its direction, its drawn
/// angular radius, and its true one. `None` when it is not drawn.
fn disc_of(u: &AtmosphereUniform, body: Body) -> Option<(DVec3, f64, f64)> {
    let v = |a: [f32; 4]| DVec3::new(f64::from(a[0]), f64::from(a[1]), f64::from(a[2]));
    match body {
        Body::Sun => {
            let scale = f64::from(u.discs[0]).max(0.0);
            (scale > 0.0).then(|| {
                (
                    v(u.sun).normalize(),
                    SUN_ANGULAR_RADIUS * scale,
                    SUN_ANGULAR_RADIUS,
                )
            })
        }
        Body::Moon => {
            let rho = f64::from(u.moon_disc[3]);
            let scale = f64::from(u.discs[1]).max(0.0);
            (u.moon[3] > 0.0 && rho > 0.0 && scale > 0.0)
                .then(|| (v(u.moon).normalize(), rho * scale, rho))
        }
    }
}

/// Whether the line of sight `n` (unit) from the eye at `height` with up
/// `up` meets the planet the air is laid on: what hides a disc in the sky.
fn below_horizon(n: DVec3, up: DVec3, height: f64) -> bool {
    let h = height.max(0.0);
    let r = LUT_RADIUS + h;
    let mu = n.dot(up);
    mu < 0.0 && r * r * mu * mu - h * (2.0 * LUT_RADIUS + h) >= 0.0
}

/// What the disc of `body` should send the eye, in the model's units: its
/// radiance — its irradiance outside the air over its true solid angle,
/// times its law, normalised over the disc — through the air along each
/// direction of the drawn disc, where the planet does not stand before it,
/// summed over the disc. A quadrature of `n`×`n` points over the disc's
/// square, in f64 over the model's tables.
///
/// The sun's law is the linear limb darkening `1 − 0.6 (1 − μ)` over its
/// mean `0.8`; the Moon's, Lommel-Seeliger `μ₀/(μ₀ + μ)` with its earthshine,
/// over the mean the uniform carries — the laws the shader draws them by.
pub fn expected(u: &AtmosphereUniform, model: &LutAtmosphere, body: Body, n: usize) -> [f64; 3] {
    let Some((c, rho, true_rho)) = disc_of(u, body) else {
        return [0.0; 3];
    };
    let v = |a: [f32; 4]| DVec3::new(f64::from(a[0]), f64::from(a[1]), f64::from(a[2]));
    let up = v(u.up).normalize();
    let height = f64::from(u.sun[3]);
    let r = LUT_RADIUS + height.max(0.0);
    let solid = 2.0 * std::f64::consts::PI * (1.0 - true_rho.cos());
    let a = c.any_orthonormal_vector();
    let b = c.cross(a);
    let lit = v(u.moon_disc);
    let step = 2.0 / n as f64;
    let mut sum = [0.0; 3];
    for j in 0..n {
        for i in 0..n {
            let x = -1.0 + (i as f64 + 0.5) * step;
            let y = -1.0 + (j as f64 + 0.5) * step;
            let x2 = x * x + y * y;
            if x2 >= 1.0 {
                continue;
            }
            let dir = (c + (a * x + b * y) * rho).normalize();
            if below_horizon(dir, up, height) {
                continue;
            }
            let mu = (1.0 - x2).sqrt();
            let law = match body {
                Body::Sun => (1.0 - 0.6 * (1.0 - mu)) / 0.8,
                Body::Moon => {
                    // The surface's normal, towards the eye's side.
                    let normal = a * x + b * y - c * mu;
                    let mu0 = normal.dot(lit);
                    let mut shade = f64::from(u.moon_shade[1]);
                    if mu0 > 0.0 {
                        shade += mu0 / (mu0 + mu);
                    }
                    shade / f64::from(u.moon_shade[0]) * f64::from(u.moon[3])
                }
            };
            let t = model.transmittance(r, dir.dot(up));
            let d_omega = rho * rho * step * step;
            for k in 0..3 {
                sum[k] += law / solid * t[k] * d_omega;
            }
        }
    }
    sum
}

/// Every disc drawn in a frame's light, measured: `linear` RGBA in the
/// renderer's units, `width`×`height`, seen through `camera`.
pub fn measure(
    frame: u32,
    linear: &[f32],
    width: u32,
    height: u32,
    camera: &FrameCamera,
    sky: SkyOut<'_>,
) -> Vec<DiscLight> {
    let pinhole = Pinhole::of(camera, width, height);
    let u = sky.uniform;
    let unit = [0, 1, 2].map(|k| f64::from(u.illuminance[k]).max(1e-300));
    let mut out = Vec::new();
    for body in [Body::Sun, Body::Moon] {
        let Some((c, rho, _)) = disc_of(u, body) else {
            continue;
        };
        let Some((cx, cy)) = pinhole.pixel_of(c) else {
            continue;
        };
        let pixel = pinhole.solid_angle(pinhole.ray(
            (cx.max(0.0) as u32).min(width - 1),
            (cy.max(0.0) as u32).min(height - 1),
        ));
        let p = pixel.sqrt();
        // Inside: the disc and two pixels about it. The ring: from three
        // to six pixels past it.
        let inner = rho + 2.0 * p;
        let (ring_in, ring_out) = (rho + 3.0 * p, rho + 6.0 * p);
        let reach = (ring_out / p).ceil() as i64 + 2;
        // A disc wholly off the picture is not measured.
        let (wide, high) = (f64::from(width), f64::from(height));
        let margin = reach as f64;
        if cx < -margin || cy < -margin || cx > wide + margin || cy > high + margin {
            continue;
        }
        let mut cut = false;
        let mut ring: [Vec<f64>; 3] = Default::default();
        let mut inside = Vec::new();
        for y in (cy as i64 - reach)..=(cy as i64 + reach) {
            for x in (cx as i64 - reach)..=(cx as i64 + reach) {
                let off = x < 0 || y < 0 || x >= i64::from(width) || y >= i64::from(height);
                let (xu, yu) = (x.max(0) as u32, y.max(0) as u32);
                let d = pinhole.ray(xu, yu);
                let apart = d.normalize().dot(c).clamp(-1.0, 1.0).acos();
                if apart > ring_out {
                    continue;
                }
                if off {
                    cut = true;
                    continue;
                }
                let i = ((yu * width + xu) * 4) as usize;
                let px = [0, 1, 2].map(|k| f64::from(linear[i + k]) / unit[k]);
                if apart <= inner {
                    inside.push((px, pinhole.solid_angle(d)));
                } else if apart >= ring_in {
                    for k in 0..3 {
                        ring[k].push(px[k]);
                    }
                }
            }
        }
        let background = [0, 1, 2].map(|k| median(&mut ring[k]));
        let mut measured = [0.0; 3];
        for (px, omega) in &inside {
            for k in 0..3 {
                measured[k] += (px[k] - background[k]) * omega;
            }
        }
        out.push(DiscLight {
            frame,
            body,
            across_px: 2.0 * rho / p,
            measured,
            expected: expected(u, sky.model, body, 192),
            background,
            cut,
        });
    }
    out
}

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// An instrument on a film: the light of every disc of every frame
/// ([`measure`]), against what the air lets through ([`expected`]). Asks
/// for the frames' light ([`Observer::wants_light`]); a frame without a
/// physical atmosphere, or with two eyes, is not measured.
#[derive(Debug, Default)]
pub struct DiscEnergy {
    rows: Vec<DiscLight>,
    dir: Option<PathBuf>,
}

impl DiscEnergy {
    /// Writing `discs.parquet` into `dir` when [`Self::write`] is called.
    pub fn into(dir: impl Into<PathBuf>) -> Self {
        Self {
            rows: Vec::new(),
            dir: Some(dir.into()),
        }
    }

    /// Every disc measured so far, in the order the frames came.
    pub fn rows(&self) -> &[DiscLight] {
        &self.rows
    }

    /// A line a body: how many frames, the ratio's range, the size.
    pub fn summary(&self) -> String {
        let mut lines = Vec::new();
        for body in [Body::Sun, Body::Moon] {
            let rows: Vec<&DiscLight> = self.rows.iter().filter(|r| r.body == body).collect();
            if rows.is_empty() {
                continue;
            }
            let (lo, hi) = rows
                .iter()
                .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), r| {
                    (lo.min(r.ratio()), hi.max(r.ratio()))
                });
            lines.push(format!(
                "discs: the {} in {} frame(s), {:.1} px across, light measured over expected {:.3} to {:.3}",
                body.name(),
                rows.len(),
                rows[0].across_px,
                lo,
                hi
            ));
        }
        if lines.is_empty() {
            "discs: no disc measured".to_string()
        } else {
            lines.join("\n")
        }
    }

    /// `discs.parquet` into the directory given, a row a disc a frame:
    /// `frame`, `body`, `across_px`, `measured_{r,g,b}`, `expected_{r,g,b}`,
    /// `background_{r,g,b}`, `ratio`, `cut`.
    pub fn write(&self) -> Result<Option<PathBuf>, crate::Error> {
        let Some(dir) = &self.dir else {
            return Ok(None);
        };
        std::fs::create_dir_all(dir)?;
        let path = Path::new(dir).join("discs.parquet");
        write_parquet(&path, &self.rows)?;
        Ok(Some(path))
    }
}

/// `rows` as a Parquet file at `path`, one row group.
fn write_parquet(path: &Path, rows: &[DiscLight]) -> Result<(), crate::Error> {
    use parquet::data_type::{BoolType, ByteArray, ByteArrayType, DoubleType, Int32Type};
    use parquet::file::properties::WriterProperties;
    use parquet::file::writer::SerializedFileWriter;
    use parquet::schema::parser::parse_message_type;
    use std::sync::Arc;

    let doubles = [
        "across_px",
        "measured_r",
        "measured_g",
        "measured_b",
        "expected_r",
        "expected_g",
        "expected_b",
        "background_r",
        "background_g",
        "background_b",
        "ratio",
    ];
    let mut message =
        String::from("message discs { REQUIRED INT32 frame; REQUIRED BYTE_ARRAY body (UTF8); ");
    for name in doubles {
        message.push_str(&format!("REQUIRED DOUBLE {name}; "));
    }
    message.push_str("REQUIRED BOOLEAN cut; }");
    let schema = Arc::new(parse_message_type(&message)?);
    let file = std::fs::File::create(path)?;
    let mut writer =
        SerializedFileWriter::new(file, schema, Arc::new(WriterProperties::builder().build()))?;
    let mut group = writer.next_row_group()?;
    let double = |k: usize, r: &DiscLight| -> f64 {
        match k {
            0 => r.across_px,
            1..=3 => r.measured[k - 1],
            4..=6 => r.expected[k - 4],
            7..=9 => r.background[k - 7],
            _ => r.ratio(),
        }
    };
    let mut column = 0usize;
    while let Some(mut c) = group.next_column()? {
        match column {
            0 => {
                let v: Vec<i32> = rows.iter().map(|r| r.frame as i32).collect();
                c.typed::<Int32Type>().write_batch(&v, None, None)?;
            }
            1 => {
                let v: Vec<ByteArray> = rows
                    .iter()
                    .map(|r| ByteArray::from(r.body.name()))
                    .collect();
                c.typed::<ByteArrayType>().write_batch(&v, None, None)?;
            }
            k if k < 2 + doubles.len() => {
                let v: Vec<f64> = rows.iter().map(|r| double(k - 2, r)).collect();
                c.typed::<DoubleType>().write_batch(&v, None, None)?;
            }
            _ => {
                let v: Vec<bool> = rows.iter().map(|r| r.cut).collect();
                c.typed::<BoolType>().write_batch(&v, None, None)?;
            }
        }
        c.close()?;
        column += 1;
    }
    group.close()?;
    writer.close()?;
    Ok(())
}

impl Observer for DiscEnergy {
    fn frame(&mut self, frame: &FrameOut<'_>) {
        let (Some(linear), Some(sky)) = (frame.linear, frame.sky) else {
            return;
        };
        self.rows.extend(measure(
            frame.frame,
            linear,
            frame.width,
            frame.height,
            frame.camera,
            sky,
        ));
    }

    fn wants_light(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parquet::file::reader::{FileReader, SerializedFileReader};

    #[test]
    fn the_table_is_a_parquet_file_with_a_row_a_disc() {
        let dir = tempfile::tempdir().expect("dir");
        let mut energy = DiscEnergy::into(dir.path());
        for (frame, body) in [(1, Body::Sun), (1, Body::Moon), (2, Body::Sun)] {
            energy.rows.push(DiscLight {
                frame,
                body,
                across_px: 8.0,
                measured: [0.5; 3],
                expected: [1.0; 3],
                background: [0.0; 3],
                cut: false,
            });
        }
        let path = energy.write().expect("written").expect("a path");
        let reader = SerializedFileReader::new(std::fs::File::open(&path).expect("open"))
            .expect("a Parquet file");
        let meta = reader.metadata().file_metadata();
        assert_eq!(meta.num_rows(), 3);
        assert_eq!(meta.schema_descr().num_columns(), 14);
        assert_eq!(meta.schema_descr().column(1).name(), "body");
        assert_eq!(meta.schema_descr().column(12).name(), "ratio");
    }
}
