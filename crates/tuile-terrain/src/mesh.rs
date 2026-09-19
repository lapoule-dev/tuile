// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Turns a decoded [`QuantizedMesh`] + its tile rectangle into a
//! render-ready [`DecodedTileContent`] — the same type the glTF path
//! produces, so `tuile-wgpu` renders terrain with no special case.
//!
//! Vertices map from normalized (u, v, height) to ECEF via the WGS84
//! ellipsoid, then rebase onto a local origin (f64 → f32), the anti-jitter
//! protocol of `docs/01-architecture.md`. Skirts extrude the tile edges
//! downward to hide cracks between neighbouring LOD levels.

use crate::decode::QuantizedMesh;
use crate::tiling::GeoRect;
use glam::{DVec3, Mat4};
use tuile_core::content::{DecodedMesh, DecodedTileContent, MaterialDesc};
use tuile_core::geo::{geodetic_to_ecef, Geodetic, WGS84_A};

/// Converts a quantized-mesh tile to render-ready geometry.
///
/// `rect` is the tile's geographic rectangle (radians). `skirt_height`
/// (meters) extrudes the edges downward; pass `0.0` to disable skirts.
/// The rebasing origin is the tile's quantized-mesh center.
pub fn to_decoded(mesh: &QuantizedMesh, rect: &GeoRect, skirt_height: f64) -> DecodedTileContent {
    let origin = DVec3::from(mesh.header.center);
    let min_h = mesh.header.min_height as f64;
    let max_h = mesh.header.max_height as f64;

    // The offsets are zero for every surface vertex and non-zero only for a
    // skirt, which hangs below its edge vertex *and* a hair outside the tile.
    let to_ecef = |i: usize, dlon: f64, dlat: f64, dh: f64| -> DVec3 {
        let lon = lerp(rect.west, rect.east, mesh.u[i]) + dlon;
        let lat = lerp(rect.south, rect.north, mesh.v[i]) + dlat;
        let height = lerp(min_h, max_h, mesh.height[i]) + dh;
        geodetic_to_ecef(Geodetic { lon, lat, height })
    };
    let local = |p: DVec3| {
        let r = p - origin;
        [r.x as f32, r.y as f32, r.z as f32]
    };

    let vc = mesh.vertex_count();
    let mut positions: Vec<[f32; 3]> = (0..vc).map(|i| local(to_ecef(i, 0.0, 0.0, 0.0))).collect();
    let mut normals: Option<Vec<[f32; 3]>> = mesh.normals.clone();
    let mut indices = mesh.indices.clone();
    let mut uvs = surface_uvs(mesh);

    if skirt_height > 0.0 {
        append_skirts(
            mesh,
            rect,
            skirt_height,
            &to_ecef,
            &local,
            &mut Vertices {
                positions: &mut positions,
                normals: &mut normals,
                uvs: &mut uvs,
                indices: &mut indices,
            },
        );
    }

    DecodedTileContent {
        withheld_drape: None,
        meshes: vec![DecodedMesh {
            positions,
            normals,
            // The mesh states its own tile coordinates, so they are used rather
            // than recovered from the vertex positions. See `surface_uvs`.
            uvs: Some(uvs),
            indices,
            material: MaterialDesc::default(),
        }],
        textures: Vec::new(),
        // Terrain owns no texture; imagery is draped by whoever composes the
        // globe, and referenced rather than copied in here.
        imagery: Vec::new(),
        local_origin_ecef: origin,
        transform_local: Mat4::IDENTITY,
    }
}

/// Per-vertex position within the tile, which is what every imagery layer maps
/// out of.
///
/// Taken straight from the quantized mesh, which already states it. The
/// alternative — projecting each vertex back to a longitude and latitude and
/// measuring it against the tile's rectangle — is slower, and wrong at the
/// antimeridian: `ecef_to_geodetic` returns longitude in `(-180°, +180°]`, so on
/// the easternmost column of tiles a vertex just past the cut comes back as
/// -179.99° instead of +180.01°. Differencing that against a west edge of +179°
/// gives a large negative number, which clamps to the *opposite* side of the
/// texture. The result was a bright torn seam running pole to pole through the
/// Pacific, where every east-edge vertex sampled the west edge's texels.
///
/// The reference implementation does the same thing for the same reason: it
/// never recovers a vertex's tile coordinates from its position, because the
/// format already carries them and a branch cut lies between the two.
///
/// `v` is flipped: the format counts it northward, texture space counts it down.
/// Returned parallel to the base vertices; skirt vertices reuse their source
/// edge's value.
pub fn surface_uvs(mesh: &QuantizedMesh) -> Vec<[f32; 2]> {
    (0..mesh.vertex_count())
        .map(|i| [mesh.u[i] as f32, 1.0 - mesh.v[i] as f32])
        .collect()
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// The vertex arrays under construction, so that adding a vertex is one act
/// rather than four that have to be kept in step.
struct Vertices<'a> {
    positions: &'a mut Vec<[f32; 3]>,
    normals: &'a mut Option<Vec<[f32; 3]>>,
    uvs: &'a mut Vec<[f32; 2]>,
    indices: &'a mut Vec<u32>,
}

/// How far a skirt hangs below its edge, in metres.
///
/// Five times the tile's geometric error, which is the rule both reference
/// implementations reach independently (`QuantizedMeshLoader.cpp:384`,
/// `CesiumTerrainProvider.js:798`). A quadtree tile's geometric error is taken
/// as that of a 65×65 heightfield whose vertical error is a quarter of its
/// horizontal sample spacing, so the error per radian is `R / 256` and the
/// skirt scales with exactly the quantity it has to cover: how far two
/// neighbours can disagree about the height of the edge they share.
pub fn skirt_height(rect: &GeoRect) -> f64 {
    /// Samples along a tile edge, less one — a 65×65 heightfield.
    const SPACINGS: f64 = 64.0;
    /// Vertical error as a fraction of the horizontal sample spacing.
    const VERTICAL_FRACTION: f64 = 0.25;
    /// How many geometric errors of headroom the skirt is given.
    const ERRORS_OF_HEADROOM: f64 = 5.0;
    let geometric_error = rect.width() * WGS84_A * VERTICAL_FRACTION / SPACINGS;
    geometric_error * ERRORS_OF_HEADROOM
}

/// Which side of the tile a wall stands on, and everything that follows from it.
#[derive(Clone, Copy)]
enum Side {
    West,
    South,
    East,
    North,
}

impl Side {
    /// The order `mesh.edges` arrives in — see `decode::decode`.
    const IN_FILE_ORDER: [Side; 4] = [Side::West, Side::South, Side::East, Side::North];

    /// Where a vertex sits along this edge, increasing in the direction the
    /// wall must be walked.
    ///
    /// The direction is not a preference. With the wall wound as
    /// `[top0, top1, bot0]`, the face normal comes out as `along × down`, so
    /// walking west→north→east→south around the tile is what makes all four
    /// walls face *outward* and survive back-face culling identically. Walk one
    /// of them backwards and that wall alone turns its back to the crack it was
    /// built to fill.
    fn position_along(self, mesh: &QuantizedMesh, vertex: usize) -> f64 {
        match self {
            Side::West => mesh.v[vertex],
            Side::East => -mesh.v[vertex],
            Side::North => mesh.u[vertex],
            Side::South => -mesh.u[vertex],
        }
    }

    /// The outward nudge, in radians of longitude and latitude.
    ///
    /// A skirt dropped straight down lands in the plane its neighbour's skirt
    /// also lands in: two coplanar walls, z-fighting along every tile boundary.
    /// Both references splay the wall outward by a ten-thousandth of the tile so
    /// the two overlap instead of colliding.
    fn outward(self, rect: &GeoRect) -> (f64, f64) {
        const SPLAY: f64 = 1.0e-4;
        let (dlon, dlat) = (rect.width() * SPLAY, rect.height() * SPLAY);
        match self {
            Side::West => (-dlon, 0.0),
            Side::East => (dlon, 0.0),
            Side::South => (0.0, -dlat),
            Side::North => (0.0, dlat),
        }
    }
}

/// Extrudes each edge downward into a wall that hides the crack between this
/// tile and its neighbour.
///
/// The edge lists arrive in whatever order the server wrote them — the format
/// promises no ordering, and `decode` passes them through untouched. An earlier
/// version of this walked them as given and joined consecutive entries, which
/// stitches a wall out of a shuffled list: long triangles sweeping right across
/// the tile, read on screen as horizontal smears over every slope. The reference
/// implementation sorts first, for this reason, and so does this.
fn append_skirts(
    mesh: &QuantizedMesh,
    rect: &GeoRect,
    skirt_height: f64,
    to_ecef: &dyn Fn(usize, f64, f64, f64) -> DVec3,
    local: &dyn Fn(DVec3) -> [f32; 3],
    out: &mut Vertices<'_>,
) {
    for (side, edge) in Side::IN_FILE_ORDER.iter().zip(&mesh.edges) {
        if edge.len() < 2 {
            continue;
        }
        let mut walk: Vec<u32> = edge.clone();
        walk.sort_by(|&a, &b| {
            side.position_along(mesh, a as usize)
                .total_cmp(&side.position_along(mesh, b as usize))
        });

        let (dlon, dlat) = side.outward(rect);
        let base = out.positions.len() as u32;
        for &vi in &walk {
            out.positions
                .push(local(to_ecef(vi as usize, dlon, dlat, -skirt_height)));
            // The skirt vertex stands at its edge vertex's place in the tile, so
            // it takes that vertex's texture and its light with it. That is what
            // makes a wall that does show through invisible rather than a stripe
            // of some other colour.
            out.uvs.push(out.uvs[vi as usize]);
            if let Some(ns) = out.normals.as_mut() {
                ns.push(ns[vi as usize]);
            }
        }

        for seg in 0..walk.len() - 1 {
            let (top0, top1) = (walk[seg], walk[seg + 1]);
            let (bot0, bot1) = (base + seg as u32, base + seg as u32 + 1);
            out.indices
                .extend_from_slice(&[top0, top1, bot0, bot0, top1, bot1]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::Header;
    use crate::tiling::{GeographicTilingScheme, TileCoord};
    use tuile_core::geo::ecef_to_geodetic;

    fn quad_mesh(center: [f64; 3]) -> QuantizedMesh {
        QuantizedMesh {
            header: Header {
                center,
                min_height: 0.0,
                max_height: 1000.0,
                bounding_sphere_center: center,
                bounding_sphere_radius: 1.0e6,
                horizon_occlusion: [0.0; 3],
            },
            u: vec![0.0, 1.0, 0.0, 1.0],
            v: vec![0.0, 0.0, 1.0, 1.0],
            height: vec![0.0, 0.0, 0.0, 0.0],
            indices: vec![0, 1, 2, 2, 1, 3],
            normals: Some(vec![[0.0, 0.0, 1.0]; 4]),
            edges: [vec![0, 2], vec![0, 1], vec![1, 3], vec![2, 3]],
            metadata_available: None,
        }
    }

    /// The antimeridian seam, as a unit test.
    ///
    /// The easternmost column of tiles ends exactly at the branch cut of
    /// longitude. Recovering a vertex's tile coordinate from its position has to
    /// difference two longitudes across that cut, and gets a full turn wrong;
    /// reading the coordinate the mesh already states does not. This asserts
    /// both halves — that the mesh's own u spans the tile, and that the
    /// recovery would not have.
    #[test]
    fn the_easternmost_tile_is_not_torn_by_the_branch_cut() {
        use tuile_core::raster::{uvs_geographic, GeoRect as RasterRect};

        let scheme = GeographicTilingScheme::default();
        // Level 3: x runs 0..16, so x = 15 is the column that ends at +180°.
        let coord = TileCoord::new(3, 15, 4);
        let rect = scheme.tile_rect(coord);
        assert!(
            (rect.east - std::f64::consts::PI).abs() < 1e-9,
            "the fixture must sit against the cut, east is {}",
            rect.east
        );

        let (clon, clat) = rect.center();
        let centre = geodetic_to_ecef(Geodetic {
            lon: clon,
            lat: clat,
            height: 0.0,
        });
        let decoded = to_decoded(&quad_mesh([centre.x, centre.y, centre.z]), &rect, 0.0);
        let mesh = &decoded.meshes[0];
        let uvs = mesh.uvs.as_ref().expect("the mesh carries its own uv");

        // The fixture's four vertices sit at the tile's corners, so their u must
        // be 0 on the west edge and 1 on the east — the whole tile, once.
        let us: Vec<f32> = uvs.iter().map(|uv| uv[0]).collect();
        assert_eq!(us, vec![0.0, 1.0, 0.0, 1.0], "the tile is torn");

        // And the recovery this replaced collapses the east edge onto the west,
        // which is exactly the bright seam it drew down the Pacific.
        let recovered = uvs_geographic(
            &mesh.positions,
            decoded.local_origin_ecef,
            &RasterRect {
                west: rect.west,
                south: rect.south,
                east: rect.east,
                north: rect.north,
            },
        );
        let recovered_us: Vec<f32> = recovered.iter().map(|uv| uv[0]).collect();
        assert_ne!(
            recovered_us, us,
            "the fixture no longer reproduces the tear, so it no longer guards \
             against it"
        );
    }

    #[test]
    fn vertices_land_inside_the_tile_rectangle() {
        let scheme = GeographicTilingScheme::default();
        // A small tile over Europe (level 6: x in 0..128, y in 0..64).
        let coord = TileCoord::new(6, 67, 49);
        let rect = scheme.tile_rect(coord);
        assert!(rect.north <= std::f64::consts::FRAC_PI_2 + 1e-9);
        let (clon, clat) = rect.center();
        let center = geodetic_to_ecef(Geodetic {
            lon: clon,
            lat: clat,
            height: 0.0,
        });
        let mesh = quad_mesh([center.x, center.y, center.z]);

        let decoded = to_decoded(&mesh, &rect, 0.0);
        let m = &decoded.meshes[0];
        assert_eq!(m.positions.len(), 4);

        // Each vertex, un-rebased, must fall within the tile rectangle.
        for p in &m.positions {
            let world =
                decoded.local_origin_ecef + DVec3::new(p[0] as f64, p[1] as f64, p[2] as f64);
            let g = ecef_to_geodetic(world);
            assert!(
                g.lon >= rect.west - 1e-6 && g.lon <= rect.east + 1e-6,
                "lon {} not in [{}, {}]",
                g.lon,
                rect.west,
                rect.east
            );
            assert!(g.lat >= rect.south - 1e-6 && g.lat <= rect.north + 1e-6);
        }
    }

    #[test]
    fn rebasing_keeps_positions_small() {
        // Center at ECEF magnitude ~6.4e6; rebased positions must be tiny
        // (tile-local), proving the f64→f32 rebasing.
        let mesh = quad_mesh([6.378e6, 0.0, 0.0]);
        let rect = GeoRect {
            west: -0.01,
            south: -0.01,
            east: 0.01,
            north: 0.01,
        };
        let decoded = to_decoded(&mesh, &rect, 0.0);
        for p in &decoded.meshes[0].positions {
            assert!(
                p[0].abs() < 1.0e5 && p[1].abs() < 1.0e5 && p[2].abs() < 1.0e5,
                "rebased position too large: {p:?}"
            );
        }
    }

    #[test]
    fn skirts_add_geometry_below_the_surface() {
        let mesh = quad_mesh([6.378e6, 0.0, 0.0]);
        let rect = GeoRect {
            west: -0.01,
            south: -0.01,
            east: 0.01,
            north: 0.01,
        };
        let without = to_decoded(&mesh, &rect, 0.0);
        let with = to_decoded(&mesh, &rect, 500.0);
        assert!(with.meshes[0].positions.len() > without.meshes[0].positions.len());
        assert!(with.meshes[0].indices.len() > without.meshes[0].indices.len());
        // Skirt vertices are radially closer to the geocenter (extruded down).
        let origin = with.local_origin_ecef;
        let base_n = without.meshes[0].positions.len();
        let surface = origin
            + DVec3::new(
                without.meshes[0].positions[0][0] as f64,
                without.meshes[0].positions[0][1] as f64,
                without.meshes[0].positions[0][2] as f64,
            );
        let skirt = origin
            + DVec3::new(
                with.meshes[0].positions[base_n][0] as f64,
                with.meshes[0].positions[base_n][1] as f64,
                with.meshes[0].positions[base_n][2] as f64,
            );
        assert!(
            skirt.length() < surface.length(),
            "skirt should extrude downward"
        );
    }

    /// A tile whose every edge holds three vertices, listed **out of order** —
    /// which is what the wire actually delivers, the format promising nothing
    /// about the order of its edge lists.
    fn shuffled_grid(center: [f64; 3]) -> QuantizedMesh {
        QuantizedMesh {
            header: Header {
                center,
                min_height: 0.0,
                max_height: 0.0,
                bounding_sphere_center: center,
                bounding_sphere_radius: 1.0e6,
                horizon_occlusion: [0.0; 3],
            },
            //   6 7 8   v = 1
            //   3 4 5   v = 0.5
            //   0 1 2   v = 0
            u: vec![0.0, 0.5, 1.0, 0.0, 0.5, 1.0, 0.0, 0.5, 1.0],
            v: vec![0.0, 0.0, 0.0, 0.5, 0.5, 0.5, 1.0, 1.0, 1.0],
            height: vec![0.0; 9],
            indices: vec![
                0, 1, 3, 3, 1, 4, 1, 2, 4, 4, 2, 5, 3, 4, 6, 6, 4, 7, 4, 5, 7, 7, 5, 8,
            ],
            normals: None,
            // Each list holds the right vertices in the wrong order.
            edges: [
                vec![3, 6, 0], // west
                vec![2, 0, 1], // south
                vec![8, 2, 5], // east
                vec![7, 8, 6], // north
            ],
            metadata_available: None,
        }
    }

    fn small_rect() -> GeoRect {
        GeoRect {
            west: 0.10,
            south: 0.20,
            east: 0.12,
            north: 0.22,
        }
    }

    fn at_centre(rect: &GeoRect) -> [f64; 3] {
        let (lon, lat) = rect.center();
        let c = geodetic_to_ecef(Geodetic {
            lon,
            lat,
            height: 0.0,
        });
        [c.x, c.y, c.z]
    }

    /// One quad of wall: the two tile vertices it hangs from, and its two
    /// triangles as ECEF corners.
    struct Segment {
        top0: u32,
        top1: u32,
        triangles: [[DVec3; 3]; 2],
    }

    /// Every wall segment a tile's skirts are made of.
    fn walls(mesh: &QuantizedMesh, rect: &GeoRect, height: f64) -> Vec<Segment> {
        let surface_indices = mesh.indices.len();
        let decoded = to_decoded(mesh, rect, height);
        let m = &decoded.meshes[0];
        let origin = decoded.local_origin_ecef;
        let at = |i: u32| {
            let p = m.positions[i as usize];
            origin + DVec3::new(p[0] as f64, p[1] as f64, p[2] as f64)
        };
        // A segment is `[top0, top1, bot0, bot0, top1, bot1]` — two triangles,
        // and only the first names both of the tile's own vertices.
        m.indices[surface_indices..]
            .chunks_exact(6)
            .map(|s| Segment {
                top0: s[0],
                top1: s[1],
                triangles: [
                    [at(s[0]), at(s[1]), at(s[2])],
                    [at(s[3]), at(s[4]), at(s[5])],
                ],
            })
            .collect()
    }

    /// **A wall joins neighbours along its edge, never across the tile.**
    ///
    /// The edge lists arrive shuffled. Stitching them as given connects whatever
    /// happens to be adjacent *in the list*, which draws long triangles right
    /// across the tile — on screen, horizontal smears over every slope, which is
    /// exactly what an earlier attempt at skirts produced and why they were
    /// removed. Sorting first is the whole difference.
    #[test]
    fn a_wall_only_ever_joins_vertices_adjacent_along_its_edge() {
        let rect = small_rect();
        let mesh = shuffled_grid(at_centre(&rect));
        let vertex_count = mesh.vertex_count() as u32;

        // The grid has three vertices per edge, so one step is half the tile.
        const STEP: f64 = 0.5;
        for seg in walls(&mesh, &rect, 500.0) {
            assert!(
                seg.top0 < vertex_count && seg.top1 < vertex_count,
                "a wall's upper edge must be the tile's own vertices"
            );
            let (a, b) = (seg.top0 as usize, seg.top1 as usize);
            let span = (mesh.u[a] - mesh.u[b])
                .abs()
                .max((mesh.v[a] - mesh.v[b]).abs());
            assert!(
                span <= STEP + 1.0e-9,
                "a wall segment spans {span} of the tile, joining vertex {a} to {b} — \
                 the edge list was stitched in the order it arrived, not sorted"
            );
        }
    }

    /// **Every wall faces outward, so back-face culling keeps all four.**
    ///
    /// The direction each edge is walked decides the winding. Walk one backwards
    /// and that wall alone is culled — invisible from outside, and useless
    /// against the crack it exists to fill.
    #[test]
    fn all_four_walls_face_away_from_the_tile() {
        let rect = small_rect();
        let centre = DVec3::from(at_centre(&rect));
        let mesh = shuffled_grid(at_centre(&rect));

        for seg in walls(&mesh, &rect, 500.0) {
            for [a, b, c] in seg.triangles {
                let normal = (b - a).cross(c - a).normalize();
                // Outward, with the vertical taken out: the wall hangs down, and
                // that says nothing about which way it faces.
                let centroid = (a + b + c) / 3.0;
                let up = centroid.normalize();
                let away = centroid - centre;
                let outward = (away - up * away.dot(up)).normalize();
                assert!(
                    normal.dot(outward) > 0.0,
                    "the wall joining {} to {} faces inward ({:.3})",
                    seg.top0,
                    seg.top1,
                    normal.dot(outward)
                );
            }
        }
    }

    /// **A skirt stands a hair outside its tile, not exactly on the boundary.**
    ///
    /// Dropped straight down, two neighbours' walls land in the very same plane
    /// and z-fight along every shared edge. Both references splay them outward
    /// by a ten-thousandth of the tile so they overlap instead.
    #[test]
    fn a_skirt_hangs_just_outside_the_tile_it_belongs_to() {
        let rect = small_rect();
        let mesh = shuffled_grid(at_centre(&rect));
        let surface = to_decoded(&mesh, &rect, 0.0).meshes[0].positions.len();
        let decoded = to_decoded(&mesh, &rect, 500.0);
        let m = &decoded.meshes[0];

        let mut outside = 0;
        for p in &m.positions[surface..] {
            let g = ecef_to_geodetic(
                decoded.local_origin_ecef + DVec3::new(p[0] as f64, p[1] as f64, p[2] as f64),
            );
            if g.lon < rect.west || g.lon > rect.east || g.lat < rect.south || g.lat > rect.north {
                outside += 1;
            }
        }
        assert_eq!(
            outside,
            m.positions.len() - surface,
            "every skirt vertex must sit outside the tile's own rectangle"
        );
    }

    /// **The skirt is as deep as the disagreement it has to cover.**
    ///
    /// Five geometric errors, the rule both references reach independently.
    #[test]
    fn skirt_depth_follows_the_level_it_covers() {
        let coarse = GeoRect {
            west: 0.0,
            south: 0.0,
            east: std::f64::consts::PI / 2.0,
            north: std::f64::consts::PI / 2.0,
        };
        let expected = coarse.width() * WGS84_A * 5.0 / 256.0;
        assert!((skirt_height(&coarse) - expected).abs() < 1.0e-9);

        // And a tile a quarter the width needs a quarter the skirt.
        let fine = GeoRect {
            east: coarse.width() / 4.0,
            ..coarse
        };
        assert!((skirt_height(&fine) - skirt_height(&coarse) / 4.0).abs() < 1.0e-9);
    }

    #[test]
    fn surface_uvs_flip_v_for_image_convention() {
        let mesh = quad_mesh([1.0, 0.0, 0.0]);
        let uvs = surface_uvs(&mesh);
        // v=0 (south) → texture v=1 (bottom), v=1 (north) → texture v=0 (top).
        assert_eq!(uvs[0], [0.0, 1.0]);
        assert_eq!(uvs[2], [0.0, 0.0]);
    }
}

/// How many vertices a fill mesh has along each edge.
///
/// A fill stands in for terrain nobody has yet, so it carries no detail worth
/// resolving — its whole job is to occupy the tile's ground with *one* surface
/// at approximately the right height. What the subdivision buys is not shape but
/// **curvature**: a single quad across a level-3 tile spans hundreds of
/// kilometres and cuts a visible chord through the planet. Five by five follows
/// the ellipsoid closely enough to hide that at any level, for 25 vertices and
/// 32 triangles.
const FILL_RESOLUTION: usize = 5;

/// A stand-in surface for a tile whose real geometry has not arrived.
///
/// # Why this exists rather than drawing the ancestor
///
/// The obvious fallback — draw the nearest resident ancestor wherever a tile is
/// missing — puts **two surfaces over the same ground**: the ancestor covers all
/// of its descendants, including the siblings that did arrive and are drawn at
/// full detail. Two different approximations of one hillside interpenetrate
/// within a few metres, and the depth test then picks a winner per pixel, which
/// is the shimmering the reference implementation avoids by doing exactly this
/// instead. A fill covers the missing tile's rectangle and nothing else, so
/// there is one surface per patch of ground, always.
///
/// # Heights
///
/// `corner_heights` are the four corners in `[sw, se, nw, ne]` order, each
/// optional. Missing ones are filled the way the reference does it: the mean of
/// the two adjacent corners, else whichever adjacent one exists, else the
/// opposite one, else `fallback` — which callers should give as the mid-height
/// of whatever bounding region they have. Interior vertices are bilinear
/// between the four.
///
/// The result is deliberately *approximate*. It is replaced the moment the real
/// tile lands, and being slightly wrong for a few frames is what it is for.
pub fn fill_content(
    rect: &GeoRect,
    corner_heights: [Option<f64>; 4],
    fallback: f64,
) -> DecodedTileContent {
    let [sw, se, nw, ne] = corner_heights;
    // Each corner asks its two neighbours first, then the diagonal, then the
    // caller's fallback — `TerrainFillMesh::fillMissingCorner` in the reference.
    let corner = |own: Option<f64>, adj1: Option<f64>, adj2: Option<f64>, opp: Option<f64>| {
        own.or(match (adj1, adj2) {
            (Some(a), Some(b)) => Some((a + b) * 0.5),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => opp,
        })
        .unwrap_or(fallback)
    };
    let h_sw = corner(sw, se, nw, ne);
    let h_se = corner(se, sw, ne, nw);
    let h_nw = corner(nw, sw, ne, se);
    let h_ne = corner(ne, se, nw, sw);

    let n = FILL_RESOLUTION;
    let last = (n - 1) as f64;
    let mut world: Vec<DVec3> = Vec::with_capacity(n * n);
    let mut uvs: Vec<[f32; 2]> = Vec::with_capacity(n * n);
    for row in 0..n {
        // `v` grows northward here and southward in the uv the imagery uses, as
        // everywhere else in this crate; `surface_uvs` states the same thing.
        let fy = row as f64 / last;
        let lat = rect.south + (rect.north - rect.south) * fy;
        for col in 0..n {
            let fx = col as f64 / last;
            let lon = rect.west + (rect.east - rect.west) * fx;
            let south = h_sw + (h_se - h_sw) * fx;
            let north = h_nw + (h_ne - h_nw) * fx;
            world.push(geodetic_to_ecef(Geodetic {
                lon,
                lat,
                height: south + (north - south) * fy,
            }));
            uvs.push([fx as f32, (1.0 - fy) as f32]);
        }
    }

    // Rebase on the patch's own centre, like every other tile: the positions
    // that reach the GPU are f32, and absolute ECEF in f32 resolves to about
    // half a metre at the Earth's radius.
    let origin = world.iter().copied().sum::<DVec3>() / (world.len() as f64);
    let positions: Vec<[f32; 3]> = world
        .iter()
        .map(|p| (*p - origin).as_vec3().into())
        .collect();
    // The outward normal of the ellipsoid, which for a surface this smooth is
    // indistinguishable from the true one and costs no cross products.
    let normals: Vec<[f32; 3]> = world
        .iter()
        .map(|p| p.normalize_or_zero().as_vec3().into())
        .collect();

    let mut indices = Vec::with_capacity((n - 1) * (n - 1) * 6);
    for row in 0..n - 1 {
        for col in 0..n - 1 {
            let i = (row * n + col) as u32;
            let right = i + 1;
            let up = i + n as u32;
            let up_right = up + 1;
            indices.extend_from_slice(&[i, right, up_right, i, up_right, up]);
        }
    }

    DecodedTileContent {
        withheld_drape: None,
        meshes: vec![DecodedMesh {
            positions,
            normals: Some(normals),
            uvs: Some(uvs),
            indices,
            material: MaterialDesc::default(),
        }],
        textures: Vec::new(),
        imagery: Vec::new(),
        local_origin_ecef: origin,
        transform_local: Mat4::IDENTITY,
    }
}

#[cfg(test)]
mod fill_tests {
    use super::*;

    fn tile_rect() -> GeoRect {
        GeoRect {
            west: 0.0,
            south: 0.0,
            east: 0.1,
            north: 0.1,
        }
    }

    /// A fill covers its rectangle and nothing beyond it. This is the whole
    /// reason it exists rather than an ancestor being drawn: one surface per
    /// patch of ground, so nothing interpenetrates anything.
    #[test]
    fn a_fill_stays_inside_its_own_rectangle() {
        let rect = tile_rect();
        let content = fill_content(&rect, [Some(0.0); 4], 0.0);
        let mesh = &content.meshes[0];
        for p in &mesh.positions {
            let world = content.local_origin_ecef + glam::DVec3::from(p.map(f64::from));
            let g = tuile_core::geo::ecef_to_geodetic(world);
            // Expressed as a distance, not as a bare epsilon. The comparison
            // survives a geodetic → ECEF → geodetic round trip, which loses a
            // few milliradians of the last bits; 1e-7 rad is about 60 cm on the
            // ground, far below a tile and far above the round trip's error
            // (measured at 1.3e-9 rad, some 8 mm).
            let slack = 1e-7;
            assert!(
                g.lon >= rect.west - slack && g.lon <= rect.east + slack,
                "longitude {} escaped [{}, {}]",
                g.lon,
                rect.west,
                rect.east
            );
            assert!(
                g.lat >= rect.south - slack && g.lat <= rect.north + slack,
                "latitude {} escaped [{}, {}]",
                g.lat,
                rect.south,
                rect.north
            );
        }
    }

    /// The corner cascade, which is where a fill gets its height and where it is
    /// easiest to be quietly wrong: two adjacent corners average, one adjacent
    /// stands alone, and the diagonal is the last resort before the caller's
    /// fallback.
    #[test]
    fn a_missing_corner_takes_its_neighbours_before_the_fallback() {
        let rect = tile_rect();
        let height_at = |content: &DecodedTileContent, index: usize| {
            let p = content.meshes[0].positions[index];
            let world = content.local_origin_ecef + glam::DVec3::from(p.map(f64::from));
            tuile_core::geo::ecef_to_geodetic(world).height
        };
        // [sw, se, nw, ne]; sw is missing and its two adjacent corners are se
        // and nw, so it must be their mean (150), never the fallback.
        let content = fill_content(&rect, [None, Some(100.0), Some(200.0), Some(0.0)], -9999.0);
        assert!(
            (height_at(&content, 0) - 150.0).abs() < 1.0,
            "the missing corner took {} instead of the mean of its neighbours",
            height_at(&content, 0)
        );

        // Nothing known at all: every corner falls back, and the fallback is
        // the only height in the patch.
        let bare = fill_content(&rect, [None; 4], 42.0);
        assert!((height_at(&bare, 0) - 42.0).abs() < 1.0);
    }
}

/// How far below the ellipsoid the backstop shell sits, in metres.
///
/// Below every land surface the terrain source serves, and above nothing that
/// matters. Real terrain is therefore always in front of it and always wins the
/// depth test, so the shell can never fight with the ground it is standing in
/// for — the failure that made an earlier stand-in worse than the hole it
/// filled.
///
/// Five hundred metres covers the Dead Sea at −430 m and Death Valley at −86 m.
/// The ocean floor is far deeper, and irrelevant: terrain sources render the
/// ocean *surface* at zero.
const SHELL_DEPTH: f64 = -500.0;

/// Quads around the equator. The shell is a backstop, not scenery — what it has
/// to do is follow the curve closely enough that its edge is never visible
/// against the real globe, and 180 × 90 puts a vertex every two degrees.
const SHELL_LON: usize = 180;
const SHELL_LAT: usize = 90;

/// A whole-planet shell, drawn under everything as the guarantee that ground is
/// never bare.
///
/// # Why a shell rather than better streaming
///
/// Because "never black" cannot be a property of a traversal. Every attempt to
/// guarantee it upstream — pin a level, stand in for a missing tile, refuse an
/// overlapping ancestor — guarantees it only for the cases that were thought
/// of, and the reports kept arriving from the cases that were not. A surface
/// that is *always* there needs no case analysis: whatever the traversal
/// selects, whatever arrives late, whatever is evicted, the ground behind it is
/// already drawn.
///
/// It costs one draw call and 32 400 triangles, once, for the whole session.
///
/// The colour is the caller's. A deliberately unnatural one says "this is not
/// data" at a glance; an earth tone hides the fault and flatters the picture,
/// which is the wrong trade while a bug is being chased.
pub fn globe_shell(colour: [f32; 4]) -> DecodedTileContent {
    let mut world: Vec<DVec3> = Vec::with_capacity((SHELL_LON + 1) * (SHELL_LAT + 1));
    let mut uvs: Vec<[f32; 2]> = Vec::with_capacity((SHELL_LON + 1) * (SHELL_LAT + 1));
    for row in 0..=SHELL_LAT {
        let v = row as f64 / SHELL_LAT as f64;
        let lat = -std::f64::consts::FRAC_PI_2 + v * std::f64::consts::PI;
        for col in 0..=SHELL_LON {
            let u = col as f64 / SHELL_LON as f64;
            let lon = -std::f64::consts::PI + u * std::f64::consts::TAU;
            world.push(geodetic_to_ecef(Geodetic {
                lon,
                lat,
                height: SHELL_DEPTH,
            }));
            uvs.push([u as f32, (1.0 - v) as f32]);
        }
    }

    // Rebased on the Earth's centre, which is the only origin a whole-planet
    // mesh can have. The f32 that reaches the GPU then resolves to about half a
    // metre — three orders of magnitude finer than the five hundred metres
    // separating this from the terrain above it, so the ordering is never in
    // doubt.
    let origin = DVec3::ZERO;
    let positions: Vec<[f32; 3]> = world.iter().map(|p| p.as_vec3().into()).collect();
    let normals: Vec<[f32; 3]> = world
        .iter()
        .map(|p| p.normalize_or_zero().as_vec3().into())
        .collect();

    let stride = SHELL_LON + 1;
    let mut indices = Vec::with_capacity(SHELL_LON * SHELL_LAT * 6);
    for row in 0..SHELL_LAT {
        for col in 0..SHELL_LON {
            let i = (row * stride + col) as u32;
            let right = i + 1;
            let up = i + stride as u32;
            let up_right = up + 1;
            indices.extend_from_slice(&[i, right, up_right, i, up_right, up]);
        }
    }

    DecodedTileContent {
        withheld_drape: None,
        meshes: vec![DecodedMesh {
            positions,
            normals: Some(normals),
            uvs: Some(uvs),
            indices,
            material: MaterialDesc {
                base_color_factor: colour,
                ..MaterialDesc::default()
            },
        }],
        textures: Vec::new(),
        imagery: Vec::new(),
        local_origin_ecef: origin,
        transform_local: Mat4::IDENTITY,
    }
}
