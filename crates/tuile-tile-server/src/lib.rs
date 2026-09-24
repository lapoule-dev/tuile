// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Source tiles, kept once.
//!
//! A tile server's job, minus the HTTP: terrain and imagery tiles as their
//! source delivered them, stored in PMTiles archives on an object store, one
//! set of archives per **zone** and per **layer**, served back by tile address.
//!
//! - [`grid`] maps a source's own addressing onto an archive's quadtree.
//! - [`layer`] says how a layer is split into zones and cut into epochs.
//! - [`archive`] writes and reads one immutable archive.
//! - [`manifest`] is the list of archives a zone is made of, replaced by
//!   conditional writes.
//! - [`store`] puts it together: a mutable buffer in memory, deltas, a
//!   streaming compaction, and cleanup — safe with any number of writers.
//! - [`service`] adds the upstream: fetched once, stored, served.
//!
//! No router and no authentication here: those belong to whoever deploys it.

pub mod archive;
pub mod grid;
pub mod layer;
pub mod manifest;
pub mod service;
pub mod store;
pub mod upstream;

pub use grid::{Grid, OutOfGrid};
pub use layer::{Layer, Zone, DURABLE_EPOCH};
pub use pmtiles::{Compression, TileType};
pub use service::{LayerMeta, ServiceError, TileResponse, TileService};
pub use store::{Clock, Compaction, StoreConfig, TileStore};
pub use upstream::{TemplateUpstream, Upstream, UpstreamError};

/// A failure of the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("unknown layer {0}")]
    UnknownLayer(String),
    #[error(transparent)]
    OutOfGrid(#[from] OutOfGrid),
    #[error("object store: {0}")]
    ObjectStore(#[from] object_store::Error),
    #[error("archive: {0}")]
    Archive(#[from] pmtiles::PmtError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt: {0}")]
    Corrupt(String),
    #[error("{0}: the manifest kept changing under every attempt")]
    Contended(String),
    #[error("a lock was poisoned")]
    Poisoned,
    #[doc(hidden)]
    #[error("injected fault: {0}")]
    Fault(&'static str),
}
