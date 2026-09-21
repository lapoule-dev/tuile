// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A serializable summary of the geometry the server produces — a JSON view of
//! "what got generated", independent of any renderer and of the driving mode.
//!
//! Build it incrementally from [`ServerMessage::Content`](crate::protocol::ServerMessage::Content)
//! as tiles stream in
//! (progressive), or from a [`crate::drive::BulkFrame`] in one shot (bulk):
//! either way [`GeometryReport::add`] folds one decoded tile into the report.
//! Per mesh it records vertex/triangle counts, the local bounding box, the
//! **UV range** (values outside `[0,1]` flag imagery-drape stretching) and the
//! count of **degenerate triangles** (zero area — e.g. collapsed skirts), the
//! two things that make a globe render look wrong.

use crate::content::DecodedTileContent;
use crate::source::TileId;
use glam::Vec3;
use serde::Serialize;

#[derive(Debug, Default, Serialize)]
pub struct GeometryReport {
    pub totals: Totals,
    pub tiles: Vec<TileReport>,
}

#[derive(Debug, Default, Serialize)]
pub struct Totals {
    pub tiles: usize,
    pub meshes: usize,
    pub vertices: usize,
    pub triangles: usize,
    pub degenerate_triangles: usize,
    pub textures: usize,
    pub approx_bytes: usize,
}

#[derive(Debug, Serialize)]
pub struct TileReport {
    /// Raw [`TileId`] payload (source-private; decode with the source's scheme).
    pub tile: u64,
    pub local_origin_ecef: [f64; 3],
    pub meshes: Vec<MeshReport>,
    pub textures: Vec<TexReport>,
}

#[derive(Debug, Serialize)]
pub struct MeshReport {
    pub vertices: usize,
    pub triangles: usize,
    /// Triangles with ~zero area (collapsed/degenerate) — skirt or seam smell.
    pub degenerate_triangles: usize,
    pub has_normals: bool,
    pub has_uvs: bool,
    /// Local-space (meters, relative to `local_origin_ecef`) bounds.
    pub bbox_min: [f32; 3],
    pub bbox_max: [f32; 3],
    /// UV bounds when present; outside `[0,1]` ⇒ the drape texture is being
    /// stretched/clamped over the tile.
    pub uv_min: Option<[f32; 2]>,
    pub uv_max: Option<[f32; 2]>,
    pub base_color_texture: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct TexReport {
    pub width: u32,
    pub height: u32,
}

impl GeometryReport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one decoded tile into the report. Call once per
    /// `ServerMessage::Content` (streaming) or per `BulkFrame` entry (bulk).
    pub fn add(&mut self, tile: TileId, content: &DecodedTileContent) {
        let mut meshes = Vec::with_capacity(content.meshes.len());
        for m in &content.meshes {
            let (bmin, bmax) = bounds(&m.positions);
            let (uv_min, uv_max) = m.uvs.as_deref().map(uv_bounds).unzip();
            let degenerate = degenerate_triangles(&m.positions, &m.indices);
            let triangles = m.indices.len() / 3;

            self.totals.vertices += m.positions.len();
            self.totals.triangles += triangles;
            self.totals.degenerate_triangles += degenerate;
            self.totals.meshes += 1;

            meshes.push(MeshReport {
                vertices: m.positions.len(),
                triangles,
                degenerate_triangles: degenerate,
                has_normals: m.normals.is_some(),
                has_uvs: m.uvs.is_some(),
                bbox_min: bmin,
                bbox_max: bmax,
                uv_min,
                uv_max,
                base_color_texture: m.material.base_color_texture,
            });
        }
        let textures = content
            .textures
            .iter()
            .map(|t| TexReport {
                width: t.width,
                height: t.height,
            })
            .collect();
        self.totals.textures += content.textures.len();
        self.totals.tiles += 1;
        self.totals.approx_bytes += content.byte_size();

        let o = content.local_origin_ecef;
        self.tiles.push(TileReport {
            tile: tile.0,
            local_origin_ecef: [o.x, o.y, o.z],
            meshes,
            textures,
        });
    }

    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).expect("GeometryReport serializes")
    }
}

fn bounds(positions: &[[f32; 3]]) -> ([f32; 3], [f32; 3]) {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for p in positions {
        for i in 0..3 {
            min[i] = min[i].min(p[i]);
            max[i] = max[i].max(p[i]);
        }
    }
    if positions.is_empty() {
        (([0.0; 3]), ([0.0; 3]))
    } else {
        (min, max)
    }
}

fn uv_bounds(uvs: &[[f32; 2]]) -> ([f32; 2], [f32; 2]) {
    let mut min = [f32::INFINITY; 2];
    let mut max = [f32::NEG_INFINITY; 2];
    for uv in uvs {
        for i in 0..2 {
            min[i] = min[i].min(uv[i]);
            max[i] = max[i].max(uv[i]);
        }
    }
    if uvs.is_empty() {
        ([0.0; 2], [0.0; 2])
    } else {
        (min, max)
    }
}

fn degenerate_triangles(positions: &[[f32; 3]], indices: &[u32]) -> usize {
    let mut n = 0;
    for tri in indices.as_chunks::<3>().0 {
        let (i0, i1, i2) = (tri[0] as usize, tri[1] as usize, tri[2] as usize);
        if i0 == i1 || i1 == i2 || i0 == i2 {
            n += 1;
            continue;
        }
        let (Some(a), Some(b), Some(c)) = (positions.get(i0), positions.get(i1), positions.get(i2))
        else {
            n += 1;
            continue;
        };
        let a = Vec3::from(*a);
        let area = (Vec3::from(*b) - a).cross(Vec3::from(*c) - a).length() * 0.5;
        if area < 1e-6 {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{DecodedMesh, DecodedTexture, MaterialDesc};
    use glam::{DVec3, Mat4};

    #[test]
    fn report_counts_geometry_and_flags_degenerate_skirts() {
        // One real triangle + one collapsed (degenerate) triangle.
        let mesh = DecodedMesh {
            positions: vec![
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [2.0, 2.0, 2.0],
            ],
            normals: None,
            uvs: Some(vec![[0.0, 0.0], [1.5, 0.0], [0.0, 1.0], [0.0, 0.0]]),
            indices: vec![0, 1, 2, /* collapsed: */ 3, 3, 3],
            material: MaterialDesc {
                base_color_texture: Some(0),
                ..Default::default()
            },
        };
        let content = DecodedTileContent {
            withheld_drape: None,
            meshes: vec![mesh],
            textures: vec![DecodedTexture {
                width: 256,
                height: 256,
                rgba8: vec![0; 256 * 256 * 4],
            }],
            imagery: Vec::new(),
            local_origin_ecef: DVec3::new(1.0, 2.0, 3.0),
            transform_local: Mat4::IDENTITY,
        };

        let mut report = GeometryReport::new();
        report.add(TileId(42), &content);

        assert_eq!(report.totals.tiles, 1);
        assert_eq!(report.totals.triangles, 2);
        assert_eq!(report.totals.degenerate_triangles, 1);
        let m = &report.tiles[0].meshes[0];
        assert_eq!(m.vertices, 4);
        // UV max.x = 1.5 ⇒ stretched past the tile, which the report surfaces.
        assert_eq!(m.uv_max.expect("uv present")[0], 1.5);
        assert!(!m.has_normals && m.has_uvs);
        assert!(report.to_json_pretty().contains("degenerate_triangles"));
    }
}
