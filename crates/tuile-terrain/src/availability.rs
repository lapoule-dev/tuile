// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Growing tile availability.
//!
//! Cesium World Terrain advertises only a shallow slice of its quadtree in
//! `layer.json` (often just the root); the existence of deeper tiles is
//! delivered *per tile*, in each tile's `metadata` extension. So availability
//! is not static — it grows as tiles download.
//!
//! [`Availability`] is the shared, interior-mutable structure that captures
//! this: the [`TerrainTree`](crate::tree::TerrainTree) reads it to decide
//! children during traversal, and the loader writes the ranges it parses from
//! each downloaded tile. Both hold the same `Arc`, so refinement reaches the
//! finest LOD as the path streams in — mirroring `CesiumTerrainProvider`'s
//! `TileAvailability`.

use std::sync::RwLock;

use crate::layer::{AvailabilityRange, LayerJson};
use crate::tiling::TileCoord;

/// Available tile ranges per level, growing as tiles reveal their descendants.
#[derive(Debug, Default)]
pub struct Availability {
    /// `levels[z]` = the ranges of existing tiles known at level `z`.
    levels: RwLock<Vec<Vec<AvailabilityRange>>>,
}

impl Availability {
    /// Seeds from a parsed `layer.json`. Uses its static `available` table when
    /// present; when the layer is metadata-driven (only a shallow seed, or
    /// none), ensures level 0 at least holds the `root_tiles` so traversal can
    /// start and discover the rest.
    pub fn from_layer(layer: &LayerJson, root_tiles_x: u64, root_tiles_y: u64) -> Self {
        let mut levels = layer.available.clone();
        if levels.first().is_none_or(|l| l.is_empty()) {
            if levels.is_empty() {
                levels.push(Vec::new());
            }
            levels[0] = vec![AvailabilityRange {
                start_x: 0,
                start_y: 0,
                end_x: root_tiles_x.saturating_sub(1),
                end_y: root_tiles_y.saturating_sub(1),
            }];
        }
        Self {
            levels: RwLock::new(levels),
        }
    }

    /// Whether a tile is known to exist.
    pub fn is_available(&self, c: TileCoord) -> bool {
        let levels = self.levels.read().expect("availability");
        levels
            .get(c.level as usize)
            .is_some_and(|ranges| ranges.iter().any(|r| r.contains(c.x, c.y)))
    }

    /// Folds in a tile's `metadata` availability: `ranges[offset]` describes the
    /// existing tiles at level `base_level + offset + 1` (quantized-mesh
    /// `metadata` extension convention).
    pub fn add_descendant_ranges(&self, base_level: u32, ranges: &[Vec<AvailabilityRange>]) {
        let mut levels = self.levels.write().expect("availability");
        for (offset, at_level) in ranges.iter().enumerate() {
            let level = base_level as usize + offset + 1;
            if level >= levels.len() {
                levels.resize_with(level + 1, Vec::new);
            }
            levels[level].extend_from_slice(at_level);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer_with(available: &str, extra: &str) -> LayerJson {
        let json = format!(
            r#"{{ "format":"quantized-mesh-1.0","tiles":["{{z}}/{{x}}/{{y}}.terrain"],
                  "maxzoom":15 {extra}, "available":{available} }}"#
        );
        LayerJson::from_slice(json.as_bytes()).expect("layer")
    }

    #[test]
    fn metadata_layer_seeds_roots_then_grows() {
        // Pure metadata-availability: empty static table.
        let layer = layer_with("[]", r#", "metadataAvailability": 10"#);
        let a = Availability::from_layer(&layer, 2, 1);
        // Roots seeded.
        assert!(a.is_available(TileCoord::new(0, 0, 0)));
        assert!(a.is_available(TileCoord::new(0, 1, 0)));
        // Nothing deeper yet.
        assert!(!a.is_available(TileCoord::new(1, 0, 0)));

        // A level-0 tile reveals levels 1 and 2.
        a.add_descendant_ranges(
            0,
            &[
                vec![AvailabilityRange { start_x: 0, start_y: 0, end_x: 3, end_y: 1 }],
                vec![AvailabilityRange { start_x: 0, start_y: 0, end_x: 7, end_y: 3 }],
            ],
        );
        assert!(a.is_available(TileCoord::new(1, 3, 1)));
        assert!(a.is_available(TileCoord::new(2, 7, 3)));
        assert!(!a.is_available(TileCoord::new(2, 8, 3)));
    }

    #[test]
    fn static_table_is_used_as_is() {
        let layer = layer_with(
            r#"[[{"startX":0,"startY":0,"endX":1,"endY":0}]]"#,
            "",
        );
        let a = Availability::from_layer(&layer, 2, 1);
        assert!(a.is_available(TileCoord::new(0, 1, 0)));
        assert!(!a.is_available(TileCoord::new(1, 0, 0)));
    }
}
