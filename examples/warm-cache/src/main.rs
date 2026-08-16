// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Fills the tile store with the coarse pyramid, once, so a session never
//! starts cold.
//!
//! The shallow levels are what every fallback lands on: a tile that has not
//! arrived is drawn by its nearest resident ancestor, and if that walk reaches
//! the top and finds nothing, the ground is bare. Leaving them to be discovered
//! by the camera builds the safety net out of exactly the tiles a fast movement
//! has not fetched yet — measured on ground already flown several times, level
//! 5 held 291 of its 1024 tiles and level 3 held none at all.
//!
//! A job rather than something the viewer does at startup. It is slow, it is
//! one-off, its result is on disk and shared by every later session, and it has
//! no business competing for bandwidth with the frame someone is waiting on.
//!
//! ```text
//! cargo run --release -p warm-cache          # levels 0..=5
//! cargo run --release -p warm-cache -- 6     # deeper, at four times the cost
//! ```
//!
//! The whole planet at level 5 is `4^5` imagery tiles — about 40 MiB
//! compressed, once. Every level below is a quarter of the one above, so the
//! sum is a third again. Level 8 would be 65 536 tiles and is not a warm-up.

use std::sync::Arc;

use tuile_bing::{BingImageryProvider, BingMetadata};
use tuile_cesium_ion::{AssetEndpoint, IonClient, IonTerrainSource};
use tuile_core::raster::CachedImagery;
use tuile_core::storage::ContentStore;
use tuile_planetary::{globe_on, GlobeOptions};
use tuile_storage_foyer::FoyerStore;
use tuile_terrain::CachedTerrain;

/// How deep to go when nothing is asked for. Matches the viewer's pinned level,
/// which is what makes the two agree about what "coarse" means.
const DEFAULT_THROUGH: u32 = 5;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .without_time()
        .with_target(false)
        .init();

    let through: u32 = std::env::args()
        .nth(1)
        .map_or(Ok(DEFAULT_THROUGH), |a| a.parse())
        .map_err(|_| anyhow::anyhow!("usage: warm-cache [through-level]"))?;
    // Past this the count stops being a warm-up and starts being a download of
    // the planet: level 8 alone is 65 536 imagery tiles.
    anyhow::ensure!(through <= 8, "level {through} is too deep for a warm-up");

    let token = std::env::var("CESIUM_ION_TOKEN")
        .map_err(|_| anyhow::anyhow!("set CESIUM_ION_TOKEN (env or .env)"))?;

    let http = Arc::new(tuile_native_fetchers::NativeHttp::shared().await?);
    let terrain = IonTerrainSource::new(IonClient::new(Arc::clone(&http), token.clone()), 1);
    let layer = terrain.layer().await?;

    let ion = IonClient::new(Arc::clone(&http), token);
    let endpoint = match ion.asset_endpoint(2).await? {
        AssetEndpoint::Imagery(e) => e,
        _ => anyhow::bail!("ion asset 2 is not imagery"),
    };
    let o = &endpoint.options;
    // The same decision the viewer makes, from the same function: a warm-up
    // that filled a differently named cache warmed nothing anyone reads.
    let style = tuile_bing::map_style(o.map_style.as_deref());
    tracing::info!(style, "imagery style");
    let meta_url = BingMetadata::metadata_url(
        o.url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("bing url"))?,
        &style,
        o.key
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("bing key"))?,
    );
    let bing = BingImageryProvider::from_metadata_url(Arc::clone(&http), &meta_url)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // The same store the viewer opens, under the same name, or this warms
    // something nobody will read.
    let store = Arc::new(FoyerStore::shared(&tuile_bing::cache_name(&style)).await?);
    let shared = Arc::clone(&store) as Arc<dyn ContentStore>;
    let (_tree, loader, _detail, _heights) = globe_on(
        CachedTerrain::new(terrain, Arc::clone(&shared), "ion-cwt"),
        CachedImagery::new(bing, shared, tuile_bing::cache_namespace(&style)),
        layer,
        GlobeOptions::default(),
        // A batch job is where the decode threads pay best: nothing here is
        // waiting on a frame, so every core can go to the work.
        tuile_core::offload::threaded(),
    );

    tracing::info!(through, "warming the tile store");
    loader.warm_up(through).await;

    // Foyer buffers its writes: a store that is merely dropped leaves the disk
    // exactly as cold as it found it, and this whole job with it.
    let m = tuile_core::metrics::metrics();
    match store.close().await {
        Ok(()) => tracing::info!(
            "store flushed: {} fetched, {} already cached, {:.0} MiB from the network | \
             absent: {} terrain, {} imagery (remembered, not re-asked)",
            m.store_misses.get(),
            m.store_hits.get(),
            m.store_bytes_fetched.get() as f64 / (1024.0 * 1024.0),
            m.terrain_absent.get(),
            m.imagery_absent.get(),
        ),
        Err(e) => anyhow::bail!("the store was not flushed ({e}); nothing was kept"),
    }
    Ok(())
}
