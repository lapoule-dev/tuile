// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Does the selection cover the ground the camera can see?
//!
//! Written while chasing a black band across the top third of every farm
//! frame — the far ground missing, reproducibly, at the camera the farm
//! actually flies. Culling was the standing suspect, because switching it off
//! (`TUILE_CULL=0`) filled the frame. These tests are what that suspicion cost
//! and what it bought: **the traversal is not where the band comes from.**
//!
//! Two things make them worth keeping rather than deleting with the
//! investigation:
//!
//! * They ask the only question that matters, and it is not the one the stats
//!   answer. A traversal reporting hundreds of selected tiles can still leave a
//!   band of the frame with nothing drawn on it — as `CLAUDE.md` puts it, a
//!   green counter is not a covered globe. So this samples ground the camera
//!   sees and asks, per sample, whether anything selected covers it.
//! * They run the real geometry with no network at all. A terrain tree's
//!   children and bounding volumes are computed, not fetched; only `roots()`
//!   consults availability. So a layer with no availability table gives the
//!   real quadtree, the real bounding volumes and the real geometric errors,
//!   in milliseconds.
//!
//! What they cannot see is anything downstream of the selection: loading,
//! draping, baking, or what the renderer does with a tile it was handed. That
//! is where the band has to be, and saying so is the point.

use std::collections::HashSet;

use glam::dvec2;
use tuile_core::source::{TileId, TileTree};
use tuile_core::traversal::{traverse, Config, ResidencyView, TraversalOutput, ViewState};
use tuile_terrain::layer::LayerJson;
use tuile_terrain::tiling::{GeographicTilingScheme, TileCoord};
use tuile_terrain::tree::TerrainTree;

/// A terrain quadtree with no availability table, which means every tile
/// exists. Availability gates `roots()` and the loader's choice between
/// fetching and upsampling — never the shape of the tree, never a bounding
/// volume — so this is the real geometry, offline.
fn offline_terrain() -> TerrainTree {
    let layer: LayerJson = serde_json::from_str(
        r#"{"format":"quantized-mesh-1.0","minzoom":0,"maxzoom":15,
            "tiles":["{z}/{x}/{y}.terrain"]}"#,
    )
    .expect("layer.json");
    // What `globe_on` sets from the imagery provider's maximum level, rather
    // than the absolute backstop: dividing past the imagery buys smaller tiles
    // and identical pictures.
    TerrainTree::new(layer).with_max_level(19)
}

/// The settings the farm renders with, not the library defaults. A test that
/// does not run the host's configuration is not testing the host.
fn farm_config() -> Config {
    Config {
        maximum_screen_space_error: 3.0,
        forbid_holes: true,
        stand_ins: false,
        uniform_detail: true,
        uniform_detail_radius: 8.0,
        cull: true,
        horizon_culling: true,
        loading_descendant_limit: u32::MAX,
        pinned_level: Some(5),
        preload_siblings: false,
        ..Config::default()
    }
}

/// The camera of the frame the band was measured on: 5 005 m above the
/// ellipsoid over the eastern Pyrenees, looking west and down. At this
/// altitude and field of view the frame covers ground from about 3.6 km to
/// about 19 km ahead.
fn farm_view() -> ViewState {
    let cam = tuile_core::geo::geodetic_to_ecef(tuile_core::geo::Geodetic {
        lon: 2.2673_f64.to_radians(),
        lat: 42.5198_f64.to_radians(),
        height: 5005.0,
    });
    let target = tuile_core::geo::geodetic_to_ecef(tuile_core::geo::Geodetic {
        lon: 2.17_f64.to_radians(),
        lat: 42.52_f64.to_radians(),
        height: 0.0,
    });
    ViewState::perspective(
        cam,
        (target - cam).normalize(),
        cam.normalize(),
        dvec2(1280.0, 960.0),
        45f64.to_radians(),
    )
}

/// Ground `km` ahead of the eye, along the horizontal component of the view.
fn ground_ahead(view: &ViewState, km: f64) -> tuile_core::geo::Geodetic {
    let up = view.position().normalize();
    let forward = view.direction();
    let horizontal = (forward - up * forward.dot(up)).normalize();
    tuile_core::geo::ecef_to_geodetic(view.position() + horizontal * (km * 1000.0))
}

/// The tile containing a point, at a given level.
fn tile_at(scheme: &GeographicTilingScheme, g: &tuile_core::geo::Geodetic, level: u32) -> TileId {
    use std::f64::consts::{FRAC_PI_2, PI};
    let x = (((g.lon + PI) / (2.0 * PI)) * scheme.tiles_x(level) as f64).floor() as u64;
    let y = (((g.lat + FRAC_PI_2) / PI) * scheme.tiles_y(level) as f64).floor() as u64;
    TileId::from_terrain(level, x, y)
}

/// No level of the tree calls visible ground invisible.
///
/// This is the test that cleared culling. The suspicion was that some ancestor
/// was being dropped — a tile reported culled is reported *ready and covering*,
/// so an ancestor is released on the strength of a child that then draws
/// nothing, and the hole has clean edges and no log line. If that were
/// happening over ground in the frame, some level of the chain from root to
/// leaf would say so. None does.
#[test]
fn no_level_of_the_chain_culls_ground_the_camera_can_see() {
    use tuile_core::math::Occluder;

    let tree = offline_terrain();
    let view = farm_view();
    let occluder = tree.occluder().expect("a global terrain has a planet");
    assert_eq!(occluder, Occluder {
        center: glam::DVec3::ZERO,
        radius: tuile_core::geo::WGS84_B,
    });
    let scheme = GeographicTilingScheme::default();

    // 5 km is near the bottom of the frame, 19 km near the top. 25 km is past
    // it, and is here to show the test can tell the difference.
    for km in [5.0_f64, 12.0, 15.0, 17.0, 19.0] {
        let g = ground_ahead(&view, km);
        for level in 0..=19u32 {
            let volume = tree.properties(tile_at(&scheme, &g, level)).bounding_volume;
            assert!(
                volume.intersects_frustum(view.frustum()),
                "level {level} over ground {km} km ahead is outside the frustum"
            );
            assert!(
                !occluder.hides(&volume, view.position()),
                "level {level} over ground {km} km ahead is called hidden behind \
                 the planet, {km} km away, with a horizon at hundreds of km"
            );
        }
    }
}

/// The frame is covered, sample by sample, once the loads have landed.
///
/// The convergence is modelled the way the server drives it — traverse, take
/// the head of the request queue, mark it resident, traverse again — with the
/// pinned floor resident from the start, as its contract says. What comes out
/// is a selection that covers every sample from 2 km to 22 km.
///
/// So the traversal is not the black band. Whatever loses the far ground loses
/// it after this point: in the loading, the draping, the bake, or in what the
/// renderer does with what it was handed.
///
/// What this does **not** claim: that the ground is covered *sharply*. Holding
/// works — a coarse ancestor stands in for a subtree that is still streaming —
/// so coverage survives a fault that only costs detail. Checked by shortening
/// the culling frustum's far plane to 12 km, which is the symptom this was
/// written to look for: the chain test names the exact level and distance that
/// went outside the frustum, and this one reports bare ground at 22 km while
/// ancestors still cover 13 to 19. Both fail; only one of them says where.
#[test]
fn every_sample_of_visible_ground_is_covered_by_something_selected() {
    let tree = offline_terrain();
    let config = farm_config();
    let view = farm_view();
    let views = [view];
    let scheme = GeographicTilingScheme::default();

    let mut residency = ResidencyView::default();
    // The pinned floor stays resident for the life of a session, whatever the
    // ceilings say — that is the guarantee the fallback chain rests on.
    for x in 0..scheme.tiles_x(5) {
        for y in 0..scheme.tiles_y(5) {
            residency.insert(TileId::from_terrain(5, x, y));
        }
    }

    let mut out = TraversalOutput::default();
    let mut rendered: HashSet<TileId> = HashSet::new();
    // What the farm allows in flight. Bounded rounds, because a convergence
    // that needs unbounded rounds is a hang, and this test would rather fail.
    const IN_FLIGHT: usize = 256;
    let mut rounds = 0;
    for _ in 0..60 {
        traverse(&tree, &residency, &views, &config, 0, &rendered, &mut out);
        rendered = out.selected.iter().map(|(t, _)| *t).collect();
        // A held tile's descendants are wanted, resident and drawn by nothing;
        // the server marks them rendered at exactly this point, and a model
        // that does not re-requests them for ever.
        rendered.extend(out.awaiting.iter().copied());
        let pending: Vec<_> = out
            .requests
            .iter()
            .take(IN_FLIGHT)
            .map(|r| r.tile)
            .filter(|t| !residency.is_resident(*t))
            .collect();
        rounds += 1;
        if pending.is_empty() {
            break;
        }
        for tile in pending {
            residency.insert(tile);
        }
    }
    assert!(
        out.requests.is_empty(),
        "did not converge in {rounds} rounds, {} still wanted",
        out.requests.len()
    );
    assert_eq!(out.stats.gaps, 0, "a terrain traversal must reach no empty leaf");

    let rects: Vec<_> = out
        .selected
        .iter()
        .map(|(id, _)| {
            let (level, x, y) = id.terrain_coord();
            scheme.tile_rect(TileCoord::new(level, x, y))
        })
        .collect();

    let bare: Vec<f64> = [2.0_f64, 5.0, 8.0, 11.0, 13.0, 15.0, 17.0, 19.0, 22.0]
        .into_iter()
        .filter(|km| {
            let g = ground_ahead(&view, *km);
            !rects.iter().any(|r| {
                g.lon >= r.west && g.lon < r.east && g.lat >= r.south && g.lat < r.north
            })
        })
        .collect();
    assert!(
        bare.is_empty(),
        "ground the camera sees is covered by nothing selected at {bare:?} km — \
         that is a black band, and the {} selected tiles do not report it",
        out.selected.len()
    );
}
