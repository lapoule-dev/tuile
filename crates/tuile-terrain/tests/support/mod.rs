// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Made-up terrain for the seam tests: one relief, measured by each tile on
//! a grid of its own, so that two tiles state the line they share from
//! different vertices — as a real source's tiles do.

#![allow(dead_code)]

use tuile_core::geo::{geodetic_to_ecef, Geodetic, WGS84_A};
use tuile_core::seam::{Meeting, Side};
use tuile_core::DecodedTileContent;
use tuile_terrain::{GeographicTilingScheme, Header, QuantizedMesh, TileCoord};

/// Where the scene is, in degrees.
pub const LON: f64 = -2.86;
pub const LAT: f64 = 52.51;

/// Metres over the ellipsoid: hills a couple of kilometres across and 120 m
/// high, with smaller ones of 35 m on them.
pub fn relief(lon: f64, lat: f64) -> f64 {
    let x = lon * WGS84_A * LAT.to_radians().cos();
    let y = lat * WGS84_A;
    let tau = std::f64::consts::TAU;
    300.0
        + 120.0 * (tau * x / 1700.0).sin() * (tau * y / 2300.0).cos()
        + 35.0 * (tau * x / 430.0 + 1.0).sin() * (tau * y / 510.0).sin()
}

/// The tile of `level` that holds a place, in degrees.
pub fn tile_at(level: u32, lon: f64, lat: f64) -> TileCoord {
    TileCoord::new(
        level,
        ((lon + 180.0) / 360.0 * (2u64 << level) as f64) as u64,
        ((lat + 90.0) / 180.0 * (1u64 << level) as f64) as u64,
    )
}

/// A terrain tile as a source holds it: the relief on a grid of `steps`
/// spacings a side, with its four edge lists.
pub fn measured(coord: TileCoord, steps: usize) -> QuantizedMesh {
    let rect = GeographicTilingScheme::default().tile_rect(coord);
    let n = steps + 1;
    let (mut u, mut v, mut metres) = (Vec::new(), Vec::new(), Vec::new());
    for j in 0..n {
        for i in 0..n {
            let (fu, fv) = (i as f64 / steps as f64, j as f64 / steps as f64);
            u.push(fu);
            v.push(fv);
            metres.push(relief(
                rect.west + fu * rect.width(),
                rect.south + fv * rect.height(),
            ));
        }
    }
    let low = metres.iter().copied().fold(f64::INFINITY, f64::min);
    let high = metres.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let mut indices = Vec::new();
    for j in 0..steps {
        for i in 0..steps {
            let a = (j * n + i) as u32;
            let (b, c, d) = (a + 1, a + n as u32, a + n as u32 + 1);
            indices.extend_from_slice(&[a, b, c, c, b, d]);
        }
    }
    let (lon, lat) = rect.center();
    let centre = geodetic_to_ecef(Geodetic {
        lon,
        lat,
        height: (low + high) / 2.0,
    });
    let line = |f: &dyn Fn(usize) -> usize| (0..n).map(|k| f(k) as u32).collect::<Vec<u32>>();
    QuantizedMesh {
        header: Header {
            center: centre.to_array(),
            min_height: low as f32,
            max_height: high as f32,
            bounding_sphere_center: centre.to_array(),
            bounding_sphere_radius: rect.height() * WGS84_A,
            horizon_occlusion: [0.0; 3],
        },
        u,
        v,
        height: metres.iter().map(|m| (m - low) / (high - low)).collect(),
        indices,
        normals: None,
        edges: [
            line(&|k| k * n),
            line(&|k| k),
            line(&|k| k * n + steps),
            line(&|k| (n - 1) * n + k),
        ],
        metadata_available: None,
        cut: 0,
    }
}

/// The tiles of `level` under `of` that lie against its east or west side,
/// south to north.
pub fn along(of: TileCoord, level: u32, side: Side) -> Vec<TileCoord> {
    let down = level - of.level;
    let x = match side {
        Side::East => ((of.x + 1) << down) - 1,
        _ => of.x << down,
    };
    (0..1u64 << down)
        .map(|j| TileCoord::new(level, x, (of.y << down) + j))
        .collect()
}

/// `west` and `east`, two tiles on either side of a meridian, where they
/// meet: `None` if they share none of it.
pub fn meeting<'a>(
    west: (TileCoord, &'a DecodedTileContent),
    east: (TileCoord, &'a DecodedTileContent),
) -> Option<(Meeting<'a>, Meeting<'a>)> {
    const DEEP: u32 = 30;
    let rows = |c: TileCoord| {
        let shift = DEEP - c.level;
        ((c.y << shift) as f64, ((c.y + 1) << shift) as f64)
    };
    let ((w0, w1), (e0, e1)) = (rows(west.0), rows(east.0));
    let (from, to) = (w0.max(e0), w1.min(e1));
    if from >= to {
        return None;
    }
    let span = |(a, b): (f64, f64)| ((from - a) / (b - a), (to - a) / (b - a));
    Some((
        Meeting {
            content: west.1,
            side: Side::East,
            span: span((w0, w1)),
        },
        Meeting {
            content: east.1,
            side: Side::West,
            span: span((e0, e1)),
        },
    ))
}

/// A quantized-mesh tile's bytes, as a source serves them: enough of the
/// format for `tuile_terrain::decode` to read the mesh back.
pub fn encoded(mesh: &QuantizedMesh) -> Vec<u8> {
    // The format's indices are high-water-mark coded, which wants the
    // vertices numbered in the order the triangles first use them.
    let mut order: Vec<u32> = Vec::new();
    let mut renumbered = vec![u32::MAX; mesh.vertex_count()];
    for &i in &mesh.indices {
        if renumbered[i as usize] == u32::MAX {
            renumbered[i as usize] = order.len() as u32;
            order.push(i);
        }
    }
    let mut out = Vec::new();
    let h = &mesh.header;
    for v in h.center {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&h.min_height.to_le_bytes());
    out.extend_from_slice(&h.max_height.to_le_bytes());
    for v in h.bounding_sphere_center {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&h.bounding_sphere_radius.to_le_bytes());
    for v in h.horizon_occlusion {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&(order.len() as u32).to_le_bytes());
    for values in [&mesh.u, &mesh.v, &mesh.height] {
        let mut before = 0i32;
        for &i in &order {
            let now = (values[i as usize] * 32767.0).round() as i32;
            let delta = now - before;
            before = now;
            let zigzag = ((delta << 1) ^ (delta >> 31)) as u16;
            out.extend_from_slice(&zigzag.to_le_bytes());
        }
    }
    out.extend_from_slice(&((mesh.indices.len() / 3) as u32).to_le_bytes());
    let mut highest = 0u32;
    for &i in &mesh.indices {
        let i = renumbered[i as usize];
        out.extend_from_slice(&((highest - i) as u16).to_le_bytes());
        if i == highest {
            highest += 1;
        }
    }
    for edge in &mesh.edges {
        out.extend_from_slice(&(edge.len() as u32).to_le_bytes());
        for &i in edge {
            out.extend_from_slice(&(renumbered[i as usize] as u16).to_le_bytes());
        }
    }
    out
}
