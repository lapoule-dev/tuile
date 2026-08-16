// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Building a tile's mesh out of an ancestor's, for ground the source has no
//! data for.
//!
//! # Why a quadtree must be able to refine past its data
//!
//! A terrain source is a mosaic. Cesium World Terrain has level-14 data over
//! one valley and level-11 over the next, and the boundaries between them run as
//! straight lines through the quadtree. Treating those boundaries as the end of
//! the tree has two consequences, and both were reported as bugs before they
//! were understood as one:
//!
//! - **Holes.** Refinement is REPLACE. Handing back only the children that exist
//!   lets them take over and release their parent, leaving the ground with no
//!   child drawn by nothing at all — black strips with clean tile edges, which
//!   do not heal because every frame reaches the same decision.
//! - **A ceiling on imagery.** Draping a level-`z` tile with level-`L` imagery
//!   needs about `4^(L-z-1)` imagery tiles, so one tile can only ever carry
//!   imagery a couple of levels finer than itself. Bing reaches level 19 where
//!   the terrain stops at 12, and none of that detail could be shown, because
//!   the thing it would be drawn on never got any smaller. Raising the layer
//!   budget does not help: four times the layers buys exactly one level.
//!
//! Refining past the data fixes both, because it makes the tiles smaller. The
//! reference implementation does the same and for the same reason — its
//! `canRefine` never asks whether a particular child exists, only whether the
//! answer is *knowable*.
//!
//! # What this produces
//!
//! Not new detail — there is none to be had. The upsampled tile is the ancestor's
//! own surface, clipped to the child's quadrant and restated in the child's
//! coordinates. It has the same shape, described by fewer, larger triangles per
//! unit of ground, and that is exactly right: the mesh stops improving where the
//! data stops, while the imagery draped on it keeps sharpening.

use crate::decode::{Header, QuantizedMesh};
use crate::tiling::TileCoord;

/// One vertex during clipping, in the ancestor's coordinates and absolute
/// metres — heights are denormalised first because the child's own height range
/// is not known until the clipping is done.
#[derive(Clone, Copy)]
struct Vertex {
    u: f64,
    v: f64,
    height: f64,
    normal: Option<[f32; 3]>,
}

impl Vertex {
    /// Where a clipped edge crosses a boundary. `t` is along `self → other`.
    fn lerp(self, other: Self, t: f64) -> Self {
        let mix = |a: f64, b: f64| a + (b - a) * t;
        Self {
            u: mix(self.u, other.u),
            v: mix(self.v, other.v),
            height: mix(self.height, other.height),
            normal: match (self.normal, other.normal) {
                (Some(a), Some(b)) => {
                    let n = glam::Vec3::from(a).lerp(glam::Vec3::from(b), t as f32);
                    Some(n.normalize_or(glam::Vec3::from(a)).to_array())
                }
                _ => None,
            },
        }
    }
}

/// The quadrant of `ancestor` that `child` occupies, in `(u, v)`.
///
/// Both axes run the same way — TMS puts `y = 0` at the south edge and the mesh
/// format counts `v` northward — so neither is flipped. Returns `None` when the
/// two coordinates are not in an ancestor relationship, which is a caller's
/// mistake rather than a case to paper over.
fn quadrant(ancestor: TileCoord, child: TileCoord) -> Option<(f64, f64, f64)> {
    if child.level <= ancestor.level {
        return None;
    }
    let levels = child.level - ancestor.level;
    let span = 1u64 << levels;
    let (x0, y0) = (ancestor.x * span, ancestor.y * span);
    if child.x < x0 || child.x >= x0 + span || child.y < y0 || child.y >= y0 + span {
        return None;
    }
    let step = 1.0 / span as f64;
    Some((
        (child.x - x0) as f64 * step,
        (child.y - y0) as f64 * step,
        step,
    ))
}

/// Clips a convex polygon to one side of an axis-aligned line.
///
/// Sutherland–Hodgman, one half-plane at a time. `inside` says which side to
/// keep and `coordinate` reads the axis being cut, so the same routine serves
/// all four boundaries.
fn clip_to(
    polygon: &[Vertex],
    coordinate: impl Fn(&Vertex) -> f64,
    limit: f64,
    keep_above: bool,
) -> Vec<Vertex> {
    let inside = |vertex: &Vertex| {
        if keep_above {
            coordinate(vertex) >= limit
        } else {
            coordinate(vertex) <= limit
        }
    };
    let mut out = Vec::with_capacity(polygon.len() + 1);
    for (index, &current) in polygon.iter().enumerate() {
        let previous = polygon[(index + polygon.len() - 1) % polygon.len()];
        let (was_in, is_in) = (inside(&previous), inside(&current));
        if was_in != is_in {
            // The edge crosses the boundary: emit where it does.
            let (a, b) = (coordinate(&previous), coordinate(&current));
            let span = b - a;
            let t = if span.abs() < f64::EPSILON {
                0.0
            } else {
                (limit - a) / span
            };
            out.push(previous.lerp(current, t.clamp(0.0, 1.0)));
        }
        if is_in {
            out.push(current);
        }
    }
    out
}

/// Builds `child`'s mesh from `ancestor`'s.
///
/// Every triangle is clipped to the child's quadrant and the survivors are
/// restated in the child's own `(u, v)`, so the result is a mesh of exactly the
/// same surface over exactly the child's ground.
///
/// Returns `None` when `child` is not below `ancestor`, or when nothing of the
/// ancestor's surface falls inside the quadrant — the second cannot happen for a
/// well-formed tile and is worth a caller noticing rather than an empty mesh
/// that renders as a hole.
pub fn upsample(
    ancestor: &QuantizedMesh,
    ancestor_coord: TileCoord,
    child: TileCoord,
) -> Option<QuantizedMesh> {
    let (u0, v0, step) = quadrant(ancestor_coord, child)?;
    let (u1, v1) = (u0 + step, v0 + step);
    let (min_h, max_h) = (
        f64::from(ancestor.header.min_height),
        f64::from(ancestor.header.max_height),
    );
    let denormalise = |t: f64| min_h + (max_h - min_h) * t;

    let source = |i: usize| Vertex {
        u: ancestor.u[i],
        v: ancestor.v[i],
        height: denormalise(ancestor.height[i]),
        normal: ancestor.normals.as_ref().map(|n| n[i]),
    };

    let mut kept: Vec<Vertex> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    for triangle in ancestor.indices.chunks_exact(3) {
        let mut polygon = vec![
            source(triangle[0] as usize),
            source(triangle[1] as usize),
            source(triangle[2] as usize),
        ];
        polygon = clip_to(&polygon, |p| p.u, u0, true);
        if polygon.len() >= 3 {
            polygon = clip_to(&polygon, |p| p.u, u1, false);
        }
        if polygon.len() >= 3 {
            polygon = clip_to(&polygon, |p| p.v, v0, true);
        }
        if polygon.len() >= 3 {
            polygon = clip_to(&polygon, |p| p.v, v1, false);
        }
        if polygon.len() < 3 {
            continue;
        }
        // A convex polygon, so a fan from its first vertex triangulates it.
        let base = kept.len() as u32;
        kept.extend_from_slice(&polygon);
        for corner in 1..polygon.len() as u32 - 1 {
            indices.extend_from_slice(&[base, base + corner, base + corner + 1]);
        }
    }
    if indices.is_empty() {
        return None;
    }

    // The child's own height range, which is generally narrower than the
    // ancestor's — that narrowing is most of what makes an upsampled tile's
    // quantisation no worse than its parent's.
    let (mut low, mut high) = (f64::INFINITY, f64::NEG_INFINITY);
    for vertex in &kept {
        low = low.min(vertex.height);
        high = high.max(vertex.height);
    }
    let range = (high - low).max(f64::EPSILON);

    Some(QuantizedMesh {
        header: Header {
            // The rebasing origin only has to be *near* the geometry, and an
            // ancestor's centre is at worst a few tiles away — well inside what
            // f32 holds to the centimetre.
            center: ancestor.header.center,
            min_height: low as f32,
            max_height: high as f32,
            bounding_sphere_center: ancestor.header.bounding_sphere_center,
            bounding_sphere_radius: ancestor.header.bounding_sphere_radius,
            horizon_occlusion: ancestor.header.horizon_occlusion,
        },
        u: kept
            .iter()
            .map(|p| ((p.u - u0) / step).clamp(0.0, 1.0))
            .collect(),
        v: kept
            .iter()
            .map(|p| ((p.v - v0) / step).clamp(0.0, 1.0))
            .collect(),
        height: kept.iter().map(|p| (p.height - low) / range).collect(),
        indices,
        normals: kept.iter().map(|p| p.normal).collect::<Option<Vec<_>>>(),
        // Skirts are not drawn on this globe, and an upsampled tile shares its
        // edges exactly with the ancestor it came from anyway.
        edges: [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
        // Availability is the source's to state, and this tile is not from it.
        metadata_available: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tilted plane over the whole tile, finely enough tessellated that
    /// clipping has real work to do. Height rises with `u`, so where a vertex
    /// ends up is checkable against where it came from.
    fn sloping_tile(steps: usize) -> QuantizedMesh {
        let (mut u, mut v, mut height) = (Vec::new(), Vec::new(), Vec::new());
        for j in 0..=steps {
            for i in 0..=steps {
                u.push(i as f64 / steps as f64);
                v.push(j as f64 / steps as f64);
                // Normalised 0..1 over a 0..1000 m range, rising eastward.
                height.push(i as f64 / steps as f64);
            }
        }
        let row = (steps + 1) as u32;
        let mut indices = Vec::new();
        for j in 0..steps as u32 {
            for i in 0..steps as u32 {
                let a = j * row + i;
                indices.extend_from_slice(&[a, a + 1, a + row, a + 1, a + row + 1, a + row]);
            }
        }
        QuantizedMesh {
            header: Header {
                center: [6_378_137.0, 0.0, 0.0],
                min_height: 0.0,
                max_height: 1000.0,
                bounding_sphere_center: [6_378_137.0, 0.0, 0.0],
                bounding_sphere_radius: 1.0e6,
                horizon_occlusion: [0.0; 3],
            },
            u,
            v,
            height,
            indices,
            normals: None,
            edges: [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
            metadata_available: None,
        }
    }

    fn heights_of(mesh: &QuantizedMesh) -> Vec<f64> {
        let (lo, hi) = (
            f64::from(mesh.header.min_height),
            f64::from(mesh.header.max_height),
        );
        mesh.height.iter().map(|t| lo + (hi - lo) * t).collect()
    }

    /// The four children between them cover the parent exactly: each takes one
    /// quadrant, and each fills its own unit square.
    #[test]
    fn the_four_children_tile_the_parent_and_each_fills_its_own_square() {
        let parent = sloping_tile(8);
        let coord = TileCoord::new(5, 3, 7);
        for child in coord.children() {
            let up = upsample(&parent, coord, child).expect("a child of its own parent");
            let (u_lo, u_hi) = (
                up.u.iter().cloned().fold(f64::INFINITY, f64::min),
                up.u.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
            );
            let (v_lo, v_hi) = (
                up.v.iter().cloned().fold(f64::INFINITY, f64::min),
                up.v.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
            );
            assert!(u_lo < 1e-9 && u_hi > 1.0 - 1e-9, "u spans {u_lo}..{u_hi}");
            assert!(v_lo < 1e-9 && v_hi > 1.0 - 1e-9, "v spans {v_lo}..{v_hi}");
            assert!(!up.indices.is_empty());
            assert!(up.indices.iter().all(|&i| (i as usize) < up.u.len()));
        }
    }

    /// The surface itself must not move. The parent rises linearly eastward, so
    /// a child's height at a given `u` is known in closed form — which is the
    /// point of using a slope rather than a flat tile, where every mistake in
    /// the interpolation would cancel.
    #[test]
    fn the_surface_is_the_parents_surface_and_not_a_new_one() {
        let parent = sloping_tile(8);
        let coord = TileCoord::new(3, 1, 1);
        // The north-east child: u in 0.5..1, so heights 500..1000 m.
        let child = TileCoord::new(4, 3, 3);
        let up = upsample(&parent, coord, child).expect("upsampled");

        let heights = heights_of(&up);
        for (index, height) in heights.iter().enumerate() {
            // Child u maps back to parent u = 0.5 + child_u / 2.
            let expected = 1000.0 * (0.5 + up.u[index] * 0.5);
            assert!(
                (height - expected).abs() < 1.0,
                "vertex {index} at u {} is {height} m, the parent's surface is {expected} m",
                up.u[index]
            );
        }
        // And the child's own range is the half it actually covers, not the
        // parent's whole range — which is what keeps its quantisation as good.
        assert!((f64::from(up.header.min_height) - 500.0).abs() < 1.0);
        assert!((f64::from(up.header.max_height) - 1000.0).abs() < 1.0);
    }

    /// Refinement can go several levels past the data, which is the whole point:
    /// the imagery keeps sharpening long after the mesh has stopped.
    #[test]
    fn a_distant_descendant_still_gets_its_own_quadrant() {
        let parent = sloping_tile(16);
        let coord = TileCoord::new(2, 1, 1);
        // Four levels down, the far north-east corner: the ancestor's own
        // (1, 1) scaled by sixteen, plus the last step in each axis.
        let span = 16;
        let deep = TileCoord::new(6, span + span - 1, span + span - 1);
        let up = upsample(&parent, coord, deep).expect("a distant descendant");
        assert!(!up.indices.is_empty(), "nothing survived the clip");

        // That corner sits at parent u in 15/16..1, so heights 937.5..1000 m.
        let heights = heights_of(&up);
        let lowest = heights.iter().cloned().fold(f64::INFINITY, f64::min);
        assert!(
            lowest > 930.0,
            "the deep corner took ground from elsewhere: lowest {lowest} m"
        );
    }

    /// A parent with a ridge inside one of its quadrants, so that the quadrant's
    /// four corners say nothing about what is between them.
    ///
    /// The peak sits at `u = 0.25`, which is the middle of the western children
    /// and an interior point of them — the one place a plane through the corners
    /// cannot follow.
    fn ridged_tile(steps: usize) -> QuantizedMesh {
        let mut mesh = sloping_tile(steps);
        for i in 0..mesh.u.len() {
            let u = mesh.u[i];
            // A tent peaking at u = 0.25 and reaching zero at 0 and 0.5.
            mesh.height[i] = (1.0 - (4.0 * u - 1.0).abs()).max(0.0);
        }
        mesh
    }

    /// **A stand-in built this way follows the ground; a plane through the
    /// corners does not, and the gap is hundreds of metres.**
    ///
    /// This is the test the flat stand-in never had. `fill_content` builds a
    /// ruled surface from four corner heights, which is exact where the terrain
    /// is flat and wrong by the whole relief where it is not. Used as a stand-in
    /// it put a plane under ground that has ridges, and wherever an ancestor was
    /// still drawn beside it — which happens whenever the loader declines to
    /// build one — the real relief **punched through the plane**: large flat
    /// patches with ridges showing through in ragged outlines following the
    /// terrain rather than the tile grid. Measured on screen twice.
    ///
    /// So the property a stand-in mesh must have is not "cheap" or "smooth", it
    /// is *this*: the same surface as what it stands in for. The assertion below
    /// is the difference between the two constructions, in metres.
    #[test]
    fn the_upsampled_surface_departs_from_a_plane_through_its_corners() {
        let parent = ridged_tile(8);
        let coord = TileCoord::new(5, 3, 7);

        let worst = coord
            .children()
            .into_iter()
            .map(|child| {
                let up = upsample(&parent, coord, child).expect("a child of its own parent");
                let h = heights_of(&up);
                // The four corners, as `fill_content` samples them.
                let corner = |cu: f64, cv: f64| {
                    up.u
                        .iter()
                        .zip(&up.v)
                        .zip(&h)
                        .filter(|((u, v), _)| (**u - cu).abs() < 1e-9 && (**v - cv).abs() < 1e-9)
                        .map(|(_, h)| *h)
                        .next()
                        .unwrap_or(0.0)
                };
                let (sw, se) = (corner(0.0, 0.0), corner(1.0, 0.0));
                let (nw, ne) = (corner(0.0, 1.0), corner(1.0, 1.0));
                // The ruled surface between them — `fill_content`'s whole mesh.
                up.u
                    .iter()
                    .zip(&up.v)
                    .zip(&h)
                    .map(|((u, v), h)| {
                        let south = sw + (se - sw) * u;
                        let north = nw + (ne - nw) * u;
                        (h - (south + (north - south) * v)).abs()
                    })
                    .fold(0.0f64, f64::max)
            })
            .fold(0.0f64, f64::max);

        assert!(
            worst > 100.0,
            "the ridge inside the quadrant is only {worst:.1} m away from a \
             plane through its corners — the fixture has stopped exercising the \
             difference, and the test no longer says anything"
        );
    }

    /// Asking for a tile that is not below the one being upsampled is a caller's
    /// mistake, and answering with a plausible mesh would hide it.
    #[test]
    fn only_a_descendant_can_be_upsampled() {
        let parent = sloping_tile(4);
        let coord = TileCoord::new(3, 2, 2);
        assert!(upsample(&parent, coord, coord).is_none(), "itself");
        assert!(
            upsample(&parent, coord, TileCoord::new(2, 1, 1)).is_none(),
            "an ancestor"
        );
        assert!(
            upsample(&parent, coord, TileCoord::new(4, 0, 0)).is_none(),
            "a cousin"
        );
    }
}
