// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! [`TerrainTree`] — the geographic quadtree of a quantized-mesh terrain set,
//! exposed as a [`tuile_core::source::TileTree`] so the core's SSE traversal
//! drives it exactly like a 3D Tiles tileset.
//!
//! Tiles are implicit: children are computed `(z+1, 2x|.., 2y|..)` and gated
//! by the `layer.json` availability. Bounding volumes use an estimated
//! height range (the real min/max is only known once a tile is decoded);
//! this is enough for SSE and frustum culling, and refines later.

use std::sync::Arc;

use crate::availability::Availability;
use crate::layer::LayerJson;
use crate::tiling::{level_geometric_error, GeographicTilingScheme, TileCoord};
use tuile_core::geo::{region_to_obb, WGS84_A};
use tuile_core::math::BoundingVolume;
use tuile_core::source::{TileId, TileProperties, TileTree};
use tuile_core::tileset::Refine;

/// A terrain quadtree as a [`TileTree`].
#[derive(Debug, Clone)]
pub struct TerrainTree {
    scheme: GeographicTilingScheme,
    layer: LayerJson,
    /// Tile existence, shared with the loader so the per-tile `metadata`
    /// availability it discovers feeds this traversal (reach the finest LOD).
    availability: Arc<Availability>,
    /// No availability info at all (legacy heightmap): assume present up to
    /// maxzoom rather than consult the (root-only) table.
    assume_full: bool,
    /// Estimated height range (meters) for bounding volumes, until a tile's
    /// real min/max is known.
    min_height: f64,
    max_height: f64,
}

impl TerrainTree {
    /// Builds a terrain tree from a parsed `layer.json`, with its own
    /// availability. For the streaming globe, prefer [`TerrainTree::with_availability`]
    /// so the tree and loader share one growing availability.
    pub fn new(layer: LayerJson) -> Self {
        let scheme = GeographicTilingScheme::default();
        let availability = Arc::new(Availability::from_layer(
            &layer,
            scheme.root_tiles_x,
            scheme.root_tiles_y,
        ));
        Self::with_availability(layer, availability)
    }

    /// Builds a terrain tree reading the given shared availability — the same
    /// `Arc` the loader writes its discovered ranges into.
    pub fn with_availability(layer: LayerJson, availability: Arc<Availability>) -> Self {
        let assume_full = layer.available.is_empty() && layer.metadata_availability.is_none();
        Self {
            scheme: GeographicTilingScheme::default(),
            assume_full,
            availability,
            layer,
            // Generous global bounds (Dead Sea shore ≈ -430 m, Everest ≈ 8849 m).
            min_height: -1000.0,
            max_height: 9000.0,
        }
    }

    /// Overrides the estimated height range used for bounding volumes.
    pub fn with_height_range(mut self, min: f64, max: f64) -> Self {
        self.min_height = min;
        self.max_height = max;
        self
    }

    pub fn scheme(&self) -> &GeographicTilingScheme {
        &self.scheme
    }

    pub fn layer(&self) -> &LayerJson {
        &self.layer
    }

    fn coord(id: TileId) -> TileCoord {
        let (level, x, y) = id.terrain_coord();
        TileCoord::new(level, x, y)
    }

    fn id(c: TileCoord) -> TileId {
        TileId::from_terrain(c.level, c.x, c.y)
    }

    /// Whether a tile exists: the shared availability (seeded from `layer.json`,
    /// grown from each tile's `metadata` extension), or — for a legacy layer
    /// with no availability info at all — assumed present up to maxzoom.
    fn available(&self, c: TileCoord) -> bool {
        if self.assume_full {
            c.level <= self.layer.maxzoom
        } else {
            self.availability.is_available(c)
        }
    }

    /// The shared availability — clone the `Arc` to give the loader the writer
    /// side (it folds in each tile's discovered `metadata` ranges).
    pub fn availability(&self) -> Arc<Availability> {
        Arc::clone(&self.availability)
    }
}

impl TileTree for TerrainTree {
    fn roots(&self) -> Vec<TileId> {
        let mut out = Vec::new();
        for x in 0..self.scheme.root_tiles_x {
            for y in 0..self.scheme.root_tiles_y {
                let c = TileCoord::new(0, x, y);
                if self.available(c) {
                    out.push(Self::id(c));
                }
            }
        }
        out
    }

    fn children(&self, id: TileId) -> Vec<TileId> {
        let c = Self::coord(id);
        if c.level >= self.layer.maxzoom {
            return Vec::new();
        }
        c.children()
            .into_iter()
            .filter(|child| self.available(*child))
            .map(Self::id)
            .collect()
    }

    fn parent(&self, id: TileId) -> Option<TileId> {
        let c = Self::coord(id);
        (c.level > 0).then(|| TileId::from_terrain(c.level - 1, c.x / 2, c.y / 2))
    }

    fn properties(&self, id: TileId) -> TileProperties {
        let c = Self::coord(id);
        let rect = self.scheme.tile_rect(c);
        let region = [
            rect.west,
            rect.south,
            rect.east,
            rect.north,
            self.min_height,
            self.max_height,
        ];
        TileProperties {
            bounding_volume: BoundingVolume::Obb(region_to_obb(&region)),
            geometric_error: level_geometric_error(c.level, WGS84_A, self.scheme.root_tiles_x),
            // Terrain children replace the parent.
            refine: Refine::Replace,
            has_content: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwt_layer() -> LayerJson {
        // Cesium-World-Terrain-like: 2 roots, maxzoom 4, full availability.
        LayerJson::from_slice(
            br#"{
              "format": "quantized-mesh-1.0", "scheme": "tms",
              "projection": "EPSG:4326", "tiles": ["{z}/{x}/{y}.terrain"],
              "maxzoom": 4,
              "available": [
                [{"startX":0,"startY":0,"endX":1,"endY":0}],
                [{"startX":0,"startY":0,"endX":3,"endY":1}],
                [{"startX":0,"startY":0,"endX":7,"endY":3}],
                [{"startX":0,"startY":0,"endX":15,"endY":7}],
                [{"startX":0,"startY":0,"endX":31,"endY":15}]
              ]
            }"#,
        )
        .expect("layer")
    }

    #[test]
    fn two_roots_at_level_zero() {
        let tree = TerrainTree::new(cwt_layer());
        let roots = tree.roots();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].terrain_coord(), (0, 0, 0));
        assert_eq!(roots[1].terrain_coord(), (0, 1, 0));
    }

    #[test]
    fn children_are_four_and_available() {
        let tree = TerrainTree::new(cwt_layer());
        let root = TileId::from_terrain(0, 0, 0);
        let kids = tree.children(root);
        assert_eq!(kids.len(), 4);
        for k in &kids {
            let (l, _, _) = k.terrain_coord();
            assert_eq!(l, 1);
        }
    }

    #[test]
    fn no_children_past_maxzoom() {
        let tree = TerrainTree::new(cwt_layer());
        let deep = TileId::from_terrain(4, 0, 0); // maxzoom
        assert!(tree.children(deep).is_empty());
    }

    #[test]
    fn unavailable_children_are_filtered() {
        // A layer where level 1 only has x in 0..1 available.
        let layer = LayerJson::from_slice(
            br#"{ "format":"quantized-mesh-1.0","tiles":["{z}/{x}/{y}.terrain"],"maxzoom":2,
                  "available":[
                    [{"startX":0,"startY":0,"endX":1,"endY":0}],
                    [{"startX":0,"startY":0,"endX":0,"endY":1}]
                  ]}"#,
        )
        .expect("layer");
        let tree = TerrainTree::new(layer);
        let kids = tree.children(TileId::from_terrain(0, 0, 0));
        // Of the 4 children (1,0,0),(1,1,0),(1,0,1),(1,1,1), only x=0 ones exist.
        assert_eq!(kids.len(), 2);
        for k in &kids {
            assert_eq!(k.terrain_coord().1, 0, "only x=0 available");
        }
    }

    #[test]
    fn geometric_error_decreases_with_depth_and_obb_contains_the_tile() {
        use tuile_core::geo::{geodetic_to_ecef, Geodetic};

        let tree = TerrainTree::new(cwt_layer());
        let p0 = tree.properties(TileId::from_terrain(0, 0, 0));
        let p1 = tree.properties(TileId::from_terrain(1, 0, 0));
        assert!(p0.geometric_error > p1.geometric_error);
        assert_eq!(p0.refine, Refine::Replace);
        assert!(p0.has_content);

        // The right invariant is *containment*, at every level: the tile's
        // surface points must sit inside its OBB (a coarse level-0 hemisphere
        // legitimately has its center deep inside the Earth — see geo tests).
        for id in [
            TileId::from_terrain(0, 0, 0),
            TileId::from_terrain(10, 540, 380),
        ] {
            let (level, x, y) = id.terrain_coord();
            let rect = tree.scheme().tile_rect(TileCoord::new(level, x, y));
            let props = tree.properties(id);
            for i in 0..=3 {
                for j in 0..=3 {
                    let g = Geodetic {
                        lon: rect.west + (rect.east - rect.west) * i as f64 / 3.0,
                        lat: rect.south + (rect.north - rect.south) * j as f64 / 3.0,
                        height: 0.0,
                    };
                    let d = props.bounding_volume.distance_to_point(geodetic_to_ecef(g));
                    assert!(d < 2.0, "level {level}: surface point outside OBB by {d} m");
                }
            }
        }
    }
}
