// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The imagery need, independent of the terrain level.
//!
//! A tile's imagery level is chosen from how large one imagery texel looks
//! on screen at the tile's closest visible point (distance and obliquity),
//! against a target number of pixels per texel. It is **not** derived from
//! the terrain's geometric error, nor from a global altitude: a plain at a
//! shallow terrain level can wear imagery much deeper than its terrain.
//!
//! The level is computed with `+ − × ÷ √` and comparisons against exact
//! powers of two only. Those IEEE operations are correctly rounded on every
//! platform, so the level is the same everywhere, which the decision digest
//! relies on. No `log2`.

/// A tile's footprint in longitude and latitude, radians.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeoRect {
    /// Western edge.
    pub west: f64,
    /// Southern edge.
    pub south: f64,
    /// Eastern edge.
    pub east: f64,
    /// Northern edge.
    pub north: f64,
}

/// An invalid footprint input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FootprintError {
    /// A distance, focal length, texel size or target that is not finite
    /// and positive.
    #[error("footprint input `{0}` is not finite and positive")]
    NotPositive(&'static str),
    /// A cosine of obliquity outside `(0, 1]`.
    #[error("cosine of obliquity outside (0, 1]")]
    Obliquity,
}

/// How large a level-0 imagery texel looks on screen, in target pixels.
///
/// Level `L` is sharp enough when `2^L ≥ ratio`: each level halves the texel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TexelFootprint {
    ratio: f64,
}

impl TexelFootprint {
    /// The footprint of a level-0 texel of `texel_l0_m` metres, seen at
    /// `distance_m` with the surface tilted from the view ray by an angle of
    /// cosine `cos_obliquity`, through a lens of `focal_px` pixels per radian,
    /// against a target of `target_px_per_texel`.
    ///
    /// Obliquity foreshortens the texel along the view direction only; the
    /// footprint uses the geometric mean of the two axes, `√cos`.
    pub fn new(
        distance_m: f64,
        cos_obliquity: f64,
        focal_px: f64,
        texel_l0_m: f64,
        target_px_per_texel: f64,
    ) -> Result<Self, FootprintError> {
        let positive = |v: f64, name| {
            if v.is_finite() && v > 0.0 {
                Ok(v)
            } else {
                Err(FootprintError::NotPositive(name))
            }
        };
        positive(distance_m, "distance")?;
        positive(focal_px, "focal")?;
        positive(texel_l0_m, "texel")?;
        positive(target_px_per_texel, "target")?;
        if !(cos_obliquity.is_finite() && cos_obliquity > 0.0 && cos_obliquity <= 1.0) {
            return Err(FootprintError::Obliquity);
        }
        let ratio =
            focal_px * texel_l0_m * cos_obliquity.sqrt() / (distance_m * target_px_per_texel);
        if !ratio.is_finite() {
            return Err(FootprintError::NotPositive("ratio"));
        }
        Ok(TexelFootprint { ratio })
    }

    /// The ratio `2^L` must reach.
    pub fn ratio(self) -> f64 {
        self.ratio
    }

    /// The footprint under an LOD relaxation of `scale_milli` thousandths:
    /// the target pixels per texel grow with it, so imagery coarsens under
    /// pressure as terrain does (a global bias, never a hole).
    pub fn relaxed(self, scale_milli: u32) -> Self {
        let s = f64::from(scale_milli.max(1_000));
        TexelFootprint {
            ratio: self.ratio * 1_000.0 / s,
        }
    }
}

/// The bounds a needed imagery level is clamped to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevelClamp {
    /// The pinned floor.
    pub floor: u32,
    /// The tile's terrain level.
    pub terrain_level: u32,
    /// The most levels imagery may sit above terrain.
    pub over_terrain_cap: u32,
    /// The source's deepest level.
    pub source_max: u32,
}

/// The coarsest level sharp enough for `fp`, unclamped and without
/// hysteresis.
pub fn ideal_level(fp: TexelFootprint) -> u32 {
    let mut level = 0;
    while level < MAX_LEVEL && pow2(level) < fp.ratio {
        level += 1;
    }
    level
}

/// The deepest level considered.
const MAX_LEVEL: u32 = 40;

/// `2^level`, exactly.
fn pow2(level: u32) -> f64 {
    f64::from_bits(u64::from(1023 + level.min(1000)) << 52)
}

/// The level a tile needs this step, given the level it needed last step:
/// it moves only when the ideal level leaves `prev` by more than half a level
/// (±0.5 hysteresis), then it is clamped to
/// `[floor, terrain_level + over_terrain_cap]` and to the source's maximum.
pub fn needed_level(prev: Option<u32>, fp: TexelFootprint, clamp: LevelClamp) -> u32 {
    let ideal = ideal_level(fp);
    let level = match prev {
        // Keep `p` while the ideal level stays within (p − 1.5, p + 0.5]:
        // `2^(p − 1.5) < ratio ≤ 2^(p + 0.5)`.
        Some(p) if p > 0 => {
            let low = pow2(p - 1) / std::f64::consts::SQRT_2;
            let high = pow2(p) * std::f64::consts::SQRT_2;
            if fp.ratio > low && fp.ratio <= high {
                p
            } else {
                ideal
            }
        }
        Some(0) if fp.ratio <= std::f64::consts::SQRT_2 => 0,
        _ => ideal,
    };
    let ceiling = clamp
        .terrain_level
        .saturating_add(clamp.over_terrain_cap)
        .min(clamp.source_max);
    level.max(clamp.floor).min(ceiling)
}

/// Maps a tile's UV square into a drape that covers it (its own at another
/// level, or an ancestor's): `uv_in_drape = offset + uv × scale`.
///
/// `v` grows southward, as in the drape mosaics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UvTransform {
    /// Scale of `(u, v)`.
    pub scale: [f64; 2],
    /// Offset of `(u, v)`.
    pub offset: [f64; 2],
}

impl UvTransform {
    /// The identity: the drape is the tile's own.
    pub const IDENTITY: UvTransform = UvTransform {
        scale: [1.0, 1.0],
        offset: [0.0, 0.0],
    };

    /// The transform from `tile`'s UV square into `cover`'s, or `None` when
    /// `cover` does not contain `tile` or is degenerate.
    pub fn between(tile: GeoRect, cover: GeoRect) -> Option<UvTransform> {
        let (w, h) = (cover.east - cover.west, cover.north - cover.south);
        if !(w > 0.0 && h > 0.0) {
            return None;
        }
        let inside = tile.west >= cover.west
            && tile.east <= cover.east
            && tile.south >= cover.south
            && tile.north <= cover.north
            && tile.east > tile.west
            && tile.north > tile.south;
        if !inside {
            return None;
        }
        Some(UvTransform {
            scale: [(tile.east - tile.west) / w, (tile.north - tile.south) / h],
            offset: [(tile.west - cover.west) / w, (cover.north - tile.north) / h],
        })
    }
}

/// A drawable drape that could dress a tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DrapeCandidate {
    /// Its imagery level.
    pub level: u32,
    /// 0 for the tile's own drape, 1 for its parent's, and so on.
    pub up: u32,
}

/// The best drawable drape for a tile that needs `need`: never blurrier than
/// necessary. The coarsest candidate at or above `need`; failing that, the
/// sharpest below it. At equal level the closest owner wins (its own first).
pub fn best_drape(
    need: u32,
    candidates: impl IntoIterator<Item = DrapeCandidate>,
) -> Option<DrapeCandidate> {
    let mut best: Option<DrapeCandidate> = None;
    for c in candidates {
        best = Some(match best {
            Some(b) if drape_rank(need, c) >= drape_rank(need, b) => b,
            _ => c,
        });
    }
    best
}

/// The order of [`best_drape`], smaller first: at or above the need,
/// coarsest first; below it, sharpest; at equal level, the closest owner.
pub(crate) fn drape_rank(need: u32, x: DrapeCandidate) -> ((u8, u32), u32) {
    let rank = if x.level >= need {
        (0, x.level - need)
    } else {
        (1, need - x.level)
    };
    (rank, x.up)
}
