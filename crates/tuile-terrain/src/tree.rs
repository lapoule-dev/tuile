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

/// A backstop for a source that declares nothing, so a degenerate view cannot
/// divide without end. Never reached in practice: refinement stops at the data.
const ABSOLUTE_MAX_LEVEL: u32 = 22;

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
    /// How far past the data the quadtree may keep dividing.
    ///
    /// Refinement does not stop where the source runs out: a tile with no data
    /// is built from its nearest available ancestor's surface. See
    /// [`crate::upsample`] for why that has to be possible at all — briefly,
    /// because a mosaic's coverage boundaries would otherwise punch holes, and
    /// because imagery can be far sharper than terrain and has nothing small
    /// enough to be drawn on.
    ///
    /// So this is a bound on *division*, not on data. Screen-space error stops
    /// the traversal long before it in any real view — an upsampled tile's
    /// geometric error halves like any other — and this only keeps a degenerate
    /// view from dividing without end.
    max_level: u32,
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
        let max_level = ABSOLUTE_MAX_LEVEL;
        Self {
            scheme: GeographicTilingScheme::default(),
            assume_full,
            availability,
            layer,
            // Generous global bounds (Dead Sea shore ≈ -430 m, Everest ≈ 8849 m).
            min_height: -1000.0,
            max_height: 9000.0,
            max_level,
        }
    }

    /// How far the quadtree may divide past the data. See
    /// [`TerrainTree::max_level`].
    pub fn with_max_level(mut self, level: u32) -> Self {
        self.max_level = level;
        self
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
        // Not `layer.maxzoom`. That is where the source's *data* stops, and it
        // used to stop the division too — which is why the ground could never be
        // sharper than the terrain, however much finer the imagery went. Bing
        // reaches level 19 over ground where Cesium World Terrain stops at 12,
        // and none of it could be shown, because nothing small enough existed to
        // draw it on. Past the data a tile is built from its ancestor's surface;
        // see [`crate::upsample`].
        if c.level >= self.max_level {
            return Vec::new();
        }

        // Refine only while there is data to refine *into*, and then into all
        // four children whether they have data or not.
        //
        // Both halves matter and each was wrong on its own:
        //
        // - Handing back only the children with data punched black strips
        //   through the globe. Refinement is REPLACE, so those took over and
        //   released their parent, leaving the quadrants without a child drawn
        //   by nothing at all. A source of this kind is a mosaic whose coverage
        //   stops mid-tile — twenty-seven such tiles in one session over
        //   Kamchatka, one of them with a single available child, right where
        //   two boundaries crossed.
        //
        // - Refining regardless, on the theory that smaller tiles are what let
        //   imagery be sharper than terrain, divided to level twenty-two over
        //   ground whose data ended at twelve. That is sixty-five thousand times
        //   the tiles for a surface no more accurate, because an upsampled tile
        //   has its ancestor's shape and only its ancestor's shape. The server
        //   drowned and the view stopped following the camera.
        //
        // The reference implementation's rule is the one that holds: it refuses
        // to refine when all four children would be pure upsamples — "no point
        // in rendering the children because they're all upsampled" — and gets
        // sharp imagery on coarse terrain a different way entirely, by putting
        // *more imagery layers* on one tile and drawing it in several passes
        // when they outrun the texture units. Tile size is not the lever.
        if !c.children().iter().any(|child| self.available(*child)) {
            return Vec::new();
        }

        //
        // Filtering by availability was the earlier answer and it punched holes
        // through the globe. A source of this kind is a mosaic whose coverage
        // stops mid-tile: Cesium World Terrain has level-11 data over one
        // quadrant and nothing over the next, and the boundaries run as straight
        // lines through the quadtree. Twenty-seven such tiles were in one
        // session over Kamchatka, one of them with a single available child,
        // right where two boundaries crossed. Refinement is REPLACE, so the
        // children that existed took over and released their parent — leaving
        // the quadrants with no child drawn by nothing at all. Black strips with
        // clean tile edges, which never healed, because every frame reached the
        // same decision.
        //
        // The reference implementation reads the same availability and never
        // asks this question: its `canRefine` only checks that the answer is
        // *knowable*, and a child with no data is built from its parent's
        // surface instead. That is what [`crate::upsample`] is for, and it is
        // what makes this line safe.
        c.children().map(Self::id).to_vec()
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

    /// Availability that stops mid-tile, which a mosaic source really does.
    ///
    /// Level 2 reaches x = 2 but not x = 3, so the level-1 tile at x = 1 has two
    /// of its four children and the one at x = 0 has all four.
    fn layer_with_a_coverage_boundary() -> LayerJson {
        LayerJson::from_slice(
            br#"{
              "format": "quantized-mesh-1.0", "scheme": "tms",
              "projection": "EPSG:4326", "tiles": ["{z}/{x}/{y}.terrain"],
              "maxzoom": 2,
              "available": [
                [{"startX":0,"startY":0,"endX":1,"endY":0}],
                [{"startX":0,"startY":0,"endX":3,"endY":1}],
                [{"startX":0,"startY":0,"endX":2,"endY":3}]
              ]
            }"#,
        )
        .expect("layer")
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

    /// Refinement stops where the data does, and not one level further.
    ///
    /// Dividing past the data was tried and it is a trap: an upsampled tile has
    /// its ancestor's surface and only that, so every extra level multiplies the
    /// tile count by four for a shape no more accurate. At an SSE of 2 it
    /// reached level twenty-two over ground whose data ended at twelve. The
    /// reference implementation refuses at exactly this line — it will not
    /// refine a tile whose four children would all be upsamples.
    #[test]
    fn refinement_stops_where_the_data_stops() {
        let tree = TerrainTree::new(cwt_layer());

        // Inside the data: divides.
        assert_eq!(tree.children(TileId::from_terrain(3, 4, 2)).len(), 4);

        // The fixture's data ends at level 4, so a level-4 tile has no child
        // with anything in it, and dividing it would buy nothing.
        assert!(
            tree.children(TileId::from_terrain(4, 8, 4)).is_empty(),
            "nothing divides past the data"
        );
    }

    /// But a tile that straddles a coverage boundary still divides into **all
    /// four** children — the ones without data are built from its own surface.
    ///
    /// Handing back only the children that exist is what punched black strips
    /// through the globe: refinement is REPLACE, so they took over and released
    /// their parent, leaving the rest of its ground drawn by nothing.
    #[test]
    fn a_coverage_boundary_still_divides_into_four() {
        let tree = TerrainTree::new(layer_with_a_coverage_boundary());
        let straddling = TileId::from_terrain(1, 1, 0);

        let with_data = TileCoord::new(1, 1, 0)
            .children()
            .into_iter()
            .filter(|c| tree.available(*c))
            .count();
        assert_eq!(with_data, 2, "the fixture must straddle the boundary");
        assert_eq!(
            tree.children(straddling).len(),
            4,
            "a quadtree node has four children, whatever the coverage says"
        );
    }

    #[test]
    fn unavailable_children_are_still_children() {
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

        // Two of the four have data. All four are in the tree: the two without
        // are built from their parent's surface when they load.
        assert_eq!(tree.children(TileId::from_terrain(0, 0, 0)).len(), 4);
        let with_data = TileCoord::new(0, 0, 0)
            .children()
            .into_iter()
            .filter(|c| tree.available(*c))
            .count();
        assert_eq!(with_data, 2, "only the x = 0 children have data");
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
