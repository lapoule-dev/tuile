// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The globe a session shows, crossed from whatever the host's sources answer
//! with.
//!
//! Nothing here knows a provider: terrain and imagery arrive as the two traits
//! `tuile-planetary` crosses, asked for by asset number through
//! [`Sources`]. What this module adds is the part every host would otherwise
//! write again — the tile store between runs, with each source filed under its
//! own namespace, and the decode work moved off the server's thread.

use std::sync::Arc;

use tuile_core::raster::{CachedImagery, ImageryProvider};
use tuile_core::source::{TileLoader, TileTree};
use tuile_core::storage::ContentStore;
use tuile_planetary::{globe_on, GlobeOptions, Held, ImageryDetail, LayerBudget, Sources};
use tuile_storage_foyer::FoyerStore;
use tuile_terrain::{CachedTerrain, TerrainHeights, TerrainSource};

use crate::embed::{ImageryChoice, TerrainChoice};

/// Everything the session needs from the crossing.
pub(crate) struct Globe {
    pub tree: Box<dyn TileTree>,
    pub loader: Arc<dyn TileLoader>,
    pub detail: ImageryDetail,
    pub heights: Arc<TerrainHeights>,
    /// Kept so it can be flushed on the way out: a store that is merely
    /// dropped leaves the disk as cold as it found it.
    pub store: Option<Arc<FoyerStore>>,
    pub budget: LayerBudget,
}

/// One imagery layer of the host's, through the store when there is one.
///
/// Namespaced by layer, and it matters: the store is keyed by tile address, so
/// two layers under one namespace would serve whichever was stored first —
/// silently, and looking exactly like a working session showing the wrong
/// picture.
pub(crate) async fn imagery(
    sources: &dyn Sources,
    choice: &ImageryChoice,
    store: Option<&Arc<FoyerStore>>,
) -> anyhow::Result<Arc<dyn ImageryProvider>> {
    let provider = sources
        .imagery(choice.asset)
        .await
        .map_err(|why| anyhow::anyhow!("imagery {:?}: {why}", choice.name))?;
    Ok(match store {
        Some(store) => Arc::new(CachedImagery::new(
            Held(provider),
            Arc::clone(store) as Arc<dyn ContentStore>,
            choice.cache.clone(),
        )),
        None => provider,
    })
}

/// Resolves the host's terrain and its opening imagery layer and crosses them.
/// The tile store an application keeps between runs, or `None` with a line
/// saying so.
///
/// One store, several callers: the terrain source and each imagery layer keep
/// their own bytes in it, keyed by tile rather than by URL. The server evicts
/// decoded tiles to stay inside its GPU budget, and this is what makes coming
/// back to them cheap. A store that fails to open is not worth failing the
/// application over.
pub(crate) async fn open_the_store(name: &str) -> Option<Arc<FoyerStore>> {
    match FoyerStore::shared(name).await {
        Ok(store) => Some(Arc::new(store)),
        Err(e) => {
            tracing::warn!("no tile store ({e}); every tile will be re-fetched");
            None
        }
    }
}

pub(crate) async fn assemble(
    sources: &dyn Sources,
    store: Option<Arc<FoyerStore>>,
    terrain: &TerrainChoice,
    opening: &ImageryChoice,
) -> anyhow::Result<Globe> {

    let (layer, ground) = sources
        .terrain(terrain.asset)
        .await
        .map_err(|why| anyhow::anyhow!("terrain: {why}"))?;
    let ground: Arc<dyn TerrainSource> = match &store {
        Some(store) => Arc::new(CachedTerrain::new(
            Held(ground),
            Arc::clone(store) as Arc<dyn ContentStore>,
            terrain.cache.clone(),
        )),
        None => ground,
    };
    let picture = imagery(sources, opening, store.as_ref()).await?;
    tracing::info!(layer = %opening.name, "imagery");

    // Decoding and resampling go to their own threads. Left on the server's
    // thread they run one at a time no matter how many loads are in flight —
    // measured at sixty-four outstanding and a single core busy.
    let decode = tuile_core::offload::threaded();

    // How many imagery layers a drape may carry is the renderer's answer, and
    // the renderer does not exist yet — the window comes after the stream. The
    // handle is passed now and filled in by `start_window`; until then it reads
    // the correctness floor.
    let budget = LayerBudget::default();
    let opts = GlobeOptions {
        imagery_slots: budget.clone(),
        ..Default::default()
    };
    let (tree, loader, detail, heights) =
        globe_on(Held(ground), Held(picture), layer, opts, decode);
    Ok(Globe {
        tree,
        loader,
        detail,
        heights,
        store,
        budget,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Sources that say what was asked of them, and have nothing.
    #[derive(Default)]
    struct Asked(Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl Sources for Asked {
        async fn terrain(
            &self,
            asset: i64,
        ) -> Result<(tuile_terrain::LayerJson, Arc<dyn TerrainSource>), String> {
            self.0.lock().expect("asked").push(format!("terrain {asset}"));
            Err("no terrain here".to_owned())
        }

        async fn imagery(&self, asset: i64) -> Result<Arc<dyn ImageryProvider>, String> {
            self.0.lock().expect("asked").push(format!("imagery {asset}"));
            Err("no imagery here".to_owned())
        }
    }

    /// The host's numbers are the numbers its sources are asked for, and a
    /// refusal reaches the person in the host's own words, with what it was
    /// about.
    #[test]
    fn the_globe_asks_the_hosts_sources_for_the_hosts_assets() {
        let asked = Asked::default();
        let terrain = TerrainChoice {
            asset: 41,
            cache: "t".into(),
        };
        let layer = ImageryChoice::new("survey", "Survey", 77, "© someone");
        let refused = futures_executor::block_on(assemble(&asked, None, &terrain, &layer))
            .err()
            .expect("there is no terrain");
        assert_eq!(refused.to_string(), "terrain: no terrain here");
        assert_eq!(*asked.0.lock().expect("asked"), ["terrain 41"]);

        let refused = futures_executor::block_on(imagery(&asked, &layer, None))
            .err()
            .expect("there is no imagery");
        assert_eq!(refused.to_string(), "imagery \"Survey\": no imagery here");
        assert_eq!(
            *asked.0.lock().expect("asked"),
            ["terrain 41", "imagery 77"]
        );
    }
}
