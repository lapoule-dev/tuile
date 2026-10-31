// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The ground as it is drawn, answered on the CPU: heights under places and
//! where a ray meets it.
//!
//! [`crate::ground::GroundHeight`] answers from a terrain's *summary* — the
//! highest point of the finest tile known — which is right for keeping a
//! camera out of a mountain and wrong for standing anything on the ground: a
//! unit placed on a tile's summit floats over every valley in it. What a
//! host places on the ground must sit on the surface the renderer draws: the
//! same triangles, at the same level of detail, this frame.
//!
//! A [`Surface`] holds the meshes a renderer has resident, each marked
//! whether it is drawn, and answers from them:
//!
//! - [`Surface::height_at`]: where the vertical through a place meets the
//!   finest **drawn** tile over it — a ray down the ellipsoid's normal,
//!   intersected with that tile's triangles, so the answer is the drawn
//!   surface's, not an interpolation of samples. Where no drawn tile covers
//!   the place (it is off screen), the finest **resident** one answers, and
//!   the answer says which level it came from. Where nothing is resident it
//!   is the ellipsoid, and says so. It never fails.
//! - [`Surface::pick`]: where a ray — a screen pixel's — first meets that
//!   surface.
//!
//! Every tile is given a **generation** when it enters the surface, and a
//! height carries the generation of the tile that answered it: a caller that
//! cached a height keeps it while the generation it got is still the one it
//! gets, and asks again when the level of detail under it changed.
//!
//! The surface is a snapshot: [`Surface::with`] makes the next one from this
//! one, sharing every tile already built, so a renderer can hand a fresh one
//! to another thread every frame and the readers never wait.
//!
//! What it does not see: the stitch a renderer may apply where two levels
//! meet (`crate::stitch`), which moves a tile's edge vertices onto its
//! coarser neighbour's edge on the GPU. Within a tile the answer is exact;
//! along a seam between levels it is the tile's own edge.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use glam::{DMat4, DVec3};

use crate::content::DecodedTileContent;
use crate::geo::{ecef_to_geodetic, geodetic_to_ecef, Geodetic};

/// Triangles a cell of a tile's index holds, about: the index is as fine as
/// that makes it, between 4 and 64 cells a side.
const PER_CELL: usize = 8;

/// A triangle whose face is within this cosine of vertical (against the
/// geocentric direction, which is within a fifth of a degree of the normal)
/// is a wall (a skirt), never ground: 87°.
const WALL: f64 = 0.05;

/// Where the vertical through a place starts: above any ground there is.
const ABOVE_M: f64 = 12_000.0;

/// What [`Surface::height_at`] answers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SurfaceHeight {
    /// Metres above the ellipsoid.
    pub height: f64,
    /// The level of the tile that answered, `None` for the ellipsoid (no
    /// tile over the place is resident).
    pub level: Option<u32>,
    /// The generation of the tile that answered (0 for the ellipsoid):
    /// unchanged as long as the same tile answers.
    pub generation: u64,
    /// Whether that tile is drawn this frame. `false` is a coarser resident
    /// stand-in, for a place off screen.
    pub drawn: bool,
}

/// Where a ray met the surface.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SurfaceHit {
    /// ECEF metres.
    pub position: DVec3,
    /// The place, radians, and its height above the ellipsoid.
    pub lon: f64,
    pub lat: f64,
    pub height: f64,
    /// Metres along the ray.
    pub distance: f64,
    pub level: Option<u32>,
    pub generation: u64,
}

/// One tile's mesh, indexed for vertical rays.
pub struct SurfaceTile {
    level: u32,
    generation: u64,
    /// The tile's extent, radians: west, south, east, north.
    rect: [f64; 4],
    /// Positions to ECEF.
    to_ecef: DMat4,
    content: Arc<DecodedTileContent>,
    /// Built on the first height asked of the tile: most resident tiles are
    /// never under anything a host places.
    index: OnceLock<Index>,
}

/// A tile's triangles by cell, for vertical lines.
struct Index {
    /// Cells along each side.
    grid: usize,
    /// `cells[c]` is the range of `triangles` in cell `c`.
    cells: Vec<(u32, u32)>,
    /// `(mesh, first index)` of each triangle, cell by cell.
    triangles: Vec<(u32, u32)>,
}

fn to_ecef_of(content: &DecodedTileContent) -> DMat4 {
    DMat4::from_translation(content.local_origin_ecef) * content.transform_local.as_dmat4()
}

fn point(to_ecef: &DMat4, m: &crate::content::DecodedMesh, i: u32) -> DVec3 {
    let p = m.positions[i as usize];
    to_ecef.transform_point3(DVec3::new(
        f64::from(p[0]),
        f64::from(p[1]),
        f64::from(p[2]),
    ))
}

impl SurfaceTile {
    /// `content`, for tile `level`, as generation `generation`, over `rect`
    /// (radians: west, south, east, north) when the caller knows the tile's
    /// extent — a terrain tile's is its grid's — or over its vertices'
    /// extent otherwise. `None` when it has no vertex. Nothing is indexed
    /// until a height is asked of it.
    pub fn new(
        content: Arc<DecodedTileContent>,
        level: u32,
        generation: u64,
        rect: Option<[f64; 4]>,
    ) -> Option<Self> {
        let to_ecef = to_ecef_of(&content);
        let rect = match rect {
            Some(rect) => rect,
            None => {
                let mut rect = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
                for m in &content.meshes {
                    for i in 0..m.positions.len() as u32 {
                        let g = ecef_to_geodetic(point(&to_ecef, m, i));
                        rect = [
                            rect[0].min(g.lon),
                            rect[1].min(g.lat),
                            rect[2].max(g.lon),
                            rect[3].max(g.lat),
                        ];
                    }
                }
                rect
            }
        };
        if rect[0] > rect[2] || rect[1] > rect[3] {
            return None;
        }
        Some(Self {
            level,
            generation,
            rect,
            to_ecef,
            content,
            index: OnceLock::new(),
        })
    }

    fn index(&self) -> &Index {
        self.index.get_or_init(|| self.build())
    }

    fn build(&self) -> Index {
        let rect = self.rect;
        let (w, h) = (
            (rect[2] - rect[0]).max(1e-12),
            (rect[3] - rect[1]).max(1e-12),
        );
        let count: usize = self
            .content
            .meshes
            .iter()
            .map(|m| m.indices.len() / 3)
            .sum();
        let grid = ((count / PER_CELL) as f64).sqrt().ceil().clamp(4.0, 64.0) as usize;
        let cell = |lon: f64, lat: f64| {
            let x = (((lon - rect[0]) / w) * grid as f64)
                .floor()
                .clamp(0.0, (grid - 1) as f64);
            let y = (((lat - rect[1]) / h) * grid as f64)
                .floor()
                .clamp(0.0, (grid - 1) as f64);
            (x as usize, y as usize)
        };
        let mut binned: Vec<Vec<(u32, u32)>> = vec![Vec::new(); grid * grid];
        for (mi, mesh) in self.content.meshes.iter().enumerate() {
            // Every vertex's place, once.
            let places: Vec<(f64, f64)> = (0..mesh.positions.len() as u32)
                .map(|i| {
                    let g = ecef_to_geodetic(point(&self.to_ecef, mesh, i));
                    (g.lon, g.lat)
                })
                .collect();
            for (t, tri) in mesh.indices.as_chunks::<3>().0.iter().enumerate() {
                // A wall — a skirt hung under an edge, a hair outside the
                // tile — is no ground to stand on: a vertical line through a
                // place just outside the tile would otherwise land on it,
                // metres below the surface. Left out of the index.
                let [a, b, c] = [tri[0], tri[1], tri[2]].map(|i| point(&self.to_ecef, mesh, i));
                let normal = (b - a).cross(c - a).normalize_or_zero();
                if normal.dot(a.normalize_or_zero()).abs() < WALL {
                    continue;
                }
                let cells = [tri[0], tri[1], tri[2]].map(|i| {
                    let (lon, lat) = places[i as usize];
                    cell(lon, lat)
                });
                let (x0, y0) = cells
                    .iter()
                    .fold((grid, grid), |a, c| (a.0.min(c.0), a.1.min(c.1)));
                let (x1, y1) = cells
                    .iter()
                    .fold((0, 0), |a, c| (a.0.max(c.0), a.1.max(c.1)));
                for y in y0..=y1 {
                    for x in x0..=x1 {
                        binned[y * grid + x].push((mi as u32, (t * 3) as u32));
                    }
                }
            }
        }
        let mut cells = Vec::with_capacity(grid * grid);
        let mut triangles = Vec::new();
        for list in binned {
            let start = triangles.len() as u32;
            triangles.extend(list);
            cells.push((start, triangles.len() as u32));
        }
        Index {
            grid,
            cells,
            triangles,
        }
    }

    pub fn level(&self) -> u32 {
        self.level
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn covers(&self, lon: f64, lat: f64) -> bool {
        lon >= self.rect[0] && lon <= self.rect[2] && lat >= self.rect[1] && lat <= self.rect[3]
    }

    fn corners(&self, mesh: u32, first: u32) -> [DVec3; 3] {
        let m = &self.content.meshes[mesh as usize];
        [0, 1, 2].map(|k| {
            let p = m.positions[m.indices[(first + k) as usize] as usize];
            self.to_ecef.transform_point3(DVec3::new(
                f64::from(p[0]),
                f64::from(p[1]),
                f64::from(p[2]),
            ))
        })
    }

    /// The height of this tile's surface under `lon`/`lat`, if it has one
    /// there: the first triangle a ray down the normal from above meets.
    pub fn height_at(&self, lon: f64, lat: f64) -> Option<f64> {
        if !self.covers(lon, lat) {
            return None;
        }
        let (w, h) = (
            (self.rect[2] - self.rect[0]).max(1e-12),
            (self.rect[3] - self.rect[1]).max(1e-12),
        );
        let index = self.index();
        let grid = index.grid;
        let x = (((lon - self.rect[0]) / w) * grid as f64)
            .floor()
            .clamp(0.0, (grid - 1) as f64) as usize;
        let y = (((lat - self.rect[1]) / h) * grid as f64)
            .floor()
            .clamp(0.0, (grid - 1) as f64) as usize;
        let (start, end) = index.cells[y * grid + x];
        if start == end {
            return None;
        }
        let ground = geodetic_to_ecef(Geodetic {
            lon,
            lat,
            height: 0.0,
        });
        // The ellipsoid's normal there, unit: the line heights are measured
        // along.
        let up = DVec3::new(lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin());
        let from = ground + up * ABOVE_M;
        let mut nearest: Option<f64> = None;
        for &(mesh, first) in &index.triangles[start as usize..end as usize] {
            if let Some(t) = intersect(from, -up, self.corners(mesh, first)) {
                if nearest.is_none_or(|n| t < n) {
                    nearest = Some(t);
                }
            }
        }
        nearest.map(|t| ABOVE_M - t)
    }
}

/// Möller–Trumbore, both faces: the distance along `dir` (unit) from `from`
/// to the triangle, when the ray meets it ahead.
fn intersect(from: DVec3, dir: DVec3, [a, b, c]: [DVec3; 3]) -> Option<f64> {
    let (e1, e2) = (b - a, c - a);
    let p = dir.cross(e2);
    let det = e1.dot(p);
    if det.abs() < 1e-12 {
        return None;
    }
    let inv = 1.0 / det;
    let s = from - a;
    let u = s.dot(p) * inv;
    // A hair of tolerance, so a ray down a shared edge is caught by one of
    // its two triangles whatever the rounding.
    const EPS: f64 = 1e-9;
    if !(-EPS..=1.0 + EPS).contains(&u) {
        return None;
    }
    let q = s.cross(e1);
    let v = dir.dot(q) * inv;
    if v < -EPS || u + v > 1.0 + EPS {
        return None;
    }
    let t = e2.dot(q) * inv;
    (t >= 0.0).then_some(t)
}

/// One resident mesh, as [`Surface::with`] is told of it.
#[derive(Clone, Copy)]
pub struct Resident<'a> {
    /// What the renderer calls the tile: the same key, frame after frame.
    pub key: u64,
    pub level: u32,
    pub content: &'a Arc<DecodedTileContent>,
    /// Its extent, radians (west, south, east, north), when known — a terrain
    /// tile's grid rectangle; `None` reads it off the vertices.
    pub rect: Option<[f64; 4]>,
}

/// The resident meshes, finest drawn first: see the module.
#[derive(Clone, Default)]
pub struct Surface {
    /// Drawn tiles first, then finer before coarser.
    order: Vec<(u64, bool)>,
    tiles: HashMap<u64, Arc<SurfaceTile>>,
    next_generation: u64,
    /// Bumped whenever what answers may have changed.
    version: u64,
}

impl Surface {
    /// The next snapshot: the tiles `resident` names, `drawn` marking those
    /// drawn (by key). A tile already held
    /// under the same key and content keeps its index and its generation; a
    /// new one, or one whose content changed, is indexed and given the next
    /// generation.
    pub fn with<'a>(
        &self,
        resident: impl IntoIterator<Item = Resident<'a>>,
        drawn: &std::collections::HashSet<u64>,
    ) -> Self {
        let mut next = Self {
            order: Vec::new(),
            tiles: HashMap::new(),
            next_generation: self.next_generation,
            version: self.version,
        };
        for Resident {
            key,
            level,
            content,
            rect,
        } in resident
        {
            let kept = self
                .tiles
                .get(&key)
                .filter(|t| t.level == level && Arc::ptr_eq(&t.content, content))
                .cloned();
            let tile = match kept {
                Some(tile) => tile,
                None => {
                    next.next_generation += 1;
                    match SurfaceTile::new(Arc::clone(content), level, next.next_generation, rect) {
                        Some(tile) => Arc::new(tile),
                        None => continue,
                    }
                }
            };
            next.tiles.insert(key, tile);
            next.order.push((key, drawn.contains(&key)));
        }
        let level = |k: &u64| next.tiles.get(k).map_or(0, |t| t.level);
        next.order.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then(level(&b.0).cmp(&level(&a.0)))
                .then(a.0.cmp(&b.0))
        });
        if next.order != self.order
            || next
                .tiles
                .iter()
                .any(|(k, t)| self.tiles.get(k).is_none_or(|old| !Arc::ptr_eq(old, t)))
        {
            next.version += 1;
        }
        next
    }

    /// Bumped whenever an answer may differ from the last snapshot's: a
    /// caller with nothing else to go on re-asks when it moves.
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// The height under `lon`/`lat` (radians): see the module.
    pub fn height_at(&self, lon: f64, lat: f64) -> SurfaceHeight {
        for &(key, drawn) in &self.order {
            let Some(tile) = self.tiles.get(&key) else {
                continue;
            };
            if let Some(height) = tile.height_at(lon, lat) {
                return SurfaceHeight {
                    height,
                    level: Some(tile.level),
                    generation: tile.generation,
                    drawn,
                };
            }
        }
        SurfaceHeight {
            height: 0.0,
            level: None,
            generation: 0,
            drawn: false,
        }
    }

    /// Where the ray from `from` along `dir` (ECEF) first meets the surface,
    /// within `max_distance` metres; `None` when it passes over it.
    ///
    /// Marched rather than intersected with every triangle: each step goes
    /// as far as the ray's clearance over the ground allows for slopes up to
    /// [`MAX_SLOPE`], and the crossing is then found by bisection to a
    /// centimetre, the height there being the drawn surface's.
    pub fn pick(&self, from: DVec3, dir: DVec3, max_distance: f64) -> Option<SurfaceHit> {
        let dir = dir.try_normalize()?;
        let at = |t: f64| {
            let p = from + dir * t;
            let g = ecef_to_geodetic(p);
            let ground = self.height_at(g.lon, g.lat);
            (p, g, ground)
        };
        let clearance = |t: f64| {
            let (_, g, ground) = at(t);
            g.height - ground.height
        };
        let mut t = 0.0;
        let mut above = clearance(t);
        if above <= 0.0 {
            return None;
        }
        let mut steps = 0;
        let hit = loop {
            steps += 1;
            if steps > 4096 || t > max_distance {
                return None;
            }
            let (p, g, _) = at(t);
            let up = (geodetic_to_ecef(Geodetic {
                height: g.height + 1.0,
                ..g
            }) - p)
                .normalize();
            let down = (-dir.dot(up)).max(0.0);
            let across = (1.0 - dir.dot(up).powi(2)).max(0.0).sqrt();
            let step = (0.9 * above / (down + MAX_SLOPE * across).max(1e-6)).max(0.25);
            let next = t + step;
            let ahead = clearance(next);
            if ahead <= 0.0 {
                break (t, next);
            }
            t = next;
            above = ahead;
        };
        let (mut lo, mut hi) = hit;
        while hi - lo > 0.01 {
            let mid = 0.5 * (lo + hi);
            if clearance(mid) > 0.0 {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let (position, g, ground) = at(hi);
        Some(SurfaceHit {
            position,
            lon: g.lon,
            lat: g.lat,
            height: ground.height,
            distance: hi,
            level: ground.level,
            generation: ground.generation,
        })
    }
}

/// The steepest ground [`Surface::pick`] steps over without missing it:
/// rise over run, 3 (72°).
pub const MAX_SLOPE: f64 = 3.0;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{DecodedMesh, MaterialDesc};

    /// A square of `side` metres centred under `lon`/`lat` (degrees), its
    /// corners at the heights given (sw, se, ne, nw), two triangles.
    fn square(lon: f64, lat: f64, side: f64, heights: [f64; 4]) -> Arc<DecodedTileContent> {
        let centre = Geodetic {
            lon: lon.to_radians(),
            lat: lat.to_radians(),
            height: 0.0,
        };
        let origin = geodetic_to_ecef(centre);
        let d_lon = side / 2.0 / (6_378_137.0 * centre.lat.cos());
        let d_lat = side / 2.0 / 6_356_752.0;
        let corners = [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)];
        let positions = corners
            .iter()
            .zip(heights)
            .map(|(&(e, n), height)| {
                let p = geodetic_to_ecef(Geodetic {
                    lon: centre.lon + e * d_lon,
                    lat: centre.lat + n * d_lat,
                    height,
                }) - origin;
                [p.x as f32, p.y as f32, p.z as f32]
            })
            .collect();
        Arc::new(DecodedTileContent {
            meshes: vec![DecodedMesh {
                positions,
                normals: None,
                uvs: None,
                indices: vec![0, 1, 2, 0, 2, 3],
                material: MaterialDesc::default(),
            }],
            textures: Vec::new(),
            imagery: Vec::new(),
            withheld_drape: None,
            copyright: None,
            local_origin_ecef: origin,
            transform_local: glam::Mat4::IDENTITY,
        })
    }

    fn r(key: u64, level: u32, content: &Arc<DecodedTileContent>) -> Resident<'_> {
        Resident {
            key,
            level,
            content,
            rect: None,
        }
    }

    fn drawn(keys: &[u64]) -> std::collections::HashSet<u64> {
        keys.iter().copied().collect()
    }

    /// The chord a flat triangle draws sags under the curved ellipsoid by
    /// `d² / 8R`: what the drawn surface is, and so what is answered.
    fn sag(side: f64) -> f64 {
        let half_diagonal = side * std::f64::consts::SQRT_2 / 2.0;
        half_diagonal * half_diagonal / (2.0 * 6_371_000.0) + 0.01
    }

    /// A flat square at 100 m answers 100 m inside it, the ellipsoid outside.
    #[test]
    fn a_flat_tile_answers_its_height_and_nothing_answers_the_ellipsoid() {
        let tile = square(10.0, 45.0, 1_000.0, [100.0; 4]);
        let s = Surface::default().with([r(1, 12, &tile)], &drawn(&[1]));
        let h = s.height_at(10f64.to_radians(), 45f64.to_radians());
        assert!((h.height - 100.0).abs() < sag(1_000.0), "{h:?}");
        assert_eq!(h.level, Some(12));
        assert!(h.drawn);
        let away = s.height_at(11f64.to_radians(), 45f64.to_radians());
        assert_eq!(
            away,
            SurfaceHeight {
                height: 0.0,
                level: None,
                generation: 0,
                drawn: false
            }
        );
    }

    /// A slope is followed exactly: the plane through the triangle, not the
    /// nearest vertex.
    #[test]
    fn a_slope_is_read_off_its_triangle() {
        // West edge at 0 m, east edge at 200 m: the middle is 100 m.
        let tile = square(10.0, 45.0, 1_000.0, [0.0, 200.0, 200.0, 0.0]);
        let s = Surface::default().with([r(1, 12, &tile)], &drawn(&[1]));
        let h = s.height_at(10f64.to_radians(), 45f64.to_radians());
        assert!((h.height - 100.0).abs() < sag(1_000.0), "{h:?}");
    }

    /// The drawn tile answers before a finer one that is resident and not
    /// drawn, and the finest drawn one before a coarser drawn one.
    #[test]
    fn the_finest_drawn_tile_answers() {
        let coarse = square(10.0, 45.0, 4_000.0, [50.0; 4]);
        let fine = square(10.0, 45.0, 1_000.0, [60.0; 4]);
        let finer = square(10.0, 45.0, 500.0, [70.0; 4]);
        let s = Surface::default().with(
            [r(1, 10, &coarse), r(2, 12, &fine), r(3, 13, &finer)],
            &drawn(&[1, 2]),
        );
        let at = |lon: f64| s.height_at(lon.to_radians(), 45f64.to_radians());
        assert_eq!(
            at(10.0).level,
            Some(12),
            "the finest drawn, not the finer resident"
        );
        assert!((at(10.0).height - 60.0).abs() < sag(1_000.0));
        // Outside the fine tile, inside the coarse one.
        let east = 10.0
            + 1_500.0 / (6_378_137.0 * 45f64.to_radians().cos()) * 180.0 / std::f64::consts::PI;
        assert_eq!(at(east).level, Some(10));
    }

    /// Off screen, a resident tile answers, and says it is not drawn.
    #[test]
    fn a_resident_tile_answers_where_nothing_is_drawn() {
        let parent = square(10.0, 45.0, 4_000.0, [50.0; 4]);
        let s = Surface::default().with([r(7, 9, &parent)], &drawn(&[]));
        let h = s.height_at(10f64.to_radians(), 45f64.to_radians());
        assert_eq!(h.level, Some(9));
        assert!(!h.drawn);
        assert!((h.height - 50.0).abs() < sag(4_000.0));
    }

    /// A tile keeps its generation from one snapshot to the next; a new one
    /// in its place has a new generation, and the version moves with it.
    #[test]
    fn generations_follow_the_tile_that_answers() {
        let coarse = square(10.0, 45.0, 4_000.0, [50.0; 4]);
        let fine = square(10.0, 45.0, 1_000.0, [60.0; 4]);
        let a = Surface::default().with([r(1, 10, &coarse)], &drawn(&[1]));
        let b = a.with([r(1, 10, &coarse)], &drawn(&[1]));
        let here = |s: &Surface| s.height_at(10f64.to_radians(), 45f64.to_radians());
        assert_eq!(here(&a).generation, here(&b).generation);
        assert_eq!(a.version(), b.version(), "nothing changed");
        let c = b.with([r(1, 10, &coarse), r(2, 12, &fine)], &drawn(&[1, 2]));
        assert_ne!(
            here(&c).generation,
            here(&b).generation,
            "a finer tile answers now"
        );
        assert!(c.version() > b.version());
        // The fine tile leaves: the coarse one answers again, with the
        // generation it always had.
        let d = c.with([r(1, 10, &coarse)], &drawn(&[1]));
        assert_eq!(here(&d).generation, here(&a).generation);
        assert!(d.version() > c.version());
    }

    /// A ray from above meets the ground where it should, on a slope too.
    #[test]
    fn a_ray_is_picked_where_it_meets_the_ground() {
        let tile = square(10.0, 45.0, 2_000.0, [0.0, 200.0, 200.0, 0.0]);
        let s = Surface::default().with([r(1, 12, &tile)], &drawn(&[1]));
        let target = geodetic_to_ecef(Geodetic {
            lon: 10f64.to_radians(),
            lat: 45f64.to_radians(),
            height: 100.0,
        });
        // From 3 km away, 30° down, from the west.
        let eye = target
            + (geodetic_to_ecef(Geodetic {
                lon: 9.97f64.to_radians(),
                lat: 45f64.to_radians(),
                height: 100.0,
            }) - target)
                .normalize()
                * 2_600.0
            + (target
                - geodetic_to_ecef(Geodetic {
                    height: 0.0,
                    ..ecef_to_geodetic(target)
                }))
            .normalize()
                * 1_500.0;
        let hit = s.pick(eye, target - eye, 10_000.0).expect("a hit");
        let at = s.height_at(hit.lon, hit.lat);
        assert!((hit.height - at.height).abs() < 0.05, "{hit:?} vs {at:?}");
        assert!((ecef_to_geodetic(hit.position).height - hit.height).abs() < 0.05);
        // A ray pointing up meets nothing.
        assert!(s.pick(eye, eye - target, 10_000.0).is_none());
    }
}
