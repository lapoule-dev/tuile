// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The ground under a surface drawn in its place.
//!
//! A source may draw a mesh of the whole surface — buildings, trees and the
//! ground beneath them — in place of the globe's ground
//! ([`SourceOptions::covers_ground`](crate::source::SourceOptions)). Where it
//! is drawn, the globe's ground is hidden by it (the renderer keeps the
//! ground out of the pixels it took), so refining that ground to the
//! traversal's error loads detail no one sees. The ground under it is
//! refined at a relaxed error instead ([`COVER_RELAX`] times the
//! traversal's): it stays loaded, a few levels coarser, as the fallback for
//! wherever the surface has nothing — a tile not loaded yet, a hole in the
//! data, the frame after it is switched off. Coarse ground is never black
//! ground.
//!
//! What covers is read from what was drawn the step before, deterministically:
//! each drawn covering tile's inner footprint, a square in longitude and
//! latitude inscribed in its bounding volume's horizontal extent, so that a
//! footprint never claims ground its tile does not reach. A ground tile is
//! covered when its four corners and its centre each lie in some footprint
//! — five points, so a hole between two covering tiles smaller than the
//! ground tile can be missed: the ground there is then the coarse fallback,
//! drawn where the surface has nothing.

use crate::budget::GeoRect;
use crate::math::BoundingVolume;
use crate::source::{TileId, TileLoader, TileProperties, TileTree};

/// How much coarser the ground under a covering surface is refined: its
/// screen-space error threshold times this. Eight puts a globe at 2 px under
/// a city at 16 px, the 3D Tiles convention's error, where the ground is a
/// fallback a few levels up.
pub const COVER_RELAX: f64 = 8.0;

/// The footprints of the covering tiles drawn at the last step.
#[derive(Debug, Default, Clone)]
pub struct Cover {
    /// Inner footprints, radians, in the order of their tiles.
    rects: Vec<GeoRect>,
    /// Their union's bounds: what a ground tile is tested against first.
    bounds: Option<GeoRect>,
}

impl Cover {
    /// The cover of the covering tiles among `drawn`, read from `tree`.
    pub fn of(tree: &dyn TileTree, drawn: impl IntoIterator<Item = TileId>) -> Self {
        let mut tiles: Vec<TileId> = drawn
            .into_iter()
            .filter(|t| tree.covers_ground(*t))
            .collect();
        // The rects' order follows the tiles', whatever order they came in.
        tiles.sort_unstable();
        let rects: Vec<GeoRect> = tiles
            .iter()
            .filter_map(|t| inner_footprint(&tree.properties(*t)))
            .collect();
        let bounds = rects.iter().copied().reduce(|a, b| GeoRect {
            west: a.west.min(b.west),
            south: a.south.min(b.south),
            east: a.east.max(b.east),
            north: a.north.max(b.north),
        });
        Self { rects, bounds }
    }

    /// Whether nothing covers anything.
    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// Whether the ground `rect` lies under the cover: its four corners and
    /// its centre each in some footprint.
    pub fn covers(&self, rect: &GeoRect) -> bool {
        let Some(b) = self.bounds else {
            return false;
        };
        if rect.west < b.west || rect.east > b.east || rect.south < b.south || rect.north > b.north
        {
            return false;
        }
        let points = [
            (rect.west, rect.south),
            (rect.east, rect.south),
            (rect.west, rect.north),
            (rect.east, rect.north),
            (
                (rect.west + rect.east) / 2.0,
                (rect.south + rect.north) / 2.0,
            ),
        ];
        points.iter().all(|&(lon, lat)| {
            self.rects
                .iter()
                .any(|r| r.west <= lon && lon <= r.east && r.south <= lat && lat <= r.north)
        })
    }
}

/// A square in longitude and latitude inscribed in the horizontal extent of
/// a tile's bounding volume, radians: a sphere's, or a box's whose two most
/// horizontal axes span the ground (the smaller of their horizontal lengths
/// is the radius of a disc the box's section holds). `None` for a volume at
/// the geocentre or too small to say anything.
fn inner_footprint(p: &TileProperties) -> Option<GeoRect> {
    let (center, radius) = match &p.bounding_volume {
        BoundingVolume::Sphere(s) => (s.center, s.radius),
        BoundingVolume::Obb(b) => {
            let up = b.center.normalize_or_zero();
            if up == glam::DVec3::ZERO {
                return None;
            }
            let mut horizontal: Vec<(f64, f64)> = (0..3)
                .map(|i| {
                    let h = b.half_axes.col(i);
                    let length = h.length();
                    let across = (h - up * h.dot(up)).length();
                    (if length > 0.0 { across / length } else { 0.0 }, across)
                })
                .collect();
            // The two most horizontal axes, the smaller of their lengths.
            horizontal.sort_by(|a, b| b.0.total_cmp(&a.0));
            (b.center, horizontal[0].1.min(horizontal[1].1))
        }
    };
    let r = center.length();
    if r <= 0.0 || radius <= 0.0 {
        return None;
    }
    let g = crate::geo::ecef_to_geodetic(center);
    // The square inscribed in the disc, as angles at the volume's centre.
    let half = radius * std::f64::consts::FRAC_1_SQRT_2 / r;
    let cos_lat = g.lat.cos();
    if cos_lat <= 1e-6 {
        return None;
    }
    Some(GeoRect {
        west: g.lon - half / cos_lat,
        east: g.lon + half / cos_lat,
        south: g.lat - half,
        north: g.lat + half,
    })
}

/// A tree whose ground under the cover is refined [`COVER_RELAX`] times
/// coarser: every answer is the tree's, but for
/// [`TileTree::sse_factor`] of a ground tile the cover holds. What a
/// session's traversal and need read while something covers the ground.
pub struct Covered<'a> {
    pub tree: &'a dyn TileTree,
    /// Where a ground tile lies: its drape's footprint.
    pub loader: &'a dyn TileLoader,
    pub cover: &'a Cover,
}

impl TileTree for Covered<'_> {
    fn roots(&self) -> Vec<TileId> {
        self.tree.roots()
    }
    fn children(&self, id: TileId) -> Vec<TileId> {
        self.tree.children(id)
    }
    fn properties(&self, id: TileId) -> TileProperties {
        self.tree.properties(id)
    }
    fn parent(&self, id: TileId) -> Option<TileId> {
        self.tree.parent(id)
    }
    fn occluder(&self) -> Option<crate::math::Occluder> {
        self.tree.occluder()
    }
    fn occluder_of(&self, id: TileId) -> Option<crate::math::Occluder> {
        self.tree.occluder_of(id)
    }
    fn covers_ground(&self, id: TileId) -> bool {
        self.tree.covers_ground(id)
    }
    fn defer_edits(&self) {
        self.tree.defer_edits();
    }
    fn apply_deferred_edits(&self) -> Vec<(TileId, String)> {
        self.tree.apply_deferred_edits()
    }
    fn revision(&self) -> Option<u64> {
        self.tree.revision()
    }
    fn level(&self, id: TileId) -> u32 {
        self.tree.level(id)
    }
    fn mesh_class(&self, id: TileId) -> crate::budget::Class {
        self.tree.mesh_class(id)
    }

    /// The tree's, times [`COVER_RELAX`] for ground under the cover.
    fn sse_factor(&self, id: TileId) -> f64 {
        let factor = self.tree.sse_factor(id);
        if self.cover.is_empty()
            || !self.tree.mesh_class(id).is_ground()
            || self.tree.covers_ground(id)
        {
            return factor;
        }
        match self.loader.drapes(id) {
            Some(reach) if self.cover.covers(&reach.rect) => factor * COVER_RELAX,
            _ => factor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::{Obb, Sphere};
    use glam::{DMat3, DVec3};

    fn at(lon_deg: f64, lat_deg: f64) -> DVec3 {
        crate::geo::geodetic_to_ecef(crate::geo::Geodetic {
            lon: lon_deg.to_radians(),
            lat: lat_deg.to_radians(),
            height: 0.0,
        })
    }

    fn props(v: BoundingVolume) -> TileProperties {
        TileProperties {
            bounding_volume: v,
            geometric_error: 1.0,
            refine: crate::tileset::Refine::Replace,
            has_content: true,
        }
    }

    /// Ground at the equator under a covering source, as a stub tree and
    /// loader: tile 1 ground inside the cover, tile 2 ground outside it,
    /// tile 3 the covering surface itself.
    struct Stub;

    impl TileTree for Stub {
        fn roots(&self) -> Vec<TileId> {
            vec![TileId(1), TileId(2), TileId(3)]
        }
        fn children(&self, _id: TileId) -> Vec<TileId> {
            Vec::new()
        }
        fn properties(&self, _id: TileId) -> TileProperties {
            props(BoundingVolume::Sphere(Sphere {
                center: at(0.0, 0.0),
                radius: 1.0,
            }))
        }
        fn mesh_class(&self, id: TileId) -> crate::budget::Class {
            if id == TileId(3) {
                crate::budget::Class::Tiles3dMesh
            } else {
                crate::budget::Class::TerrainMesh
            }
        }
        fn covers_ground(&self, id: TileId) -> bool {
            id == TileId(3)
        }
        fn sse_factor(&self, id: TileId) -> f64 {
            if id == TileId(3) {
                8.0
            } else {
                1.0
            }
        }
    }

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
    impl TileLoader for Stub {
        async fn load(
            &self,
            _id: TileId,
        ) -> Result<crate::source::Loaded, crate::source::LoadError> {
            Err(crate::source::LoadError::Failed("nothing".into()))
        }
        fn drapes(&self, id: TileId) -> Option<crate::source::DrapeReach> {
            let (w, e) = if id == TileId(1) {
                (0.1, 0.2)
            } else {
                (2.1, 2.2)
            };
            Some(crate::source::DrapeReach {
                texel_l0_m: 1.0,
                rect: GeoRect {
                    west: w,
                    south: 0.1,
                    east: e,
                    north: 0.2,
                },
                layers: Vec::new(),
            })
        }
    }

    /// **The ground under the cover is refined coarser, and only it**: a
    /// ground tile whose footprint the cover holds has its error threshold
    /// [`COVER_RELAX`] times its own, one outside keeps its own, and the
    /// covering surface keeps its own. Red with the cover ignored: the
    /// covered ground at its own threshold.
    #[test]
    fn the_ground_under_the_cover_is_refined_coarser() {
        let rect = GeoRect {
            west: 0.0,
            south: 0.0,
            east: 1.0,
            north: 1.0,
        };
        let cover = Cover {
            rects: vec![rect],
            bounds: Some(rect),
        };
        let covered = Covered {
            tree: &Stub,
            loader: &Stub,
            cover: &cover,
        };
        assert_eq!(covered.sse_factor(TileId(1)), COVER_RELAX);
        assert_eq!(covered.sse_factor(TileId(2)), 1.0);
        assert_eq!(covered.sse_factor(TileId(3)), 8.0);
        let nothing = Cover::default();
        let bare = Covered {
            tree: &Stub,
            loader: &Stub,
            cover: &nothing,
        };
        assert_eq!(bare.sse_factor(TileId(1)), 1.0);
    }

    /// **The cover is read from the covering tiles drawn**, whatever order
    /// they came in, and from nothing else.
    #[test]
    fn the_cover_is_the_covering_tiles_drawn() {
        let cover = Cover::of(&Stub, [TileId(2), TileId(3), TileId(1)]);
        assert_eq!(cover.rects.len(), 1, "only the covering tile");
        assert_eq!(
            Cover::of(&Stub, [TileId(3), TileId(1)]).rects,
            Cover::of(&Stub, [TileId(1), TileId(3)]).rects
        );
        assert!(Cover::of(&Stub, [TileId(1), TileId(2)]).is_empty());
    }

    /// A footprint sits inside its volume: a box 1 km by 1 km at the
    /// equator, upright, gives a square a little smaller than the box's own,
    /// centred on it.
    #[test]
    fn an_upright_boxs_footprint_is_inside_it() {
        let c = at(10.0, 0.0);
        let up = c.normalize();
        let east = DVec3::new(-(10f64.to_radians()).sin(), (10f64.to_radians()).cos(), 0.0);
        let north = up.cross(east);
        let b = Obb {
            center: c,
            half_axes: DMat3::from_cols(east * 500.0, north * 500.0, up * 50.0),
        };
        let r = inner_footprint(&props(BoundingVolume::Obb(b))).expect("a footprint");
        let deg = |x: f64| x.to_degrees();
        let half_box = deg(500.0 / c.length());
        let half = (deg(r.east) - deg(r.west)) / 2.0;
        assert!(
            half > 0.6 * half_box && half < half_box,
            "{half} against {half_box}"
        );
        assert!((deg((r.east + r.west) / 2.0) - 10.0).abs() < 1e-9);
        assert!((deg((r.north + r.south) / 2.0)).abs() < 1e-6);
    }

    /// A ground tile is covered when its corners and centre are each under
    /// some footprint; one that sticks out, or a gap between two, is not.
    #[test]
    fn the_ground_is_covered_by_its_corners_and_centre() {
        let rect = |w: f64, s: f64, e: f64, n: f64| GeoRect {
            west: w,
            south: s,
            east: e,
            north: n,
        };
        let cover = Cover {
            rects: vec![rect(0.0, 0.0, 1.0, 1.0), rect(1.0, 0.0, 2.0, 1.0)],
            bounds: Some(rect(0.0, 0.0, 2.0, 1.0)),
        };
        assert!(cover.covers(&rect(0.5, 0.2, 1.5, 0.8)), "across two");
        assert!(!cover.covers(&rect(1.5, 0.5, 2.5, 0.9)), "sticking out");
        let gap = Cover {
            rects: vec![rect(0.0, 0.0, 0.9, 1.0), rect(1.1, 0.0, 2.0, 1.0)],
            bounds: Some(rect(0.0, 0.0, 2.0, 1.0)),
        };
        assert!(
            !gap.covers(&rect(0.5, 0.2, 1.5, 0.8)),
            "the centre in the gap"
        );
        assert!(!Cover::default().covers(&rect(0.5, 0.2, 0.6, 0.3)));
        // A sphere's footprint: inscribed in its disc.
        let s = inner_footprint(&props(BoundingVolume::Sphere(Sphere {
            center: at(0.0, 45.0),
            radius: 1_000.0,
        })))
        .expect("a footprint");
        assert!(s.north - s.south < 2_000.0 / at(0.0, 45.0).length());
    }
}
