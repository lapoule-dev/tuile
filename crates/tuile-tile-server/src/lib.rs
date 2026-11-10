// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

pub mod absences;
pub mod archive;
pub mod catalog;
pub mod content;
pub mod disk;
pub mod due;
pub mod extract;
pub mod fetched;
pub mod grid;
pub mod index;
mod knob;
pub mod layer;
pub mod ledger;
pub mod lru;
pub mod manifest;
pub mod peers;
pub mod projection;
pub mod register;
pub mod service;
pub mod store;
pub mod tiered;
pub mod tiering;
pub mod upstream;

pub use absences::{Absence, Absences};
pub use catalog::{Catalog, LayerDef};
pub use content::StoreContent;
pub use disk::DiskCacheConfig;
pub use due::{due, Due, Why};
pub use extract::{ExtractAsk, ExtractReport, Extracts};
pub use grid::{Grid, OutOfGrid};
pub use layer::{Layer, Zone, DURABLE_EPOCH};
pub use ledger::{IndexReader, KnownZone, Ledger, Publication, RegisteredTile, TileBatches};
pub use peers::{Announce, Claim, Emitted, FreshTiles, SharedTile, SharedTiles};
pub use pmtiles::{Compression, TileType};
pub use projection::{Eye, Footprint, LayerProjection, ProjectionReport, ZoneListing};
pub use register::MemoryRegister;
pub use service::{
    etag_of, LayerMeta, ServiceConfig, ServiceError, ServiceStats, Source, TileResponse,
    TileService,
};
pub use store::{Census, Clock, Compaction, Fresh, Scrub, StatsSnapshot, StoreConfig, TileStore};
pub use tiered::{Tier, TierClock, Tiered, TieredStats, Touch, TOUCH_EVERY};
pub use tiering::Tiering;
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
    /// A failure shared by every reader waiting on the same remote read.
    #[error("remote: {0}")]
    Remote(String),
    #[error("corrupt: {0}")]
    Corrupt(String),
    #[error("{0}: the manifest kept changing under every attempt")]
    Contended(String),
    #[error("a lock was poisoned")]
    Poisoned,
    /// The host's register could not record an archive about to be written;
    /// it was not written.
    #[error("ledger: {0}")]
    Ledger(String),
    /// Tiles a reader named that the store does not hold: `absent` of them,
    /// `said` naming the first.
    #[error("{said}")]
    Incomplete { absent: u64, said: String },
    #[doc(hidden)]
    #[error("injected fault: {0}")]
    Fault(&'static str),
}
