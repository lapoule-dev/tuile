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
use tuile_core::geo::{geodetic_to_ecef, Geodetic};

/// Converts a quantized-mesh tile to render-ready geometry.
///
/// `rect` is the tile's geographic rectangle (radians). `skirt_height`
/// (meters) extrudes the edges downward; pass `0.0` to disable skirts.
/// The rebasing origin is the tile's quantized-mesh center.
pub fn to_decoded(mesh: &QuantizedMesh, rect: &GeoRect, skirt_height: f64) -> DecodedTileContent {
    let origin = DVec3::from(mesh.header.center);
    let min_h = mesh.header.min_height as f64;
    let max_h = mesh.header.max_height as f64;

    let to_ecef = |i: usize, height_offset: f64| -> DVec3 {
        let lon = lerp(rect.west, rect.east, mesh.u[i]);
        let lat = lerp(rect.south, rect.north, mesh.v[i]);
        let height = lerp(min_h, max_h, mesh.height[i]) + height_offset;
        geodetic_to_ecef(Geodetic { lon, lat, height })
    };
    let local = |p: DVec3| {
        let r = p - origin;
        [r.x as f32, r.y as f32, r.z as f32]
    };

    let vc = mesh.vertex_count();
    let mut positions: Vec<[f32; 3]> = (0..vc).map(|i| local(to_ecef(i, 0.0))).collect();
    let mut normals: Option<Vec<[f32; 3]>> = mesh.normals.clone();
    let mut indices = mesh.indices.clone();
    let mut uvs = surface_uvs(mesh);

    if skirt_height > 0.0 {
        append_skirts(
            mesh,
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

/// Extrudes each edge vertex downward by `skirt_height` and stitches a
/// vertical wall, mirroring cesium-native's `addSkirt`.
fn append_skirts(
    mesh: &QuantizedMesh,
    skirt_height: f64,
    to_ecef: &dyn Fn(usize, f64) -> DVec3,
    local: &dyn Fn(DVec3) -> [f32; 3],
    out: &mut Vertices<'_>,
) {
    for edge in &mesh.edges {
        if edge.len() < 2 {
            continue;
        }
        // New skirt vertices: the edge vertices pushed down.
        let base = out.positions.len() as u32;
        for &vi in edge {
            out.positions
                .push(local(to_ecef(vi as usize, -skirt_height)));
            // A skirt vertex hangs directly below its edge vertex, so it stands
            // at the same place in the tile and takes the same texture with it.
            out.uvs.push(out.uvs[vi as usize]);
            if let Some(ns) = out.normals.as_mut() {
                // Reuse the surface normal at the edge vertex.
                ns.push(ns[vi as usize]);
            }
        }
        // Two triangles per edge segment, forming the wall.
        for seg in 0..edge.len() - 1 {
            let top0 = edge[seg];
            let top1 = edge[seg + 1];
            let bot0 = base + seg as u32;
            let bot1 = base + seg as u32 + 1;
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

    #[test]
    fn surface_uvs_flip_v_for_image_convention() {
        let mesh = quad_mesh([1.0, 0.0, 0.0]);
        let uvs = surface_uvs(&mesh);
        // v=0 (south) → texture v=1 (bottom), v=1 (north) → texture v=0 (top).
        assert_eq!(uvs[0], [0.0, 1.0]);
        assert_eq!(uvs[2], [0.0, 0.0]);
    }
}
