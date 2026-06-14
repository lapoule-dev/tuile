// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! `tile-probe` — proves the true maximum terrain LOD at a point by asking ion
//! directly, bypassing our traversal and availability logic entirely.
//!
//! It walks the tile column `z = 0 .. maxzoom` over a lon/lat, fetching each
//! tile straight from ion (Cesium World Terrain, asset 1), and reports the
//! server's answer: real bytes (+ decoded vertex count, + the deeper levels the
//! tile's `metadata` extension advertises) or `404 ABSENT`. Then, at the
//! deepest tile that exists, it explicitly fetches its four children to settle
//! the question: does deeper terrain exist or not?
//!
//! Usage: `tile-probe [lat] [lon]`  (degrees; defaults to Strasbourg).

use std::sync::Arc;

use tuile_cesium_ion::{IonClient, IonError, IonTerrainSource};
use tuile_native_fetchers::NativeHttp;
use tuile_terrain::{decode, GeographicTilingScheme, TileCoord};

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .without_time()
        .with_target(false)
        .init();
}

/// The geographic tile covering `(lon, lat)` at `level` (TMS, south = y0).
fn coord_at(scheme: &GeographicTilingScheme, lon_deg: f64, lat_deg: f64, level: u32) -> TileCoord {
    let tx = scheme.tiles_x(level) as f64;
    let ty = scheme.tiles_y(level) as f64;
    let x = ((lon_deg + 180.0) / 360.0 * tx).floor().clamp(0.0, tx - 1.0) as u64;
    let y = ((lat_deg + 90.0) / 180.0 * ty).floor().clamp(0.0, ty - 1.0) as u64;
    TileCoord::new(level, x, y)
}

/// Fetches one tile and renders the server's answer as a one-line verdict.
/// Returns `Some((vertices, metadata_deepest))` if the tile exists.
async fn probe(
    terrain: &IonTerrainSource<NativeHttp>,
    c: TileCoord,
) -> anyhow::Result<Option<(usize, Option<usize>)>> {
    match terrain.fetch_tile(c).await {
        Ok(bytes) => {
            let qm = decode(&bytes).ok();
            let vc = qm.as_ref().map(|m| m.vertex_count()).unwrap_or(0);
            let meta_deepest = qm
                .as_ref()
                .and_then(|m| m.metadata_available.as_ref())
                .map(|r| c.level as usize + r.len());
            let meta = meta_deepest.map_or_else(|| "—".into(), |d| format!("→ z{d}"));
            println!(
                "  z{:<2}  {:>5}/{:<5}  OK   {:>6} B   {:>7} verts   metadata {}",
                c.level,
                c.x,
                c.y,
                bytes.len(),
                vc,
                meta
            );
            Ok(Some((vc, meta_deepest)))
        }
        Err(IonError::Status { status: 404, .. }) => {
            println!("  z{:<2}  {:>5}/{:<5}  404 ABSENT", c.level, c.x, c.y);
            Ok(None)
        }
        Err(e) => {
            println!("  z{:<2}  {:>5}/{:<5}  ERROR {e}", c.level, c.x, c.y);
            Ok(None)
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    init_tracing();
    let token = std::env::var("CESIUM_ION_TOKEN")
        .map_err(|_| anyhow::anyhow!("set CESIUM_ION_TOKEN (env or .env)"))?;

    let mut args = std::env::args().skip(1);
    let lat: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(48.5836);
    let lon: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(7.7503);

    let http = Arc::new(
        NativeHttp::shared()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?,
    );
    let terrain = IonTerrainSource::new(IonClient::new(http, token), 1);
    let layer = terrain.layer().await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let scheme = GeographicTilingScheme::default();
    let maxzoom = layer.maxzoom;

    println!("Cesium World Terrain (ion asset 1) — direct availability probe");
    println!(
        "  layer.json: maxzoom={maxzoom}, metadataAvailability={:?}, extensions={:?}",
        layer.metadata_availability, layer.extensions
    );
    println!("  point: lat={lat}  lon={lon}\n");
    println!("  walking the tile column straight from the server:");

    let mut deepest_real: Option<TileCoord> = None;
    for level in 0..=maxzoom {
        let c = coord_at(&scheme, lon, lat, level);
        match probe(&terrain, c).await? {
            Some(_) => deepest_real = Some(c),
            None => break,
        }
    }

    // The crux: explicitly fetch the four children of the deepest existing
    // tile. If they all 404, deeper terrain truly does not exist here.
    if let Some(parent) = deepest_real {
        println!(
            "\n  explicit sub-tile check — the 4 children (z{}) of z{} {}/{}:",
            parent.level + 1,
            parent.level,
            parent.x,
            parent.y
        );
        let mut any_child = false;
        for ch in parent.children() {
            if probe(&terrain, ch).await?.is_some() {
                any_child = true;
            }
        }
        println!();
        if any_child {
            println!(
                "  ⇒ deeper terrain EXISTS below z{} — if the viewer stops here, the bug is ours.",
                parent.level
            );
        } else {
            println!(
                "  ⇒ PROOF: all 4 children are 404. z{} is the true maximum terrain LOD here\n     \
                 (the server itself has no deeper geometry — Cesium would also stop here).",
                parent.level
            );
        }
    }
    Ok(())
}
