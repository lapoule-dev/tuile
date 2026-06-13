// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! `layer.json` — the metadata document of a quantized-mesh terrain set
//! (the terrain equivalent of `tileset.json`). Describes the format, tiling
//! scheme, tile URL templates, available extensions, and which tiles exist
//! at each level (availability ranges).
//!
//! Modelled on cesium-native's `LayerSpec` / CesiumJS `CesiumTerrainProvider`
//! `layer.json` parsing.

use crate::tiling::TileCoord;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum LayerError {
    #[error("invalid layer.json: {0}")]
    Json(#[from] serde_json::Error),
}

/// A rectangle of available tile coordinates at one level
/// (`{startX, startY, endX, endY}`, inclusive, TMS convention).
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct AvailabilityRange {
    #[serde(rename = "startX")]
    pub start_x: u64,
    #[serde(rename = "startY")]
    pub start_y: u64,
    #[serde(rename = "endX")]
    pub end_x: u64,
    #[serde(rename = "endY")]
    pub end_y: u64,
}

impl AvailabilityRange {
    pub fn contains(&self, x: u64, y: u64) -> bool {
        x >= self.start_x && x <= self.end_x && y >= self.start_y && y <= self.end_y
    }
}

/// Parsed `layer.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct LayerJson {
    /// e.g. "quantized-mesh-1.0".
    pub format: String,
    /// "tms" (default) or "slippyMap".
    #[serde(default)]
    pub scheme: Option<String>,
    /// "EPSG:4326" (geographic) or "EPSG:3857".
    #[serde(default)]
    pub projection: Option<String>,
    /// URL templates, e.g. "{z}/{x}/{y}.terrain?v={version}".
    #[serde(default)]
    pub tiles: Vec<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub minzoom: u32,
    #[serde(default)]
    pub maxzoom: u32,
    /// Available extensions: "octvertexnormals", "watermask", "metadata".
    #[serde(default)]
    pub extensions: Vec<String>,
    /// `available[level]` = ranges of existing tiles at that level (TMS).
    #[serde(default)]
    pub available: Vec<Vec<AvailabilityRange>>,
    #[serde(default)]
    pub attribution: Option<String>,
}

impl LayerJson {
    pub fn from_slice(bytes: &[u8]) -> Result<Self, LayerError> {
        Ok(serde_json::from_slice(bytes)?)
    }

    pub fn is_quantized_mesh(&self) -> bool {
        self.format.starts_with("quantized-mesh")
    }

    pub fn has_extension(&self, name: &str) -> bool {
        self.extensions.iter().any(|e| e == name)
    }

    /// Extension query value for tile requests, e.g.
    /// "octvertexnormals-watermask" (only the extensions this layer offers).
    pub fn extensions_query(&self) -> Option<String> {
        let wanted: Vec<&str> = ["octvertexnormals", "watermask", "metadata"]
            .into_iter()
            .filter(|e| self.has_extension(e))
            .collect();
        (!wanted.is_empty()).then(|| wanted.join("-"))
    }

    /// Whether a tile exists, per the availability ranges.
    pub fn is_available(&self, c: TileCoord) -> bool {
        match self.available.get(c.level as usize) {
            Some(ranges) => ranges.iter().any(|r| r.contains(c.x, c.y)),
            None => false,
        }
    }

    /// Expands a tile URL template (`{z}/{x}/{y}`, `{version}`) for a tile.
    /// `y` is given TMS (south = 0); the template is filled as-is.
    pub fn tile_url(&self, c: TileCoord) -> Option<String> {
        let template = self.tiles.first()?;
        let version = self.version.as_deref().unwrap_or("");
        Some(
            template
                .replace("{z}", &c.level.to_string())
                .replace("{x}", &c.x.to_string())
                .replace("{y}", &c.y.to_string())
                .replace("{version}", version),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A trimmed Cesium World Terrain layer.json shape.
    const LAYER: &str = r#"{
      "tilejson": "2.1.0",
      "format": "quantized-mesh-1.0",
      "version": "1.2.0",
      "scheme": "tms",
      "projection": "EPSG:4326",
      "tiles": ["{z}/{x}/{y}.terrain?v={version}"],
      "minzoom": 0,
      "maxzoom": 19,
      "bounds": [-180, -90, 180, 90],
      "extensions": ["watermask", "metadata", "octvertexnormals"],
      "available": [
        [{ "startX": 0, "startY": 0, "endX": 1, "endY": 0 }],
        [{ "startX": 0, "startY": 0, "endX": 3, "endY": 1 }]
      ]
    }"#;

    #[test]
    fn parses_cwt_layer() {
        let l = LayerJson::from_slice(LAYER.as_bytes()).expect("parse");
        assert!(l.is_quantized_mesh());
        assert_eq!(l.scheme.as_deref(), Some("tms"));
        assert_eq!(l.maxzoom, 19);
        assert!(l.has_extension("octvertexnormals"));
    }

    #[test]
    fn extensions_query_keeps_known_only() {
        let l = LayerJson::from_slice(LAYER.as_bytes()).expect("parse");
        // Order is normals/watermask/metadata regardless of layer order.
        assert_eq!(
            l.extensions_query().as_deref(),
            Some("octvertexnormals-watermask-metadata")
        );
    }

    #[test]
    fn availability_gates_tiles() {
        let l = LayerJson::from_slice(LAYER.as_bytes()).expect("parse");
        assert!(l.is_available(TileCoord::new(0, 0, 0)));
        assert!(l.is_available(TileCoord::new(0, 1, 0)));
        assert!(!l.is_available(TileCoord::new(0, 2, 0)), "x=2 absent at level 0");
        assert!(l.is_available(TileCoord::new(1, 3, 1)));
        assert!(!l.is_available(TileCoord::new(5, 0, 0)), "level 5 has no ranges");
    }

    #[test]
    fn tile_url_fills_template() {
        let l = LayerJson::from_slice(LAYER.as_bytes()).expect("parse");
        assert_eq!(
            l.tile_url(TileCoord::new(9, 541, 386)).as_deref(),
            Some("9/541/386.terrain?v=1.2.0")
        );
    }
}
