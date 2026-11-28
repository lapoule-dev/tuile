// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! [`TerrainTree`] — the geographic quadtree of a quantized-mesh terrain set,
//! exposed as a [`tuile_core::source::TileTree`] so the core's SSE traversal
//! drives it exactly like a 3D Tiles tileset.
//!
//! Tiles are implicit: children are computed `(z+1, 2x|.., 2y|..)` and gated
//! by the `layer.json` availability. Bounding volumes span the height range
//! of the tile's own decoded header or, before it lands, its nearest decoded
//! ancestor's, widened by that ancestor's error ([`TerrainHeights`]); the
//! generous global range only before anything above it has landed.

use std::sync::Arc;

use crate::availability::Availability;
use crate::heights::TerrainHeights;
use crate::layer::LayerJson;
use crate::tiling::{level_geometric_error, GeographicTilingScheme, TileCoord};
use tuile_core::geo::{region_to_obb, WGS84_A};
use tuile_core::math::BoundingVolume;
use tuile_core::source::{TileId, TileProperties, TileTree};
use tuile_core::tileset::Refine;

/// How far past its data the quadtree keeps dividing.
///
/// Where division stops when nobody says otherwise.
///
/// A backstop, not a policy. The policy belongs to whoever composes the scene:
/// dividing past the terrain's data is only worth doing while the *imagery* can
/// still get sharper on the smaller tiles it produces, so the bound is the
/// imagery's own maximum level, and `tuile-planetary` sets it from the provider
/// it was handed. See [`TerrainTree::with_max_level`].
///
/// It has to be bounded by something, because screen-space error will not do it:
/// an upsampled tile's error keeps halving with the level while its surface,
/// being its parent's, gets no more accurate at all. Unbounded, an SSE of 2
/// reached level 22.
const ABSOLUTE_MAX_LEVEL: u32 = 22;

/// The least a tile not decoded yet is allowed above and below its
/// ancestor's decoded range, metres, besides twice the ancestor's error: a
/// floor for the fine levels, where the error is centimetres and a building
/// the terrain left out is not. See [`TerrainTree::height_range`].
const RELIEF_FLOOR_M: f64 = 20.0;

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
    /// The height range (metres) of a tile's bounding volume when neither it
    /// nor any tile above it has been decoded yet ([`Self::heights`]).
    min_height: f64,
    max_height: f64,
    /// The decoded tiles' height ranges, shared with the loader that records
    /// them: what bounds each tile's volume once it or an ancestor has
    /// landed. `None`: every tile spans the global range.
    heights: Option<Arc<TerrainHeights>>,
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
            heights: None,
            max_level,
        }
    }

    /// Bounds each tile's volume by the decoded height ranges in `heights`
    /// (the loader's [`TerrainHeights::record_range`]), the same `Arc` the
    /// loader writes. See `TerrainTree::height_range`.
    pub fn with_heights(mut self, heights: Arc<TerrainHeights>) -> Self {
        self.heights = Some(heights);
        self
    }

    /// The height range a tile's bounding volume spans, metres.
    ///
    /// **Why not the global range.** Every tile used to span -1000..9000 m,
    /// the Dead Sea to Everest. A level-21 tile is ten metres wide, so its
    /// box was a ten-kilometre column, and two things went wrong with it
    /// (measured 2026-11-28: 801 m over the Landes, 50 deg below the horizon,
    /// 784 of the 1078 tiles selected were nowhere near the field of view).
    /// Every eye under 9000 m is *inside* the column over the ground beneath
    /// it, so that tile is priced at distance zero and refined to the bottom
    /// of the tree; and a column behind the eye crosses each frustum plane
    /// on some side, so the plane test keeps it. The result was a disc of
    /// level-22 ground drawn under and behind the camera, where nothing is
    /// on screen.
    ///
    /// **What bounds it instead: its parent's decoded range.** A tile lies
    /// under the surface of its parent, which the parent's mesh approximates
    /// to its geometric error; the parent's header says that mesh's height
    /// range. So the range is the parent's, widened by twice the parent's
    /// error and [`RELIEF_FLOOR_M`] (a ridge the coarse mesh shaved off must
    /// not cull ground on screen: a culled tile draws nothing, and nothing
    /// stands in for it), never wider than the global range. The roots, and
    /// a tile whose parent has not been decoded, span the global range.
    ///
    /// **Why the parent's and never the tile's own**, though its own is
    /// exact: the traversal visits a tile only once its parent has been
    /// decoded ([`TileTree::can_refine`]), so the parent's range is there
    /// every time it is read, whatever else has landed by then. The tile's
    /// own may or may not be — a preload, the next frames of a bake loaded
    /// ahead — and a selection that read it when it happened to be there
    /// would depend on the order tiles landed in.
    fn height_range(&self, c: TileCoord) -> (f64, f64) {
        let parent = (c.level > 0).then(|| TileCoord::new(c.level - 1, c.x / 2, c.y / 2));
        let Some((p, (lo, hi))) = parent.and_then(|p| {
            self.heights
                .as_ref()
                .and_then(|h| h.range_of(p))
                .map(|r| (p, r))
        }) else {
            return (self.min_height, self.max_height);
        };
        let slack = 2.0 * level_geometric_error(p.level, WGS84_A, self.scheme.root_tiles_x)
            + RELIEF_FLOOR_M;
        (
            (f64::from(lo) - slack).max(self.min_height),
            (f64::from(hi) + slack).min(self.max_height),
        )
    }

    /// Tile `c` over the heights `low..high`.
    fn properties_over(&self, c: TileCoord, (low, high): (f64, f64)) -> TileProperties {
        let rect = self.scheme.tile_rect(c);
        let region = [rect.west, rect.south, rect.east, rect.north, low, high];
        TileProperties {
            bounding_volume: BoundingVolume::Obb(region_to_obb(&region)),
            geometric_error: level_geometric_error(c.level, WGS84_A, self.scheme.root_tiles_x),
            // Terrain children replace the parent.
            refine: Refine::Replace,
            has_content: true,
        }
    }

    /// How far the quadtree may divide past the data. See `TerrainTree`'s
    /// own `max_level`.
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
    /// Reveals are held in the shared availability until the gap between two
    /// passes.
    fn defer_edits(&self) {
        self.availability.defer();
        if let Some(h) = &self.heights {
            h.defer();
        }
    }

    fn apply_deferred_edits(&self) -> Vec<(TileId, String)> {
        self.availability.apply_held();
        if let Some(h) = &self.heights {
            h.apply_held();
        }
        Vec::new()
    }

    /// The quadtree's shape never changes: `children` is all four down to
    /// `max_level`. Neither it nor `properties` reads the availability, which
    /// only decides whether a tile has data of its own or is built from its
    /// ancestor's — so the reveals that grow it as tiles land leave this
    /// constant. The roots do read it, and the contract leaves those to the
    /// caller.
    ///
    /// `properties` does read the decoded height ranges (a tile's volume,
    /// `TerrainTree::height_range`), so the revision is their generation:
    /// it moves when a range is folded in, between two passes. Without
    /// heights, a constant.
    fn revision(&self) -> Option<u64> {
        Some(self.heights.as_ref().map_or(0, |h| h.generation()))
    }

    /// The planet, as the largest sphere that fits inside it.
    ///
    /// A quadtree over the whole ellipsoid is the one dataset where most of
    /// what a frustum contains is on the other side of the world. The semi-minor
    /// axis, not the semi-major: a sphere inscribed in the ellipsoid hides
    /// strictly less than the ellipsoid does, so nothing this culls could have
    /// been drawn. Terrain relief needs no allowance here — it is inside each
    /// tile's own bounding volume, and `Occluder::hides` accounts for the whole
    /// volume rather than its centre.
    fn occluder(&self) -> Option<tuile_core::math::Occluder> {
        Some(tuile_core::math::Occluder {
            center: glam::DVec3::ZERO,
            radius: tuile_core::geo::WGS84_B,
        })
    }

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

        // All four children, always. Availability decides whether a tile has
        // *data*, never whether it exists — and division carries on past the
        // data, bounded by `max_level`.
        //
        // Handing back only the children with data was the earlier answer and it
        // punched black strips through the globe: refinement is REPLACE, so
        // those took over and released their parent, leaving the quadrants
        // without a child drawn by nothing at all. A source of this kind is a
        // mosaic whose coverage stops mid-tile — twenty-seven such tiles in one
        // session over Kamchatka, one with a single available child, right where
        // two boundaries crossed.
        //
        // Stopping at the data was the other answer, and it costs the sharpest
        // imagery: the tiles stay large, and a large tile can only carry imagery
        // a couple of levels finer than itself however much finer the source
        // goes. The reference implementation stops there and recovers the detail
        // by binding *more layers per tile* and drawing it in several passes;
        // until that exists here, dividing is what gets the ground sharp.
        //

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

    /// The level is packed into the id here, so this is the one tree that may
    /// read it out of the handle — it owns the encoding.
    fn level(&self, id: TileId) -> u32 {
        Self::coord(id).level
    }

    /// Over the global range, whatever has been decoded: see
    /// [`TileTree::prior_properties`].
    fn prior_properties(&self, id: TileId) -> TileProperties {
        self.properties_over(Self::coord(id), (self.min_height, self.max_height))
    }

    /// Once its own range is decoded, with heights: its children's volumes
    /// are made of it (`TerrainTree::height_range`). Always, without.
    fn can_refine(&self, id: TileId) -> bool {
        self.heights
            .as_ref()
            .is_none_or(|h| h.range_of(Self::coord(id)).is_some())
    }

    fn properties(&self, id: TileId) -> TileProperties {
        let c = Self::coord(id);
        self.properties_over(c, self.height_range(c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The occluder has to survive the trip through `dyn TileTree`.
    ///
    /// Written after the cull silently did nothing on a real frame: the test
    /// that was missing is not "does the maths work" — that one existed and
    /// passed — but "does the traversal ever get told there is a planet".
    /// Through the tree, as the server holds it: deferred, a reveal waits for
    /// the tree's own apply.
    #[test]
    fn the_tree_defers_and_applies_its_availability() {
        let layer = LayerJson::from_slice(
            br#"{ "format": "quantized-mesh-1.0", "tiles": ["{z}/{x}/{y}.terrain"],
                  "maxzoom": 10, "metadataAvailability": 10, "available": [] }"#,
        )
        .expect("layer");
        let boxed: Box<dyn TileTree> = Box::new(TerrainTree::new(layer.clone()));
        let tree = TerrainTree::new(layer);
        let availability = tree.availability();
        let revealed = TileCoord::new(1, 3, 1);
        let reveal = || {
            availability.add_descendant_ranges(
                0,
                &[vec![crate::layer::AvailabilityRange {
                    start_x: 0,
                    start_y: 0,
                    end_x: 3,
                    end_y: 1,
                }]],
            );
        };
        tree.defer_edits();
        reveal();
        assert!(!availability.is_available(revealed), "held");
        assert!(tree.apply_deferred_edits().is_empty());
        assert!(availability.is_available(revealed), "applied");
        // And the trait object forwards to the same.
        boxed.defer_edits();
        assert!(boxed.apply_deferred_edits().is_empty());
    }

    /// The revision is a promise that `children` and `properties` have not
    /// moved, and a reveal is the one thing that happens to this tree while a
    /// session runs. So: reveal a whole level's worth of descendants, and
    /// every answer below the roots must be what it was — down to the bits of
    /// the bounding volume.
    #[test]
    fn a_reveal_leaves_the_shape_the_revision_vouches_for() {
        let layer = LayerJson::from_slice(
            br#"{ "format": "quantized-mesh-1.0", "tiles": ["{z}/{x}/{y}.terrain"],
                  "maxzoom": 10, "metadataAvailability": 10,
                  "available": [[{"startX":0,"startY":0,"endX":1,"endY":0}]] }"#,
        )
        .expect("layer");
        let tree = TerrainTree::new(layer);
        let tiles = [
            TileId::from_terrain(0, 0, 0),
            TileId::from_terrain(1, 3, 1),
            TileId::from_terrain(2, 5, 2),
            TileId::from_terrain(6, 40, 20),
        ];
        let shape = |tree: &TerrainTree| {
            tiles
                .iter()
                .map(|&t| format!("{:?} {:?}", tree.children(t), tree.properties(t)))
                .collect::<Vec<_>>()
        };
        let before = (tree.revision(), shape(&tree));
        let revealed = TileCoord::new(2, 5, 2);
        assert!(!tree.availability().is_available(revealed));
        tree.availability().add_descendant_ranges(
            0,
            &[
                vec![crate::layer::AvailabilityRange {
                    start_x: 0,
                    start_y: 0,
                    end_x: 3,
                    end_y: 1,
                }],
                vec![crate::layer::AvailabilityRange {
                    start_x: 0,
                    start_y: 0,
                    end_x: 7,
                    end_y: 3,
                }],
            ],
        );
        assert!(
            tree.availability().is_available(revealed),
            "the reveal did nothing, so this test proves nothing"
        );
        assert_eq!((tree.revision(), shape(&tree)), before);
        assert!(before.0.is_some(), "a terrain tree vouches for its shape");
    }

    #[test]
    fn the_tree_offers_its_planet_as_an_occluder() {
        let tree = TerrainTree::new(cwt_layer());
        let occluder = tree.occluder().expect("a global terrain has a planet");
        assert_eq!(occluder.center, glam::DVec3::ZERO);
        assert!(
            (occluder.radius - tuile_core::geo::WGS84_B).abs() < 1.0,
            "{} is not the inscribed radius",
            occluder.radius
        );
        // Through the trait object, which is how the runtime holds it — the
        // one thing a direct call cannot prove.
        let boxed: Box<dyn TileTree> = Box::new(TerrainTree::new(cwt_layer()));
        assert_eq!(boxed.occluder(), Some(occluder));
    }

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

    /// Division carries on past the data, and stops where it is told to.
    ///
    /// Past the data a tile is built from its parent's surface, which is what
    /// keeps a coverage boundary from punching holes — the children a boundary
    /// leaves without data still exist and are still drawn.
    ///
    /// Where it stops is not this crate's call. Dividing past the terrain is
    /// only worth doing while the *imagery* draped on the smaller tiles can
    /// still get sharper, so the bound is the imagery's maximum and
    /// `tuile-planetary` sets it from the provider it was handed. Picking a
    /// number here instead is how the ground once stopped sharpening several
    /// levels short of what the photography actually had.
    #[test]
    fn division_runs_past_the_data_and_stops_where_it_is_told() {
        // The fixture declares data to level 4.
        let tree = TerrainTree::new(cwt_layer()).with_max_level(9);
        assert_eq!(
            tree.children(TileId::from_terrain(4, 20, 12)).len(),
            4,
            "the deepest tile with data divides"
        );
        assert_eq!(
            tree.children(TileId::from_terrain(8, 320, 192)).len(),
            4,
            "four levels past the data, still dividing"
        );
        assert!(
            tree.children(TileId::from_terrain(9, 640, 384)).is_empty(),
            "and it stops at the level it was given"
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

    /// How many tiles one traversal of the globe visits from an eye at
    /// `lat` (radians), `lon` (degrees) and `height` (metres), looking almost
    /// straight down with the interactive configuration, an empty residency
    /// and a 1920x1080 view.
    ///
    /// Run on its own thread and waited for with a deadline, so that a
    /// traversal which does not come back fails the test instead of hanging
    /// the suite. The orphaned thread dies with the test binary.
    fn visited_from(lat: f64, lon_deg: f64, height: f64) -> u32 {
        use tuile_core::geo::{geodetic_to_ecef, Geodetic};
        use tuile_core::traversal::{traverse, Config, ResidentSet, TraversalOutput, ViewState};

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let tree = TerrainTree::new(cwt_layer());
            let lon = lon_deg.to_radians();
            let eye = geodetic_to_ecef(Geodetic { lon, lat, height });
            let up = eye.normalize();
            // The local north at the eye, then pitched 88 degrees below the
            // horizon: almost the nadir, heading north.
            let east = glam::DVec3::new(-lon.sin(), lon.cos(), 0.0);
            let north = up.cross(east).normalize();
            let pitch = 88f64.to_radians();
            let forward = north * pitch.cos() - up * pitch.sin();
            let view = ViewState::perspective(
                eye,
                forward.normalize(),
                up,
                glam::dvec2(1920.0, 1080.0),
                60f64.to_radians(),
            );
            let mut out = TraversalOutput::default();
            traverse(
                &tree,
                &ResidentSet::default(),
                &[view],
                &Config::interactive_globe(),
                Default::default(),
                &Default::default(),
                &Default::default(),
                &mut out,
            );
            let _ = tx.send(out.stats.visited);
        });
        rx.recv_timeout(std::time::Duration::from_secs(10))
            .expect("the traversal did not come back in 10 s: nothing bounds it")
    }

    /// Where the predictor's clamp put an eye (`lat = pi/2 - 1e-9`, 100 m up):
    /// the 2^(l+1) tiles of every level's polar row meet at the pole, are all
    /// at distance ~0, and all refine to the deepest level - about 1e8 tiles
    /// and 20 GB. A traversal is bounded in nodes, whatever the view.
    #[test]
    fn a_traversal_from_the_pole_at_ground_level_is_bounded() {
        let visited = visited_from(std::f64::consts::FRAC_PI_2 - 1e-9, 0.7912, 100.0);
        let bound = tuile_core::traversal::Config::interactive_globe().max_visits;
        assert!(
            bound <= 500_000,
            "the interactive bound ({bound}) is not a bound"
        );
        assert!(visited <= bound, "visited {visited} tiles, bound {bound}");
    }

    /// The same view anywhere else is the ordinary case, and stays far below
    /// the bound: only the pole explodes.
    #[test]
    fn a_traversal_at_an_ordinary_latitude_is_small() {
        let visited = visited_from(42.8108_f64.to_radians(), 0.7912, 100.0);
        eprintln!("ordinary visited {visited}");
        assert!(visited < 50_000, "visited {visited} tiles");
    }

    /// And the pole seen from far away is ordinary too.
    #[test]
    fn a_traversal_from_the_pole_at_altitude_is_small() {
        let visited = visited_from(std::f64::consts::FRAC_PI_2 - 1e-9, 0.7912, 565_000.0);
        eprintln!("pole 565 km visited {visited}");
        assert!(visited < 50_000, "visited {visited} tiles");
    }

    /// The vertical slack of a tile's test sphere, metres: the ground's
    /// relief round the test view (the Landes, under 100 m) that a sphere
    /// about the tile's ground centre does not know.
    const TEST_RELIEF_M: f64 = 60.0;

    /// The view of the measure (Laurent, 2026-11-28): 44.8947 N, 0.5212 W,
    /// 801 m up, heading 20°, 50° below the horizon, 45° vertical field,
    /// 1600x900.
    fn tilted_view() -> tuile_core::traversal::ViewState {
        use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};
        let g = Geodetic {
            lon: (-0.5212f64).to_radians(),
            lat: 44.8947f64.to_radians(),
            height: 0.0,
        };
        let enu = enu_frame(g);
        let (east, north, up) = (enu.col(0), enu.col(1), enu.col(2));
        let (heading, pitch) = (20f64.to_radians(), 50f64.to_radians());
        let horizontal = north * heading.cos() + east * heading.sin();
        let forward = horizontal * pitch.cos() - up * pitch.sin();
        let cam_up = horizontal * pitch.sin() + up * pitch.cos();
        tuile_core::traversal::ViewState::perspective(
            geodetic_to_ecef(g) + up * 801.0,
            forward,
            cam_up,
            glam::dvec2(1600.0, 900.0),
            45f64.to_radians(),
        )
    }

    /// Whether terrain tile `id` can touch `view`'s culling frustum: a
    /// sphere about its ground centre, its half diagonal plus
    /// [`TEST_RELIEF_M`] in radius. Outside it is outside by more than its
    /// own size and the ground's relief.
    fn touches(tree: &TerrainTree, view: &tuile_core::traversal::ViewState, id: TileId) -> bool {
        use tuile_core::geo::{geodetic_to_ecef, Geodetic};
        use tuile_core::math::{BoundingVolume, Sphere};
        let (level, x, y) = id.terrain_coord();
        let r = tree.scheme().tile_rect(TileCoord::new(level, x, y));
        let at = |lon, lat| {
            geodetic_to_ecef(Geodetic {
                lon,
                lat,
                height: 0.0,
            })
        };
        let centre = at((r.west + r.east) / 2.0, (r.south + r.north) / 2.0);
        let half = (at(r.west, r.south) - at(r.east, r.north))
            .length()
            .max((at(r.west, r.north) - at(r.east, r.south)).length())
            / 2.0;
        BoundingVolume::Sphere(Sphere {
            center: centre,
            radius: half + TEST_RELIEF_M,
        })
        .intersects_frustum(view.frustum())
    }

    /// The tile of `level` under the test view's eye.
    fn under_the_eye(level: u32) -> TileCoord {
        let (lon, lat) = ((-0.5212f64).to_radians(), 44.8947f64.to_radians());
        let s = GeographicTilingScheme::default();
        let x =
            ((lon + std::f64::consts::PI) / std::f64::consts::TAU * s.tiles_x(level) as f64) as u64;
        let y = ((lat + std::f64::consts::FRAC_PI_2) / std::f64::consts::PI
            * s.tiles_y(level) as f64) as u64;
        TileCoord::new(level, x, y)
    }

    /// One pass of the interactive traversal from the test view over
    /// `resident`: what it selects, and what it asks for.
    fn pass(
        tree: &TerrainTree,
        resident: &tuile_core::traversal::ResidentSet,
    ) -> tuile_core::traversal::TraversalOutput {
        use tuile_core::traversal::{traverse, Config, TraversalOutput};
        let mut out = TraversalOutput::default();
        traverse(
            tree,
            resident,
            &[tilted_view()],
            &Config::interactive_globe(),
            Default::default(),
            &Default::default(),
            &Default::default(),
            &mut out,
        );
        out
    }

    /// `selected` by level: (selected, outside the test view's field).
    fn by_level(
        tree: &TerrainTree,
        selected: &[TileId],
    ) -> std::collections::BTreeMap<u32, (u32, u32)> {
        let view = tilted_view();
        let mut by_level = std::collections::BTreeMap::new();
        for &id in selected {
            let e: &mut (u32, u32) = by_level.entry(id.terrain_coord().0).or_default();
            e.0 += 1;
            e.1 += u32::from(!touches(tree, &view, id));
        }
        by_level
    }

    /// The ground the test view sees, a grid of its pixels cast down to 50 m
    /// (the Landes' height), each found in exactly one selected tile: no
    /// black ground in the field, and no ground drawn twice.
    fn assert_the_field_is_covered(tree: &TerrainTree, selected: &[TileId]) {
        use tuile_core::geo::{ecef_to_geodetic, Geodetic};
        let view = tilted_view();
        let (eye, fwd) = (view.position(), view.direction());
        let right = fwd.cross(eye.normalize()).normalize();
        let up = right.cross(fwd);
        let tan_y = (45f64.to_radians() / 2.0).tan();
        let tan_x = tan_y * 1600.0 / 900.0;
        let rects: Vec<_> = selected
            .iter()
            .map(|&id| {
                let (l, x, y) = id.terrain_coord();
                tree.scheme().tile_rect(TileCoord::new(l, x, y))
            })
            .collect();
        let mut seen = 0;
        for i in 0..=16 {
            for j in 0..=16 {
                let (u, v) = (f64::from(i) / 8.0 - 1.0, f64::from(j) / 8.0 - 1.0);
                let ray = (fwd + right * (u * tan_x) + up * (v * tan_y)).normalize();
                let height = |t: f64| ecef_to_geodetic(eye + ray * t).height;
                if height(50_000.0) > 50.0 {
                    continue; // sky
                }
                let (mut a, mut b) = (0.0, 50_000.0);
                for _ in 0..80 {
                    let m = 0.5 * (a + b);
                    if height(m) > 50.0 {
                        a = m;
                    } else {
                        b = m;
                    }
                }
                let Geodetic { lon, lat, .. } = ecef_to_geodetic(eye + ray * b);
                let n = rects
                    .iter()
                    .filter(|r| r.west <= lon && lon < r.east && r.south <= lat && lat < r.north)
                    .count();
                assert_eq!(n, 1, "pixel ({u:.2}, {v:.2}): ground in {n} selected tiles");
                seen += 1;
            }
        }
        assert!(seen > 200, "the test view sees the ground ({seen} rays)");
    }

    /// What the ground under the test view really spans at `level`, as a
    /// decoded header would say: the Pyrenees and Spain in a level-5 tile,
    /// the Landes' forest from level 10, a few tens of metres from 13.
    fn landes_range(level: u32) -> (f32, f32) {
        match level {
            0..=6 => (-100.0, 3400.0),
            7..=9 => (-20.0, 800.0),
            10..=12 => (0.0, 150.0),
            _ => (20.0, 80.0),
        }
    }

    /// What a session from the test view settles on, from nothing loaded:
    /// round after round, every tile asked for lands (decoded, its range
    /// recorded, resident), `preloads` whether the speculative asks too,
    /// until a pass selects only resident tiles and asks for nothing but
    /// preloads. The tree, the settled selection, and every tile loaded on
    /// the way.
    fn settle(
        heights: bool,
        preloads: bool,
    ) -> (TerrainTree, Vec<TileId>, std::collections::BTreeSet<TileId>) {
        use tuile_core::traversal::{PriorityGroup, ResidentSet};
        let h = Arc::new(TerrainHeights::new(GeographicTilingScheme::default()));
        let tree = if heights {
            TerrainTree::new(cwt_layer()).with_heights(Arc::clone(&h))
        } else {
            TerrainTree::new(cwt_layer())
        };
        let mut resident = ResidentSet::default();
        let mut loaded = std::collections::BTreeSet::new();
        for round in 0..64 {
            let out = pass(&tree, &resident);
            let asked: Vec<TileId> = out
                .requests
                .iter()
                .filter(|r| preloads || r.group != PriorityGroup::Preload)
                .map(|r| r.tile)
                .filter(|t| !loaded.contains(t))
                .collect();
            if asked.is_empty()
                && out
                    .requests
                    .iter()
                    .all(|r| r.group == PriorityGroup::Preload)
            {
                let selected = out.selected.iter().map(|s| s.0).collect();
                eprintln!("settled after {round} rounds of loads");
                return (tree, selected, loaded);
            }
            for t in asked {
                let (l, x, y) = t.terrain_coord();
                let (lo, hi) = landes_range(l);
                h.record_range(TileCoord::new(l, x, y), lo, hi);
                resident.insert(t);
                loaded.insert(t);
            }
        }
        panic!("the session did not settle in 64 rounds");
    }

    /// The traversal selects only what touches the field of view.
    ///
    /// A camera 801 m up looking 50 deg below the horizon sees ground from
    /// some 250 m ahead; the ground under and behind it is not on screen.
    /// Measured in the viewer (2026-11-28): 1196 of 1480 drawn terrain
    /// meshes were outside this view's frustum, a disc round the eye, every
    /// tile at levels 20 and 21. The cause was each tile's volume spanning
    /// the global -1000..9000 m: the eye inside the column over the ground
    /// beneath it priced that ground at distance zero, and columns behind the
    /// eye crossed every frustum plane ([`TerrainTree::height_range`]).
    /// Without the decoded ranges the settled selection is 1078 tiles, 784
    /// of them outside the field.
    ///
    /// Covers the settled selection of one still view. Does not cover the
    /// passes before it: until the ancestry under the eye has landed, a
    /// coarse range that reaches the eye's height still refines the ground
    /// under it (each round is printed).
    #[test]
    fn a_tilted_view_selects_nothing_outside_its_field() {
        for preloads in [false, true] {
            let (tree, selected, loaded) = settle(true, preloads);
            let levels = by_level(&tree, &selected);
            let outside: u32 = levels.values().map(|e| e.1).sum();
            eprintln!(
                "ranges, preloads {preloads}: selected {} outside {outside}, loaded {}; {levels:?}",
                selected.len(),
                loaded.len()
            );
            assert_eq!(outside, 0, "selected outside the field: {levels:?}");
            assert_the_field_is_covered(&tree, &selected);
        }
    }

    /// The settled selection is the same whatever else has landed: the
    /// speculative loads (preloads) record ranges the session did not wait
    /// for, and change nothing. What a bake's lookahead needs.
    #[test]
    fn the_settled_selection_does_not_depend_on_what_else_landed() {
        let (_, without, _) = settle(true, false);
        let (_, with, _) = settle(true, true);
        assert_eq!(without, with);
    }

    /// The same session without the decoded ranges: what the viewer drew.
    /// Not a requirement, the measure the ones above are against.
    #[test]
    fn without_the_ranges_the_settled_selection_reaches_behind_the_eye() {
        let (tree, selected, loaded) = settle(false, false);
        let levels = by_level(&tree, &selected);
        let outside: u32 = levels.values().map(|e| e.1).sum();
        eprintln!(
            "global range: selected {} outside {outside}, loaded {}; {levels:?}",
            selected.len(),
            loaded.len()
        );
        assert!(outside > 0);
        assert_the_field_is_covered(&tree, &selected);
    }

    /// A tile's volume spans its parent's decoded range, widened, never past
    /// the global range; the global range before its parent is decoded, and
    /// for a root. A tile refines once its own range is decoded, and the
    /// revision moves when a range lands.
    #[test]
    fn a_tiles_volume_spans_its_parents_decoded_range() {
        let heights = Arc::new(TerrainHeights::new(GeographicTilingScheme::default()));
        let tree = TerrainTree::new(cwt_layer()).with_heights(Arc::clone(&heights));
        let c = under_the_eye(16);
        let parent = under_the_eye(15);
        assert_eq!(parent, TileCoord::new(15, c.x / 2, c.y / 2));
        assert_eq!(tree.height_range(c), (-1000.0, 9000.0));
        assert!(!tree.can_refine(TerrainTree::id(parent)));
        let r0 = tree.revision();
        heights.record_range(parent, 10.0, 90.0);
        assert_ne!(tree.revision(), r0);
        assert!(tree.can_refine(TerrainTree::id(parent)));
        let slack = 2.0 * level_geometric_error(15, WGS84_A, 2) + RELIEF_FLOOR_M;
        assert_eq!(tree.height_range(c), (10.0 - slack, 90.0 + slack));
        // Its own range, decoded, does not move its volume.
        heights.record_range(c, 20.0, 40.0);
        assert_eq!(tree.height_range(c), (10.0 - slack, 90.0 + slack));
        heights.record_range(under_the_eye(1), -500.0, 8800.0);
        assert_eq!(tree.height_range(under_the_eye(2)), (-1000.0, 9000.0));
        assert_eq!(
            tree.height_range(TileCoord::new(0, 0, 0)),
            (-1000.0, 9000.0)
        );
        // Without heights, every tile refines and spans the global range.
        let plain = TerrainTree::new(cwt_layer());
        assert!(plain.can_refine(TerrainTree::id(parent)));
        assert_eq!(plain.height_range(c), (-1000.0, 9000.0));
    }
}
