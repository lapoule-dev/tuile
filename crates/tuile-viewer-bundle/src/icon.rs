// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The application's icon, drawn rather than kept: a globe made of tiles.
//!
//! It is computed, pixel by pixel, from the few numbers below — no artwork, no
//! photograph, no imagery. That is deliberate twice over. A still of the real
//! globe would put a provider's imagery, with its licence, into the
//! repository as the project's own face; and an icon that is a function can
//! be regenerated, reviewed as a diff of its parameters, and never goes
//! missing.
//!
//! What it shows is what the engine does: a sphere cut into the quadtree of a
//! geographic tiling scheme, coarse at the limb and four levels finer toward
//! the point the eye looks at — level of detail, in one picture.
//!
//! The canvas follows the platform's icon grid: a 1024-pixel square holding an
//! 824-pixel rounded square, the rest transparent margin for the shadow the
//! system draws.

use image::{Rgba, RgbaImage};

/// Side of the master image, in pixels. Every size in the `.icns` is resampled
/// from this one.
pub const MASTER: u32 = 1024;

/// Half the side of the rounded square, as a fraction of the half canvas:
/// 824 / 1024.
const PLATE: f64 = 824.0 / 1024.0;
/// Exponent of the superellipse that is the plate's outline. Five is close to
/// the platform's continuous-corner shape without its piecewise definition.
const SQUIRCLE: f64 = 5.0;
/// Radius of the globe, as a fraction of the half canvas.
const GLOBE: f64 = 0.585;
/// Where the eye looks, degrees: over the Mediterranean, tilted north — land
/// on three sides in any atlas, and no meridian running dead vertical.
const LOOK: (f64, f64) = (14.0, 38.0);
/// Samples per pixel along each axis. Nine per pixel is enough for the edges
/// of tiles a pixel wide at the smallest sizes.
const SAMPLES: u32 = 3;

type Rgb = [f64; 3];

const NIGHT_TOP: Rgb = [0.055, 0.075, 0.140];
const NIGHT_BOTTOM: Rgb = [0.020, 0.028, 0.060];
const DEEP: Rgb = [0.050, 0.220, 0.470];
const SHALLOW: Rgb = [0.110, 0.560, 0.640];
const LEAF: Rgb = [0.300, 0.640, 0.330];
const SEAM: Rgb = [0.900, 0.960, 1.000];
const AIR: Rgb = [0.400, 0.700, 1.000];

fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// A number in [0, 1) from a tile's address: the same tile is the same colour
/// on every machine, which a random generator would not promise.
fn hash(level: u32, x: i64, y: i64) -> f64 {
    let mut h = (level as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (x as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ (y as u64).wrapping_mul(0x1656_67B1_9E37_79F9);
    h ^= h >> 29;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 32;
    (h >> 11) as f64 / (1u64 << 53) as f64
}

/// The colour of the globe at a point of its visible face, given in the eye's
/// frame: `x` right, `y` up, `z` toward the eye, on the unit sphere.
fn globe(x: f64, y: f64, z: f64) -> Rgb {
    // Turn the eye's frame into the planet's: tip by the latitude looked at,
    // then spin by the longitude.
    let (lon0, lat0) = (LOOK.0.to_radians(), LOOK.1.to_radians());
    let up = y * lat0.cos() + z * lat0.sin();
    let out = z * lat0.cos() - y * lat0.sin();
    let lat = up.clamp(-1.0, 1.0).asin().to_degrees();
    let lon = (x.atan2(out) + lon0).to_degrees();

    // Finer toward the centre of the view: `z` is the cosine of the angle
    // from it. Level 2 is 45° tiles, level 6 is under 3°.
    let level = match z {
        z if z > 0.97 => 6,
        z if z > 0.88 => 5,
        z if z > 0.70 => 4,
        z if z > 0.40 => 3,
        _ => 2,
    };
    let size = 180.0 / f64::from(1u32 << level);
    let (u, v) = ((lon + 360.0) / size, (lat + 90.0) / size);
    let (tx, ty) = (u.floor(), v.floor());
    let shade = hash(level, tx as i64, ty as i64);

    // Mostly water, some land, more of it away from the poles' tiles.
    let land = hash(level + 17, tx as i64, ty as i64) > 0.62;
    let base = if land {
        mix(LEAF, SHALLOW, shade * 0.45)
    } else {
        mix(DEEP, SHALLOW, shade * 0.75)
    };

    // The seams: a line a twentieth of a tile wide on each border, which is
    // what makes it a globe *of tiles* at every size the icon is shown at.
    let edge = (u - tx)
        .min(1.0 - (u - tx))
        .min((v - ty).min(1.0 - (v - ty)));
    let seam = 1.0 - (edge / 0.05).clamp(0.0, 1.0);
    let surface = mix(base, SEAM, seam * 0.55);

    // Lit from the upper left, as every icon on the platform is.
    let light = (-0.45 * x + 0.55 * y + 0.70 * z).max(0.0);
    let lit = mix([0.0; 3], surface, 0.30 + 0.80 * light);
    // And a breath of air at the limb.
    mix(lit, AIR, (1.0 - z).powi(4) * 0.55)
}

/// The colour and coverage of one sample, at `(x, y)` in [-1, 1]² with `y` up.
fn sample(x: f64, y: f64) -> (Rgb, f64) {
    let (px, py) = (x / PLATE, y / PLATE);
    if px.abs().powf(SQUIRCLE) + py.abs().powf(SQUIRCLE) > 1.0 {
        return ([0.0; 3], 0.0);
    }
    let plate = mix(NIGHT_BOTTOM, NIGHT_TOP, (py + 1.0) * 0.5);
    let r = (x * x + y * y).sqrt() / GLOBE;
    if r <= 1.0 {
        let (gx, gy) = (x / GLOBE, y / GLOBE);
        let gz = (1.0 - gx * gx - gy * gy).max(0.0).sqrt();
        return (globe(gx, gy, gz), 1.0);
    }
    // The halo: air seen edge on, gone within a tenth of a radius.
    let halo = (1.0 - (r - 1.0) / 0.10).clamp(0.0, 1.0).powi(3) * 0.50;
    (mix(plate, AIR, halo), 1.0)
}

fn to_srgb(linearish: f64) -> u8 {
    (linearish.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Draws the icon at `side` pixels. The master is [`MASTER`]; smaller sides
/// exist for the tests.
pub fn draw(side: u32) -> RgbaImage {
    let mut image = RgbaImage::new(side, side);
    let cells = f64::from(side * SAMPLES);
    for (px, py, pixel) in image.enumerate_pixels_mut() {
        let (mut sum, mut coverage) = ([0.0; 3], 0.0);
        for sy in 0..SAMPLES {
            for sx in 0..SAMPLES {
                let x = (f64::from(px * SAMPLES + sx) + 0.5) / cells * 2.0 - 1.0;
                let y = 1.0 - (f64::from(py * SAMPLES + sy) + 0.5) / cells * 2.0;
                let (colour, alpha) = sample(x, y);
                for (total, channel) in sum.iter_mut().zip(colour) {
                    *total += channel * alpha;
                }
                coverage += alpha;
            }
        }
        // Straight alpha: the colour is the mean of the covered samples only.
        let alpha = coverage / f64::from(SAMPLES * SAMPLES);
        let colour = if coverage > 0.0 {
            sum.map(|c| c / coverage)
        } else {
            [0.0; 3]
        };
        *pixel = Rgba([
            to_srgb(colour[0]),
            to_srgb(colour[1]),
            to_srgb(colour[2]),
            to_srgb(alpha),
        ]);
    }
    image
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape the platform expects: transparent in the corners and in the
    /// margin, opaque on the plate, and a globe in the middle that is not the
    /// plate's colour.
    #[test]
    fn the_icon_is_a_globe_on_a_rounded_plate_with_a_clear_margin() {
        let icon = draw(128);
        let at = |x: u32, y: u32| icon.get_pixel(x, y).0;
        assert_eq!(at(0, 0)[3], 0, "the corner is not transparent");
        assert_eq!(at(64, 2)[3], 0, "the margin is not transparent");
        // Inside the plate's bounding box but outside its rounded corner.
        assert_eq!(at(14, 14)[3], 0, "the plate's corner is not rounded");
        assert_eq!(at(64, 20)[3], 255, "the plate is not opaque");
        let (plate, centre) = (at(64, 20), at(64, 64));
        assert_eq!(centre[3], 255);
        let brighter = u32::from(centre[0]) + u32::from(centre[1]) + u32::from(centre[2]);
        let darker = u32::from(plate[0]) + u32::from(plate[1]) + u32::from(plate[2]);
        assert!(
            brighter > darker + 100,
            "no globe at the centre: {centre:?}"
        );
    }

    /// The file in the repository is this function's output, not a picture
    /// somebody edited. Two levels of slack per channel, for the last bit of a
    /// sine on another machine's maths library.
    #[test]
    fn the_committed_master_is_what_the_generator_draws() {
        let committed = image::load_from_memory(include_bytes!("../icon-1024.png"))
            .expect("the committed master is a PNG")
            .to_rgba8();
        let drawn = draw(MASTER);
        assert_eq!(committed.dimensions(), drawn.dimensions());
        let worst = committed
            .as_raw()
            .iter()
            .zip(drawn.as_raw())
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(0);
        assert!(
            worst <= 2,
            "the master differs from the generator by {worst} levels: \
             regenerate it with `cargo run -p tuile-viewer-bundle -- icon`"
        );
    }
}
