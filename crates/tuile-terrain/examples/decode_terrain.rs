// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Decodes a (gunzipped) `.terrain` tile and prints stats — the validation
//! tool for the quantized-mesh decoder on real Cesium World Terrain data.
//!
//! ```text
//! cargo run -p tuile-terrain --example decode_terrain -- <tile.terrain> <level> <x> <y>
//! ```

use tuile_core::geo::{ecef_to_geodetic, Geodetic};
use tuile_terrain::{decode, to_decoded, GeographicTilingScheme, TileCoord};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: decode_terrain <tile.terrain> <level> <x> <y>");
    let level: u32 = args.next().expect("level").parse().expect("level");
    let x: u64 = args.next().expect("x").parse().expect("x");
    let y: u64 = args.next().expect("y").parse().expect("y");

    let bytes = std::fs::read(&path).expect("read tile");
    let mesh = decode(&bytes).expect("decode quantized-mesh");

    let rect = GeographicTilingScheme::default().tile_rect(TileCoord::new(level, x, y));
    println!("tile {level}/{x}/{y}");
    println!(
        "  rect: lon [{:.3}, {:.3}]  lat [{:.3}, {:.3}] (rad)",
        rect.west, rect.east, rect.south, rect.north
    );
    println!(
        "  header center ECEF: ({:.1}, {:.1}, {:.1})",
        mesh.header.center[0], mesh.header.center[1], mesh.header.center[2]
    );
    println!(
        "  height range: {:.1} .. {:.1} m",
        mesh.header.min_height, mesh.header.max_height
    );
    println!("  vertices: {}", mesh.vertex_count());
    println!("  triangles: {}", mesh.indices.len() / 3);
    println!(
        "  normals: {}",
        if mesh.normals.is_some() {
            "yes (octvertexnormals)"
        } else {
            "no"
        }
    );
    println!(
        "  skirt edges: W={} S={} E={} N={}",
        mesh.edges[0].len(),
        mesh.edges[1].len(),
        mesh.edges[2].len(),
        mesh.edges[3].len()
    );

    // Validate: every vertex falls inside the tile rectangle.
    let mut min_h = f64::MAX;
    let mut max_h = f64::MIN;
    let mut outside = 0;
    for i in 0..mesh.vertex_count() {
        let lon = rect.west + (rect.east - rect.west) * mesh.u[i];
        let lat = rect.south + (rect.north - rect.south) * mesh.v[i];
        let h = mesh.header.min_height as f64
            + (mesh.header.max_height - mesh.header.min_height) as f64 * mesh.height[i];
        min_h = min_h.min(h);
        max_h = max_h.max(h);
        if lon < rect.west - 1e-6
            || lon > rect.east + 1e-6
            || lat < rect.south - 1e-6
            || lat > rect.north + 1e-6
        {
            outside += 1;
        }
    }
    println!("  decoded height span: {min_h:.1} .. {max_h:.1} m");
    println!("  vertices outside rect: {outside}");

    // Build render-ready geometry and report normal sanity.
    let decoded = to_decoded(&mesh, &rect, 1000.0);
    let m = &decoded.meshes[0];
    println!(
        "  -> DecodedTileContent: {} positions, {} indices (with skirts)",
        m.positions.len(),
        m.indices.len()
    );
    if let Some(ns) = &m.normals {
        let bad = ns
            .iter()
            .filter(|n| {
                let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                (len - 1.0).abs() > 1e-2
            })
            .count();
        println!("  non-unit normals: {bad} / {}", ns.len());
    }

    // Where on Earth is this tile centered?
    let c = ecef_to_geodetic(glam::DVec3::from(mesh.header.center));
    let _ = Geodetic { ..c };
    println!(
        "  tile center geodetic: lon {:.2}°, lat {:.2}°",
        c.lon.to_degrees(),
        c.lat.to_degrees()
    );
    println!("OK — real quantized-mesh decoded cleanly.");
}
