// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! tileset.json parsing (3D Tiles 1.0 and 1.1) and the flattened tile arena.
//!
//! The JSON layer (`*Json` types) mirrors the spec; the runtime layer is a
//! flat arena of [`Tile`]s indexed by [`TileId`] — no pointer graphs, no
//! `Rc`. External tilesets are grafted into the same arena at load time
//! (see [`Tileset::graft`]).

use crate::implicit::ImplicitTilingJson;
use crate::math::{BoundingVolume, Obb, Sphere};
use crate::source::{TileId, TileProperties, TileTree};
use glam::{DMat4, DVec3};
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum TilesetError {
    #[error("invalid tileset JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported tileset version {0:?} (supported: 0.0, 1.0, 1.1)")]
    UnsupportedVersion(String),
    #[error("tile has no usable bounding volume")]
    MissingBoundingVolume,
    #[error("invalid content uri {uri:?}: {source}")]
    InvalidUri {
        uri: String,
        source: url::ParseError,
    },
    #[error("tile {0:?} is not in this tileset")]
    UnknownTile(TileId),
}

// ---------------------------------------------------------------------------
// JSON layer (serde mirror of the spec)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TilesetJson {
    pub asset: AssetJson,
    pub geometric_error: f64,
    pub root: TileJson,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions_used: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions_required: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetJson {
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TileJson {
    pub bounding_volume: BoundingVolumeJson,
    /// Absent in some real-world tilesets: inherited from the parent then
    /// (reference-implementation behavior).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geometric_error: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refine: Option<RefineJson>,
    /// Column-major 4x4, like glTF.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transform: Option<[f64; 16]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<TileJson>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub implicit_tiling: Option<ImplicitTilingJson>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoundingVolumeJson {
    #[serde(rename = "box", skip_serializing_if = "Option::is_none")]
    pub obb: Option<[f64; 12]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<[f64; 6]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sphere: Option<[f64; 4]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentJson {
    /// 1.1 name. 1.0 used `url`; both are accepted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl ContentJson {
    pub fn uri(&self) -> Option<&str> {
        self.uri.as_deref().or(self.url.as_deref())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefineJson {
    #[serde(rename = "REPLACE", alias = "replace", alias = "Replace")]
    Replace,
    #[serde(rename = "ADD", alias = "add", alias = "Add")]
    Add,
}

// ---------------------------------------------------------------------------
// Runtime layer (arena)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refine {
    Replace,
    Add,
}

impl From<RefineJson> for Refine {
    fn from(r: RefineJson) -> Self {
        match r {
            RefineJson::Replace => Refine::Replace,
            RefineJson::Add => Refine::Add,
        }
    }
}

/// What a tile's `content.uri` points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    /// Binary payload (glb, b3dm, subtree, …) — fetch and decode.
    Binary,
    /// Another tileset.json — fetch and [`Tileset::graft`].
    ExternalTileset,
}

#[derive(Debug, Clone)]
pub struct ContentRef {
    pub url: Url,
    pub kind: ContentKind,
}

#[derive(Debug, Clone)]
pub struct Tile {
    /// Bounding volume in world (ECEF) coordinates, tile transform applied.
    pub bounding_volume: BoundingVolume,
    pub geometric_error: f64,
    pub refine: Refine,
    /// Composed root→tile transform (f64, column-major convention).
    pub world_transform: DMat4,
    pub content: Option<ContentRef>,
    pub children: Vec<TileId>,
    pub parent: Option<TileId>,
    pub depth: u32,
    /// Present when this tile is the root of an implicit subdivision
    /// (subtree decoding lands in M2; the types exist from M1).
    pub implicit: Option<ImplicitTilingJson>,
}

/// A parsed tileset: flat arena plus the root id.
#[derive(Debug, Clone)]
pub struct Tileset {
    tiles: Vec<Tile>,
    root: TileId,
    /// Root tileset geometric error (screen-space error of not rendering
    /// anything at all).
    pub geometric_error: f64,
}

impl Tileset {
    /// Parses raw tileset.json bytes. `base_url` is the URL of the
    /// tileset.json itself: content uris resolve against it.
    pub fn from_json_bytes(bytes: &[u8], base_url: &Url) -> Result<Self, TilesetError> {
        let json: TilesetJson = serde_json::from_slice(bytes)?;
        Self::from_json(json, base_url)
    }

    pub fn from_json(json: TilesetJson, base_url: &Url) -> Result<Self, TilesetError> {
        match json.asset.version.as_str() {
            "0.0" | "1.0" | "1.1" => {}
            other => return Err(TilesetError::UnsupportedVersion(other.to_owned())),
        }
        let mut tiles = Vec::new();
        // Root inherits nothing: default refine is REPLACE (spec requires it
        // on the root; be tolerant like the reference implementations).
        build_tile(
            &mut tiles,
            &json.root,
            None,
            DMat4::IDENTITY,
            Refine::Replace,
            json.geometric_error,
            0,
            base_url,
        )?;
        Ok(Self {
            tiles,
            root: TileId(0),
            geometric_error: json.geometric_error,
        })
    }

    pub fn root(&self) -> TileId {
        self.root
    }

    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    pub fn tile(&self, id: TileId) -> &Tile {
        &self.tiles[id.0 as usize]
    }

    pub fn tiles(&self) -> impl Iterator<Item = (TileId, &Tile)> {
        self.tiles
            .iter()
            .enumerate()
            .map(|(i, t)| (TileId(i as u64), t))
    }
}

/// A 3D Tiles tileset is a [`TileTree`]: arena lookups, the root as the
/// single root, content presence as `has_content`.
impl TileTree for Tileset {
    fn roots(&self) -> Vec<TileId> {
        vec![self.root]
    }

    fn children(&self, id: TileId) -> Vec<TileId> {
        self.tile(id).children.clone()
    }

    fn properties(&self, id: TileId) -> TileProperties {
        let t = self.tile(id);
        TileProperties {
            bounding_volume: t.bounding_volume,
            geometric_error: t.geometric_error,
            refine: t.refine,
            has_content: t.content.is_some(),
        }
    }

    /// The parent link the arena has carried all along.
    ///
    /// [`TileTree::parent`] defaults to `None`, and its own documentation names
    /// this case — "sources without cheap parent lookup (a 3D Tiles arena)
    /// simply don't pin ancestors". The premise was wrong: every node stores
    /// [`Tile::parent`], set when the tree is built and when an external
    /// tileset is grafted. The lookup is an index into a `Vec`.
    ///
    /// What answering it buys is not a nicety. The server pins the chain from
    /// each selected tile up to the root so a coarser tile can always stand in
    /// while a finer one streams; a tree that returns `None` gets no such pin,
    /// and its root is protected only on the passes that happen to be
    /// requesting it. Measured on a three-level fixture with room for three
    /// tiles: the root left the protected set on one pass in three, was
    /// evicted, was re-requested, and reloaded — twenty-one times before a
    /// counter stopped it. Because those loads resolve from disk inside the
    /// same poll, the server never returned `Pending` either, so the run
    /// stopped being a slow test and became a core at 100 % for ever.
    fn parent(&self, id: TileId) -> Option<TileId> {
        self.tile(id).parent
    }

    /// Read straight off the node — [`Tile::depth`], set when the tree is built
    /// and when an external tileset is grafted. The default walk would be
    /// correct and would climb the whole chain for a number already in hand.
    fn level(&self, id: TileId) -> u32 {
        self.tile(id).depth
    }
}

impl Tileset {
    /// Grafts an external tileset under `host`: the external root becomes a
    /// child of `host`, composed with its world transform, and `host` stops
    /// carrying content (it has been consumed).
    ///
    /// `base_url` is the URL of the external tileset.json (its own contents
    /// resolve against it, not against the parent tileset's URL).
    pub fn graft(
        &mut self,
        host: TileId,
        json: TilesetJson,
        base_url: &Url,
    ) -> Result<TileId, TilesetError> {
        let (host_world, host_refine, host_ge, host_depth) = {
            let h = self
                .tiles
                .get(host.0 as usize)
                .ok_or(TilesetError::UnknownTile(host))?;
            (h.world_transform, h.refine, h.geometric_error, h.depth)
        };
        let new_root = build_tile(
            &mut self.tiles,
            &json.root,
            Some(host),
            host_world,
            host_refine,
            host_ge,
            host_depth + 1,
            base_url,
        )?;
        let h = &mut self.tiles[host.0 as usize];
        h.content = None;
        h.children.push(new_root);
        Ok(new_root)
    }
}

/// Recursively appends `json` (and its children) to the arena.
#[allow(clippy::too_many_arguments)]
fn build_tile(
    tiles: &mut Vec<Tile>,
    json: &TileJson,
    parent: Option<TileId>,
    parent_world: DMat4,
    inherited_refine: Refine,
    inherited_ge: f64,
    depth: u32,
    base_url: &Url,
) -> Result<TileId, TilesetError> {
    let local = json
        .transform
        .map(|t| DMat4::from_cols_array(&t))
        .unwrap_or(DMat4::IDENTITY);
    let world = parent_world * local;

    let bounding_volume = resolve_bounding_volume(&json.bounding_volume, &world)?;
    let refine = json.refine.map(Refine::from).unwrap_or(inherited_refine);
    // geometricError is meters in tile-local space: a scaling transform
    // scales it (reference-implementation behavior). Missing values inherit
    // from the parent.
    let local_ge = json.geometric_error.unwrap_or(inherited_ge);
    let geometric_error = local_ge * max_axis_scale(&world);

    let content = match &json.content {
        Some(c) => match c.uri() {
            Some(uri) => {
                let url = base_url
                    .join(uri)
                    .map_err(|source| TilesetError::InvalidUri {
                        uri: uri.to_owned(),
                        source,
                    })?;
                let kind = if url.path().ends_with(".json") {
                    ContentKind::ExternalTileset
                } else {
                    ContentKind::Binary
                };
                Some(ContentRef { url, kind })
            }
            None => None,
        },
        None => None,
    };

    let id = TileId(tiles.len() as u64);
    tiles.push(Tile {
        bounding_volume,
        geometric_error,
        refine,
        world_transform: world,
        content,
        children: Vec::new(),
        parent,
        depth,
        implicit: json.implicit_tiling.clone(),
    });

    if let Some(children) = &json.children {
        let mut ids = Vec::with_capacity(children.len());
        for child in children {
            ids.push(build_tile(
                tiles,
                child,
                Some(id),
                world,
                refine,
                local_ge,
                depth + 1,
                base_url,
            )?);
        }
        tiles[id.0 as usize].children = ids;
    }
    Ok(id)
}

/// Largest axis scale factor of an affine transform (1.0 for rigid motions).
fn max_axis_scale(m: &DMat4) -> f64 {
    (0..3)
        .map(|i| m.col(i).truncate().length())
        .fold(0.0, f64::max)
}

/// `box` and `sphere` volumes live in the tile's frame and take the tile
/// transform; `region` volumes are always in EPSG:4978 and do not.
fn resolve_bounding_volume(
    bv: &BoundingVolumeJson,
    world: &DMat4,
) -> Result<BoundingVolume, TilesetError> {
    if let Some(b) = &bv.obb {
        return Ok(BoundingVolume::Obb(Obb::from_tiles_box(b)).transformed(world));
    }
    if let Some(s) = &bv.sphere {
        let sphere = BoundingVolume::Sphere(Sphere {
            center: DVec3::new(s[0], s[1], s[2]),
            radius: s[3],
        });
        return Ok(sphere.transformed(world));
    }
    if let Some(r) = &bv.region {
        return Ok(BoundingVolume::Obb(crate::geo::region_to_obb(r)));
    }
    Err(TilesetError::MissingBoundingVolume)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::dvec3;

    fn base() -> Url {
        Url::parse("file:///data/demo/tileset.json").expect("static url")
    }

    const MINI: &str = r#"{
      "asset": { "version": "1.1" },
      "geometricError": 64,
      "root": {
        "boundingVolume": { "box": [0,0,0, 10,0,0, 0,10,0, 0,0,10] },
        "geometricError": 32,
        "refine": "REPLACE",
        "transform": [1,0,0,0, 0,1,0,0, 0,0,1,0, 100,200,300,1],
        "content": { "uri": "root.glb" },
        "children": [
          {
            "boundingVolume": { "box": [0,0,0, 5,0,0, 0,5,0, 0,0,5] },
            "geometricError": 0,
            "content": { "uri": "child/a.glb" }
          },
          {
            "boundingVolume": { "sphere": [0,0,0, 5] },
            "geometricError": 0,
            "refine": "ADD"
          }
        ]
      }
    }"#;

    #[test]
    fn parses_and_flattens() {
        let ts = Tileset::from_json_bytes(MINI.as_bytes(), &base()).expect("parse");
        assert_eq!(ts.len(), 3);
        let root = ts.tile(ts.root());
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.depth, 0);
        assert_eq!(ts.geometric_error, 64.0);
    }

    #[test]
    fn refine_is_inherited_and_overridable() {
        let ts = Tileset::from_json_bytes(MINI.as_bytes(), &base()).expect("parse");
        let root = ts.tile(ts.root());
        let c0 = ts.tile(root.children[0]);
        let c1 = ts.tile(root.children[1]);
        assert_eq!(c0.refine, Refine::Replace, "inherited from root");
        assert_eq!(c1.refine, Refine::Add, "explicit override");
    }

    #[test]
    fn transform_composes_and_moves_bounding_volume() {
        let ts = Tileset::from_json_bytes(MINI.as_bytes(), &base()).expect("parse");
        let root = ts.tile(ts.root());
        assert_eq!(root.bounding_volume.center(), dvec3(100.0, 200.0, 300.0));
        // Children inherit the parent transform.
        let c0 = ts.tile(root.children[0]);
        assert_eq!(c0.bounding_volume.center(), dvec3(100.0, 200.0, 300.0));
        assert_eq!(
            c0.world_transform.col(3).truncate(),
            dvec3(100.0, 200.0, 300.0)
        );
    }

    #[test]
    fn content_uris_resolve_against_base() {
        let ts = Tileset::from_json_bytes(MINI.as_bytes(), &base()).expect("parse");
        let root = ts.tile(ts.root());
        let c0 = ts.tile(root.children[0]);
        let content = c0.content.as_ref().expect("content");
        assert_eq!(content.url.as_str(), "file:///data/demo/child/a.glb");
        assert_eq!(content.kind, ContentKind::Binary);
    }

    #[test]
    fn legacy_url_key_and_external_tileset_detection() {
        let json = r#"{
          "asset": { "version": "1.0" },
          "geometricError": 16,
          "root": {
            "boundingVolume": { "sphere": [0,0,0,1] },
            "geometricError": 8,
            "refine": "replace",
            "content": { "url": "sub/external.json" }
          }
        }"#;
        let ts = Tileset::from_json_bytes(json.as_bytes(), &base()).expect("parse");
        let c = ts.tile(ts.root()).content.as_ref().expect("content");
        assert_eq!(c.kind, ContentKind::ExternalTileset);
        assert_eq!(c.url.as_str(), "file:///data/demo/sub/external.json");
    }

    #[test]
    fn graft_attaches_external_root_under_host() {
        let mut ts = Tileset::from_json_bytes(MINI.as_bytes(), &base()).expect("parse");
        let host = ts.tile(ts.root()).children[0];
        let external: TilesetJson = serde_json::from_str(
            r#"{
              "asset": { "version": "1.1" },
              "geometricError": 4,
              "root": {
                "boundingVolume": { "sphere": [0,0,0, 2] },
                "geometricError": 1,
                "content": { "uri": "leaf.glb" }
              }
            }"#,
        )
        .expect("external json");
        let ext_base = Url::parse("file:///data/demo/sub/external.json").expect("url");
        let new_root = ts.graft(host, external, &ext_base).expect("graft");

        let host_tile = ts.tile(host);
        assert!(host_tile.content.is_none(), "host content consumed");
        assert_eq!(host_tile.children, vec![new_root]);

        let grafted = ts.tile(new_root);
        assert_eq!(grafted.parent, Some(host));
        assert_eq!(grafted.depth, host_tile.depth + 1);
        assert_eq!(grafted.refine, Refine::Replace, "inherited from host");
        // External content resolves against the external tileset's url.
        let c = grafted.content.as_ref().expect("content");
        assert_eq!(c.url.as_str(), "file:///data/demo/sub/leaf.glb");
        // World transform composes through the host.
        assert_eq!(grafted.bounding_volume.center(), dvec3(100.0, 200.0, 300.0));
    }

    #[test]
    fn rejects_unsupported_versions() {
        for (version, ok) in [("0.0", true), ("1.0", true), ("1.1", true), ("2.0", false)] {
            let json = format!(
                r#"{{ "asset": {{ "version": "{version}" }}, "geometricError": 1,
                     "root": {{ "boundingVolume": {{ "sphere": [0,0,0,1] }}, "geometricError": 0 }} }}"#
            );
            let r = Tileset::from_json_bytes(json.as_bytes(), &base());
            assert_eq!(r.is_ok(), ok, "version {version}");
            if !ok {
                assert!(matches!(r, Err(TilesetError::UnsupportedVersion(_))));
            }
        }
    }

    #[test]
    fn invalid_json_and_missing_bounding_volume_are_typed_errors() {
        assert!(matches!(
            Tileset::from_json_bytes(b"{ not json", &base()),
            Err(TilesetError::Json(_))
        ));
        let json = r#"{ "asset": { "version": "1.1" }, "geometricError": 1,
                       "root": { "boundingVolume": {}, "geometricError": 0 } }"#;
        assert!(matches!(
            Tileset::from_json_bytes(json.as_bytes(), &base()),
            Err(TilesetError::MissingBoundingVolume)
        ));
    }

    #[test]
    fn missing_geometric_error_inherits_from_parent() {
        let json = r#"{
          "asset": { "version": "1.1" }, "geometricError": 64,
          "root": {
            "boundingVolume": { "sphere": [0,0,0,10] }, "geometricError": 32,
            "children": [
              { "boundingVolume": { "sphere": [0,0,0,5] } }
            ]
          }
        }"#;
        let ts = Tileset::from_json_bytes(json.as_bytes(), &base()).expect("parse");
        let child = ts.tile(ts.root()).children[0];
        assert_eq!(ts.tile(child).geometric_error, 32.0);
    }

    #[test]
    fn scaling_transform_scales_geometric_error() {
        // Uniform ×3 scale on the root: geometric errors are meters in
        // tile-local space, so they scale with the transform.
        let json = r#"{
          "asset": { "version": "1.1" }, "geometricError": 64,
          "root": {
            "boundingVolume": { "sphere": [0,0,0,10] }, "geometricError": 32,
            "transform": [3,0,0,0, 0,3,0,0, 0,0,3,0, 0,0,0,1],
            "children": [
              { "boundingVolume": { "sphere": [0,0,0,5] }, "geometricError": 8 }
            ]
          }
        }"#;
        let ts = Tileset::from_json_bytes(json.as_bytes(), &base()).expect("parse");
        assert_eq!(ts.tile(ts.root()).geometric_error, 96.0);
        let child = ts.tile(ts.root()).children[0];
        assert_eq!(ts.tile(child).geometric_error, 24.0, "scale composes down");
        // The sphere radius scales too.
        match ts.tile(child).bounding_volume {
            BoundingVolume::Sphere(s) => assert_eq!(s.radius, 15.0),
            _ => unreachable!("sphere expected"),
        }
    }

    #[test]
    fn region_volumes_ignore_tile_transform() {
        // Same region with and without a translation transform: identical
        // world volume (regions are always EPSG:4978).
        let region = "[-0.01, 0.78, -0.007, 0.783, 0, 100]";
        let with_transform = format!(
            r#"{{ "asset": {{ "version": "1.1" }}, "geometricError": 1,
                 "root": {{ "boundingVolume": {{ "region": {region} }}, "geometricError": 0,
                            "transform": [1,0,0,0, 0,1,0,0, 0,0,1,0, 5000,0,0,1] }} }}"#
        );
        let without = format!(
            r#"{{ "asset": {{ "version": "1.1" }}, "geometricError": 1,
                 "root": {{ "boundingVolume": {{ "region": {region} }}, "geometricError": 0 }} }}"#
        );
        let a = Tileset::from_json_bytes(with_transform.as_bytes(), &base()).expect("a");
        let b = Tileset::from_json_bytes(without.as_bytes(), &base()).expect("b");
        assert_eq!(
            a.tile(a.root()).bounding_volume.center(),
            b.tile(b.root()).bounding_volume.center()
        );
    }

    #[test]
    fn content_uri_with_query_string_keeps_query_and_detects_kind() {
        let json = r#"{
          "asset": { "version": "1.1" }, "geometricError": 1,
          "root": { "boundingVolume": { "sphere": [0,0,0,1] }, "geometricError": 0,
                    "content": { "uri": "sub/ext.json?v=42" } }
        }"#;
        let ts = Tileset::from_json_bytes(json.as_bytes(), &base()).expect("parse");
        let c = ts.tile(ts.root()).content.as_ref().expect("content");
        assert_eq!(
            c.kind,
            ContentKind::ExternalTileset,
            "query must not fool kind detection"
        );
        assert_eq!(c.url.query(), Some("v=42"));
    }

    #[test]
    fn round_trips_through_serde() {
        let json: TilesetJson = serde_json::from_str(MINI).expect("parse");
        let text = serde_json::to_string(&json).expect("serialize");
        let again: TilesetJson = serde_json::from_str(&text).expect("reparse");
        assert_eq!(again.root.geometric_error, json.root.geometric_error);
        assert_eq!(
            again.root.children.as_ref().map(Vec::len),
            json.root.children.as_ref().map(Vec::len)
        );
    }
}
