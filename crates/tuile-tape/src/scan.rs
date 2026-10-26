// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! How much of a picture is bare **where ground was supposed to be**.
//!
//! Counting black pixels alone is a trap, and it caught the first version of
//! `tuile-scan-black`: from geostationary orbit the Earth subtends 17° in a
//! 45° field, most of the frame is legitimately sky, and a naive count
//! reported 68 % black on a frame that was perfectly drawn.
//!
//! So each pixel is cast as a ray from the camera that drew it and tested
//! against the globe. A pixel counts as ground only when its ray — and the
//! rays of its four neighbours — meet an ellipsoid a little inside the real
//! one: the limb, where a pixel is part sky and the multisampled resolve
//! blends the two, is never counted, and neither is the thin band of horizon
//! the backstop shell sits below. What is left is the ground a person sees
//! for certain, and the answer is how many of those pixels came out the clear
//! colour — the only quantity a hole moves.

use crate::Frame;

/// A pixel this dark on every channel is the clear colour rather than dark
/// ground. Ground lit by ambient alone still lands well above it.
pub const BLACK: u8 = 8;

/// WGS84's semi-axes, metres.
const EQUATORIAL: f64 = 6_378_137.0;
const POLAR: f64 = 6_356_752.314_245;

/// How far inside the ellipsoid a ray must reach to count as ground, metres:
/// past the backstop shell, which is drawn five hundred metres below.
const INSIDE: f64 = 1_000.0;

/// What a scan found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Bare {
    /// Pixels whose ray meets the globe for certain.
    pub ground: u64,
    /// Those of them left the clear colour.
    pub black: u64,
}

impl Bare {
    /// The share of the ground that is bare; zero when there is no ground.
    pub fn fraction(&self) -> f64 {
        self.black as f64 / self.ground.max(1) as f64
    }
}

/// Scans tightly packed RGBA8 of `width × height`, drawn by `camera`, every
/// `step`-th pixel in each direction. `step` 1 is every pixel.
pub fn bare_ground(rgba: &[u8], width: u32, height: u32, camera: &Frame, step: usize) -> Bare {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || rgba.len() < w * h * 4 {
        return Bare::default();
    }
    let step = step.max(1);
    let eye = camera.position;
    // The camera basis the renderer used: forward, right, up.
    let f = normalise(camera.direction);
    let r = normalise(cross(f, camera.up));
    let u = cross(r, f);
    let aspect = w as f64 / h as f64;
    let half_v = (camera.fovy * 0.5).tan();
    let half_h = half_v * aspect;
    let ray = |x: f64, y: f64| {
        let ndc_x = (x + 0.5) / w as f64 * 2.0 - 1.0;
        let ndc_y = 1.0 - (y + 0.5) / h as f64 * 2.0;
        normalise([
            f[0] + r[0] * ndc_x * half_h + u[0] * ndc_y * half_v,
            f[1] + r[1] * ndc_x * half_h + u[1] * ndc_y * half_v,
            f[2] + r[2] * ndc_x * half_h + u[2] * ndc_y * half_v,
        ])
    };
    let mut bare = Bare::default();
    for y in (0..h).step_by(step) {
        for x in (0..w).step_by(step) {
            let (fx, fy) = (x as f64, y as f64);
            let certain = [(0.0, 0.0), (-1.0, 0.0), (1.0, 0.0), (0.0, -1.0), (0.0, 1.0)]
                .iter()
                .all(|(dx, dy)| hits_globe(eye, ray(fx + dx, fy + dy)));
            if !certain {
                continue;
            }
            bare.ground += 1;
            let i = (y * w + x) * 4;
            if rgba[i] < BLACK && rgba[i + 1] < BLACK && rgba[i + 2] < BLACK {
                bare.black += 1;
            }
        }
    }
    bare
}

/// What a picture shows, measured for a diagnosis: how bright its sky and
/// its ground are, how many star-like points the sky holds, and how much
/// of it is the clear colour.
///
/// Brightness is luma (Rec. 709 weights over the picture's own sRGB-coded
/// values, 0–1): what an eye compares between two frames, and what a
/// flicker moves.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PictureStats {
    /// Pixels whose ray meets the globe for certain, and the others.
    pub ground_pixels: u64,
    pub sky_pixels: u64,
    /// Mean luma of each; 0 where there are none.
    pub ground_luma: f64,
    pub sky_luma: f64,
    /// Mean luma of the whole picture.
    pub luma: f64,
    /// Sky pixels brighter than all eight neighbours by [`STAR_CONTRAST`]
    /// over their mean: points of light, stars and planets.
    pub stars: u64,
    /// Pixels of the whole picture at the clear colour ([`BLACK`]).
    pub black: u64,
    /// Ground pixels at the clear colour: the bare ground [`bare_ground`]
    /// counts.
    pub black_ground: u64,
}

/// How far over its neighbours' mean, in luma (0–1), a sky pixel must be to
/// count as a point of light: a star a pixel wide on a dark sky, never the
/// grain of a gradient.
pub const STAR_CONTRAST: f64 = 0.08;

fn luma(rgba: &[u8], i: usize) -> f64 {
    (0.2126 * f64::from(rgba[i]) + 0.7152 * f64::from(rgba[i + 1]) + 0.0722 * f64::from(rgba[i + 2]))
        / 255.0
}

/// Measures tightly packed RGBA8 of `width × height`, drawn by `camera`:
/// see [`PictureStats`]. Every pixel; the ground is told from the sky as
/// [`bare_ground`] tells it, a pixel on the limb counted as sky.
pub fn picture_stats(rgba: &[u8], width: u32, height: u32, camera: &Frame) -> PictureStats {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || rgba.len() < w * h * 4 {
        return PictureStats::default();
    }
    let eye = camera.position;
    let f = normalise(camera.direction);
    let r = normalise(cross(f, camera.up));
    let u = cross(r, f);
    let half_v = (camera.fovy * 0.5).tan();
    let half_h = half_v * (w as f64 / h as f64);
    let ground: Vec<bool> = (0..w * h)
        .map(|p| {
            let (x, y) = ((p % w) as f64, (p / w) as f64);
            let ndc_x = (x + 0.5) / w as f64 * 2.0 - 1.0;
            let ndc_y = 1.0 - (y + 0.5) / h as f64 * 2.0;
            hits_globe(
                eye,
                normalise([
                    f[0] + r[0] * ndc_x * half_h + u[0] * ndc_y * half_v,
                    f[1] + r[1] * ndc_x * half_h + u[1] * ndc_y * half_v,
                    f[2] + r[2] * ndc_x * half_h + u[2] * ndc_y * half_v,
                ]),
            )
        })
        .collect();
    let mut s = PictureStats::default();
    let (mut sum, mut sum_ground, mut sum_sky) = (0.0, 0.0, 0.0);
    for p in 0..w * h {
        let i = p * 4;
        let l = luma(rgba, i);
        sum += l;
        let black = rgba[i] < BLACK && rgba[i + 1] < BLACK && rgba[i + 2] < BLACK;
        s.black += u64::from(black);
        if ground[p] {
            s.ground_pixels += 1;
            sum_ground += l;
            s.black_ground += u64::from(black);
            continue;
        }
        s.sky_pixels += 1;
        sum_sky += l;
        let (x, y) = (p % w, p / w);
        if x == 0 || y == 0 || x + 1 == w || y + 1 == h {
            continue;
        }
        let mut most = 0.0f64;
        let mut around = 0.0;
        for (dx, dy) in [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)] {
            let q = ((y as i64 + dy) as usize * w + (x as i64 + dx) as usize) * 4;
            let n = luma(rgba, q);
            most = most.max(n);
            around += n;
        }
        if l > most && l - around / 8.0 >= STAR_CONTRAST {
            s.stars += 1;
        }
    }
    s.luma = sum / (w * h) as f64;
    s.ground_luma = sum_ground / s.ground_pixels.max(1) as f64;
    s.sky_luma = sum_sky / s.sky_pixels.max(1) as f64;
    s
}

/// Whether a ray from `eye` along `dir` meets the ellipsoid shrunk by
/// [`INSIDE`], in front of the camera.
fn hits_globe(eye: [f64; 3], dir: [f64; 3]) -> bool {
    // Scaled into a unit sphere: x and y by the equatorial axis, z by the
    // polar one.
    let (a, b) = (EQUATORIAL - INSIDE, POLAR - INSIDE);
    let o = [eye[0] / a, eye[1] / a, eye[2] / b];
    let d = [dir[0] / a, dir[1] / a, dir[2] / b];
    let qa = dot(d, d);
    let qb = 2.0 * dot(o, d);
    let qc = dot(o, o) - 1.0;
    let discriminant = qb * qb - 4.0 * qa * qc;
    // Behind the camera does not count: at low altitude half the rays would
    // otherwise "hit" the planet through the ground beneath the eye.
    discriminant >= 0.0 && (-qb - discriminant.sqrt()) > 0.0
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalise(a: [f64; 3]) -> [f64; 3] {
    let n = dot(a, a).sqrt().max(1e-12);
    [a[0] / n, a[1] / n, a[2] / n]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Straight down from 2000 km over the equator.
    fn overhead() -> Frame {
        Frame {
            position: [EQUATORIAL + 2.0e6, 0.0, 0.0],
            direction: [-1.0, 0.0, 0.0],
            up: [0.0, 0.0, 1.0],
            fovy: 45f64.to_radians(),
        }
    }

    fn filled(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        (0..w * h)
            .flat_map(|_| [rgb[0], rgb[1], rgb[2], 255])
            .collect()
    }

    /// A frame of drawn ground has no bare pixel; the same frame left the
    /// clear colour is bare wherever there is ground, and only there.
    #[test]
    fn bare_ground_is_counted_where_ground_is_and_nowhere_else() {
        let camera = overhead();
        let drawn = bare_ground(&filled(64, 64, [90, 120, 60]), 64, 64, &camera, 1);
        assert!(drawn.ground > 3000, "{drawn:?}");
        assert_eq!(drawn.black, 0);
        let cleared = bare_ground(&filled(64, 64, [0, 0, 0]), 64, 64, &camera, 1);
        assert_eq!(cleared.ground, drawn.ground);
        assert_eq!(cleared.black, cleared.ground);
        assert!((cleared.fraction() - 1.0).abs() < 1e-12);
    }

    /// A dark sky with three single-pixel stars, over ground: the stars are
    /// counted, the ground's luma is the ground's, and a grey gradient has
    /// no star in it.
    #[test]
    fn stars_and_the_two_regions_are_measured_apart() {
        // Looking along the horizon from low: the lower half is ground.
        let camera = Frame {
            position: [EQUATORIAL + 2_000.0, 0.0, 0.0],
            direction: [0.0, 1.0, 0.0],
            up: [1.0, 0.0, 0.0],
            fovy: 45f64.to_radians(),
        };
        let (w, h) = (64u32, 64u32);
        let mut rgba = filled(w, h, [4, 4, 10]);
        for y in 32..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                rgba[i..i + 3].copy_from_slice(&[100, 100, 100]);
            }
        }
        for (x, y) in [(10, 5), (30, 12), (50, 20)] {
            let i = ((y * w + x) * 4) as usize;
            rgba[i..i + 3].copy_from_slice(&[230, 230, 230]);
        }
        let s = picture_stats(&rgba, w, h, &camera);
        assert_eq!(s.stars, 3, "{s:?}");
        assert!(s.ground_pixels > 0 && s.sky_pixels > 0, "{s:?}");
        assert!((s.ground_luma - 100.0 / 255.0).abs() < 1e-9, "{s:?}");
        assert!(s.sky_luma < 0.1, "{s:?}");
        let gradient: Vec<u8> = (0..w * h)
            .flat_map(|p| {
                let v = (p % w) as u8 * 3;
                [v, v, v, 255]
            })
            .collect();
        assert_eq!(picture_stats(&gradient, w, h, &camera).stars, 0);
    }

    /// Looking away from the planet there is no ground at all, so a black
    /// sky is not bare ground.
    #[test]
    fn the_sky_is_never_ground() {
        let camera = Frame {
            direction: [1.0, 0.0, 0.0],
            ..overhead()
        };
        let sky = bare_ground(&filled(32, 32, [0, 0, 0]), 32, 32, &camera, 1);
        assert_eq!(sky, Bare::default());
    }

    /// From geostationary orbit the Earth is a small disc: the pixels around
    /// it are sky, and the limb itself — part sky, part ground — is left out.
    #[test]
    fn the_limb_is_left_out() {
        let camera = Frame {
            position: [EQUATORIAL + 35_786_000.0, 0.0, 0.0],
            ..overhead()
        };
        let disc = bare_ground(&filled(128, 128, [0, 0, 0]), 128, 128, &camera, 1);
        let all = 128 * 128;
        assert!(disc.ground > 0 && disc.ground < all / 4, "{disc:?}");
    }
}
