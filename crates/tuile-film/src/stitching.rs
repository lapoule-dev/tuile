// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A film's tiles, stitched frame by frame.
//!
//! The rule is the engine's (`tuile_core::stitch`): from the tiles a frame
//! draws, which edge each one takes. A film's part is bookkeeping — a pack
//! is not re-baked and says nothing of neighbours, so each frame's selection
//! is planned as it is drawn, and a renderer is told which of its resident
//! tiles have a different neighbourhood than the frame before: the strips to
//! upload, and the mesh again when it gains or loses vertices on an edge.
//!
//! Free of any GPU, so a browser worker and a headless binary stitch alike.

use std::collections::HashMap;

use glam::DVec3;
use tuile_core::content::{DecodedMesh, MaterialDesc};
use tuile_core::source::TileId;
use tuile_core::stitch::{self, Edges, Stitch};

use crate::{Mesh, TileKey};

struct Held {
    at: (u32, u64, u64),
    source: u32,
    mesh: DecodedMesh,
    factor: [f32; 4],
    edges: Edges,
    /// What the renderer was last told.
    told: Stitch,
}

/// What a renderer does for one tile before it draws the frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Restitch {
    pub key: TileKey,
    /// The tile's mesh with the vertices its neighbours give it, when those
    /// changed: to upload in place of the one it holds.
    pub mesh: Option<Mesh>,
    /// `Stitch::packed`: the strips its vertex stages read.
    pub strips: Vec<[f32; 4]>,
    /// The furthest any vertex moves, in metres.
    pub reach: f32,
}

/// The tiles a film holds, and how each was last stitched.
pub struct Stitching {
    band: f32,
    held: HashMap<TileKey, Held>,
    /// The selection last planned: a frame that draws the same tiles has
    /// the same plan, and is not planned again.
    planned: Vec<TileKey>,
}

impl Default for Stitching {
    fn default() -> Self {
        Self::new(stitch::BAND)
    }
}

fn f32s<const N: usize>(bytes: &[u8]) -> Vec<[f32; N]> {
    bytes
        .chunks_exact(4 * N)
        .map(|v| {
            std::array::from_fn(|k| {
                f32::from_le_bytes([v[4 * k], v[4 * k + 1], v[4 * k + 2], v[4 * k + 3]])
            })
        })
        .collect()
}

/// A film's mesh as the engine's functions read one.
pub fn decoded(mesh: &Mesh) -> DecodedMesh {
    DecodedMesh {
        positions: f32s::<3>(&mesh.positions),
        normals: (!mesh.normals.is_empty()).then(|| f32s::<3>(&mesh.normals)),
        uvs: (!mesh.uvs.is_empty()).then(|| f32s::<2>(&mesh.uvs)),
        indices: mesh
            .indices
            .chunks_exact(4)
            .take(mesh.index_count as usize)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect(),
        material: MaterialDesc::default(),
    }
}

/// The other way: the bytes a renderer uploads.
pub fn encoded(mesh: &DecodedMesh, origin_ecef: [f64; 3], base_color_factor: [f32; 4]) -> Mesh {
    let le = |values: &mut dyn Iterator<Item = f32>| values.flat_map(f32::to_le_bytes).collect();
    Mesh {
        origin_ecef,
        positions: le(&mut mesh.positions.iter().flatten().copied()),
        normals: mesh
            .normals
            .as_ref()
            .map(|n| le(&mut n.iter().flatten().copied()))
            .unwrap_or_default(),
        uvs: mesh
            .uvs
            .as_ref()
            .map(|uv| le(&mut uv.iter().flatten().copied()))
            .unwrap_or_default(),
        indices: mesh.indices.iter().flat_map(|i| i.to_le_bytes()).collect(),
        index_count: mesh.indices.len() as u32,
        base_color_factor,
    }
}

impl Stitching {
    /// With corrections reaching `band` of a tile inward.
    pub fn new(band: f32) -> Self {
        Self {
            band,
            held: HashMap::new(),
            planned: Vec::new(),
        }
    }

    pub fn band(&self) -> f32 {
        self.band
    }

    /// A tile enters, with the mesh the renderer is handed. `source` is the
    /// level of the terrain its surface is from, when the pack says.
    pub fn enter(&mut self, key: TileKey, source: Option<u32>, mesh: &Mesh) {
        let at = TileId(key.id).terrain_coord();
        let decoded = decoded(mesh);
        let edges = match &decoded.uvs {
            Some(uvs) => {
                Edges::of_vertices(DVec3::from_array(mesh.origin_ecef), &decoded.positions, uvs)
            }
            None => Edges::default(),
        };
        self.held.insert(
            key,
            Held {
                at,
                source: source.unwrap_or(at.0),
                mesh: decoded,
                factor: mesh.base_color_factor,
                edges,
                told: Stitch::default(),
            },
        );
        // A tile that enters is one the last plan did not know.
        self.planned.clear();
    }

    pub fn leave(&mut self, key: &TileKey) {
        self.held.remove(key);
    }

    /// Whether [`Self::frame`] has anything to work out for `selection`.
    pub fn is_planned(&self, selection: &[TileKey]) -> bool {
        self.planned == selection
    }

    /// Plans the frame that draws `selection`, and says what changed since
    /// the renderer was last told — in the selection's order.
    pub fn frame(&mut self, selection: &[TileKey]) -> Vec<Restitch> {
        if self.is_planned(selection) {
            return Vec::new();
        }
        self.planned = selection.to_vec();
        let drawn: Vec<(TileKey, &Held)> = selection
            .iter()
            .filter_map(|key| Some((*key, self.held.get(key)?)))
            .collect();
        let tiles: Vec<stitch::Tile<'_>> = drawn
            .iter()
            .map(|(_, held)| stitch::Tile {
                level: held.at.0,
                x: held.at.1,
                y: held.at.2,
                source: held.source,
                edges: &held.edges,
            })
            .collect();
        let plans = stitch::plan(&tiles, (2, 1), self.band);
        let changed: Vec<(TileKey, Stitch)> = drawn
            .iter()
            .zip(plans)
            .filter(|((_, held), plan)| {
                // What is across a side is told for the record; a renderer
                // has work only when a strip or a vertex changes.
                held.told.sides != plan.sides
                    || held.told.inserts != plan.inserts
                    || held.told.laps != plan.laps
            })
            .map(|((key, _), plan)| (*key, plan))
            .collect();
        changed
            .into_iter()
            .filter_map(|(key, plan)| {
                let held = self.held.get_mut(&key)?;
                let mesh = (held.told.inserts != plan.inserts).then(|| {
                    encoded(
                        &stitch::split(&held.mesh, held.edges.origin, &plan.inserts),
                        held.edges.origin.to_array(),
                        held.factor,
                    )
                });
                let reach = plan
                    .sides
                    .iter()
                    .flatten()
                    .map(|k| glam::Vec3::from(k.delta).length())
                    .fold(0.0, f32::max);
                let strips = plan.packed();
                held.told = plan;
                Some(Restitch {
                    key,
                    mesh,
                    strips,
                    reach,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::geo::{geodetic_to_ecef, Geodetic};

    /// Tile `level/x/y` as four vertices over flat ground at `height`.
    fn flat(level: u32, x: u64, y: u64, height: f64) -> (TileKey, Mesh) {
        use std::f64::consts::PI;
        let size = PI / 2f64.powi(level as i32);
        let at = |u: f64, north: f64| {
            geodetic_to_ecef(Geodetic {
                lon: -PI + (x as f64 + u) * size,
                lat: -PI / 2.0 + (y as f64 + north) * size,
                height,
            })
        };
        let origin = at(0.5, 0.5);
        let corners = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)];
        let mesh = DecodedMesh {
            positions: corners
                .iter()
                .map(|(u, n)| (at(*u, *n) - origin).as_vec3().to_array())
                .collect(),
            normals: None,
            uvs: Some(
                corners
                    .iter()
                    .map(|(u, n)| [*u as f32, 1.0 - *n as f32])
                    .collect(),
            ),
            indices: vec![0, 1, 2, 2, 1, 3],
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

    #[test]
    fn a_renderer_is_told_only_what_changed() {
        // A level-14 tile, and east of it two level-15 tiles 5 m higher.
        let (coarse, coarse_mesh) = flat(14, 9000, 6200, 100.0);
        let (south, south_mesh) = flat(15, 18002, 12400, 105.0);
        let (north, north_mesh) = flat(15, 18002, 12401, 105.0);
        let mut stitching = Stitching::default();
        stitching.enter(coarse, None, &coarse_mesh);
        stitching.enter(south, None, &south_mesh);
        stitching.enter(north, None, &north_mesh);

        // Alone, the fine tiles have nothing to meet but each other, at
        // one height: no edge moves. The northern one, second by place,
        // laps over the southern by a hair and that is all.
        let told = stitching.frame(&[south, north]);
        assert_eq!(told.len(), 1);
        assert_eq!(
            (told[0].key, told[0].reach, told[0].strips[0]),
            (north, 0.0, [0.0; 4])
        );
        assert!(told[0].mesh.is_none());

        // With the coarse tile drawn, each fine one comes down to it — and
        // the coarse one gains the vertex where the two fine ones meet.
        let told = stitching.frame(&[coarse, south, north]);
        assert_eq!(told.len(), 3);
        let of = |key: TileKey| told.iter().find(|t| t.key == key).expect("told");
        // 5 m, and the few centimetres the coarse tile's straight edge is
        // under the curve of the Earth.
        assert!((of(south).reach - 5.0).abs() < 0.1, "{}", of(south).reach);
        assert!(of(south).mesh.is_none(), "it has the vertices it needs");
        let gained = of(coarse).mesh.as_ref().expect("a vertex more");
        assert_eq!(gained.positions.len(), 5 * 12);
        assert_eq!(gained.index_count, 9);

        // The same selection again: nothing is planned, nothing is told.
        assert!(stitching.is_planned(&[coarse, south, north]));
        assert!(stitching.frame(&[coarse, south, north]).is_empty());

        // The coarse tile no longer drawn: the fine ones go back.
        let told = stitching.frame(&[south, north]);
        assert_eq!(told.len(), 2);
        assert!(told
            .iter()
            .all(|t| t.reach == 0.0 && t.strips[0] == [0.0; 4]));
    }
}
