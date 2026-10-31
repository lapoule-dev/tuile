// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The reference ground: a 2.5D heightfield with water and edits, in any
//! frame.
//!
//! Heights sit on an `n × n` grid of samples covering the frame's bounds
//! (both edges included), interpolated bilinearly. Each column is one solid
//! span from the bottom of the world to the surface. Water is a body id per
//! sample.
//!
//! What a heightfield cannot hold, it states rather than fakes: an `Add`
//! brush floating above the ground and a `Remove` brush buried under it (an
//! overhang, a tunnel) leave the column unchanged — they need a volumetric
//! implementation of the same contract. Authoritative arithmetic uses only
//! `+ − × ÷ sqrt` and the frame's metric.

use std::collections::BTreeMap;

use glam::{DVec2, DVec3};

use crate::edit::{Edit, EditOp, EditSeq, TileChanged};
use crate::frame::{tiles_over, Column, Frame, Position, TileKey, Version};
use crate::query::{
    check, BatchMismatch, EditGround, Ground, GroundError, Hit, Ray, Span, SpanBuffer, Surface,
};
use crate::water::{WaterAt, WaterBody, WaterBodyId};

const DRY: u32 = u32::MAX;

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
    last: Option<EditSeq>,
}

impl<F: Frame> Heightfield<F> {
    /// `n × n` samples (at least 2) over the frame's bounds; `height(i, j)`
    /// gives the sample at index `i` along `u` and `j` along `v`, from the min
    /// corner.
    pub fn new(frame: F, n: usize, height: impl Fn(usize, usize) -> f64) -> Self {
        let n = n.max(2);
        let mut heights = Vec::with_capacity(n * n);
        for j in 0..n {
            for i in 0..n {
                heights.push(height(i, j));
            }
        }
        Heightfield {
            frame,
            n,
            heights,
            water: vec![DRY; n * n],
            bodies: Vec::new(),
            versions: BTreeMap::new(),
            last: None,
        }
    }

    /// Adds water: `bodies` by id order, and `covered(i, j)` the body over a
    /// sample, if any. A body id not in `bodies` reads as dry.
    pub fn with_water(
        mut self,
        bodies: Vec<WaterBody>,
        covered: impl Fn(usize, usize) -> Option<WaterBodyId>,
    ) -> Self {
        for j in 0..self.n {
            for i in 0..self.n {
                let id =
                    covered(i, j).filter(|b| usize::try_from(b.0).is_ok_and(|k| k < bodies.len()));
                self.water[j * self.n + i] = id.map_or(DRY, |b| b.0);
            }
        }
        self.bodies = bodies;
        self
    }

    /// Samples per side.
    pub fn samples_per_side(&self) -> usize {
        self.n
    }

    /// The column of sample `(i, j)`.
    pub fn sample_column(&self, i: usize, j: usize) -> Column {
        let (lo, hi) = self.frame.bounds();
        let last = to_f64(self.n - 1);
        lo + (hi - lo) * DVec2::new(to_f64(i) / last, to_f64(j) / last)
    }

    /// A stable digest of the whole state (height bits, water, versions,
    /// last edit): two replicas agree iff their digests do, for practical
    /// purposes.
    pub fn digest(&self) -> u64 {
        let mut h = Fnv::default();
        for v in &self.heights {
            h.write(&v.to_bits().to_le_bytes());
        }
        for w in &self.water {
            h.write(&w.to_le_bytes());
        }
        for (k, v) in &self.versions {
            h.write(&[k.level]);
            h.write(&k.x.to_le_bytes());
            h.write(&k.y.to_le_bytes());
            h.write(&v.0.to_le_bytes());
        }
        h.write(&self.last.map_or(u64::MAX, |s| s.0).to_le_bytes());
        h.0
    }

    fn at(&self, i: usize, j: usize) -> usize {
        j * self.n + i
    }

    /// The cell holding a column and the position inside it, `0..1`.
    fn cell(&self, c: Column) -> Option<(usize, usize, f64, f64)> {
        if !self.frame.contains(c) {
            return None;
        }
        let (lo, hi) = self.frame.bounds();
        let cells = to_f64(self.n - 1);
        let t = (c - lo) / (hi - lo) * cells;
        let index = |v: f64| -> usize {
            let f = v.floor();
            if f <= 0.0 {
                0
            } else {
                // Inside the bounds: below `cells`, integral.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let i = f as usize;
                i.min(self.n - 2)
            }
        };
        let (i, j) = (index(t.x), index(t.y));
        Some((i, j, t.x - to_f64(i), t.y - to_f64(j)))
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
        let step =
            (self.sample_column(1, 1) - self.sample_column(0, 0)) * self.frame.metres_per_unit(c);
        let gx = (h(i + 1, j) - h(i, j) + h(i + 1, j + 1) - h(i, j + 1)) / (2.0 * step.x);
        let gy = (h(i, j + 1) - h(i, j) + h(i + 1, j + 1) - h(i + 1, j)) / (2.0 * step.y);
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
            let a = u32::try_from(out.spans.len()).unwrap_or(u32::MAX);
            if let Some(top) = self.height(*c) {
                out.spans.push(Span {
                    bottom: f64::NEG_INFINITY,
                    top,
                });
            }
            let b = u32::try_from(out.spans.len()).unwrap_or(u32::MAX);
            out.ranges.push((a, b));
        }
    }

    fn solid(&self, points: &[Position], out: &mut [bool]) -> Result<(), BatchMismatch> {
        check(points.len(), out.len())?;
        for (p, o) in points.iter().zip(out.iter_mut()) {
            *o = self.is_solid(*p);
        }
        Ok(())
    }

    fn raycast(&self, rays: &[Ray], out: &mut [Option<Hit>]) -> Result<(), BatchMismatch> {
        check(rays.len(), out.len())?;
        // March at half a cell: a ridge between two samples is never stepped
        // over. The step count is a function of the ray only.
        let (lo, hi) = self.frame.bounds();
        let half_cell = (hi - lo) / to_f64(self.n - 1) / 2.0;
        for (r, o) in rays.iter().zip(out.iter_mut()) {
            let d = r.to - r.from;
            // Half a cell in metres, for the vertical part of the ray.
            let half_cell_m =
                (half_cell * self.frame.metres_per_unit(r.from.truncate())).min_element();
            let cells = (d.truncate() / half_cell)
                .abs()
                .max_element()
                .max((d.z / half_cell_m).abs());
            let steps = if cells.is_finite() {
                cells.ceil().clamp(1.0, 1_048_576.0)
            } else {
                1.0
            };
            *o = None;
            let mut k = 0.0;
            while k <= steps {
                let t = k / steps;
                let p = r.from + d * t;
                if self.is_solid(p) {
                    *o = Some(Hit { at: p, fraction: t });
                    break;
                }
                k += 1.0;
            }
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
            let Some(body) = usize::try_from(self.water[k])
                .ok()
                .and_then(|b| self.bodies.get(b))
            else {
                continue;
            };
            if body.level > ground {
                *o = Some(WaterAt {
                    body: body.id,
                    level: body.level,
                    depth: body.level - ground,
                });
            }
        }
        Ok(())
    }
}

impl<F: Frame> EditGround for Heightfield<F> {
    fn apply(&mut self, edit: &Edit, changed: &mut Vec<TileChanged>) -> Result<(), GroundError> {
        if let Some(last) = self.last.filter(|last| edit.seq <= *last) {
            return Err(GroundError::OutOfOrder {
                last,
                got: edit.seq,
            });
        }
        let (lo, hi) = edit.brush.footprint(&self.frame);
        if !(lo.is_finite() && hi.is_finite()) {
            return Err(GroundError::NotFinite);
        }
        let (blo, bhi) = self.frame.bounds();
        if hi.x < blo.x || hi.y < blo.y || lo.x >= bhi.x || lo.y >= bhi.y {
            return Err(GroundError::OutOfBounds);
        }
        let (lo, hi) = (lo.max(blo), hi.min(bhi));
        // Every sample whose column the brush may reach.
        let cells = to_f64(self.n - 1);
        let index = |v: f64, a: f64, b: f64, up: bool| -> usize {
            let t = (v - a) / (b - a) * cells;
            let t = if up { t.ceil() } else { t.floor() }.clamp(0.0, cells);
            // Clamped to 0..=n-1, integral.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let i = t as usize;
            i
        };
        for j in index(lo.y, blo.y, bhi.y, false)..=index(hi.y, blo.y, bhi.y, true) {
            for i in index(lo.x, blo.x, bhi.x, false)..=index(hi.x, blo.x, bhi.x, true) {
                let Some((bottom, top)) = edit
                    .brush
                    .column_extent(&self.frame, self.sample_column(i, j))
                else {
                    continue;
                };
                let k = self.at(i, j);
                let h = self.heights[k];
                self.heights[k] = match edit.op {
                    // Grows the column only where the brush rests on it.
                    EditOp::Add if bottom <= h => h.max(top),
                    // Carves the column only where the brush opens to the sky.
                    EditOp::Remove if top >= h => h.min(bottom),
                    _ => h,
                };
            }
        }
        // (`tile_of` clamps a column on the max edge into the last tile.)
        for tile in tiles_over(&self.frame, lo, hi, self.frame.edit_level()) {
            let v = self.versions.entry(tile).or_default();
            v.0 += 1;
            changed.push(TileChanged { tile, version: *v });
        }
        self.last = Some(edit.seq);
        Ok(())
    }
}

fn to_f64(n: usize) -> f64 {
    // Sample counts stay far below 2^53.
    #[allow(clippy::cast_precision_loss)]
    let f = n as f64;
    f
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
