// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Where a globe's tiles come from, when a host brings its own sources.
//!
//! This crate crosses *a* terrain source with *an* imagery provider and never
//! learns which. [`Sources`] is the same idea one step earlier: the host is
//! asked for the terrain and the imagery of an asset **by number**, and answers
//! with the two traits the crossing already takes. Whoever assembles a globe —
//! a bake, an interactive viewer — can then be written once and handed its
//! tiles by a connector, by a server of the host's own, or by a test.
//!
//! It lives here because this is the lowest crate that knows both halves:
//! terrain is `tuile-terrain`'s, imagery is the core's, and only their crossing
//! names the two together.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use tuile_core::fetch::{FetchError, Fetched, TileFetcher};
use tuile_core::raster::ImageryProvider;
use tuile_terrain::{LayerJson, TerrainSource};

/// Where a globe's tiles come from, when the host brings its own sources.
///
/// A host that keeps its tiles behind a service of its own — which already
/// holds the sessions, the quota and the store — implements this, and whatever
/// assembles the globe resolves nothing itself: no token is read, no endpoint
/// is asked for, no descriptor is fetched. Every tile comes from what the host
/// handed over.
///
/// The assets keep their numbers. They are what a pack records, what a tile
/// cache is keyed by and what a list of imagery choices names, so the same
/// number means the same tiles whichever implementation answers. That proviso
/// is the host's to keep.
///
/// Errors are a sentence for a person: the caller shows it and stops.
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait Sources: Send + Sync {
    /// The terrain of `asset`: its `layer.json`, and its tiles.
    async fn terrain(&self, asset: i64) -> Result<(LayerJson, Arc<dyn TerrainSource>), String>;

    /// The imagery of `asset`. The provider says which grid it is cut on
    /// ([`ImageryProvider::tiling_scheme`]); the crossing drapes either.
    async fn imagery(&self, asset: i64) -> Result<Arc<dyn ImageryProvider>, String>;

    /// The 3D Tiles tileset of `asset`: where its root document is, how it
    /// and everything beneath it are fetched, and who must be credited while
    /// it is drawn.
    ///
    /// Provided, refusing: a host that serves terrain and imagery only need
    /// not say anything about tilesets.
    async fn tileset(&self, asset: i64) -> Result<TilesetSource, String> {
        Err(format!(
            "asset {asset}: this host serves no 3D Tiles tilesets"
        ))
    }
}

/// A 3D Tiles tileset as a host serves it: see [`Sources::tileset`].
#[derive(Clone)]
pub struct TilesetSource {
    /// The root document. Its query, if it has one, is carried to every
    /// request beneath (`tuile_core::tileset`).
    pub url: url::Url,
    /// What fetches the root, the external tilesets and the contents, with
    /// whatever credential the service wants.
    pub fetcher: Arc<dyn TileFetcher>,
    /// Who must be credited, on screen, while any of it is drawn — besides
    /// what each content names itself
    /// (`tuile_core::content::DecodedTileContent::copyright`).
    pub credits: Vec<Credit>,
}

/// One credit a source requires on screen: words, and the logo that stands
/// for them where the provider asks for one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credit {
    /// What a person reads; the logo's own words when it is a logo alone.
    pub text: String,
    /// The image the provider requires be shown, if any.
    pub logo: Option<url::Url>,
    /// Where the credit leads, if anywhere.
    pub link: Option<url::Url>,
}

/// A source held by pointer, as the source it points to.
///
/// [`Sources`] answers with `Arc<dyn …>`, and the crossing is generic over a
/// *value* implementing the trait; this is the adapter between the two, so no
/// host has to write it.
pub struct Held<T: ?Sized>(pub Arc<T>);

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl TileFetcher for Held<dyn TileFetcher> {
    async fn fetch(&self, url: &url::Url) -> Result<Bytes, FetchError> {
        self.0.fetch(url).await
    }

    async fn fetch_cacheable(&self, url: &url::Url) -> Result<Fetched<Bytes>, FetchError> {
        self.0.fetch_cacheable(url).await
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl TerrainSource for Held<dyn TerrainSource> {
    async fn fetch_tile(
        &self,
        coord: tuile_terrain::TileCoord,
    ) -> Result<tuile_core::fetch::Fetched<Vec<u8>>, tuile_terrain::TerrainSourceError> {
        self.0.fetch_tile(coord).await
    }

    fn holds(&self, coord: tuile_terrain::TileCoord) -> bool {
        self.0.holds(coord)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl ImageryProvider for Held<dyn ImageryProvider> {
    fn tiling_scheme(&self) -> tuile_core::raster::TilingScheme {
        self.0.tiling_scheme()
    }

    async fn fetch_tile_stamp(
        &self,
        coord: tuile_core::raster::ImageryCoord,
    ) -> tuile_core::storage::Stamp {
        self.0.fetch_tile_stamp(coord).await
    }

    fn generation(&self) -> u64 {
        self.0.generation()
    }

    fn holds(&self, coord: tuile_core::raster::ImageryCoord) -> bool {
        self.0.holds(coord)
    }

    async fn fetch_tile_bytes(
        &self,
        coord: tuile_core::raster::ImageryCoord,
    ) -> Result<tuile_core::fetch::Fetched<bytes::Bytes>, tuile_core::raster::RasterError> {
        self.0.fetch_tile_bytes(coord).await
    }
}
