// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Implicit tiling (3D Tiles 1.1): serde types.
//!
//! The types are part of the M1 surface so tilesets that use implicit
//! tiling parse without error; decoding the binary subtree availability
//! bitstreams is M2 (`docs/03-roadmap.md`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImplicitTilingJson {
    pub subdivision_scheme: SubdivisionScheme,
    pub subtree_levels: u32,
    pub available_levels: u32,
    pub subtrees: SubtreesJson,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubdivisionScheme {
    #[serde(rename = "QUADTREE")]
    Quadtree,
    #[serde(rename = "OCTREE")]
    Octree,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtreesJson {
    /// Template URI: `{level}/{x}/{y}` (quadtree) or `{level}/{x}/{y}/{z}`.
    pub uri: String,
}

/// Address of a tile within an implicit subdivision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImplicitCoord {
    pub level: u32,
    pub x: u64,
    pub y: u64,
    /// Only meaningful for octrees.
    pub z: u64,
}

/// Expands a template URI (`{level}/{x}/{y}[/{z}]`) for a coordinate.
pub fn expand_template(template: &str, c: ImplicitCoord) -> String {
    template
        .replace("{level}", &c.level.to_string())
        .replace("{x}", &c.x.to_string())
        .replace("{y}", &c.y.to_string())
        .replace("{z}", &c.z.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spec_shape() {
        let json = r#"{
          "subdivisionScheme": "QUADTREE",
          "subtreeLevels": 4,
          "availableLevels": 12,
          "subtrees": { "uri": "subtrees/{level}/{x}/{y}.subtree" }
        }"#;
        let it: ImplicitTilingJson = serde_json::from_str(json).expect("parse");
        assert_eq!(it.subdivision_scheme, SubdivisionScheme::Quadtree);
        assert_eq!(it.subtree_levels, 4);
    }

    #[test]
    fn template_expansion() {
        let c = ImplicitCoord {
            level: 3,
            x: 5,
            y: 7,
            z: 0,
        };
        assert_eq!(
            expand_template("content/{level}/{x}/{y}.glb", c),
            "content/3/5/7.glb"
        );
    }
}
