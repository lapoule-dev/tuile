// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! An imagery provider that can be told, mid-session, to serve another
//! imagery set.
//!
//! # What it is, and what it is not
//!
//! [`SwitchableImagery`] is an [`ImageryProvider`] holding another one, and a
//! number — its [`generation`](ImageryProvider::generation) — that goes up
//! each time the one it holds is replaced. That is all it does. It drops
//! nothing, clears nothing and tells nobody.
//!
//! The rest follows from the number, by rules stated where each thing lives:
//!
//! - the loader keeps decoded tiles **per generation** and stamps every layer
//!   of a drape with the generation its pixels came from
//!   ([`ImageryLayer::source`](tuile_core::raster::ImageryLayer::source)), so
//!   a consumer never mistakes the new picture of a tile for the old one;
//! - the loader reports the generation as its
//!   [`epoch`](tuile_core::source::TileLoader::epoch), and the geometry server
//!   refreshes every resident tile from an earlier one — **coarse first, and
//!   keeping the old content until the new has arrived**.
//!
//! So a switch is one call here and no black anywhere: the old imagery stays
//! on every tile until that tile's new drape is resident, and is replaced in
//! place.
//!
//! The two imagery sets need not share a grid. The tiling scheme is the held
//! provider's, asked for again on every use, and a drape that straddles a
//! switch is thrown away and built again rather than mixed.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bytes::Bytes;
use tuile_core::fetch::Fetched;
use tuile_core::raster::{ImageryCoord, ImageryProvider, RasterError, TilingScheme};

/// See the module. Cheap to clone: every clone is the same switch, so the
/// host keeps one and hands the other to the globe.
#[derive(Clone)]
pub struct SwitchableImagery(Arc<RwLock<(u64, Arc<dyn ImageryProvider>)>>);

impl SwitchableImagery {
    /// A switch that opens on `first`, at generation 0.
    pub fn new(first: Arc<dyn ImageryProvider>) -> Self {
        Self(Arc::new(RwLock::new((0, first))))
    }

    /// Serves `next` from now on, and returns the new generation.
    ///
    /// Returns at once. What is on screen changes as the geometry server
    /// refreshes its tiles, which takes as long as the new imagery takes to
    /// arrive.
    pub fn switch_to(&self, next: Arc<dyn ImageryProvider>) -> u64 {
        let mut held = self.0.write().expect("imagery switch");
        *held = (held.0 + 1, next);
        held.0
    }

    /// The generation and the provider, read together: a tile address means a
    /// different picture on each side of a switch, so the two never travel
    /// apart.
    fn now(&self) -> (u64, Arc<dyn ImageryProvider>) {
        let held = self.0.read().expect("imagery switch");
        (held.0, Arc::clone(&held.1))
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl ImageryProvider for SwitchableImagery {
    fn tiling_scheme(&self) -> TilingScheme {
        self.now().1.tiling_scheme()
    }

    async fn fetch_tile_bytes(&self, coord: ImageryCoord) -> Result<Fetched<Bytes>, RasterError> {
        // The lock is let go before the fetch: a switch must not wait for a
        // tile, and a tile already asked for is answered by whoever was
        // serving when it was asked.
        let (_, provider) = self.now();
        provider.fetch_tile_bytes(coord).await
    }

    fn generation(&self) -> u64 {
        self.now().0
    }
}
