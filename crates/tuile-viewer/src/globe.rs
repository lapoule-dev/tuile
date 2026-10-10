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

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use tuile_core::raster::{CachedImagery, ImageryProvider};
use tuile_core::source::{TileLoader, TileTree};
use tuile_core::storage::ContentStore;
use tuile_planetary::{
    globe_on, GlobeOptions, Held, ImageryDetail, LayerBudget, Sources, SwitchableImagery,
};
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
    /// The handle the window changes imagery through.
    pub switcher: Switcher,
}

/// How a switch of imagery that was asked for ended.
pub(crate) struct Switched {
    /// The layer asked for, as an index into the host's list.
    pub layer: usize,
    /// The imagery generation the globe is now draping, or why it is not.
    pub outcome: Result<u64, String>,
    /// When it was asked for — the start of the wait a person sees.
    pub asked: std::time::Instant,
}

/// Changes the imagery a running globe drapes.
///
/// Asking is one call that returns at once: resolving a layer is a round trip
/// or two to the host's service, and the window must not wait on it. The
/// resolution runs on the session's async runtime and the answer is collected
/// by the render loop ([`Switcher::poll`]) — the same shape as the location
/// request, for the same reason.
///
/// Nothing here takes anything off the screen. A resolved layer is handed to
/// the [`SwitchableImagery`] the globe was built over, and the engine does the
/// rest: every tile keeps the old picture until its new drape is resident.
/// A layer that cannot be resolved changes nothing at all.
pub(crate) struct Switcher {
    sources: Arc<dyn Sources>,
    store: Option<Arc<FoyerStore>>,
    switch: SwitchableImagery,
    runtime: tokio::runtime::Handle,
    /// Layers already resolved, so that going back to one costs nothing — and
    /// does not open a second session with a service that counts them.
    resolved: Arc<Mutex<HashMap<usize, Arc<dyn ImageryProvider>>>>,
    done: (Sender<Switched>, Receiver<Switched>),
}

impl Switcher {
    /// Starts the switch to `layers[layer]`.
    pub(crate) fn ask(&self, layer: usize, choice: &ImageryChoice) {
        let asked = std::time::Instant::now();
        let (sources, store) = (Arc::clone(&self.sources), self.store.clone());
        let (switch, resolved) = (self.switch.clone(), Arc::clone(&self.resolved));
        let (done, choice) = (self.done.0.clone(), choice.clone());
        self.runtime.spawn(async move {
            let known = resolved.lock().ok().and_then(|r| r.get(&layer).cloned());
            let provider = match known {
                Some(provider) => Ok(provider),
                None => imagery(sources.as_ref(), &choice, store.as_ref())
                    .await
                    .map_err(|e| e.to_string()),
            };
            let outcome = provider.map(|provider| {
                if let Ok(mut resolved) = resolved.lock() {
                    resolved.insert(layer, Arc::clone(&provider));
                }
                switch.switch_to(provider)
            });
            // A closed channel means the window is gone, and with it the
            // point of saying how it went.
            let _ = done.send(Switched {
                layer,
                outcome,
                asked,
            });
        });
    }

    /// The outcome of a switch that has finished resolving, if one has.
    pub(crate) fn poll(&self) -> Option<Switched> {
        self.done.1.try_recv().ok()
    }
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

/// How deep the quadtree divides when the host offers several layers.
///
/// With one layer the tree goes as deep as that imagery does. With several it
/// cannot follow the layer being shown — the tree is built once — so it goes
/// as deep as the sharpest photography in general use, and a session that
/// opened on a coarse mosaic still has the tiles a sharper layer needs when
/// it is switched to. Under a coarse layer the extra depth costs tiles only
/// where the eye is close enough to select them.
const DEPTH_FOR_ANY_LAYER: u32 = 21;

/// Resolves the host's terrain and its opening imagery layer and crosses them.
pub(crate) async fn assemble(
    sources: Arc<dyn Sources>,
    store: Option<Arc<FoyerStore>>,
    terrain: &TerrainChoice,
    layers: &[ImageryChoice],
    opening: usize,
) -> anyhow::Result<Globe> {
    let runtime = tokio::runtime::Handle::current();
    let opening_layer = layers
        .get(opening)
        .ok_or_else(|| anyhow::anyhow!("the host offers no imagery layer"))?;
    let (sources_kept, sources) = (Arc::clone(&sources), sources.as_ref());

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
    let picture = imagery(sources, opening_layer, store.as_ref()).await?;
    tracing::info!(layer = %opening_layer.name, "imagery");
    let deepest = picture.tiling_scheme().maximum_level;
    // The globe is built over a switch, not over the layer: the layer can
    // then be changed under a running session, and the engine refreshes each
    // tile behind the old picture. See `tuile_planetary::SwitchableImagery`.
    let switch = SwitchableImagery::new(Arc::clone(&picture));

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
        max_level: Some(if layers.len() > 1 {
            deepest.max(DEPTH_FOR_ANY_LAYER)
        } else {
            deepest
        }),
        ..Default::default()
    };
    let (tree, loader, detail, heights) =
        globe_on(Held(ground), switch.clone(), layer, opts, decode);
    Ok(Globe {
        tree,
        loader,
        detail,
        heights,
        budget,
        switcher: Switcher {
            sources: sources_kept,
            store: store.clone(),
            switch,
            runtime,
            resolved: Arc::new(Mutex::new(HashMap::from([(opening, picture)]))),
            done: channel(),
        },
        store,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Sources that say what was asked of them, and have nothing.
    #[derive(Default)]
    struct Asked(Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl Sources for Asked {
        async fn terrain(
            &self,
            asset: i64,
        ) -> Result<(tuile_terrain::LayerJson, Arc<dyn TerrainSource>), String> {
            self.0
                .lock()
                .expect("asked")
                .push(format!("terrain {asset}"));
            Err("no terrain here".to_owned())
        }

        async fn imagery(&self, asset: i64) -> Result<Arc<dyn ImageryProvider>, String> {
            self.0
                .lock()
                .expect("asked")
                .push(format!("imagery {asset}"));
            Err("no imagery here".to_owned())
        }
    }

    /// The host's numbers are the numbers its sources are asked for, and a
    /// refusal reaches the person in the host's own words, with what it was
    /// about.
    #[test]
    fn the_globe_asks_the_hosts_sources_for_the_hosts_assets() {
        let asked = Arc::new(Asked::default());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime");
        let terrain = TerrainChoice {
            asset: 41,
            cache: "t".into(),
        };
        let layer = ImageryChoice::new("survey", "Survey", 77, "© someone");
        let layers = [layer.clone()];
        let refused = runtime
            .block_on(assemble(
                Arc::clone(&asked) as Arc<dyn Sources>,
                None,
                &terrain,
                &layers,
                0,
            ))
            .err()
            .expect("there is no terrain");
        assert_eq!(refused.to_string(), "terrain: no terrain here");
        assert_eq!(*asked.0.lock().expect("asked"), ["terrain 41"]);

        let refused = futures_executor::block_on(imagery(asked.as_ref(), &layer, None))
            .err()
            .expect("there is no imagery");
        assert_eq!(refused.to_string(), "imagery \"Survey\": no imagery here");
        assert_eq!(
            *asked.0.lock().expect("asked"),
            ["terrain 41", "imagery 77"]
        );
    }
}
