// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Which globe this viewer shows, and where its bytes come from.
//!
//! The choice of Cesium ion, of Bing, and of a native HTTP transport is made
//! **here** and nowhere deeper: `tuile-planetary` crosses a terrain source with
//! an imagery source and does not know either of these names. Swapping in a
//! different provider is a change to this file alone.

use std::sync::Arc;
use tuile_bing::{BingImageryProvider, BingMetadata};
use tuile_cesium_ion::{AssetEndpoint, IonClient, IonTerrainSource};
use tuile_core::raster::CachedImagery;
use tuile_core::source::{TileLoader, TileTree};
use tuile_core::storage::ContentStore;
use tuile_native_fetchers::NativeHttp;
use tuile_planetary::{globe_on, GlobeOptions, ImageryDetail};
use tuile_storage_foyer::FoyerStore;
use tuile_terrain::{CachedTerrain, TerrainHeights};

/// Resolves the Cesium-ion globe sources and crosses them through the
/// backend-agnostic `tuile-planetary`. The app decides ion + the native HTTP
/// transport here, not planetary.
pub(crate) async fn ion_globe(
    token: String,
) -> anyhow::Result<(
    Box<dyn TileTree>,
    Arc<dyn TileLoader>,
    ImageryDetail,
    Arc<TerrainHeights>,
    Option<Arc<FoyerStore>>,
    tuile_planetary::LayerBudget,
)> {
    // One pooled, cached native transport drives both ion and Bing.
    let http = Arc::new(NativeHttp::shared().await?);
    let terrain = IonTerrainSource::new(IonClient::new(Arc::clone(&http), token.clone()), 1);
    let layer = terrain.layer().await?;

    let ion2 = IonClient::new(Arc::clone(&http), token);
    let endpoint = match ion2.asset_endpoint(2).await? {
        AssetEndpoint::Imagery(e) => e,
        _ => anyhow::bail!("ion asset 2 is not imagery"),
    };
    let o = &endpoint.options;
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

    // One store, two tiers of caller: the terrain source and the imagery
    // provider each keep their own bytes in it, keyed by tile rather than by
    // URL. The server evicts decoded tiles to stay inside its GPU budget, and
    // this is what makes coming back to them cheap. A store that fails to open
    // is not worth failing the app over.
    let store = match FoyerStore::shared(&tuile_bing::cache_name(&style)).await {
        Ok(store) => Some(Arc::new(store)),
        Err(e) => {
            tracing::warn!("no tile store ({e}); every tile will be re-fetched");
            None
        }
    };

    // Decoding and resampling go to their own threads. Left on the server's
    // thread they run one at a time no matter how many loads are in flight —
    // measured at sixty-four outstanding and a single core busy.
    let decode = tuile_core::offload::threaded();

    // How many imagery layers a drape may carry is the renderer's answer, and
    // the renderer does not exist yet — the window comes after the stream. The
    // handle is passed now and filled in by `start_window`; until then it reads
    // the correctness floor.
    let budget = tuile_planetary::LayerBudget::default();
    let opts = GlobeOptions {
        imagery_slots: budget.clone(),
        ..Default::default()
    };

    let Some(store) = store else {
        let (tree, loader, detail, heights) = globe_on(terrain, bing, layer, opts, decode);
        return Ok((tree, loader, detail, heights, None, budget));
    };
    let shared = Arc::clone(&store) as Arc<dyn ContentStore>;
    let (tree, loader, detail, heights) = globe_on(
        CachedTerrain::new(terrain, Arc::clone(&shared), "ion-cwt"),
        // Namespaced by style, and it matters: the store is keyed by tile
        // coordinate, so aerial and labelled tiles for the same coordinate
        // would otherwise collide. Switching styles would then serve whichever
        // was cached first — silently, and looking exactly like a working
        // session showing the wrong picture.
        CachedImagery::new(bing, shared, tuile_bing::cache_namespace(&style)),
        layer,
        opts,
        decode,
    );
    Ok((tree, loader, detail, heights, Some(store), budget))
}
