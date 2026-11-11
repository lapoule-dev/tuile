// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A resident terrain tile's mesh, kept for the ground a camera is kept over
//! and nothing else: as small as it can be held and still give back, bit for
//! bit, the mesh that came.
//!
//! A terrain tile states each vertex as a place in the tile and a height —
//! `u` and `v` on a grid of 32767 a side, as the quantized-mesh source has
//! them — and the mesh that arrives is that, turned into ECEF about the
//! tile's origin and rounded to `f32` (`tuile_terrain::to_decoded`). So
//! that is what is kept: the grid place in two `u16`, the height in one
//! `u32` step of the tile's span of heights, and the positions computed
//! again the same way when the tile is drawn. No normals, no float
//! coordinates, no positions.
//!
//! The height is a `u32` step rather than an `f32` because an `f32` is not
//! fine enough. A vertex's position is rounded to `f32` per axis about the
//! tile's origin, and an axis on which the vertex is a metre from the origin
//! is rounded to a ten-millionth of a metre, where an `f32` height of
//! 1000 m only moves by six hundred-thousandths: measured on a level-14
//! tile, two vertices in three had no `f32` height that gives their
//! position back. A step of a span of a few hundred metres cut in 2³² is a
//! ten-millionth of a metre, and the step is chosen inside the interval of
//! heights every axis allows, worked out from the ellipsoid's own formula
//! (the position is linear in the height).
//!
//! **Bit for bit, by construction.** Nothing is assumed of a mesh. A tile's
//! coordinates are kept on the grid only if every one of them is the grid's
//! exactly, and a vertex's height only if the position computed from it is
//! the position that came, in every bit; any other vertex — a skirt's foot,
//! hung outside the tile; a vertex of a tile cut from an ancestor, whose
//! places are not on the grid — is kept as it came. The indices are kept
//! losslessly, packed. What [`Kept::mesh`] gives back is the mesh that came.

use glam::DVec3;
use tuile_core::content::DecodedMesh;
use tuile_core::geo::{ecef_to_geodetic, geodetic_to_ecef, Geodetic};
use tuile_core::source::TileId;
use tuile_terrain::{Foot, GeoRect, GeographicTilingScheme, Skirts, TileCoord};

/// The quantized-mesh grid: `u` and `v` are `q / 32767`.
const GRID: f64 = 32767.0;

/// How many steps either side of the one a position's height falls on are
/// tried before the vertex is kept as it came.
const HEIGHT_STEPS: i64 = 3;

/// A vertex kept as it came, among the heights.
const EXCEPTION: u32 = u32::MAX;

/// A tile's heights: `lowest + k · step` for `k` in `0..u32::MAX`.
#[derive(Debug, Clone, Copy)]
struct Span {
    lowest: f64,
    step: f64,
}

impl Span {
    fn height(self, k: u32) -> f64 {
        self.lowest + f64::from(k) * self.step
    }
}

/// A tile's mesh, kept for the ground.
#[derive(Debug, Clone)]
pub(crate) struct Kept {
    origin: DVec3,
    vertices: Vertices,
    /// How many indices [`Self::indices`] packs.
    index_count: usize,
    /// The indices, each as its distance from the highest one met so far,
    /// zig-zagged into a varint: a byte an index on a mesh numbered in the
    /// order its triangles first use its vertices — the quantized-mesh
    /// order.
    indices: Vec<u8>,
    /// The tile's skirts, when they came apart from its surface: kept as
    /// what they hang from, and hung again by [`Self::mesh`].
    skirts: Option<KeptSkirts>,
}

/// A tile's skirts, as what they hang from (`tuile_terrain::Skirts`): the
/// tile's rectangle, the depth, each side's edge vertices in the order the
/// wall walks them, and each one's height. Twelve bytes an edge vertex,
/// where the walls themselves were a vertex kept as it came and two
/// triangles each.
#[derive(Debug, Clone)]
struct KeptSkirts {
    rect: GeoRect,
    depth: f64,
    walks: [Vec<u32>; 4],
    /// Each walked vertex's height, side after side.
    heights: Vec<f64>,
    /// Each walked vertex's place, side after side — only when the kept
    /// surface does not state it exactly: a mesh kept as it came, or one
    /// whose places are not the grid's.
    places: Option<Vec<[f64; 2]>>,
}

#[derive(Debug, Clone)]
enum Vertices {
    /// Every vertex's place is on the grid.
    Grid {
        /// `(q_u, q_v)`: `u = q_u / 32767`, `v = q_v / 32767`.
        places: Vec<[u16; 2]>,
        /// The heights' span.
        span: Span,
        /// A vertex's height as a step of `span`, from which its position
        /// comes back bit for bit; [`EXCEPTION`] for one kept in
        /// `exceptions`.
        heights: Vec<u32>,
        /// The vertices whose position does not come back from a height,
        /// kept as they came, by index.
        exceptions: Vec<(u32, [f32; 3])>,
    },
    /// As they came.
    AsCame {
        positions: Vec<[f32; 3]>,
        uvs: Option<Vec<[f32; 2]>>,
    },
}

/// The rectangle of terrain tile `tile`, as the loader states it.
fn rect_of(tile: TileId) -> GeoRect {
    let (level, x, y) = tile.terrain_coord();
    GeographicTilingScheme::default().tile_rect(TileCoord::new(level, x, y))
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// The grid place a vertex's texture coordinates state, when they are
/// exactly the grid's: `uv = (u as f32, 1 - v as f32)`.
fn on_the_grid(uv: [f32; 2]) -> Option<[u16; 2]> {
    let q = |f: f64| {
        let q = (f * GRID).round();
        (0.0..=GRID).contains(&q).then_some(q as u16)
    };
    let place = [q(f64::from(uv[0]))?, q(1.0 - f64::from(uv[1]))?];
    (uv_of(place).map(f32::to_bits) == uv.map(f32::to_bits)).then_some(place)
}

/// The texture coordinates of a grid place, as `tuile_terrain` computes them.
fn uv_of(place: [u16; 2]) -> [f32; 2] {
    let (u, v) = (f64::from(place[0]) / GRID, f64::from(place[1]) / GRID);
    [u as f32, 1.0 - v as f32]
}

/// Where the vertex at `place` is, in longitude and latitude.
fn lon_lat(rect: &GeoRect, place: [u16; 2]) -> (f64, f64) {
    let (u, v) = (f64::from(place[0]) / GRID, f64::from(place[1]) / GRID);
    (
        lerp(rect.west, rect.east, u),
        lerp(rect.south, rect.north, v),
    )
}

/// The position of the vertex at `place` and `height`, about `origin`, as
/// `tuile_terrain::to_decoded` computes it: operation for operation.
fn position(rect: &GeoRect, origin: DVec3, place: [u16; 2], height: f64) -> [f32; 3] {
    let (lon, lat) = lon_lat(rect, place);
    let r = geodetic_to_ecef(Geodetic { lon, lat, height }) - origin;
    [r.x as f32, r.y as f32, r.z as f32]
}

/// The heights at which the vertex at `place` lands, axis by axis, on the
/// `f32` it came as: the position is `(N + h)·n` on two axes and
/// `(N(1 − e²) + h)·n` on the third, linear in `h` along the vertical `n`,
/// so each axis allows an interval of heights and the vertex is in all
/// three. Where they do not meet — rounding at the edge of one — the height
/// the position is at.
fn heights_allowed(rect: &GeoRect, origin: DVec3, place: [u16; 2], came: [f32; 3]) -> f64 {
    let (lon, lat) = lon_lat(rect, place);
    let base = geodetic_to_ecef(Geodetic {
        lon,
        lat,
        height: 0.0,
    }) - origin;
    let up = DVec3::new(lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin());
    let (mut from, mut to) = (f64::NEG_INFINITY, f64::INFINITY);
    for axis in 0..3 {
        let t = came[axis];
        if up[axis].abs() < 1.0e-9 {
            continue;
        }
        // What rounds to `t`: halfway to the `f32`s either side of it.
        let below = (f64::from(t) + f64::from(t.next_down())) / 2.0;
        let above = (f64::from(t) + f64::from(t.next_up())) / 2.0;
        let (a, b) = (
            (below - base[axis]) / up[axis],
            (above - base[axis]) / up[axis],
        );
        from = from.max(a.min(b));
        to = to.min(a.max(b));
    }
    if from <= to {
        (from + to) / 2.0
    } else {
        ecef_to_geodetic(origin + DVec3::from(came.map(f64::from))).height
    }
}

/// The step of `span` from which the position at `place` comes back as
/// `came`, in every bit, if there is one.
fn height_for(
    rect: &GeoRect,
    origin: DVec3,
    span: Span,
    place: [u16; 2],
    came: [f32; 3],
) -> Option<u32> {
    let bits = came.map(f32::to_bits);
    let k = ((heights_allowed(rect, origin, place, came) - span.lowest) / span.step).round();
    if !k.is_finite() {
        return None;
    }
    let k = k as i64;
    (0..=HEIGHT_STEPS)
        .flat_map(|d| [k + d, k - d])
        .filter_map(|k| u32::try_from(k).ok().filter(|k| *k != EXCEPTION))
        .find(|k| position(rect, origin, place, span.height(*k)).map(f32::to_bits) == bits)
}

fn pack_indices(indices: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(indices.len());
    let mut highest = 0i64;
    for &i in indices {
        let d = highest - i64::from(i);
        let mut z = ((d << 1) ^ (d >> 63)) as u64;
        while z >= 0x80 {
            out.push((z as u8) | 0x80);
            z >>= 7;
        }
        out.push(z as u8);
        highest = highest.max(i64::from(i) + 1);
    }
    out.shrink_to_fit();
    out
}

fn unpack_indices(bytes: &[u8], count: usize) -> Vec<u32> {
    let mut out = Vec::with_capacity(count);
    let mut highest = 0i64;
    let (mut z, mut shift) = (0u64, 0u32);
    for &b in bytes {
        z |= u64::from(b & 0x7f) << shift;
        if b & 0x80 != 0 {
            shift += 7;
            continue;
        }
        let d = ((z >> 1) as i64) ^ -((z & 1) as i64);
        let i = highest - d;
        out.push(i as u32);
        highest = highest.max(i + 1);
        (z, shift) = (0, 0);
    }
    out
}

impl Kept {
    /// What is kept of `mesh`, terrain tile `tile`'s mesh about `origin`.
    pub(crate) fn of(tile: TileId, origin: DVec3, mesh: &DecodedMesh) -> Self {
        let places: Option<Vec<[u16; 2]>> = mesh
            .uvs
            .as_ref()
            .filter(|uvs| uvs.len() == mesh.positions.len())
            .and_then(|uvs| uvs.iter().map(|uv| on_the_grid(*uv)).collect());
        let as_came = || Vertices::AsCame {
            positions: mesh.positions.clone(),
            uvs: mesh.uvs.clone(),
        };
        let vertices = match places {
            None => as_came(),
            Some(places) => {
                let rect = rect_of(tile);
                // The span the heights are in, a metre clear either side.
                let (lowest, highest) = mesh.positions.iter().fold(
                    (f64::INFINITY, f64::NEG_INFINITY),
                    |(low, high), p| {
                        let h = ecef_to_geodetic(origin + DVec3::from(p.map(f64::from))).height;
                        (low.min(h), high.max(h))
                    },
                );
                let span = Span {
                    lowest: lowest - 1.0,
                    step: (highest - lowest + 2.0) / f64::from(EXCEPTION - 1),
                };
                let mut exceptions = Vec::new();
                let heights: Vec<u32> = places
                    .iter()
                    .zip(&mesh.positions)
                    .enumerate()
                    .map(|(i, (place, came))| {
                        height_for(&rect, origin, span, *place, *came).unwrap_or_else(|| {
                            exceptions.push((i as u32, *came));
                            EXCEPTION
                        })
                    })
                    .collect();
                // An exception costs four times a height: past a quarter of
                // the vertices, the positions as they came are smaller.
                if exceptions.len() * 4 > places.len() {
                    as_came()
                } else {
                    Vertices::Grid {
                        places,
                        span,
                        heights,
                        exceptions,
                    }
                }
            }
        };
        Self {
            origin,
            vertices,
            index_count: mesh.indices.len(),
            indices: pack_indices(&mesh.indices),
            skirts: None,
        }
    }

    /// What is kept of `surface`, terrain tile `tile`'s surface about
    /// `origin`, and of the `skirts` that came apart from it: the mesh that
    /// [`Self::mesh`] gives back is the surface with its skirts hung under
    /// it, bit for bit what the tile would have come as whole.
    pub(crate) fn of_apart(
        tile: TileId,
        origin: DVec3,
        surface: &DecodedMesh,
        skirts: &Skirts,
    ) -> Self {
        let mut kept = Self::of(tile, origin, surface);
        let feet = || skirts.sides.iter().flatten();
        let places = match &kept.vertices {
            Vertices::Grid { places, .. } => Some(places),
            Vertices::AsCame { .. } => None,
        };
        // The places the surface states, when they are each foot's exactly.
        let stated = places.is_some_and(|places| {
            feet().all(|foot| {
                places.get(foot.vertex as usize).is_some_and(|p| {
                    let (u, v) = (f64::from(p[0]) / GRID, f64::from(p[1]) / GRID);
                    u.to_bits() == foot.u.to_bits() && v.to_bits() == foot.v.to_bits()
                })
            })
        });
        kept.skirts = Some(KeptSkirts {
            rect: skirts.rect,
            depth: skirts.depth,
            walks: std::array::from_fn(|k| skirts.sides[k].iter().map(|f| f.vertex).collect()),
            heights: feet().map(|f| f.height).collect(),
            places: (!stated).then(|| feet().map(|f| [f.u, f.v]).collect()),
        });
        kept
    }

    pub(crate) fn origin(&self) -> DVec3 {
        self.origin
    }

    /// The mesh that came, for terrain tile `tile`: positions, texture
    /// coordinates and indices, bit for bit; no normals.
    pub(crate) fn mesh(&self, tile: TileId) -> DecodedMesh {
        let (positions, uvs) = match &self.vertices {
            Vertices::AsCame { positions, uvs } => (positions.clone(), uvs.clone()),
            Vertices::Grid {
                places,
                span,
                heights,
                exceptions,
            } => {
                let rect = rect_of(tile);
                let mut positions: Vec<[f32; 3]> = places
                    .iter()
                    .zip(heights)
                    .map(|(place, k)| match *k {
                        EXCEPTION => [0.0; 3],
                        k => position(&rect, self.origin, *place, span.height(k)),
                    })
                    .collect();
                for (i, p) in exceptions {
                    positions[*i as usize] = *p;
                }
                (positions, Some(places.iter().map(|p| uv_of(*p)).collect()))
            }
        };
        let mut mesh = DecodedMesh {
            positions,
            normals: None,
            uvs,
            indices: unpack_indices(&self.indices, self.index_count),
            material: Default::default(),
        };
        if let Some(skirts) = &self.skirts {
            skirts
                .skirts(&self.vertices)
                .hang_on(self.origin, &mut mesh);
        }
        mesh
    }

    /// Whether `other` keeps the same mesh, bit for bit: what a tile that
    /// came again is kept as, when it came again unchanged.
    pub(crate) fn same_as(&self, other: &Self) -> bool {
        let f32s = |a: &[[f32; 3]], b: &[[f32; 3]]| {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|(a, b)| a.map(f32::to_bits) == b.map(f32::to_bits))
        };
        let same_vertices = match (&self.vertices, &other.vertices) {
            (
                Vertices::Grid {
                    places,
                    span,
                    heights,
                    exceptions,
                },
                Vertices::Grid {
                    places: their_places,
                    span: their_span,
                    heights: their_heights,
                    exceptions: their_exceptions,
                },
            ) => {
                places == their_places
                    && span.lowest.to_bits() == their_span.lowest.to_bits()
                    && span.step.to_bits() == their_span.step.to_bits()
                    && heights == their_heights
                    && exceptions.len() == their_exceptions.len()
                    && exceptions
                        .iter()
                        .zip(their_exceptions)
                        .all(|(a, b)| a.0 == b.0 && a.1.map(f32::to_bits) == b.1.map(f32::to_bits))
            }
            (
                Vertices::AsCame { positions, uvs },
                Vertices::AsCame {
                    positions: their_positions,
                    uvs: their_uvs,
                },
            ) => {
                f32s(positions, their_positions)
                    && match (uvs, their_uvs) {
                        (None, None) => true,
                        (Some(a), Some(b)) => {
                            a.len() == b.len()
                                && a.iter()
                                    .zip(b)
                                    .all(|(a, b)| a.map(f32::to_bits) == b.map(f32::to_bits))
                        }
                        _ => false,
                    }
            }
            _ => false,
        };
        let same_skirts = match (&self.skirts, &other.skirts) {
            (None, None) => true,
            (Some(a), Some(b)) => a.same_as(b),
            _ => false,
        };
        self.origin.to_array().map(f64::to_bits) == other.origin.to_array().map(f64::to_bits)
            && same_skirts
            && self.index_count == other.index_count
            && self.indices == other.indices
            && same_vertices
    }

    /// The bytes it holds.
    pub(crate) fn heap_bytes(&self) -> usize {
        use std::mem::size_of_val;
        self.skirts.as_ref().map_or(0, |s| {
            s.walks
                .iter()
                .map(|w| size_of_val(w.as_slice()))
                .sum::<usize>()
                + size_of_val(s.heights.as_slice())
                + s.places.as_deref().map_or(0, size_of_val)
        }) + size_of_val(self.indices.as_slice())
            + match &self.vertices {
                Vertices::Grid {
                    places,
                    heights,
                    exceptions,
                    ..
                } => {
                    size_of_val(places.as_slice())
                        + size_of_val(heights.as_slice())
                        + size_of_val(exceptions.as_slice())
                }
                Vertices::AsCame { positions, uvs } => {
                    size_of_val(positions.as_slice()) + uvs.as_deref().map_or(0, size_of_val)
                }
            }
    }

    /// How many vertices are kept as they came, of how many.
    #[cfg(test)]
    fn exceptions(&self) -> (usize, usize) {
        match &self.vertices {
            Vertices::Grid {
                places, exceptions, ..
            } => (exceptions.len(), places.len()),
            Vertices::AsCame { positions, .. } => (positions.len(), positions.len()),
        }
    }
}

impl KeptSkirts {
    /// The skirts kept, as `tuile_terrain` hangs them: each foot's place
    /// from the surface `vertices` keep, unless kept here.
    fn skirts(&self, vertices: &Vertices) -> Skirts {
        let mut heights = self.heights.iter();
        let mut places = self.places.iter().flatten();
        let sides = std::array::from_fn(|k| {
            self.walks[k]
                .iter()
                .map(|&vertex| {
                    let [u, v] = match (places.next(), vertices) {
                        (Some(place), _) => *place,
                        (None, Vertices::Grid { places, .. }) => {
                            let p = places[vertex as usize];
                            [f64::from(p[0]) / GRID, f64::from(p[1]) / GRID]
                        }
                        // Never: a surface kept as it came keeps its places here.
                        (None, Vertices::AsCame { .. }) => [0.0, 0.0],
                    };
                    Foot {
                        vertex,
                        u,
                        v,
                        height: heights.next().copied().unwrap_or_default(),
                    }
                })
                .collect()
        });
        Skirts {
            rect: self.rect,
            depth: self.depth,
            sides,
        }
    }

    fn same_as(&self, other: &Self) -> bool {
        let f64s = |a: &[f64], b: &[f64]| {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.to_bits() == b.to_bits())
        };
        let rect = |r: &GeoRect| [r.west, r.south, r.east, r.north].map(f64::to_bits);
        rect(&self.rect) == rect(&other.rect)
            && self.depth.to_bits() == other.depth.to_bits()
            && self.walks == other.walks
            && f64s(&self.heights, &other.heights)
            && match (&self.places, &other.places) {
                (None, None) => true,
                (Some(a), Some(b)) => f64s(a.as_flattened(), b.as_flattened()),
                _ => false,
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_terrain::{to_ground, Header, QuantizedMesh};

    /// A terrain tile as a source states it: a relief on a grid of `steps`
    /// spacings a side, its heights quantized.
    fn quantized(coord: TileCoord, steps: usize) -> QuantizedMesh {
        let rect = GeographicTilingScheme::default().tile_rect(coord);
        let n = steps + 1;
        let q = |f: f64| (f * GRID).round() / GRID;
        let (mut u, mut v, mut height) = (Vec::new(), Vec::new(), Vec::new());
        for j in 0..n {
            for i in 0..n {
                let (fu, fv) = (i as f64 / steps as f64, j as f64 / steps as f64);
                u.push(q(fu));
                v.push(q(fv));
                height.push(q(0.5 + 0.5 * (7.0 * fu).sin() * (5.0 * fv).cos()));
            }
        }
        let mut indices = Vec::new();
        for j in 0..steps {
            for i in 0..steps {
                let a = (j * n + i) as u32;
                let (b, c, d) = (a + 1, a + n as u32, a + n as u32 + 1);
                indices.extend_from_slice(&[a, b, c, c, b, d]);
            }
        }
        let (lon, lat) = rect.center();
        let centre = geodetic_to_ecef(Geodetic {
            lon,
            lat,
            height: 400.0,
        });
        let line = |f: &dyn Fn(usize) -> usize| (0..n).map(|k| f(k) as u32).collect::<Vec<u32>>();
        QuantizedMesh {
            header: Header {
                center: centre.to_array(),
                min_height: -35.5,
                max_height: 1832.25,
                bounding_sphere_center: centre.to_array(),
                bounding_sphere_radius: 1.0e5,
                horizon_occlusion: [0.0; 3],
            },
            u,
            v,
            height,
            indices,
            normals: None,
            edges: [
                line(&|k| k * n),
                line(&|k| k),
                line(&|k| k * n + steps),
                line(&|k| (n - 1) * n + k),
            ],
            metadata_available: None,
            cut: 0,
        }
    }

    /// Every bit of a mesh's positions, texture coordinates and indices.
    fn bits(mesh: &DecodedMesh) -> Vec<u32> {
        let uvs = mesh.uvs.iter().flatten().flatten();
        let positions = mesh.positions.iter().flatten();
        positions
            .chain(uvs)
            .map(|f| f.to_bits())
            .chain([u32::MAX])
            .chain(mesh.indices.iter().copied())
            .collect()
    }

    /// The mesh a tile is kept as comes back in every bit, its surface
    /// from heights and its skirts as they came — from less than half the
    /// bytes (measured: 41 % on a 65 × 65 tile numbered row by row, which
    /// packs its indices worse than a source's first-use order).
    #[test]
    fn a_terrain_tile_comes_back_bit_for_bit_from_less_than_half_its_bytes() {
        for (level, x, y) in [(14, 16253, 12320), (9, 507, 385), (17, 130030, 98560)] {
            let coord = TileCoord::new(level, x, y);
            let content = to_ground(
                &quantized(coord, 64),
                &GeographicTilingScheme::default().tile_rect(coord),
            );
            let came = &content.meshes[0];
            let tile = TileId::from_terrain(level, x, y);
            let kept = Kept::of(tile, content.local_origin_ecef, came);
            assert_eq!(bits(&kept.mesh(tile)), bits(came), "level {level}");
            let (exceptions, of) = kept.exceptions();
            let skirts = 4 * 65;
            assert!(
                exceptions <= skirts + of / 100,
                "level {level}: {exceptions} vertices of {of} kept as they came"
            );
            let whole = tuile_film::stitching::heap_bytes(came);
            assert!(
                kept.heap_bytes() * 2 < whole,
                "level {level}: {} bytes of {whole}",
                kept.heap_bytes()
            );
        }
    }

    /// A tile whose skirts came apart from its surface is kept as its
    /// surface and the numbers its skirts hang from, and comes back whole —
    /// the skirts hung again — in every bit of the tile it would have come
    /// as, from fewer bytes than that tile kept whole. A surface off the
    /// grid keeps its feet's places, and comes back the same (no smaller:
    /// its walls were kept as they came, and so are their places).
    #[test]
    fn a_tile_kept_apart_comes_back_whole_bit_for_bit() {
        use tuile_terrain::to_ground_apart;
        for (level, x, y) in [(14, 16253, 12320), (9, 507, 385), (17, 130030, 98560)] {
            let coord = TileCoord::new(level, x, y);
            let rect = GeographicTilingScheme::default().tile_rect(coord);
            let tile = TileId::from_terrain(level, x, y);
            for off_the_grid in [false, true] {
                let mut mesh = quantized(coord, 64);
                if off_the_grid {
                    for u in &mut mesh.u {
                        *u = (*u * 0.75 + 0.1).min(1.0);
                    }
                }
                let whole = to_ground(&mesh, &rect);
                let (surface, skirts) = to_ground_apart(&mesh, &rect, false);
                let origin = surface.local_origin_ecef;
                let apart = Kept::of_apart(tile, origin, &surface.meshes[0], &skirts);
                assert_eq!(
                    bits(&apart.mesh(tile)),
                    bits(&whole.meshes[0]),
                    "level {level}, off the grid {off_the_grid}"
                );
                let kept_whole = Kept::of(tile, whole.local_origin_ecef, &whole.meshes[0]);
                if !off_the_grid {
                    assert!(
                        apart.heap_bytes() < kept_whole.heap_bytes(),
                        "level {level}: {} bytes apart, {} whole",
                        apart.heap_bytes(),
                        kept_whole.heap_bytes()
                    );
                    let (exceptions, of) = apart.exceptions();
                    assert!(
                        exceptions <= of / 100,
                        "level {level}: {exceptions} of {of}"
                    );
                }
                assert!(apart.same_as(&Kept::of_apart(tile, origin, &surface.meshes[0], &skirts)));
                assert!(!apart.same_as(&kept_whole));
            }
        }
    }

    /// A tile cut from an ancestor has places off the grid; a tile whose
    /// rectangle is not the one its id names has positions no height gives
    /// back. Either is kept as it came, and comes back the same.
    #[test]
    fn a_mesh_off_the_grid_comes_back_as_it_came() {
        let coord = TileCoord::new(14, 16253, 12320);
        let rect = GeographicTilingScheme::default().tile_rect(coord);
        let mut mesh = quantized(coord, 16);
        for u in &mut mesh.u {
            *u = (*u * 0.75 + 0.1).min(1.0);
        }
        let content = to_ground(&mesh, &rect);
        let tile = TileId::from_terrain(14, 16253, 12320);
        let kept = Kept::of(tile, content.local_origin_ecef, &content.meshes[0]);
        assert_eq!(bits(&kept.mesh(tile)), bits(&content.meshes[0]));

        let content = to_ground(&quantized(coord, 16), &rect);
        let elsewhere = TileId::from_terrain(14, 16254, 12320);
        let kept = Kept::of(elsewhere, content.local_origin_ecef, &content.meshes[0]);
        let (exceptions, of) = kept.exceptions();
        assert_eq!(exceptions, of, "no position comes back from elsewhere");
        assert_eq!(bits(&kept.mesh(elsewhere)), bits(&content.meshes[0]));
    }

    #[test]
    fn any_indices_come_back() {
        let indices = vec![0, 1, 2, 2, 1, 3, 70_000, 5, 4_000_000_000, 0, 3, 3];
        assert_eq!(
            unpack_indices(&pack_indices(&indices), indices.len()),
            indices
        );
        assert!(unpack_indices(&pack_indices(&[]), 0).is_empty());
    }
}
