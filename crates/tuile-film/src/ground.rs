// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The ground a film draws, read as heights.
//!
//! A camera kept over the ground has to be kept over the ground the picture
//! shows: the terrain's mesh with its skirts dropped, smoothed, then stitched
//! to its neighbours — not the source's heights, nor the highest point of a
//! tile. This module is that ground on the CPU, built by the functions the
//! renderer itself calls:
//!
//! - [`prepared`] is what a renderer does to a tile's mesh before anything
//!   else sees it (skirts, [`crate::smoothing`]);
//! - [`Stitching::drawn`] is the mesh after the stitch pass, the shader's
//!   arithmetic on the CPU (`tuile_core::stitch::Stitch::offset`);
//! - [`Ground`] holds those meshes for a frame's selection and answers the
//!   height under a place: where a vertical line through it meets the drawn
//!   triangles. Nothing is interpolated between samples; the height is the
//!   surface's, where the line crosses it.

use std::sync::Arc;

use glam::DVec3;
use tuile_core::geo::{geodetic_to_ecef, Geodetic};
use tuile_core::source::TileId;
use tuile_core::stitch;
use tuile_terrain::{GeoRect, GeographicTilingScheme, TileCoord};

use crate::smoothing::{self, Smooth, Smoothed};
use crate::stitching::{decoded, encoded, Stitching};
use crate::{Mesh, TileKey};

/// How a film's renderer prepares a tile's mesh: what [`prepared`] does.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Prepare {
    /// Skirts kept under the tile's edges.
    pub skirts: bool,
    pub smooth: Smooth,
}

/// A tile's mesh as a film's renderer draws it, before stitching: skirts
/// dropped unless kept, then smoothed; and what smoothing did.
pub fn prepared(mesh: Mesh, how: Prepare) -> (Mesh, Smoothed) {
    let origin = DVec3::from_array(mesh.origin_ecef);
    let mesh = if how.skirts {
        mesh
    } else {
        encoded(
            &stitch::without_skirts(&decoded(&mesh), origin),
            mesh.origin_ecef,
            mesh.base_color_factor,
        )
    };
    let (smoothed, did) = smoothing::smooth(&decoded(&mesh), origin, how.smooth);
    let out = if how.smooth == Smooth::Off {
        mesh
    } else {
        encoded(&smoothed, mesh.origin_ecef, mesh.base_color_factor)
    };
    (out, did)
}

/// Where the line down a place starts: above any ground there is.
const ABOVE_M: f64 = 12_000.0;

/// A tile as drawn, kept for its heights alone: where its vertices are, how
/// they make triangles, and which triangles lie over which part of it.
pub(crate) struct Drawn {
    rect: GeoRect,
    origin: DVec3,
    positions: Vec<[f32; 3]>,
    indices: Vec<u32>,
    cells: Cells,
    /// The triangles no cell can vouch for ([`span`]): read wherever the
    /// tile is.
    everywhere: Vec<u32>,
}

/// A tile's triangles by cell ([`span`]), numbered as narrowly as they can
/// be.
enum Cells {
    Narrow(Grid<u16>),
    Wide(Grid<u32>),
}

impl Drawn {
    /// Tile `key`, its vertices `positions` about `origin` making the
    /// triangles `indices`, and `places[i]` where vertex `i` is
    /// ([`place_of`]) — whoever worked it out, and whenever: a vertex's
    /// place is a function of its bits.
    pub(crate) fn placed(
        key: &TileKey,
        origin: DVec3,
        positions: Vec<[f32; 3]>,
        indices: Vec<u32>,
        places: &[Geodetic],
    ) -> Self {
        let rect = rect_of(key);
        let triangles = indices.as_chunks::<3>().0;
        let boxes: Vec<Option<[f64; 4]>> = triangles
            .iter()
            .map(|t| span(&positions, places, t))
            .collect();
        let everywhere = (0..triangles.len() as u32)
            .filter(|t| boxes[*t as usize].is_none())
            .collect();
        let side = ((triangles.len() as f64 / TRIANGLES_A_CELL).sqrt().round() as usize)
            .clamp(1, MOST_A_SIDE);
        let cells = if triangles.len() <= usize::from(u16::MAX) + 1 {
            Cells::Narrow(Grid::of(rect, side, &boxes))
        } else {
            Cells::Wide(Grid::of(rect, side, &boxes))
        };
        Self {
            rect,
            origin,
            positions,
            indices,
            cells,
            everywhere,
        }
    }

    /// The bytes its vertices, triangles and cells take.
    pub(crate) fn heap_bytes(&self) -> usize {
        use std::mem::size_of_val;
        size_of_val(self.positions.as_slice())
            + size_of_val(self.indices.as_slice())
            + size_of_val(self.everywhere.as_slice())
            + match &self.cells {
                Cells::Narrow(grid) => grid.heap_bytes(),
                Cells::Wide(grid) => grid.heap_bytes(),
            }
    }

    /// How far along `dir` from `from`, in the tile's frame, the line down
    /// `lon`, `lat` first meets one of its triangles (either side): those
    /// listed over the place, and those read everywhere, are all it can
    /// meet.
    fn first_hit(&self, lon: f64, lat: f64, from: DVec3, dir: DVec3) -> Option<f64> {
        let triangles = self.indices.as_chunks::<3>().0;
        let along = |t: u32| hit(&self.positions, &triangles[t as usize], from, dir);
        let listed = match &self.cells {
            Cells::Narrow(grid) => grid
                .at(lon, lat)
                .iter()
                .filter_map(|&t| along(u32::from(t)))
                .reduce(f64::min),
            Cells::Wide(grid) => grid
                .at(lon, lat)
                .iter()
                .filter_map(|&t| along(t))
                .reduce(f64::min),
        };
        self.everywhere
            .iter()
            .filter_map(|&t| along(t))
            .chain(listed)
            .reduce(f64::min)
    }
}

/// The rectangle of the terrain tile `key` names.
fn rect_of(key: &TileKey) -> GeoRect {
    let (level, x, y) = TileId(key.id).terrain_coord();
    GeographicTilingScheme::default().tile_rect(TileCoord::new(level, x, y))
}

/// The middle longitude of tile `key`: what its vertices' longitudes are
/// taken on the side of ([`place_of`]).
pub(crate) fn middle_of(key: &TileKey) -> f64 {
    let rect = rect_of(key);
    (rect.west + rect.east) / 2.0
}

/// The coordinates a ground reads vertex `p` of a tile at, `p` about
/// `origin`: its longitude on the side of the antimeridian `middle` is,
/// latitude and height ([`geodetic`]). A function of the vertex's bits and
/// nothing else, so a vertex worked out once is worked out for good.
pub(crate) fn place_of(middle: f64, origin: DVec3, p: [f32; 3]) -> Geodetic {
    let mut g = geodetic(origin + DVec3::from(p.map(f64::from)));
    // Longitudes on the tile's side of the antimeridian.
    let off = g.lon - middle;
    g.lon = middle + (off - std::f64::consts::TAU * (off / std::f64::consts::TAU).round());
    g
}

/// [`place_of`] every vertex of tile `key`.
pub(crate) fn places_of(key: &TileKey, origin: DVec3, positions: &[[f32; 3]]) -> Vec<Geodetic> {
    let middle = middle_of(key);
    positions
        .iter()
        .map(|p| place_of(middle, origin, *p))
        .collect()
}

/// About how many triangles a cell lists: fewer cells a tile is less to
/// keep, and more triangles read a height. On 64 × 64 tiles drawn as the
/// render service draws them (`what_a_height_costs`), 2, 4, 8 and 16 read a
/// height in the same time within the noise, and 8 keeps a fifth less than
/// 2: a tile's cells come to a quarter of its mesh.
const TRIANGLES_A_CELL: f64 = 8.0;

/// The most cells a side of a tile.
const MOST_A_SIDE: usize = 128;

/// The ground of one frame: the meshes of the tiles it draws, as drawn.
///
/// A view: each tile is built once by the [`Stitching`] that holds it and
/// shared from there, so a ground costs a pointer a tile.
#[derive(Default)]
pub struct Ground {
    tiles: Vec<Arc<Drawn>>,
    /// Which tiles' rectangles, widened by [`ON`], lie over which part of
    /// the ground: `None` for a ground of no tile.
    by_place: Option<Grid<u32>>,
}

impl Ground {
    /// The ground `stitching` draws for `selection`, as last planned
    /// ([`Stitching::frame`]). Tiles it does not hold are not ground.
    pub fn of(stitching: &Stitching, selection: &[TileKey]) -> Self {
        Self::made_of(
            selection
                .iter()
                .filter_map(|key| stitching.ground(key, true))
                .collect(),
        )
    }

    /// The ground `stitching` holds for `selection`, unstitched: each tile
    /// as it entered, whatever was last planned.
    pub fn unstitched(stitching: &Stitching, selection: &[TileKey]) -> Self {
        Self::made_of(
            selection
                .iter()
                .filter_map(|key| stitching.ground(key, false))
                .collect(),
        )
    }

    /// [`Self::of`] — or [`Self::unstitched`] when not `stitched` — with
    /// the tiles not yet built handed to `build` first, with the function
    /// that builds one: a caller with threads to spare builds them on all
    /// of them. Each tile is built from its own mesh and its own plan, so
    /// the ground is the same whichever thread built which tile, and in
    /// whatever order.
    pub fn built_with(
        stitching: &Stitching,
        selection: &[TileKey],
        stitched: bool,
        build: impl FnOnce(&[TileKey], &(dyn Fn(&TileKey) + Sync)),
    ) -> Self {
        let missing: Vec<TileKey> = selection
            .iter()
            .filter(|key| !stitching.is_built(key, stitched))
            .copied()
            .collect();
        if !missing.is_empty() {
            build(&missing, &|key| {
                stitching.ground(key, stitched);
            });
        }
        if stitched {
            Self::of(stitching, selection)
        } else {
            Self::unstitched(stitching, selection)
        }
    }

    fn made_of(tiles: Vec<Arc<Drawn>>) -> Self {
        let boxes: Vec<Option<[f64; 4]>> = tiles
            .iter()
            .map(|t| {
                let r = t.rect;
                Some([r.west - ON, r.east + ON, r.south - ON, r.north + ON])
            })
            .collect();
        let by_place = boxes.iter().flatten().copied().reduce(|a, b| {
            [
                a[0].min(b[0]),
                a[1].max(b[1]),
                a[2].min(b[2]),
                a[3].max(b[3]),
            ]
        });
        let by_place = by_place.map(|[west, east, south, north]| {
            let side = ((tiles.len() as f64).sqrt() * 2.0).round() as usize;
            Grid::of(
                GeoRect {
                    west,
                    south,
                    east,
                    north,
                },
                side.clamp(1, MOST_A_SIDE),
                &boxes,
            )
        });
        Self { tiles, by_place }
    }

    /// The tiles this ground is made of.
    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    /// The bytes the tiles it is made of take: their meshes and cells —
    /// kept by the stitching that built them, and shared.
    pub fn heap_bytes(&self) -> usize {
        self.tiles.iter().map(|t| t.heap_bytes()).sum()
    }

    /// The height of the drawn ground at `lon`, `lat` (radians): where a
    /// vertical line there meets the highest triangle drawn. `None` when no
    /// drawn tile is there.
    ///
    /// Counted in `height_queries` and timed in `height_query_seconds`
    /// ([`tuile_core::metrics`]) one call in [`SAMPLED`], standing for all
    /// of them: see there.
    pub fn height_at(&self, lon: f64, lat: f64) -> Option<f64> {
        let asked = ASKED.with(|asked| {
            let n = asked.get() + 1;
            asked.set(n % SAMPLED);
            n
        });
        if asked < SAMPLED {
            return self.height_under(lon, lat);
        }
        let metrics = tuile_core::metrics::metrics();
        metrics.height_queries.add(SAMPLED);
        let started = tuile_core::metrics::stamp();
        let height = self.height_under(lon, lat);
        metrics
            .height_query_seconds
            .record_sample(started.elapsed(), SAMPLED);
        height
    }

    /// [`Self::height_at`], uncounted.
    fn height_under(&self, lon: f64, lat: f64) -> Option<f64> {
        let by_place = self.by_place.as_ref()?;
        let (top, down) = line_down(lon, lat);
        by_place
            .at(lon, lat)
            .iter()
            .map(|&k| &self.tiles[k as usize])
            .filter(|t| t.holds(lon, lat))
            .filter_map(|t| t.first_hit(lon, lat, top - t.origin, down))
            .map(|along| ABOVE_M - along)
            .reduce(f64::max)
    }
}

/// One height query in this many is counted and timed, and stands for the
/// others: a query reads a few dozen triangles, a few hundred nanoseconds,
/// and two clock reads and four atomic additions on counters every thread
/// of every bake shares are a measurable part of it — a tenth, on one
/// thread with no other (`what_a_height_costs`), and more where threads
/// contend for the counters — and of the profile that is meant to show
/// where the time goes. A thread counts its
/// own queries, unshared, and every [`SAMPLED`]th one adds that many to
/// `height_queries` and its own duration, that many times over, to
/// `height_query_seconds`, whose peak is the slowest query sampled.
///
/// The count is exact but for the fewer than [`SAMPLED`] each thread has
/// asked since its last sample; the total is an estimate whose error falls
/// as the square root of the samples. Prime, so that it does not beat with
/// the regular patterns a caller asks in.
pub const SAMPLED: u64 = 61;

thread_local! {
    /// The height queries this thread has asked since its last sample.
    static ASKED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How far outside its rectangle a place still reads a tile, in radians: a
/// place on the line two tiles share reads both.
const ON: f64 = 1.0e-12;

impl Drawn {
    /// Whether the place `lon`, `lat` reads this tile.
    fn holds(&self, lon: f64, lat: f64) -> bool {
        lon >= self.rect.west - ON
            && lon <= self.rect.east + ON
            && lat >= self.rect.south - ON
            && lat <= self.rect.north + ON
    }
}

/// A rectangle of longitude and latitude cut in `side` × `side` cells, and
/// numbers listed by cell: each in every cell its box falls in — a tile's
/// triangles, a ground's tiles.
///
/// A place falls in the cell its coordinates fall in, clamped to the grid,
/// and a box's sides are cut alike: by one function, monotonic, so that a
/// place inside a box always falls in a cell that lists it.
struct Grid<T> {
    west: f64,
    south: f64,
    /// A cell's width and height, in radians.
    step: (f64, f64),
    side: usize,
    /// Where each cell's list begins in `listed`, and where the last ends.
    starts: Vec<u32>,
    /// The cells' lists, one after the other.
    listed: Vec<T>,
}

impl<T: Copy + Default + TryFrom<usize>> Grid<T> {
    /// `rect` cut in `side` × `side`, number `n` listed in the cells
    /// `boxes[n]` — west, east, south, north — falls in; a `None` nowhere.
    /// Every number has to fit a `T`.
    fn of(rect: GeoRect, side: usize, boxes: &[Option<[f64; 4]>]) -> Self {
        let step = (rect.width() / side as f64, rect.height() / side as f64);
        let spans: Vec<Option<[usize; 4]>> = boxes
            .iter()
            .map(|b| {
                let [west, east, south, north] = (*b)?;
                Some([
                    cell(west, rect.west, step.0, side),
                    cell(east, rect.west, step.0, side),
                    cell(south, rect.south, step.1, side),
                    cell(north, rect.south, step.1, side),
                ])
            })
            .collect();
        let mut starts = vec![0u32; side * side + 1];
        for [i0, i1, j0, j1] in spans.iter().flatten() {
            for j in *j0..=*j1 {
                for i in *i0..=*i1 {
                    starts[j * side + i + 1] += 1;
                }
            }
        }
        for k in 1..starts.len() {
            starts[k] += starts[k - 1];
        }
        let mut next = starts.clone();
        let mut listed = vec![T::default(); starts[side * side] as usize];
        for (n, span) in spans.iter().enumerate() {
            let (Some([i0, i1, j0, j1]), Ok(n)) = (*span, T::try_from(n)) else {
                continue;
            };
            for j in j0..=j1 {
                for i in i0..=i1 {
                    let k = j * side + i;
                    listed[next[k] as usize] = n;
                    next[k] += 1;
                }
            }
        }
        Self {
            west: rect.west,
            south: rect.south,
            step,
            side,
            starts,
            listed,
        }
    }

    /// The numbers listed over `lon`, `lat`.
    fn at(&self, lon: f64, lat: f64) -> &[T] {
        let i = cell(lon, self.west, self.step.0, self.side);
        let j = cell(lat, self.south, self.step.1, self.side);
        let k = j * self.side + i;
        &self.listed[self.starts[k] as usize..self.starts[k + 1] as usize]
    }

    fn heap_bytes(&self) -> usize {
        use std::mem::size_of_val;
        size_of_val(self.starts.as_slice()) + size_of_val(self.listed.as_slice())
    }
}

/// The cell of `side` steps of `step` from `from` that `x` falls in,
/// clamped to them: monotonic in `x`.
fn cell(x: f64, from: f64, step: f64, side: usize) -> usize {
    // NaN casts to 0; no place that reads a tile is NaN.
    ((x - from) / step).floor().clamp(0.0, (side - 1) as f64) as usize
}

/// The longitude, latitude and height of `p`, earth-centred, by Bowring's
/// formula alone: within nanoradians of
/// [`tuile_core::geo::ecef_to_geodetic`], which
/// iterates it four times more, for any point within tens of kilometres of
/// the ellipsoid — far inside [`SLACK_M`] — at a fraction of the cost: a
/// tile's every vertex is converted when its cells are built.
fn geodetic(p: DVec3) -> Geodetic {
    use tuile_core::geo::{WGS84_A as A, WGS84_B as B};
    let e2 = 1.0 - (B * B) / (A * A);
    let ep2 = (A * A - B * B) / (B * B);
    let rho = p.x.hypot(p.y);
    let beta = (p.z * A).atan2(rho * B);
    let (sin_b, cos_b) = beta.sin_cos();
    let lat = (p.z + ep2 * B * sin_b * sin_b * sin_b).atan2(rho - e2 * A * cos_b * cos_b * cos_b);
    let (sin_lat, cos_lat) = lat.sin_cos();
    let n = A / (1.0 - e2 * sin_lat * sin_lat).sqrt();
    Geodetic {
        lon: p.y.atan2(p.x),
        lat,
        // Only ever compared with `LOWEST_M`: kilometres from any doubt.
        height: rho * cos_lat + p.z * sin_lat - A * A / n,
    }
}

/// Below every radius of curvature of the ellipsoid, less [`LOWEST_M`]:
/// metres a radian, at least, along a meridian or a parallel's great
/// circle.
const RADIUS_M: f64 = 6.3e6;
/// The deepest a vertex goes and is listed: a skirt hangs 6 km at most.
const LOWEST_M: f64 = -30_000.0;
/// The longest edge listed: the bound on the bulge below is quadratic.
const LONGEST_M: f64 = 50_000.0;
/// The furthest from the equator a triangle is listed, in radians (89°).
const POLEWARD: f64 = 1.553_343_034_274_953_2;
/// The least angle at a triangle's first vertex, as a sine, for its hit
/// test's rounding to stay below [`SLACK_M`].
const SLIVER: f64 = 1.0e-2;
/// Metres of margin round every box, for the rounding of the hit test and
/// of the vertices' coordinates.
const SLACK_M: f64 = 0.25;

/// The box, west, east, south and north (radians), that holds every place
/// whose line down meets triangle `t` — `None` for a triangle no box can be
/// vouched for, to be read wherever its tile is. `places` are the
/// vertices' coordinates.
///
/// The line down a place is the ellipsoid's normal there, and every point
/// on it — down to the axis, thousands of kilometres under any vertex —
/// has the place's longitude and latitude. A line meets a triangle at one
/// of the triangle's points, widened by [`EDGE`]: the box has to hold
/// those points' coordinates. It is the vertices' box, widened by:
///
/// - nothing for the longitude itself: a meridian bounds a half-space, two
///   less than π apart a convex wedge, and a triangle whose vertices are
///   in the wedge is in it;
/// - a bulge in latitude: the points of one latitude make a cone round the
///   axis, convex on the equator's side and curving poleward along the
///   parallel by `tan φ / (N + h)`; a flat triangle whose vertices are on
///   the equator's side of it crosses it by at most `tan φ ℓ² / 8 R` for a
///   reach `ℓ` along the parallel — taken twice over, `ℓ` its longest edge;
/// - [`EDGE`]: the widened triangle reaches `3 · 1e-6` of its longest edge
///   past the triangle;
/// - [`SLACK_M`], for the rounding: the hit test's barycentric coordinates
///   are off by about `ε |s| / (sin α cos θ)` metres — `ε` the f64
///   epsilon, `|s|` the tens of kilometres from the line's start to the
///   vertex, `α` the triangle's angle at its first vertex, `θ` the angle
///   between the line and the triangle's normal — a micrometre for any
///   triangle the line does not graze; and where it grazes one, the plane
///   is crossed as far away as the error is large, both as `1 / cos θ`. The
///   vertices' coordinates are those of the very `f32` vertices the test
///   reads, to the nanoradians of [`geodetic`].
///
/// Not vouched for: a sliver, whose rounding grows as `1 / sin α`; an edge
/// longer than [`LONGEST_M`] or a vertex deeper than [`LOWEST_M`], where the
/// bulge's bound stops holding; a vertex past [`POLEWARD`], where a
/// longitude's metres run out.
fn span(positions: &[[f32; 3]], places: &[Geodetic], t: &[u32; 3]) -> Option<[f64; 4]> {
    let vertex = |i: u32| Some(DVec3::from(positions.get(i as usize)?.map(f64::from)));
    let (a, b, c) = (vertex(t[0])?, vertex(t[1])?, vertex(t[2])?);
    let (e1, e2) = (b - a, c - a);
    let longest = e1.length().max(e2.length()).max((c - b).length());
    let sine = e1.cross(e2).length() / (e1.length() * e2.length());
    // Written so that a NaN — a triangle with an edge of no length — is not
    // vouched for either.
    let shapely = sine >= SLIVER && longest <= LONGEST_M;
    if !shapely {
        return None;
    }
    let [p, q, r] = t.map(|i| places[i as usize]);
    if p.height.min(q.height).min(r.height) < LOWEST_M {
        return None;
    }
    let poleward = p.lat.abs().max(q.lat.abs()).max(r.lat.abs());
    let placed = poleward <= POLEWARD;
    if !placed {
        return None;
    }
    let margin =
        2.0 * poleward.tan() * longest * longest / (8.0 * RADIUS_M) + 3.0e-6 * longest + SLACK_M;
    let lat = margin / RADIUS_M;
    let lon = margin / (RADIUS_M * (poleward + lat).cos());
    Some([
        p.lon.min(q.lon).min(r.lon) - lon,
        p.lon.max(q.lon).max(r.lon) + lon,
        p.lat.min(q.lat).min(r.lat) - lat,
        p.lat.max(q.lat).max(r.lat) + lat,
    ])
}

/// The vertical line down the place `lon`, `lat`: where it starts, above
/// any ground, and which way is down — earth-centred.
fn line_down(lon: f64, lat: f64) -> (DVec3, DVec3) {
    let top = geodetic_to_ecef(Geodetic {
        lon,
        lat,
        height: ABOVE_M,
    });
    let down = -DVec3::new(lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin());
    (top, down)
}

/// How far along `dir` from `from` the ray meets the triangle `t` makes of
/// `positions` (either side), in their frame: Möller–Trumbore.
fn hit(positions: &[[f32; 3]], t: &[u32; 3], from: DVec3, dir: DVec3) -> Option<f64> {
    let p = |i: u32| DVec3::from(positions[i as usize].map(f64::from));
    let (a, b, c) = (p(t[0]), p(t[1]), p(t[2]));
    let (e1, e2) = (b - a, c - a);
    let h = dir.cross(e2);
    let det = e1.dot(h);
    if det.abs() < 1.0e-12 {
        return None;
    }
    let s = from - a;
    let u = s.dot(h) / det;
    let q = s.cross(e1);
    let v = dir.dot(q) / det;
    if u < EDGE || v < EDGE || u + v > 1.0 - EDGE {
        return None;
    }
    let along = e2.dot(q) / det;
    (along >= 0.0).then_some(along)
}

/// A hair over a triangle's edges, in its own barycentric coordinates: the
/// vertices are f32 about the tile's origin, so a line down a corner can
/// miss both triangles there by a fraction of a millimetre.
const EDGE: f64 = -1.0e-6;

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;
    use tuile_core::content::{DecodedMesh, MaterialDesc};

    /// Tile `level/x/y` as a grid of `n` spacings a side, `height(u, north)`
    /// metres over the ellipsoid at each vertex.
    fn tile(
        level: u32,
        x: u64,
        y: u64,
        n: u32,
        height: impl Fn(f64, f64) -> f64,
    ) -> (TileKey, Mesh) {
        let size = PI / 2f64.powi(level as i32);
        let at = |u: f64, north: f64| {
            geodetic_to_ecef(Geodetic {
                lon: -PI + (x as f64 + u) * size,
                lat: -PI / 2.0 + (y as f64 + north) * size,
                height: height(u, north),
            })
        };
        let origin = at(0.5, 0.5);
        let mut positions = Vec::new();
        let mut uvs = Vec::new();
        for j in 0..=n {
            for i in 0..=n {
                let (u, north) = (f64::from(i) / f64::from(n), f64::from(j) / f64::from(n));
                positions.push((at(u, north) - origin).as_vec3().to_array());
                uvs.push([u as f32, 1.0 - north as f32]);
            }
        }
        let mut indices = Vec::new();
        for j in 0..n {
            for i in 0..n {
                let k = j * (n + 1) + i;
                indices.extend_from_slice(&[k, k + 1, k + n + 1, k + n + 1, k + 1, k + n + 2]);
            }
        }
        let mesh = DecodedMesh {
            positions,
            normals: None,
            uvs: Some(uvs),
            indices,
            material: MaterialDesc::default(),
        };
        (
            TileKey {
                id: TileId::from_terrain(level, x, y).0,
                drape: 0,
            },
            encoded(&mesh, origin.to_array(), [1.0; 4]),
        )
    }

    fn place(level: u32, x: u64, y: u64, u: f64, north: f64) -> (f64, f64) {
        let size = PI / 2f64.powi(level as i32);
        (
            -PI + (x as f64 + u) * size,
            -PI / 2.0 + (y as f64 + north) * size,
        )
    }

    fn ground_of(tiles: &[(TileKey, Mesh)], how: Prepare) -> Ground {
        let mut stitching = Stitching::default();
        for (key, mesh) in tiles {
            stitching.enter(*key, None, &prepared(mesh.clone(), how).0);
        }
        let selection: Vec<TileKey> = tiles.iter().map(|(k, _)| *k).collect();
        stitching.frame(&selection);
        Ground::of(&stitching, &selection)
    }

    /// A deterministic stream of numbers (splitmix64): the same places
    /// every run, so a mismatch found once is found again.
    struct Dice(u64);

    impl Dice {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            (z >> 11) as f64 / (1u64 << 53) as f64
        }

        fn between(&mut self, low: f64, high: f64) -> f64 {
            low + (high - low) * self.next()
        }

        fn pick<T: Copy>(&mut self, of: &[T]) -> T {
            of[((self.next() * of.len() as f64) as usize).min(of.len() - 1)]
        }
    }

    /// How far along `dir` from `from` the ray first meets one of the
    /// triangles `indices` makes of `positions` (both sides), in their frame.
    fn first_hit(positions: &[[f32; 3]], indices: &[u32], from: DVec3, dir: DVec3) -> Option<f64> {
        indices
            .as_chunks::<3>()
            .0
            .iter()
            .filter_map(|t| hit(positions, t, from, dir))
            .reduce(f64::min)
    }

    /// The height under a place read by brute force: every tile whose
    /// rectangle holds it, every triangle of each. What the ground has to
    /// answer, bit for bit, however it finds the triangles.
    fn brute_force(ground: &Ground, lon: f64, lat: f64) -> Option<f64> {
        let (top, down) = line_down(lon, lat);
        ground
            .tiles
            .iter()
            .filter(|t| t.holds(lon, lat))
            .filter_map(|t| first_hit(&t.positions, &t.indices, top - t.origin, down))
            .map(|along| ABOVE_M - along)
            .reduce(f64::max)
    }

    /// Metres over the ellipsoid at `lon`, `lat` (radians): hills a couple
    /// of kilometres across and 120 m high, smaller ones on them, and a
    /// cliff — the slopes a terrain source's triangles take.
    fn relief(lon: f64, lat: f64) -> f64 {
        let (x, y) = (lon * 6.378e6 * 0.6, lat * 6.378e6);
        let tau = std::f64::consts::TAU;
        let cliff = if (x / 900.0).sin() > 0.7 { 60.0 } else { 0.0 };
        300.0
            + 120.0 * (tau * x / 1700.0).sin() * (tau * y / 2300.0).cos()
            + 35.0 * (tau * x / 430.0 + 1.0).sin() * (tau * y / 510.0).sin()
            + cliff
    }

    /// Tile `level/x/y` as a terrain source states one and a renderer gets
    /// it: an irregular network of `n` spacings a side — every vertex but
    /// the corners moved at random, along its side on a side; each cell cut
    /// along a diagonal chosen at random, slivers and all — over
    /// [`relief`], decoded with its skirts by the one way a terrain tile
    /// becomes ground (`tuile_terrain::mesh::to_ground`).
    fn tin(level: u32, x: u64, y: u64, n: usize, dice: &mut Dice) -> (TileKey, Mesh) {
        use tuile_terrain::{Header, QuantizedMesh};
        let rect = GeographicTilingScheme::default().tile_rect(TileCoord::new(level, x, y));
        let m = n + 1;
        let (mut u, mut v, mut metres) = (Vec::new(), Vec::new(), Vec::new());
        for j in 0..m {
            for i in 0..m {
                let mut at = |k: usize| {
                    let moved = if k == 0 || k == n {
                        0.0
                    } else {
                        dice.between(-0.3, 0.3)
                    };
                    (k as f64 + moved) / n as f64
                };
                let (fu, fv) = (at(i), at(j));
                u.push(fu);
                v.push(fv);
                metres.push(relief(
                    rect.west + fu * rect.width(),
                    rect.south + fv * rect.height(),
                ));
            }
        }
        let mut indices = Vec::new();
        for j in 0..n {
            for i in 0..n {
                let a = (j * m + i) as u32;
                let (b, c, d) = (a + 1, a + m as u32, a + m as u32 + 1);
                if dice.next() < 0.5 {
                    indices.extend_from_slice(&[a, b, c, c, b, d]);
                } else {
                    indices.extend_from_slice(&[a, b, d, a, d, c]);
                }
            }
        }
        let low = metres.iter().copied().fold(f64::INFINITY, f64::min);
        let high = metres.iter().copied().fold(f64::NEG_INFINITY, f64::max) + 1.0;
        let (lon, lat) = rect.center();
        let centre = geodetic_to_ecef(Geodetic {
            lon,
            lat,
            height: (low + high) / 2.0,
        });
        let line = |f: &dyn Fn(usize) -> usize| (0..m).map(|k| f(k) as u32).collect::<Vec<u32>>();
        let source = QuantizedMesh {
            header: Header {
                center: centre.to_array(),
                min_height: low as f32,
                max_height: high as f32,
                bounding_sphere_center: centre.to_array(),
                bounding_sphere_radius: rect.height() * 6.4e6,
                horizon_occlusion: [0.0; 3],
            },
            u,
            v,
            height: metres
                .iter()
                .map(|h| (h - low) / (f64::from(high as f32) - f64::from(low as f32)))
                .collect(),
            indices,
            normals: None,
            edges: [
                line(&|k| k * m),
                line(&|k| k),
                line(&|k| k * m + n),
                line(&|k| (m - 1) * m + k),
            ],
            metadata_available: None,
            cut: 0,
        };
        let content = tuile_terrain::mesh::to_ground(&source, &rect);
        (
            TileKey {
                id: TileId::from_terrain(level, x, y).0,
                drape: 0,
            },
            encoded(
                &content.meshes[0],
                content.local_origin_ecef.to_array(),
                [1.0; 4],
            ),
        )
    }

    /// The tiles of a frame over [`relief`], as a selection holds them: a
    /// coarse irregular one; east of it four of the next level, finer and
    /// irregular, whose seams with it and with each other are stitched;
    /// north of those a regular 64 × 64 grid and an irregular 64 × 64 one.
    fn landscape(dice: &mut Dice) -> Vec<(TileKey, Mesh)> {
        let rough = |u: f64, north: f64| {
            let (lon, lat) = place(14, 9000, 6201, u, north);
            relief(lon, lat)
        };
        vec![
            tin(14, 9000, 6200, 40, dice),
            tin(15, 18002, 12400, 32, dice),
            tin(15, 18003, 12400, 24, dice),
            tin(15, 18002, 12401, 28, dice),
            tin(15, 18003, 12401, 32, dice),
            tile(14, 9000, 6201, 64, rough),
            tin(14, 9001, 6201, 64, dice),
        ]
    }

    /// The places a height is asked at over `ground`: anywhere around it,
    /// and wherever finding the triangles could go wrong — every vertex,
    /// and a hair off it; along the tiles' sides, on them, inside and
    /// outside the hair a side is read within; across the edges of the
    /// triangles, on them and on either side of the hair a triangle is
    /// widened by.
    fn places(ground: &Ground, dice: &mut Dice) -> Vec<(f64, f64)> {
        use tuile_core::geo::ecef_to_geodetic;
        let mut out = Vec::new();
        let (mut west, mut south, mut east, mut north) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for t in &ground.tiles {
            west = west.min(t.rect.west);
            south = south.min(t.rect.south);
            east = east.max(t.rect.east);
            north = north.max(t.rect.north);
        }
        let (w, h) = (east - west, north - south);
        for _ in 0..4000 {
            out.push((
                dice.between(west - 0.05 * w, east + 0.05 * w),
                dice.between(south - 0.05 * h, north + 0.05 * h),
            ));
        }
        let hairs = [
            0.0,
            1.0e-15,
            -1.0e-15,
            5.0e-13,
            -5.0e-13,
            ON,
            -ON,
            1.0001e-12,
            -1.0001e-12,
            3.0e-12,
            -3.0e-12,
        ];
        for t in &ground.tiles {
            let r = t.rect;
            for _ in 0..120 {
                let (lon, lat) = (dice.between(r.west, r.east), dice.between(r.south, r.north));
                let side = dice.pick(&[r.west, r.east]);
                out.push((side + dice.pick(&hairs), lat));
                let side = dice.pick(&[r.south, r.north]);
                out.push((lon, side + dice.pick(&hairs)));
            }
            for corner in [
                (r.west, r.south),
                (r.east, r.south),
                (r.west, r.north),
                (r.east, r.north),
            ] {
                for _ in 0..4 {
                    out.push((corner.0 + dice.pick(&hairs), corner.1 + dice.pick(&hairs)));
                }
            }
            let at = |p: DVec3| {
                let g = ecef_to_geodetic(t.origin + p);
                (g.lon, g.lat)
            };
            let vertex = |i: u32| DVec3::from(t.positions[i as usize].map(f64::from));
            for _ in 0..400 {
                let p = dice.pick(&t.positions);
                let (lon, lat) = at(DVec3::from(p.map(f64::from)));
                out.push((lon, lat));
                out.push((lon + dice.pick(&hairs), lat + dice.pick(&hairs)));
            }
            let triangles = t.indices.as_chunks::<3>().0;
            // On an edge, and a whisker either side of the hair `EDGE`
            // widens a triangle by: the barycentric coordinates the hit
            // test reads, set where it decides.
            let rims = [0.0, EDGE, EDGE * 0.999, EDGE * 1.001, EDGE * 2.0, 1.0e-9];
            for _ in 0..300 {
                let tri = dice.pick(triangles);
                let (a, b, c) = (vertex(tri[0]), vertex(tri[1]), vertex(tri[2]));
                let along = dice.next();
                let rim = dice.pick(&rims);
                let (u, v) = match dice.pick(&[0, 1, 2]) {
                    0 => (rim, along),
                    1 => (along, rim),
                    _ => (along * (1.0 - rim), (1.0 - along) * (1.0 - rim)),
                };
                out.push(at(a + (b - a) * u + (c - a) * v));
            }
        }
        out
    }

    /// Wherever it is asked, the ground answers exactly what reading every
    /// triangle of every tile there answers: the same `Option`, the same
    /// bits — on irregular and regular tiles, with skirts and without,
    /// smoothed or not, stitched across levels or not.
    #[test]
    fn the_height_is_every_triangle_s_answer_bit_for_bit() {
        let mut dice = Dice(0x5EED_6E0D);
        let tiles = landscape(&mut dice);
        // And the four fine tiles alone: a ground whose tiles' sides fall
        // on the lines that cut it in cells.
        let block = tiles[1..5].to_vec();
        for tiles in [tiles, block] {
            every_triangle_s_answer(&tiles, &mut dice);
        }
    }

    fn every_triangle_s_answer(tiles: &[(TileKey, Mesh)], dice: &mut Dice) {
        let selection: Vec<TileKey> = tiles.iter().map(|(k, _)| *k).collect();
        let grounds = [
            (
                "as the render service draws it",
                Prepare {
                    skirts: true,
                    smooth: Smooth::Geometry {
                        bound: smoothing::DEFAULT_BOUND_M,
                    },
                },
                true,
            ),
            ("skirts dropped, unstitched", Prepare::default(), false),
            (
                "skirts kept, stitched",
                Prepare {
                    skirts: true,
                    smooth: Smooth::Off,
                },
                true,
            ),
        ];
        for (name, how, stitch) in grounds {
            let mut stitching = Stitching::default();
            for (key, mesh) in tiles {
                stitching.enter(*key, None, &prepared(mesh.clone(), how).0);
            }
            let ground = if stitch {
                stitching.frame(&selection);
                Ground::of(&stitching, &selection)
            } else {
                Ground::unstitched(&stitching, &selection)
            };
            assert_eq!(ground.len(), tiles.len());
            let (mut asked, mut found, mut differ) = (0usize, 0usize, Vec::new());
            for (lon, lat) in places(&ground, dice) {
                let (indexed, brute) = (ground.height_at(lon, lat), brute_force(&ground, lon, lat));
                asked += 1;
                found += usize::from(brute.is_some());
                if indexed.map(f64::to_bits) != brute.map(f64::to_bits) {
                    differ.push((lon, lat, indexed, brute));
                }
            }
            assert!(found > asked / 2, "{name}: {found} of {asked} found ground");
            assert!(
                differ.is_empty(),
                "{name}: {} of {asked} places differ, first {:?}",
                differ.len(),
                &differ[..differ.len().min(5)]
            );
        }
    }

    /// What a height costs, and a ground: `cargo test --release -p
    /// tuile-film --lib what_a_height_costs -- --ignored --nocapture`.
    ///
    /// A block of `SIDE` × `SIDE` irregular 64 × 64 tiles drawn as the
    /// render service draws them, asked at places all over it: heights a
    /// second read by brute force and through the cells, with and without
    /// their metering, and what building the ground costs the first time
    /// and the next.
    #[test]
    #[ignore = "a measurement, not a check"]
    fn what_a_height_costs() {
        use std::time::Instant;
        const SIDE: u64 = 8;
        let mut dice = Dice(7);
        let tiles: Vec<(TileKey, Mesh)> = (0..SIDE * SIDE)
            .map(|k| tin(14, 9000 + k % SIDE, 6200 + k / SIDE, 64, &mut dice))
            .collect();
        let selection: Vec<TileKey> = tiles.iter().map(|(k, _)| *k).collect();
        let how = Prepare {
            skirts: true,
            smooth: Smooth::Geometry {
                bound: smoothing::DEFAULT_BOUND_M,
            },
        };
        let mut stitching = Stitching::default();
        for (key, mesh) in &tiles {
            stitching.enter(*key, None, &prepared(mesh.clone(), how).0);
        }
        stitching.frame(&selection);

        let clock = Instant::now();
        let ground = Ground::of(&stitching, &selection);
        let first = clock.elapsed();
        let clock = Instant::now();
        let again = Ground::of(&stitching, &selection);
        let next = clock.elapsed();
        let clock = Instant::now();
        let cloned: usize = selection
            .iter()
            .filter_map(|key| stitching.drawn(key))
            .map(|(_, mesh)| mesh.indices.len())
            .sum();
        let as_before = clock.elapsed();
        assert_eq!(again.len(), ground.len());
        let triangles: usize = ground.tiles.iter().map(|t| t.indices.len() / 3).sum();
        let listed: usize = ground
            .tiles
            .iter()
            .map(|t| match &t.cells {
                Cells::Narrow(grid) => grid.listed.len(),
                Cells::Wide(grid) => grid.listed.len(),
            })
            .sum();
        let everywhere: usize = ground.tiles.iter().map(|t| t.everywhere.len()).sum();
        eprintln!(
            "{} tiles, {triangles} triangles ({cloned} indices), {:.1} MiB with their cells; \
             {listed} listings, {everywhere} read everywhere",
            ground.len(),
            ground.heap_bytes() as f64 / f64::from(1u32 << 20),
        );
        eprintln!(
            "Ground::of: {first:?} building the cells, {next:?} once built; \
             the meshes drawn again as before: {as_before:?}"
        );

        let r = GeographicTilingScheme::default().tile_rect(TileCoord::new(14, 9000, 6200));
        let (w, h) = (r.width() * SIDE as f64, r.height() * SIDE as f64);
        let places: Vec<(f64, f64)> = (0..50_000)
            .map(|_| {
                (
                    dice.between(r.west, r.west + w),
                    dice.between(r.south, r.south + h),
                )
            })
            .collect();
        // And as a ray is marched: places a few tens of metres apart, one
        // after the other, a few hundred along each of many lines.
        let step = r.width() / 64.0;
        let mut marched = Vec::new();
        while marched.len() < places.len() {
            let (mut lon, mut lat) = (
                dice.between(r.west, r.west + w),
                dice.between(r.south, r.south + h),
            );
            let heading = dice.between(0.0, std::f64::consts::TAU);
            for _ in 0..300 {
                lon = (lon + step * heading.cos()).clamp(r.west, r.west + w);
                lat = (lat + step * heading.sin()).clamp(r.south, r.south + h);
                marched.push((lon, lat));
            }
        }
        // The best of a few rounds: a laptop's other work only ever slows
        // one down.
        let per_second = |name: &str, n: usize, read: &dyn Fn(f64, f64) -> Option<f64>| {
            for (how, places) in [("anywhere", &places), ("marched", &marched)] {
                let mut best = f64::INFINITY;
                let mut found = 0;
                for _ in 0..5 {
                    let clock = Instant::now();
                    found = places[..n]
                        .iter()
                        .filter(|(lon, lat)| read(*lon, *lat).is_some())
                        .count();
                    best = best.min(clock.elapsed().as_secs_f64());
                }
                eprintln!(
                    "{name}, {how}: {:.0} heights a second, {:.3} µs each ({found} of {n} found)",
                    n as f64 / best,
                    best * 1.0e6 / n as f64,
                );
            }
        };
        per_second("brute force", 400, &|lon, lat| {
            brute_force(&ground, lon, lat)
        });
        let timer = tuile_core::metrics::Timer::default();
        let counter = tuile_core::metrics::Counter::default();
        per_second("cells, unmetered", places.len(), &|lon, lat| {
            ground.height_under(lon, lat)
        });
        per_second(
            "cells, one in SAMPLED metered",
            places.len(),
            &|lon, lat| ground.height_at(lon, lat),
        );
        per_second("cells, every one metered", places.len(), &|lon, lat| {
            counter.inc();
            timer.time(|| ground.height_under(lon, lat))
        });
        per_second("every tile's rectangle", places.len(), &|lon, lat| {
            let holding = ground.tiles.iter().filter(|t| t.holds(lon, lat)).count();
            (holding > 0).then_some(0.0)
        });
        per_second(
            "the rectangles listed over the place",
            places.len(),
            &|lon, lat| {
                let by_place = ground.by_place.as_ref()?;
                let holding = by_place
                    .at(lon, lat)
                    .iter()
                    .filter(|&&k| ground.tiles[k as usize].holds(lon, lat))
                    .count();
                (holding > 0).then_some(0.0)
            },
        );
    }

    /// The cells' coordinates are the engine's to well within the margin
    /// they are widened by, from the deepest skirt to above any summit,
    /// pole to pole short of the last degree.
    #[test]
    fn the_cells_coordinates_are_the_engine_s() {
        let mut dice = Dice(11);
        let mut worst = (0.0f64, 0.0f64);
        for _ in 0..100_000 {
            let place = Geodetic {
                lon: dice.between(-PI, PI),
                lat: dice.between(-POLEWARD, POLEWARD),
                height: dice.between(LOWEST_M, 12_000.0),
            };
            let p = geodetic_to_ecef(place);
            let (ours, theirs) = (geodetic(p), tuile_core::geo::ecef_to_geodetic(p));
            assert_eq!(ours.lon, theirs.lon);
            worst.0 = worst.0.max((ours.lat - theirs.lat).abs());
            worst.1 = worst.1.max((ours.height - theirs.height).abs());
        }
        assert!(
            worst.0 * RADIUS_M < SLACK_M / 1000.0 && worst.1 < 1.0,
            "{worst:?}"
        );
    }

    #[test]
    fn flat_ground_is_read_at_its_height_and_nowhere_else() {
        let flat = tile(14, 9000, 6200, 4, |_, _| 812.0);
        let ground = ground_of(&[flat], Prepare::default());
        for (u, north) in [(0.5, 0.5), (0.13, 0.91), (0.0, 0.0), (1.0, 1.0)] {
            let (lon, lat) = place(14, 9000, 6200, u, north);
            let h = ground.height_at(lon, lat).expect("on the tile");
            assert!((h - 812.0).abs() < 0.05, "{u} {north}: {h}");
        }
        let (lon, lat) = place(14, 9000, 6200, 1.5, 0.5);
        assert_eq!(ground.height_at(lon, lat), None, "off every drawn tile");
    }

    /// A ridge across the tile: the height between two vertices is the
    /// drawn triangle's — the flat chord unsmoothed, the bulge of the
    /// smoothed patch otherwise, never more than its bound above.
    #[test]
    fn the_height_is_the_smoothed_surface_s_where_the_ground_is_smoothed() {
        let ridge = |u: f64, _: f64| 1000.0 + 400.0 * (1.0 - (2.0 * u - 1.0).abs());
        let t = tile(14, 9000, 6200, 4, ridge);
        // Halfway between the ridge's line (u = 0.5) and the next vertices.
        let (lon, lat) = place(14, 9000, 6200, 0.5, 0.375);
        let flat = ground_of(std::slice::from_ref(&t), Prepare::default());
        let raw = flat.height_at(lon, lat).expect("ground");
        assert!((raw - 1400.0).abs() < 0.5, "{raw}");
        let bound = 2.0;
        let round = ground_of(
            &[t],
            Prepare {
                smooth: Smooth::Geometry { bound },
                ..Prepare::default()
            },
        );
        let (lon, lat) = place(14, 9000, 6200, 0.4375, 0.4375);
        let chord = flat.height_at(lon, lat).expect("ground");
        let smoothed = round.height_at(lon, lat).expect("ground");
        assert!(
            (smoothed - chord).abs() > 0.1 && (smoothed - chord).abs() <= f64::from(bound) + 0.05,
            "chord {chord}, smoothed {smoothed}"
        );
    }

    /// A coarse tile beside two fine ones 5 m higher: stitched, the fine
    /// tiles' edge comes down to the coarse one, and the ground read on it
    /// is the stitched one.
    #[test]
    fn the_height_is_the_stitched_surface_s_along_a_seam() {
        let coarse = tile(14, 9000, 6200, 4, |_, _| 100.0);
        let south = tile(15, 18002, 12400, 4, |_, _| 105.0);
        let north = tile(15, 18002, 12401, 4, |_, _| 105.0);
        // On the fine tiles' western edge, which meets the coarse tile.
        let (lon, lat) = place(15, 18002, 12400, 0.0, 0.5);
        let alone = ground_of(&[south.clone(), north.clone()], Prepare::default());
        let unsewn = alone.height_at(lon + 1.0e-9, lat).expect("ground");
        assert!((unsewn - 105.0).abs() < 0.05, "{unsewn}");
        let sewn = ground_of(&[coarse, south, north], Prepare::default());
        let at_seam = sewn.height_at(lon + 1.0e-9, lat).expect("ground");
        assert!(
            at_seam < 101.0,
            "the fine edge brought down to the coarse ground: {at_seam}"
        );
        // Inside the fine tile, past the stitching band: its own height.
        let (lon, lat) = place(15, 18002, 12400, 0.6, 0.5);
        let inside = sewn.height_at(lon, lat).expect("ground");
        assert!((inside - 105.0).abs() < 0.05, "{inside}");
    }

    /// Everything a drawn tile is made of — its vertices, its triangles,
    /// which triangles each cell lists and which are read everywhere —
    /// bit for bit.
    fn bits(drawn: &Drawn) -> Vec<u64> {
        let mut out: Vec<u64> = drawn
            .positions
            .iter()
            .flatten()
            .map(|c| u64::from(c.to_bits()))
            .collect();
        out.extend(drawn.indices.iter().map(|i| u64::from(*i)));
        out.extend(drawn.everywhere.iter().map(|i| u64::from(*i)));
        match &drawn.cells {
            Cells::Narrow(grid) => {
                out.extend(grid.starts.iter().map(|i| u64::from(*i)));
                out.extend(grid.listed.iter().map(|i| u64::from(*i)));
            }
            Cells::Wide(grid) => {
                out.extend(grid.starts.iter().map(|i| u64::from(*i)));
                out.extend(grid.listed.iter().map(|i| u64::from(*i)));
            }
        }
        out
    }

    /// A tile drawn again for another plan reads the places of the
    /// vertices that plan left where they were rather than working them
    /// out: what it draws is, bit for bit, what working out every vertex's
    /// place draws — whichever selections came before, and built on
    /// whichever thread.
    #[test]
    fn a_tile_drawn_again_reads_the_places_of_the_vertices_left_alone() {
        let mut dice = Dice(0x0B5E_55ED);
        let tiles = landscape(&mut dice);
        let how = Prepare {
            skirts: true,
            smooth: Smooth::Geometry {
                bound: smoothing::DEFAULT_BOUND_M,
            },
        };
        let prepared_tiles: Vec<(TileKey, Mesh)> = tiles
            .iter()
            .map(|(key, mesh)| (*key, prepared(mesh.clone(), how).0))
            .collect();
        let all: Vec<TileKey> = tiles.iter().map(|(k, _)| *k).collect();
        // The coarse tile alone with its regular neighbour, then the whole
        // landscape, then the fine block without the coarse tile: each
        // tile's plan changes at least once along the way.
        let selections = [
            vec![all[0], all[5]],
            all.clone(),
            all[1..5].to_vec(),
            all.clone(),
        ];
        let mut stitching = Stitching::default();
        for (key, mesh) in &prepared_tiles {
            stitching.enter(*key, None, mesh);
        }
        let (mut rebuilt, mut moved) = (0, 0);
        for selection in &selections {
            stitching.frame(selection);
            let ground = Ground::built_with(&stitching, selection, true, |missing, build| {
                rebuilt += missing.len();
                std::thread::scope(|scope| {
                    for key in missing {
                        scope.spawn(move || build(key));
                    }
                });
            });
            // Each tile as drawn for this plan, every vertex's place
            // worked out afresh.
            let afresh: Vec<Drawn> = selection
                .iter()
                .map(|key| {
                    let (origin, mesh) = stitching.drawn(key).expect("held");
                    let places = places_of(key, origin, &mesh.positions);
                    Drawn::placed(key, origin, mesh.positions, mesh.indices, &places)
                })
                .collect();
            assert_eq!(ground.len(), afresh.len());
            for (again, once) in ground.tiles.iter().zip(&afresh) {
                assert!(bits(again) == bits(once), "a tile drawn again differs");
            }
            moved += selection
                .iter()
                .filter(|key| {
                    let (_, mesh) = stitching.drawn(key).expect("held");
                    let (_, entered) = stitching.held(key).expect("held");
                    mesh.positions.len() > entered.positions.len()
                })
                .count();
        }
        assert!(rebuilt > all.len(), "tiles were drawn again: {rebuilt}");
        assert!(moved > 0, "some plan gave a tile vertices");
    }
}
