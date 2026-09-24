// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Assembles a streaming globe from Cesium ion, behind the boundary.
//!
//! Every other consumer of the core resolves its own sources — that is the
//! point of the `TileTree` and `TileLoader` seams, and [`Session::new`] still
//! takes them ready-made. This module exists because *this* host cannot: a C++
//! plugin has no way to call `tuile_planetary::globe`, and asking it to
//! reimplement ion's endpoint dance and Bing's metadata document across an FFI
//! boundary would be worse than doing it here.
//!
//! So the facade is specialised for its host, per `docs/01-architecture.md`,
//! and the specialisation is exactly this: one turnkey constructor.

use std::sync::Arc;
use std::time::Duration;

use tuile_bing::{BingImageryProvider, BingMetadata};
use tuile_cesium_ion::{tms::TmsImagery, AssetEndpoint, IonClient, IonTerrainSource};
use tuile_core::offload;
use tuile_native_fetchers::{NativeHttp, RetryConfig, TransportConfig};
use tuile_planetary::{globe_on, GlobeOptions, ImageryDetail, LayerBudget};

use crate::session::{Session, SessionConfig};

/// How many levels finer than the terrain the baked imagery may go.
///
/// Default +1, and that number is a MEMORY decision as much as a sharpness
/// one: each extra level quadruples the imagery held per terrain tile (+4
/// run locally ballooned to 60 GB allocated on a frame that never
/// converged; measured the hard way). `TUILE_IMAGERY_BOOST=0` disables the
/// boost outright — imagery exactly matches the terrain level — and a farm
/// job raises it to taste, where the RAM is real. Whatever the value, it
/// stays proportional per tile: one imagery level per terrain level, the
/// How deep the quadtree may divide, from `TUILE_MAX_LEVEL`.
///
/// Unset means "as deep as the imagery provider goes", which is the right
/// answer for a render. It is a knob because a diagnostic run wants to ask
/// what a shallower globe would have selected, and because a number nobody
/// can vary is a number nobody can rule out.
fn max_level() -> Option<u32> {
    std::env::var("TUILE_MAX_LEVEL")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|level| *level > 0)
}

/// checkerboard's cure.
///
/// Publique parce que le digest de scène doit la lire.
///
/// Ce plafond décide de combien de niveaux l'imagerie descend sous le terrain,
/// donc il décide du CONTENU d'un pack. Il n'entrait pourtant pas dans le
/// digest : mesuré le 17 septembre 2026, deux cuissons de la même trajectoire
/// à boost 1 et boost 2 portaient le même nom de scène, `9fb2b0f3559debc6`, et
/// se seraient donc répondu l'une pour l'autre. Il n'est pas lu par
/// `exact_traversal` — c'est une option du chargeur, pas de la traversée — donc
/// il fallait aller le chercher ici.
pub fn imagery_boost_cap() -> u32 {
    std::env::var("TUILE_IMAGERY_BOOST")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1)
}

/// ion's asset id for Cesium World Terrain.
pub const CESIUM_WORLD_TERRAIN: i64 = 1;
/// ion's asset id for Bing Aerial imagery.
pub const BING_AERIAL: i64 = 2;

/// What a host must state to open a globe.
#[derive(Debug, Clone)]
pub struct GlobeConfig {
    /// An ion access token. Never logged, never stored.
    pub ion_token: String,
    /// The terrain asset. [`CESIUM_WORLD_TERRAIN`] unless you know otherwise.
    pub terrain_asset_id: i64,
    /// The imagery asset, or `None` for terrain only — the geometry debug view.
    pub imagery_asset_id: Option<i64>,
    /// Where to keep the on-disk tile cache. `None` uses the per-user default.
    ///
    /// Worth setting deliberately on a render farm: every node starting with a
    /// cold cache multiplies the traffic to ion and Bing by the node count, and
    /// on a shot whose frames overlap heavily that is the dominant cost long
    /// before the renderer is.
    pub cache_dir: Option<std::path::PathBuf>,
    /// What the caller already holds, so its layers are never fetched.
    ///
    /// A bake writing a pack is the caller that has one: after the first frame
    /// an orbit re-selects almost the same ground, and every one of those tiles
    /// is already stored. See [`tuile_planetary::HeldDrape`], and
    /// [`tuile_core::raster::drape_identity`] for what an identity is.
    pub held_drape: Option<tuile_planetary::HeldDrape>,
    /// Where source tiles are kept between runs and between machines, keyed
    /// by source and tile address; `None` fetches every tile from its source.
    ///
    /// The terrain is cached under the namespace [`source_namespace`] of its
    /// asset, the imagery under its own. What is stored is what the source
    /// served, so a pack baked through a cache is the pack baked without one.
    pub tile_cache: Option<TileCache>,
    pub session: SessionConfig,
}

/// A [`ContentStore`](tuile_core::storage::ContentStore) the globe caches
/// source tiles in.
#[derive(Clone)]
pub struct TileCache(pub Arc<dyn tuile_core::storage::ContentStore>);

impl std::fmt::Debug for TileCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TileCache")
    }
}

/// The cache namespace of a source asset: `asset-1` for the terrain asset 1.
/// A tile store names its layers the same way.
pub fn source_namespace(asset_id: i64) -> String {
    format!("asset-{asset_id}")
}

impl GlobeConfig {
    /// The common case: World Terrain draped with Bing Aerial.
    pub fn new(ion_token: impl Into<String>) -> Self {
        Self {
            ion_token: ion_token.into(),
            terrain_asset_id: CESIUM_WORLD_TERRAIN,
            imagery_asset_id: Some(BING_AERIAL),
            cache_dir: None,
            held_drape: None,
            tile_cache: None,
            session: SessionConfig::default(),
        }
    }
}

/// Why a globe could not be opened.
///
/// Deliberately distinct from a frame failing. These are all configuration or
/// connectivity problems that happen once, before any geometry exists, and a
/// host should report them differently from a tile that did not arrive.
#[derive(Debug, thiserror::Error)]
pub enum GlobeError {
    #[error("no ion token was supplied")]
    MissingToken,
    #[error("starting the tokio runtime: {0}")]
    Runtime(#[from] std::io::Error),
    #[error("the native HTTP transport: {0}")]
    Transport(String),
    #[error("ion: {0}")]
    Ion(String),
    #[error("ion asset {0} is not imagery")]
    NotImagery(i64),
    #[error("bing: {0}")]
    Bing(String),
    /// ion proxies an imagery provider this crate has no connector for.
    ///
    /// Named rather than mistreated: every imagery endpoint used to be handed
    /// to the Bing path, so an Azure or Google asset failed with "endpoint has
    /// no key" — a message about Bing, for an asset that is not Bing.
    #[error("ion asset {asset} is {kind} imagery, and this build has no connector for it")]
    UnsupportedImagery { asset: i64, kind: String },
    #[error("ion imagery: {0}")]
    Imagery(String),
}

impl Session {
    /// Opens a streaming globe on ion, resolving its sources first.
    ///
    /// Blocks: resolving means fetching an ion endpoint and a Bing metadata
    /// document, and there is nothing to hand back until they arrive. Call it
    /// once, off whatever thread the host can afford to block.
    pub fn globe(config: GlobeConfig) -> Result<Self, GlobeError> {
        if config.ion_token.trim().is_empty() {
            return Err(GlobeError::MissingToken);
        }

        let runtime = Session::runtime()?;
        // Sources resolve on the session's own runtime rather than a temporary
        // one, so the connection pool and cache that serve this call are the
        // same ones that will serve every tile afterwards.
        let (tree, loader, detail) = runtime.block_on(resolve(&config))?;

        let mut session_config = config.session.clone();
        session_config.dataset = config.dataset_name();
        let mut session = Session::from_parts(runtime, tree, loader, session_config)?;
        // The session drives imagery resolution from the camera each frame —
        // decoupled from terrain LOD, so exposure seams between imagery
        // capture batches stop lining up with terrain level boundaries.
        session.set_imagery_detail(detail);
        Ok(session)
    }
}

impl GlobeConfig {
    /// A stable name for the data this configuration reads.
    ///
    /// Derived from the ion asset ids and nothing else — not the token, not the
    /// cache directory, not the screen-space error. Those change *how* the same
    /// tiles are fetched, and two sessions differing only in them serve
    /// identical imagery, so sharing a name is correct rather than a collision.
    ///
    /// What it must never do is depend on the order sessions are opened: two
    /// farm nodes rendering the same frame would then emit different asset
    /// paths for identical data, and a comparison would report a difference
    /// that is not there.
    pub fn dataset_name(&self) -> String {
        match self.imagery_asset_id {
            Some(imagery) => format!("ion-{}-{imagery}", self.terrain_asset_id),
            None => format!("ion-{}-noimagery", self.terrain_asset_id),
        }
    }
}

/// Stands in for an imagery provider when there is to be no imagery.
///
/// `globe()` is generic over a provider and there is no null implementation to
/// hand it, so terrain-only would otherwise mean resolving Bing and then
/// throwing the result away — a network round trip and a set of credentials for
/// data nothing reads.
///
/// Its fetch is unreachable rather than empty: with `no_imagery` set the loader
/// never drapes, so a call here would mean the option stopped being honoured,
/// and an error naming that is worth more than a blank texture that quietly
/// looks like a missing tile.
struct NoImagery;

#[async_trait::async_trait]
impl tuile_core::raster::ImageryProvider for NoImagery {
    fn tiling_scheme(&self) -> tuile_core::raster::TilingScheme {
        // Never consulted for draping; shaped to be obviously inert if it is.
        tuile_core::raster::TilingScheme {
            projection: tuile_core::raster::Projection::Geographic,
            root_tiles_x: 2,
            root_tiles_y: 1,
            tile_size: 1,
            minimum_level: 0,
            maximum_level: 0,
        }
    }

    async fn fetch_tile_bytes(
        &self,
        coord: tuile_core::raster::ImageryCoord,
    ) -> Result<tuile_core::fetch::Fetched<bytes::Bytes>, tuile_core::raster::RasterError> {
        Err(tuile_core::raster::RasterError::Image(format!(
            "imagery is disabled for this session, but {coord:?} was requested"
        )))
    }
}

/// A source, behind the tile cache or not — one type either way, so the globe
/// is assembled by the same code in both cases.
enum Cached<T, C> {
    Direct(T),
    Through(C),
}

impl<T: tuile_terrain::TerrainSource>
    Cached<T, tuile_terrain::CachedTerrain<T>>
{
    fn wrap_terrain(
        inner: T,
        cache: Option<Arc<dyn tuile_core::storage::ContentStore>>,
        asset: i64,
    ) -> Self {
        match cache {
            Some(c) => Cached::Through(tuile_terrain::CachedTerrain::new(inner, c, source_namespace(asset))),
            None => Cached::Direct(inner),
        }
    }
}

impl<P: tuile_core::raster::ImageryProvider>
    Cached<P, tuile_core::raster::CachedImagery<P>>
{
    fn wrap_imagery(
        inner: P,
        cache: Option<Arc<dyn tuile_core::storage::ContentStore>>,
        asset: i64,
    ) -> Self {
        match cache {
            Some(c) => Cached::Through(tuile_core::raster::CachedImagery::new(inner, c, source_namespace(asset))),
            None => Cached::Direct(inner),
        }
    }
}

#[async_trait::async_trait]
impl<T: tuile_terrain::TerrainSource, C: tuile_terrain::TerrainSource> tuile_terrain::TerrainSource for Cached<T, C> {
    async fn fetch_tile(
        &self,
        coord: tuile_terrain::TileCoord,
    ) -> Result<tuile_core::fetch::Fetched<Vec<u8>>, tuile_terrain::TerrainSourceError> {
        match self {
            Cached::Direct(s) => s.fetch_tile(coord).await,
            Cached::Through(s) => s.fetch_tile(coord).await,
        }
    }
}

#[async_trait::async_trait]
impl<P: tuile_core::raster::ImageryProvider, C: tuile_core::raster::ImageryProvider> tuile_core::raster::ImageryProvider
    for Cached<P, C>
{
    fn tiling_scheme(&self) -> tuile_core::raster::TilingScheme {
        match self {
            Cached::Direct(p) => p.tiling_scheme(),
            Cached::Through(p) => p.tiling_scheme(),
        }
    }

    async fn fetch_tile_bytes(
        &self,
        coord: tuile_core::raster::ImageryCoord,
    ) -> Result<tuile_core::fetch::Fetched<bytes::Bytes>, tuile_core::raster::RasterError> {
        match self {
            Cached::Direct(p) => p.fetch_tile_bytes(coord).await,
            Cached::Through(p) => p.fetch_tile_bytes(coord).await,
        }
    }
}

/// Resolves terrain and imagery into the two seams the core consumes.
///
/// A near-transcription of what the example apps do, and deliberately so: the
/// value here is that it happens behind the boundary, not that it is different.
async fn resolve(
    config: &GlobeConfig,
) -> Result<
    (
        Box<dyn tuile_core::source::TileTree>,
        Arc<dyn tuile_core::source::TileLoader>,
        ImageryDetail,
    ),
    GlobeError,
> {
    // Not a budget — the count of imagery layers one drape may CARRY. The
    // loader silently drops layers past it and the ground reverts to the
    // coarse capture exactly where tiles straddle worst: measured as an
    // exposure checkerboard across the whole frame, absent from the viewer
    // over the same data. The bake runs on the CPU, where a layer costs a
    // loop iteration and not a per-fragment fetch, so the ceiling applies.
    let imagery_slots = LayerBudget::default();
    imagery_slots.set_unbounded();
    // One pooled, cached transport drives ion and Bing both, so they share a
    // connection pool and a cache rather than competing for sockets.
    // Patient, parce qu'une cuisson est tout-ou-rien.
    //
    // Le défaut — trois tentatives, cinq secondes — est dimensionné pour un
    // viewport, où une tuile qui ne vient pas vaut mieux sautée qu'attendue.
    // Ici c'est l'inverse : personne ne regarde, la frame doit être exacte, et
    // abandonner jette tout ce qui est déjà cuit. Le 19 septembre 2026 une
    // cuisson est morte à la frame 2807 sur 2880, après vingt-sept minutes, sur
    // un unique `http status 500`. Voir `RetryConfig::patient`.
    let transport = TransportConfig {
        retry: RetryConfig::patient(),
        ..match &config.cache_dir {
            Some(dir) => TransportConfig::at(dir),
            None => TransportConfig::at(tuile_native_fetchers::default_cache_dir()),
        }
    };
    let http = Arc::new(
        NativeHttp::with_transport(transport)
            .await
            .map_err(|e| GlobeError::Transport(e.to_string()))?,
    );

    let terrain = IonTerrainSource::new(
        IonClient::new(Arc::clone(&http), config.ion_token.clone()),
        config.terrain_asset_id as u64,
    );
    let layer = terrain
        .layer()
        .await
        .map_err(|e| GlobeError::Ion(e.to_string()))?;
    // Through the tile cache when there is one: a tile any earlier run fetched
    // is read from it, and every tile fetched now is offered to it.
    let terrain = Cached::wrap_terrain(terrain, config.tile_cache.clone().map(|c| c.0), config.terrain_asset_id);

    // Terrain only: a valid mode, and the one to reach for when the geometry
    // looks wrong and a texture is the last thing you want on top of it.
    let Some(imagery_asset_id) = config.imagery_asset_id else {
        // Decode and resample on a real pool: the bulk driver blocks one
        // thread polling the server, and `globe()`'s inline offload would put
        // every tile's decode on that same thread — measured at sixty-four
        // loads in flight and one core busy.
        let (tree, loader, detail, _heights) = globe_on(
            terrain,
            NoImagery,
            layer,
            GlobeOptions {
                no_imagery: true,
                max_level: max_level(),
                imagery_slots: imagery_slots.clone(),
                imagery_boost_cap: imagery_boost_cap(),
                deterministic_floor: true,
                held_drape: config.held_drape.clone(),
                composed_at: config.session.bake_max_size,
            },
            offload::threaded(),
        );
        return Ok((tree, loader, detail));
    };

    let ion = IonClient::new(Arc::clone(&http), config.ion_token.clone());
    let endpoint = match ion
        .asset_endpoint(imagery_asset_id as u64)
        .await
        .map_err(|e| GlobeError::Ion(e.to_string()))?
    {
        AssetEndpoint::Imagery(e) => e,
        _ => return Err(GlobeError::NotImagery(imagery_asset_id)),
    };

    let options = GlobeOptions {
        no_imagery: false,
        max_level: max_level(),
        imagery_slots,
        imagery_boost_cap: imagery_boost_cap(),
        // On, and not a knob. Everything else in this crate is exact by
        // construction — `exact_traversal` refuses stand-ins and forbids
        // holes for the same reason — and a coarse layer chosen from
        // whatever a bounded cache happens to hold is the last decision
        // here that depends on how fast tiles arrived. It was measured
        // re-draping 16 of 80 identically selected tiles between two runs
        // of one frame.
        deterministic_floor: true,
        held_drape: config.held_drape.clone(),
        composed_at: config.session.bake_max_size,
    };

    // Ce que l'endpoint est, et non ce qu'on espère qu'il soit.
    //
    // Toute imagerie partait au chemin Bing, qui exige `options.key` : viser
    // un asset hébergé par ion — Sentinel-2, 3954 — ne donnait pas une image
    // douteuse, ça donnait « endpoint has no key », un message sur Bing pour
    // un asset qui n'est pas Bing. `externalType` absent veut dire
    // TileMapService, exactement la branche par défaut de cesium-native
    // (`IonRasterOverlay.cpp`, le `else` après `BING`).
    let (tree, loader, detail, _heights) = match endpoint.external_type.as_deref() {
        None => {
            let tms = TmsImagery::from_endpoint(ion, imagery_asset_id as u64, endpoint)
                .await
                .map_err(|e| GlobeError::Imagery(e.to_string()))?;
            let tms = Cached::wrap_imagery(tms, config.tile_cache.clone().map(|c| c.0), imagery_asset_id);
            globe_on(terrain, tms, layer, options, offload::threaded())
        }
        Some("BING") => {
            let o = &endpoint.options;
            let metadata_url = BingMetadata::metadata_url(
                o.url
                    .as_deref()
                    .ok_or_else(|| GlobeError::Bing("endpoint has no url".into()))?,
                o.map_style.as_deref().unwrap_or("Aerial"),
                o.key
                    .as_deref()
                    .ok_or_else(|| GlobeError::Bing("endpoint has no key".into()))?,
            );
            let bing = BingImageryProvider::from_metadata_url(Arc::clone(&http), &metadata_url)
                .await
                .map_err(|e| GlobeError::Bing(e.to_string()))?;
            let bing = Cached::wrap_imagery(bing, config.tile_cache.clone().map(|c| c.0), imagery_asset_id);
            globe_on(terrain, bing, layer, options, offload::threaded())
        }
        Some(kind) => {
            return Err(GlobeError::UnsupportedImagery {
                asset: imagery_asset_id,
                kind: kind.to_string(),
            })
        }
    };
    Ok((tree, loader, detail))
}

/// Seconds, as a C ABI carries a duration, into a `Duration`.
///
/// Non-finite and non-positive both mean "the caller did not choose", which is
/// different from "the caller chose zero" — a zero timeout would fail every
/// frame instantly, and that is never what someone means.
///
/// Public because the ABI is what carries seconds as a bare `f64`, and the ABI
/// now lives in another crate. The rule it encodes belongs beside the session
/// it defends, not beside the boundary that happens to invoke it.
pub fn duration_or(seconds: f64, fallback: Duration) -> Duration {
    if seconds.is_finite() && seconds > 0.0 {
        Duration::from_secs_f64(seconds)
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty token is a configuration mistake worth naming, not a network
    /// error to be discovered three seconds later against ion's 401.
    #[test]
    #[allow(clippy::panic, reason = "a test asserting by panicking")]
    fn an_empty_token_is_refused_before_any_request() {
        for token in ["", "   "] {
            match Session::globe(GlobeConfig::new(token)) {
                Err(GlobeError::MissingToken) => {}
                Err(other) => panic!("wrong error for {token:?}: {other:?}"),
                Ok(_) => panic!("{token:?} was accepted as a token"),
            }
        }
    }

    #[test]
    fn the_default_globe_is_world_terrain_with_bing() {
        let config = GlobeConfig::new("token");
        assert_eq!(config.terrain_asset_id, CESIUM_WORLD_TERRAIN);
        assert_eq!(config.imagery_asset_id, Some(BING_AERIAL));
    }

    /// Zero and NaN both mean "unset" across the boundary, because a C struct
    /// cannot hold an Option and a zero timeout would fail every frame.
    #[test]
    fn an_unset_duration_falls_back_rather_than_becoming_zero() {
        let fallback = Duration::from_secs(120);
        for unset in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(duration_or(unset, fallback), fallback, "{unset}");
        }
        assert_eq!(duration_or(2.5, fallback), Duration::from_secs_f64(2.5));
    }
}
