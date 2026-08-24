// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The invariant a recorder's pixel guard enforces, stated as geometry:
//! **every ground point visible in the frustum belongs to a selected tile.**
//!
//! Found the hard way: a deterministic encode of a real flight died at frame
//! 17 501 with a diagonal magenta stripe — a band of ground covered by *no*
//! selected tile — along a dead-straight boundary between finely-refined and
//! coarsely-refined terrain, seen at a grazing angle. A straight LOD frontier
//! like that is what a terrain **availability boundary** produces: the source
//! serves deep levels on one side and stops several levels earlier on the
//! other, exactly like Cesium World Terrain does at its coverage edges.
//!
//! So this test builds that world on purpose: full availability to level 6
//! everywhere, and a small window refined to level 13 west of the zero
//! meridian only. Everything available is resident (the failing frame's
//! state: `provisional() == 0`). The recorder's own camera — 800 m orbit,
//! 30° tilt — walks a full turn of yaws next to the boundary, and for each
//! frame a grid of view rays is cast at the ellipsoid: any ray that lands on
//! ground no selected tile contains is the magenta stripe, caught with pure
//! CPU and named in the failure.

use std::collections::HashSet;

use glam::{DVec2, DVec3};
use tuile_core::geo::{ecef_to_geodetic, enu_frame, geodetic_to_ecef, Geodetic, WGS84_A};
use tuile_core::source::{TileId, TileTree};
use tuile_core::traversal::{traverse, Config, ResidencyView, TraversalOutput, ViewState};
use tuile_terrain::LayerJson;

/// Everything is available to here, everywhere — the coarse world.
const COARSE_MAX: u32 = 6;
/// The fine window refines to here, west of the zero meridian only.
const FINE_MAX: u32 = 13;
/// The fine window: 1° of longitude west of the boundary, 1° of latitude.
const FINE_WEST_DEG: f64 = -1.0;
const FINE_SOUTH_DEG: f64 = 47.5;
const FINE_NORTH_DEG: f64 = 48.5;

const WGS84_B: f64 = 6_356_752.314_245_179;

fn tiles_x(level: u32) -> u64 {
    2u64 << level
}
fn tiles_y(level: u32) -> u64 {
    1u64 << level
}

/// Column/row window of the fine zone at `level` (TMS, y = 0 south).
fn fine_window(level: u32) -> (u64, u64, u64, u64) {
    let nx = tiles_x(level) as f64;
    let ny = tiles_y(level) as f64;
    let x0 = (((FINE_WEST_DEG + 180.0) / 360.0) * nx).floor() as u64;
    let x1 = ((180.0 / 360.0) * nx).floor() as u64 - 1; // strictly west of lon 0
    let y0 = (((FINE_SOUTH_DEG + 90.0) / 180.0) * ny).floor() as u64;
    let y1 = (((FINE_NORTH_DEG + 90.0) / 180.0) * ny).floor() as u64;
    (x0, x1, y0, y1)
}

/// The availability-bounded world described above, as the source's own
/// `layer.json` — built by the same parser a real session uses.
fn boundary_layer() -> LayerJson {
    let mut ranges = Vec::new();
    for level in 0..=FINE_MAX {
        if level <= COARSE_MAX {
            let (x, y) = (tiles_x(level), tiles_y(level));
            ranges.push(format!(
                r#"[{{"startX":0,"startY":0,"endX":{},"endY":{}}}]"#,
                x - 1,
                y - 1
            ));
        } else {
            let (x0, x1, y0, y1) = fine_window(level);
            ranges.push(format!(
                r#"[{{"startX":{x0},"startY":{y0},"endX":{x1},"endY":{y1}}}]"#
            ));
        }
    }
    let doc = format!(
        r#"{{"tilejson":"2.1.0","format":"quantized-mesh-1.0","scheme":"tms",
            "projection":"EPSG:4326","tiles":["{{z}}/{{x}}/{{y}}.terrain"],
            "bounds":[-180,-90,180,90],"available":[{}]}}"#,
        ranges.join(",")
    );
    LayerJson::from_slice(doc.as_bytes()).expect("layer.json")
}

/// Everything the source serves, resident — the failing frame's state.
///
/// Walked with a depth cap, deliberately: the tree generates children PAST
/// terrain availability by design (upsampled meshes so imagery can refine),
/// so an unbounded walk heads for level 22 across the whole planet and
/// never returns — measured as a test that outlived every timeout it was
/// given. Residency to the availability ceiling is what the scenario needs.
fn all_resident(tree: &dyn TileTree) -> ResidencyView {
    let mut residency = ResidencyView::default();
    let mut stack: Vec<TileId> = tree.roots();
    while let Some(id) = stack.pop() {
        residency.insert(id);
        if id.terrain_coord().0 < FINE_MAX {
            stack.extend(tree.children(id));
        }
    }
    residency
}

/// The recorder's orbit camera: 800 m from a ground target, 30° tilt,
/// yaw in compass degrees.
fn record_camera(target: Geodetic, yaw_deg: f64) -> ViewState {
    let distance = 800.0;
    let tilt = 30f64.to_radians();
    let yaw = yaw_deg.to_radians();
    let centre = geodetic_to_ecef(target);
    let enu = enu_frame(target);
    let forward = enu.y_axis * yaw.cos() + enu.x_axis * yaw.sin();
    let eye = centre + enu.z_axis * (distance * tilt.sin()) - forward * (distance * tilt.cos());
    let dir = (centre - eye).normalize();
    let up = (enu.z_axis - dir * dir.dot(enu.z_axis)).normalize();
    ViewState::perspective(eye, dir, up, DVec2::new(1920.0, 1080.0), 45f64.to_radians())
}

/// First intersection of `origin + t·dir` with the WGS84 ellipsoid, if any.
fn hit_ground(origin: DVec3, dir: DVec3) -> Option<DVec3> {
    // Scale z so the ellipsoid becomes a sphere of radius a.
    let s = WGS84_A / WGS84_B;
    let o = DVec3::new(origin.x, origin.y, origin.z * s);
    let d = DVec3::new(dir.x, dir.y, dir.z * s);
    let (a, b, c) = (
        d.dot(d),
        2.0 * o.dot(d),
        o.dot(o) - WGS84_A * WGS84_A,
    );
    let disc = b * b - 4.0 * a * c;
    if disc < 0.0 {
        return None;
    }
    let t = (-b - disc.sqrt()) / (2.0 * a);
    (t > 0.0).then(|| origin + dir * t)
}

/// Whether some selected tile contains this geodetic point, at any level
/// the selection holds — the engine refines past terrain availability by
/// design (upsampled meshes so imagery can sharpen), so capping the walk at
/// the availability ceiling silently misses fine selections and reports
/// covered ground as bare.
fn covered(selected: &HashSet<(u32, u64, u64)>, max_level: u32, geo: Geodetic) -> bool {
    for level in 0..=max_level {
        let nx = tiles_x(level) as f64;
        let ny = tiles_y(level) as f64;
        let x = (((geo.lon + std::f64::consts::PI) / std::f64::consts::TAU) * nx)
            .floor()
            .clamp(0.0, nx - 1.0) as u64;
        let y = (((geo.lat + std::f64::consts::FRAC_PI_2) / std::f64::consts::PI) * ny)
            .floor()
            .clamp(0.0, ny - 1.0) as u64;
        if selected.contains(&(level, x, y)) {
            return true;
        }
    }
    false
}

#[test]
fn every_visible_ground_point_is_selected() {
    let tree = tuile_planetary::terrain_tree(boundary_layer());
    let residency = all_resident(tree.as_ref());
    let mut config = Config::interactive_globe(Some(4));
    config.stand_ins = false; // the recorder's exact-rendering profile

    // The recorder's target: on the ground just west of the availability
    // boundary, inside the fine window — where the flight was.
    let target = Geodetic {
        lon: (-0.10f64).to_radians(),
        lat: 48.0f64.to_radians(),
        height: 0.0,
    };

    let mut rendered_last: HashSet<TileId> = HashSet::new();
    let mut out = TraversalOutput::default();
    let mut misses: Vec<(f64, f64, f64)> = Vec::new();

    // A full pan, the recorder's own motion — every 15° of yaw (CI-sized;
    // widen locally when hunting), in steady state: each traversal sees the
    // previous one's selection.
    for yaw_tenths in (0..3600).step_by(150) {
        let yaw = f64::from(yaw_tenths) / 10.0;
        let view = record_camera(target, yaw);
        traverse(
            tree.as_ref(),
            &residency,
            &[view],
            &config,
            0,
            &rendered_last,
            &mut out,
        );
        rendered_last = out.selected.iter().map(|(t, _)| *t).collect();
        let selected: HashSet<(u32, u64, u64)> =
            out.selected.iter().map(|(t, _)| t.terrain_coord()).collect();
        let max_level = selected.iter().map(|&(z, _, _)| z).max().unwrap_or(0);

        // A grid of view rays over the full frustum.
        let p = tuile_core::traversal::ViewStateParams::from(view);
        let right = p.direction.cross(p.up).normalize();
        let tan_h = (45f64.to_radians() * 0.5).tan();
        let aspect = 1920.0 / 1080.0;
        for sy in 0..36 {
            for sx in 0..64 {
                let ndc_x = (f64::from(sx) + 0.5) / 64.0 * 2.0 - 1.0;
                let ndc_y = 1.0 - (f64::from(sy) + 0.5) / 36.0 * 2.0;
                let dir = (p.direction + right * (ndc_x * tan_h * aspect) + p.up * (ndc_y * tan_h))
                    .normalize();
                let Some(ground) = hit_ground(p.position, dir) else {
                    continue; // sky: no ground on this ray
                };
                let geo = ecef_to_geodetic(ground);
                if !covered(&selected, max_level, geo) {
                    misses.push((yaw, geo.lon.to_degrees(), geo.lat.to_degrees()));
                }
            }
        }
    }

    assert!(
        misses.is_empty(),
        "{} view rays hit ground that no selected tile covers — the magenta \
         stripe. First three (yaw°, lon°, lat°): {:?}",
        misses.len(),
        &misses[..misses.len().min(3)]
    );
}
