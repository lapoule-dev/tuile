// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The reference ground: a 2.5D heightfield with water and edits, in any
//! frame.
//!
//! Heights sit on an `n × n` grid of samples covering the frame's bounds
//! (both edges included), interpolated bilinearly inside each cell. Each
//! column is one solid span from the bottom of the world to the surface.
//! Water is a body per sample.
//!
//! Edits resolve at the sample spacing: a brush changes the samples whose
//! columns it covers, and the surface between samples follows the bilinear
//! patches. What a heightfield cannot hold, it states rather than fakes: an
//! `Add` brush floating above the ground and a `Remove` brush buried under it
//! (an overhang, a tunnel) leave the column unchanged — they need a
//! volumetric implementation of the same contract. Authoritative arithmetic
//! uses only `+ − × ÷ sqrt` and the frame's metric.
//!
//! On a frame that wraps around `u` (a geodetic frame over the whole globe),
//! the first and last sample meridians are the same place, held as two
//! samples: edits measure both the same way and keep them equal up to
//! rounding. Rays do not wrap.
//!
//! # Costs
//!
//! - `surface`, `solid`, `water`: O(1) per query; `spans`: O(1) per column.
//! - `raycast`: O(cells crossed) per ray, at most `2(n − 1) + 1` cells, each
//!   an exact segment–patch test (a quadratic).
//! - `apply`: O(samples under the brush's footprint + tiles reported), both
//!   at most `n²` — the edit level is checked at build to give tiles of at
//!   least one cell.

use std::collections::{BTreeMap, BTreeSet};

use glam::{DVec2, DVec3};
use thiserror::Error;

use crate::edit::{Edit, EditOp, EditSeq, TileChanged};
use crate::frame::{
    grid_edge, grid_index, tiles_over, Column, Frame, InvalidFrame, Position, TileKey, Version,
};
use crate::query::{
    check, BatchMismatch, EditGround, Ground, GroundError, Hit, Ray, Span, SpanBuffer, Surface,
};
use crate::water::{WaterAt, WaterBody, WaterBodyId, WaterKind};

const DRY: u32 = u32::MAX;

/// Why a heightfield could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum InvalidGround {
    #[error("cells or edit-level tiles too narrow for their edges to be told apart")]
    DegenerateCells,
    #[error(transparent)]
    Frame(#[from] InvalidFrame),
    #[error("{0} samples per side: at least 2, and n² must fit in memory")]
    Samples(usize),
    #[error(
        "edit level {level}: its 2^{level} tiles per side would be finer than the {cells} cells"
    )]
    EditLevelTooFine { level: u8, cells: usize },
    #[error("sample ({i}, {j}) has a height that is not finite")]
    Height { i: usize, j: usize },
    #[error("water body {0} has a level that is not finite")]
    WaterLevel(usize),
    #[error("sample ({i}, {j}) is covered by water body {body}, which does not exist")]
    WaterBody { i: usize, j: usize, body: u32 },
}

/// A heightfield ground over the frame `F`.
#[derive(Debug, Clone, PartialEq)]
pub struct Heightfield<F: Frame> {
    frame: F,
    /// Samples per side.
    n: usize,
    heights: Vec<f64>,
    water: Vec<u32>,
    bodies: Vec<WaterBody>,
    versions: BTreeMap<TileKey, Version>,
    last: EditSeq,
}

impl<F: Frame> Heightfield<F> {
    /// `n × n` samples over the frame's bounds; `height(i, j)` gives the
    /// sample at index `i` along `u` and `j` along `v`, from the min corner.
    ///
    /// Refused: an invalid frame, `n < 2` or `n²` overflowing, an edit level
    /// with more tiles per side than cells (`2^level > n − 1`), a height that
    /// is not finite.
    pub fn new(
        frame: F,
        n: usize,
        height: impl Fn(usize, usize) -> f64,
    ) -> Result<Self, InvalidGround> {
        frame.validate()?;
        let Some(len) = n.checked_mul(n).filter(|_| n >= 2) else {
            return Err(InvalidGround::Samples(n));
        };
        let level = frame.edit_level();
        if (1usize << level) > n - 1 {
            return Err(InvalidGround::EditLevelTooFine {
                level,
                cells: n - 1,
            });
        }
        // Every edge strictly after the one before, for the cells and the
        // edit-level tiles, so cells and tiles have widths and tile_of
        // agrees with tile_bounds. O(n).
        let (lo, hi) = frame.bounds();
        for axis in 0..2 {
            for count in [as_u64(n - 1), 1u64 << level] {
                for i in 0..count {
                    if grid_edge(lo[axis], hi[axis], i, count)
                        >= grid_edge(lo[axis], hi[axis], i + 1, count)
                    {
                        return Err(InvalidGround::DegenerateCells);
                    }
                }
            }
        }
        let mut heights = Vec::with_capacity(len);
        for j in 0..n {
            for i in 0..n {
                let h = height(i, j);
                if !h.is_finite() {
                    return Err(InvalidGround::Height { i, j });
                }
                heights.push(h);
            }
        }
        Ok(Heightfield {
            frame,
            n,
            heights,
            water: vec![DRY; len],
            bodies: Vec::new(),
            versions: BTreeMap::new(),
            last: EditSeq(0),
        })
    }

    /// Adds water: `bodies`, each named by its index, and `covered(i, j)`
    /// the body over a sample, if any. Refused: a level that is not finite, a
    /// body that is not in `bodies`.
    pub fn with_water(
        mut self,
        bodies: Vec<WaterBody>,
        covered: impl Fn(usize, usize) -> Option<WaterBodyId>,
    ) -> Result<Self, InvalidGround> {
        if let Some(k) = bodies.iter().position(|b| !b.level.is_finite()) {
            return Err(InvalidGround::WaterLevel(k));
        }
        for j in 0..self.n {
            for i in 0..self.n {
                let id = match covered(i, j) {
                    None => DRY,
                    Some(WaterBodyId(b)) => {
                        if usize::try_from(b).map_or(true, |k| k >= bodies.len()) {
                            return Err(InvalidGround::WaterBody { i, j, body: b });
                        }
                        b
                    }
                };
                let k = self.at(i, j);
                self.water[k] = id;
            }
        }
        self.bodies = bodies;
        Ok(self)
    }

    /// Samples per side.
    pub fn samples_per_side(&self) -> usize {
        self.n
    }

    /// The column of sample `(i, j)`.
    pub fn sample_column(&self, i: usize, j: usize) -> Column {
        DVec2::new(self.edge(i, 0), self.edge(j, 1))
    }

    /// The coordinate of sample line `i` along `axis` (0 for `u`, 1 for `v`).
    fn edge(&self, i: usize, axis: usize) -> f64 {
        let (lo, hi) = self.frame.bounds();
        grid_edge(lo[axis], hi[axis], as_u64(i), as_u64(self.n - 1))
    }

    /// The cell along `axis` holding a coordinate: `edge(i) <= v <
    /// edge(i + 1)`, clamped to the cells.
    fn cell_index(&self, v: f64, axis: usize) -> usize {
        let (lo, hi) = self.frame.bounds();
        let i = grid_index(v, lo[axis], hi[axis], as_u64(self.n - 1));
        // Below n − 1.
        usize::try_from(i).unwrap_or(self.n - 2)
    }

    fn at(&self, i: usize, j: usize) -> usize {
        j * self.n + i
    }

    /// The cell holding a column and the position inside it, `0..1`.
    fn cell(&self, c: Column) -> Option<(usize, usize, f64, f64)> {
        if !self.frame.contains(c) {
            return None;
        }
        let (i, j) = (self.cell_index(c.x, 0), self.cell_index(c.y, 1));
        let (x0, x1) = (self.edge(i, 0), self.edge(i + 1, 0));
        let (y0, y1) = (self.edge(j, 1), self.edge(j + 1, 1));
        Some((i, j, (c.x - x0) / (x1 - x0), (c.y - y0) / (y1 - y0)))
    }

    fn height(&self, c: Column) -> Option<f64> {
        let (i, j, fx, fy) = self.cell(c)?;
        let h = |a: usize, b: usize| self.heights[self.at(a, b)];
        let south = h(i, j) * (1.0 - fx) + h(i + 1, j) * fx;
        let north = h(i, j + 1) * (1.0 - fx) + h(i + 1, j + 1) * fx;
        Some(south * (1.0 - fy) + north * fy)
    }

    /// The cell's mean normal, in metres east-north-up.
    fn normal(&self, c: Column) -> DVec3 {
        let Some((i, j, _, _)) = self.cell(c) else {
            return DVec3::Z;
        };
        let h = |a: usize, b: usize| self.heights[self.at(a, b)];
        let step = DVec2::new(
            self.edge(i + 1, 0) - self.edge(i, 0),
            self.edge(j + 1, 1) - self.edge(j, 1),
        ) * self.frame.metres_per_unit(c);
        // A cell with no width in metres (a row of samples at a pole, where
        // every longitude is one place) has no slope across it.
        let slope = |dh: f64, w: f64| if w > 0.0 { dh / (2.0 * w) } else { 0.0 };
        let gx = slope(
            h(i + 1, j) - h(i, j) + h(i + 1, j + 1) - h(i, j + 1),
            step.x,
        );
        let gy = slope(
            h(i, j + 1) - h(i, j) + h(i + 1, j + 1) - h(i + 1, j),
            step.y,
        );
        let v = DVec3::new(-gx, -gy, 1.0);
        v / (v.x * v.x + v.y * v.y + v.z * v.z).sqrt()
    }

    fn nearest(&self, c: Column) -> Option<usize> {
        let (i, j, fx, fy) = self.cell(c)?;
        Some(self.at(
            if fx >= 0.5 { i + 1 } else { i },
            if fy >= 0.5 { j + 1 } else { j },
        ))
    }

    fn is_solid(&self, p: Position) -> bool {
        self.height(p.truncate()).is_some_and(|h| p.z <= h)
    }

    /// The first entry of a segment into solid: the part of the segment
    /// inside the bounds, walked cell by cell, each cell's bilinear patch
    /// met exactly.
    ///
    /// The entry face is closed: a segment entering through a max edge
    /// below the surface hits there (on the edge, which is otherwise outside
    /// the ground), as it does through a min edge.
    fn cast(&self, r: &Ray) -> Option<Hit> {
        if !(r.from.is_finite() && r.to.is_finite()) {
            return None;
        }
        let (lo, hi) = self.frame.bounds();
        let d = r.to - r.from;
        // The parameters inside the bounds.
        let (mut t0, mut t1) = (0.0f64, 1.0f64);
        for axis in 0..2 {
            let (o, dd) = (r.from[axis], d[axis]);
            if dd == 0.0 {
                if !(o >= lo[axis] && o < hi[axis]) {
                    return None;
                }
            } else {
                let (a, b) = ((lo[axis] - o) / dd, (hi[axis] - o) / dd);
                t0 = t0.max(a.min(b));
                t1 = t1.min(a.max(b));
            }
        }
        if t0 > t1 {
            return None;
        }
        let start = r.from + d * t0;
        // Solid at the start agrees with `solid` wherever that answers.
        if self.is_solid(start) {
            return Some(Hit {
                at: start,
                fraction: t0,
            });
        }
        let (mut i, mut j) = (self.cell_index(start.x, 0), self.cell_index(start.y, 1));
        let last = self.n - 2;
        let crossing = |k: usize, axis: usize| -> f64 {
            let dd = d[axis];
            if dd > 0.0 {
                (self.edge(k + 1, axis) - r.from[axis]) / dd
            } else if dd < 0.0 {
                (self.edge(k, axis) - r.from[axis]) / dd
            } else {
                f64::INFINITY
            }
        };
        let mut t = t0;
        // A segment crosses at most 2(n − 1) − 1 cell edges.
        for _ in 0..2 * self.n {
            let (tx, ty) = (crossing(i, 0), crossing(j, 1));
            let end = tx.min(ty).min(t1).max(t);
            if let Some(th) = self.enter_patch(r, d, i, j, t, end) {
                let at = r.from + d * th;
                // The max edges belong to no cell, except as the entry face.
                return (th == t0 || self.frame.contains(at.truncate()))
                    .then_some(Hit { at, fraction: th });
            }
            if end >= t1 {
                break;
            }
            if tx <= ty {
                match (d.x > 0.0, i) {
                    (true, k) if k < last => i += 1,
                    (false, k) if k > 0 => i -= 1,
                    _ => break,
                }
            } else {
                match (d.y > 0.0, j) {
                    (true, k) if k < last => j += 1,
                    (false, k) if k > 0 => j -= 1,
                    _ => break,
                }
            }
            t = end;
        }
        None
    }

    /// The smallest `t` in `[ta, tb]` where the segment is at or below the
    /// bilinear patch of cell `(i, j)`: the entry root of a quadratic in
    /// `s = t − ta`, expressed around the segment's point at `ta` so a long
    /// segment does not extrapolate the patch across the map.
    fn enter_patch(&self, r: &Ray, d: DVec3, i: usize, j: usize, ta: f64, tb: f64) -> Option<f64> {
        let (x0, x1) = (self.edge(i, 0), self.edge(i + 1, 0));
        let (y0, y1) = (self.edge(j, 1), self.edge(j + 1, 1));
        let h = |a: usize, b: usize| self.heights[self.at(a, b)];
        let h00 = h(i, j);
        let (e, f) = (h(i + 1, j) - h00, h(i, j + 1) - h00);
        let k = h00 - h(i + 1, j) - h(i, j + 1) + h(i + 1, j + 1);
        let p = r.from + d * ta;
        // fx = ax + bx·s, fy = ay + by·s.
        let (ax, bx) = ((p.x - x0) / (x1 - x0), d.x / (x1 - x0));
        let (ay, by) = ((p.y - y0) / (y1 - y0), d.y / (y1 - y0));
        // patch − z = c0 + c1·s + c2·s².
        let c0 = h00 + e * ax + f * ay + k * ax * ay - p.z;
        if c0 >= 0.0 {
            return Some(ta);
        }
        let c1 = e * bx + f * by + k * (ax * by + ay * bx) - d.z;
        let c2 = k * bx * by;
        let sb = tb - ta;
        // Below the patch at s = 0: the entry is where the gap first comes
        // back to 0 — the upper root of a convex gap (the lower one is
        // negative), the lower root of a concave one.
        let root = if c2 == 0.0 {
            (c1 > 0.0).then(|| -c0 / c1)
        } else {
            let disc = c1 * c1 - 4.0 * c2 * c0;
            (disc >= 0.0)
                .then(|| {
                    // The two roots without cancellation.
                    let sq = disc.sqrt();
                    let m = -0.5 * (c1 + if c1 >= 0.0 { sq } else { -sq });
                    let r1 = m / c2;
                    let r2 = if m == 0.0 { r1 } else { c0 / m };
                    (r1.min(r2), r1.max(r2))
                })
                .and_then(|(low, high)| {
                    if c2 > 0.0 {
                        Some(high)
                    } else {
                        // Both roots on one side of 0: none ahead, or the
                        // lower one is the entry.
                        (high >= 0.0).then_some(low)
                    }
                })
        };
        match root.map(|s| s.max(0.0)) {
            Some(s) if s <= sb => Some((ta + s).min(tb)),
            // Rounding may hide a root the end of the interval shows.
            _ => (c0 + sb * (c1 + sb * c2) >= 0.0).then_some(tb),
        }
    }

    /// The samples an edit changes, and the rectangle of columns whose
    /// answers they decide: every cell with a changed corner.
    fn carve(&mut self, edit: &Edit, lo: Column, hi: Column) -> Option<(Column, Column)> {
        let (i0, i1) = (self.cell_index(lo.x, 0), self.cell_index(hi.x, 0) + 1);
        let (j0, j1) = (self.cell_index(lo.y, 1), self.cell_index(hi.y, 1) + 1);
        let mut changed: Option<(usize, usize, usize, usize)> = None;
        for j in j0..=j1 {
            for i in i0..=i1 {
                let Some((bottom, top)) = edit
                    .brush
                    .column_extent(&self.frame, self.sample_column(i, j))
                else {
                    continue;
                };
                let k = self.at(i, j);
                let h = self.heights[k];
                let new = match edit.op {
                    // Grows the column only where the brush rests on it.
                    EditOp::Add if bottom <= h => h.max(top),
                    // Carves the column only where the brush opens to the sky.
                    EditOp::Remove if top >= h => h.min(bottom),
                    _ => h,
                };
                if new.to_bits() != h.to_bits() {
                    self.heights[k] = new;
                    changed = Some(changed.map_or((i, i, j, j), |(a, b, c, e)| {
                        (a.min(i), b.max(i), c.min(j), e.max(j))
                    }));
                }
            }
        }
        // A sample is a corner of the cells on each side of it: columns from
        // the sample line before the first changed one, up to (excluded) the
        // line after the last.
        let (ia, ib, ja, jb) = changed?;
        let before = |k: usize| k.saturating_sub(1);
        let after = |k: usize| (k + 1).min(self.n - 1);
        Some((
            DVec2::new(self.edge(before(ia), 0), self.edge(before(ja), 1)),
            DVec2::new(
                self.edge(after(ib), 0).next_down(),
                self.edge(after(jb), 1).next_down(),
            ),
        ))
    }
}

impl<F: Frame> Ground for Heightfield<F> {
    fn frame(&self) -> &dyn Frame {
        &self.frame
    }

    fn version(&self, tile: TileKey) -> Version {
        self.versions.get(&tile).copied().unwrap_or_default()
    }

    fn water_bodies(&self) -> &[WaterBody] {
        &self.bodies
    }

    fn surface(
        &self,
        probes: &[Position],
        out: &mut [Option<Surface>],
    ) -> Result<(), BatchMismatch> {
        check(probes.len(), out.len())?;
        for (p, o) in probes.iter().zip(out.iter_mut()) {
            *o = self
                .height(p.truncate())
                .filter(|h| *h <= p.z)
                .map(|height| Surface {
                    height,
                    normal: self.normal(p.truncate()),
                });
        }
        Ok(())
    }

    fn spans(&self, columns: &[Column], out: &mut SpanBuffer) {
        out.clear();
        for c in columns {
            let a = out.spans.len();
            if let Some(top) = self.height(*c) {
                out.spans.push(Span {
                    bottom: f64::NEG_INFINITY,
                    top,
                });
            }
            out.ranges.push((a, out.spans.len()));
        }
    }

    fn solid(&self, points: &[Position], out: &mut [bool]) -> Result<(), BatchMismatch> {
        check(points.len(), out.len())?;
        for (p, o) in points.iter().zip(out.iter_mut()) {
            *o = self.is_solid(*p);
        }
        Ok(())
    }

    /// Exact up to rounding: the segment is walked cell by cell, and in each
    /// cell its first point at or below the bilinear patch is a root of a
    /// quadratic. A one-sample peak is never stepped over.
    fn raycast(&self, rays: &[Ray], out: &mut [Option<Hit>]) -> Result<(), BatchMismatch> {
        check(rays.len(), out.len())?;
        for (r, o) in rays.iter().zip(out.iter_mut()) {
            *o = self.cast(r);
        }
        Ok(())
    }

    fn water(&self, columns: &[Column], out: &mut [Option<WaterAt>]) -> Result<(), BatchMismatch> {
        check(columns.len(), out.len())?;
        for (c, o) in columns.iter().zip(out.iter_mut()) {
            *o = None;
            let (Some(k), Some(ground)) = (self.nearest(*c), self.height(*c)) else {
                continue;
            };
            let id = self.water[k];
            let Some(body) = usize::try_from(id).ok().and_then(|b| self.bodies.get(b)) else {
                continue;
            };
            if body.level > ground {
                *o = Some(WaterAt {
                    body: WaterBodyId(id),
                    level: body.level,
                    depth: body.level - ground,
                });
            }
        }
        Ok(())
    }
}

impl<F: Frame> EditGround for Heightfield<F> {
    fn last_seq(&self) -> EditSeq {
        self.last
    }

    fn apply(&mut self, edit: &Edit, changed: &mut Vec<TileChanged>) -> Result<(), GroundError> {
        if edit.seq <= self.last {
            return Err(GroundError::Stale {
                last: self.last,
                got: edit.seq,
            });
        }
        // `last` is below `edit.seq`, so below u64::MAX.
        let expected = EditSeq(self.last.0 + 1);
        if edit.seq != expected {
            return Err(GroundError::Gap {
                expected,
                got: edit.seq,
            });
        }
        // From here the number is consumed, whatever the content.
        self.last = edit.seq;
        if !edit.brush.is_valid_in(&self.frame) {
            return Err(GroundError::InvalidBrush);
        }
        let (lo, hi) = edit.brush.footprint(&self.frame);
        if !(lo.is_finite() && hi.is_finite()) {
            return Err(GroundError::InvalidBrush);
        }
        // The footprint's parts inside the bounds: itself, clipped, and on a
        // frame that wraps around `u`, its copies a period away; all of `u`
        // when it spans a period (a cap around a pole).
        let (blo, bhi) = self.frame.bounds();
        let mut parts: Vec<(Column, Column)> = Vec::with_capacity(3);
        let mut take = |l: Column, h: Column| {
            if !(h.x < blo.x || h.y < blo.y || l.x >= bhi.x || l.y >= bhi.y) {
                parts.push((l.max(blo), h.min(bhi)));
            }
        };
        match self.frame.u_period() {
            Some(p) if hi.x - lo.x >= p => {
                take(DVec2::new(blo.x, lo.y), DVec2::new(bhi.x, hi.y));
            }
            Some(p) => {
                for shift in [0.0, -p, p] {
                    let s = DVec2::new(shift, 0.0);
                    take(lo + s, hi + s);
                }
            }
            None => take(lo, hi),
        }
        if parts.is_empty() {
            return Err(GroundError::OutOfBounds);
        }
        let level = self.frame.edit_level();
        let mut tiles = BTreeSet::new();
        for (l, h) in parts {
            if let Some((a, b)) = self.carve(edit, l, h) {
                tiles.extend(tiles_over(&self.frame, a, b, level).map(|k| (k.y, k.x)));
            }
        }
        for (y, x) in tiles {
            let tile = TileKey { level, x, y };
            let v = self.versions.entry(tile).or_default();
            // 2^64 edits of one tile cannot happen; saturating rather than
            // wrapping keeps versions from going back.
            v.0 = v.0.saturating_add(1);
            changed.push(TileChanged { tile, version: *v });
        }
        Ok(())
    }

    /// FNV-1a over the frame (bounds, edit level, the metric at both
    /// corners; not a planar frame's anchor, which only places the world for
    /// a renderer and decides no answer), the samples, the water (coverage, each body's kind and
    /// level), the versions and the last consumed edit.
    fn digest(&self) -> u64 {
        let mut h = Fnv::default();
        let (lo, hi) = self.frame.bounds();
        let (mlo, mhi) = (
            self.frame.metres_per_unit(lo),
            self.frame.metres_per_unit(hi),
        );
        for v in [lo, hi, mlo, mhi] {
            h.write(&v.x.to_bits().to_le_bytes());
            h.write(&v.y.to_bits().to_le_bytes());
        }
        h.write(&[self.frame.edit_level()]);
        h.write(&as_u64(self.n).to_le_bytes());
        for v in &self.heights {
            h.write(&v.to_bits().to_le_bytes());
        }
        for w in &self.water {
            h.write(&w.to_le_bytes());
        }
        h.write(&as_u64(self.bodies.len()).to_le_bytes());
        for b in &self.bodies {
            h.write(&[match b.kind {
                WaterKind::Sea => 0,
                WaterKind::Lake => 1,
                WaterKind::River => 2,
            }]);
            h.write(&b.level.to_bits().to_le_bytes());
        }
        for (k, v) in &self.versions {
            h.write(&[k.level]);
            h.write(&k.x.to_le_bytes());
            h.write(&k.y.to_le_bytes());
            h.write(&v.0.to_le_bytes());
        }
        h.write(&self.last.0.to_le_bytes());
        h.0
    }
}

fn as_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// FNV-1a: a stable, dependency-free digest for tests and replicas.
#[derive(Debug)]
struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}
