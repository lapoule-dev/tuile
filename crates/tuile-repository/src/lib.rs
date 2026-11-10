// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

mod archives;
pub mod bench;
mod cached;
mod config;
mod films;
mod objects;
pub mod remote;
mod revalidated;
mod runs;
mod scenes;
mod tiles;
pub mod tone;

#[cfg(not(target_arch = "wasm32"))]
mod native;

pub use archives::{ArchivedTiles, Now, PinReport, PinnedLayer, Pins, EXTRACT_BANDS, PINS_PREFIX};
pub use bench::{
    block_segment, Asked, Bench, Later, LiveAnswer, LiveAnswers, LiveAsk, LiveAsked, Project,
    Reply, StoreAt, StoreObjects, BLOCK, LIVE_AT_ONCE, LIVE_MANY,
};
pub use cached::{Cached, ChunkStore, CHUNK};
pub use config::{Config, Layout, Place, ProjectConfig};
pub use films::{Chunk, Film, FilmRepository, FilmSummary, Unreadable};
pub use objects::{Entry, Listing, Objects, Read, RepoError};
pub use remote::{BlockCounts, Get, Got, RemoteBlocks, RemoteLive, RemoteStore};
pub use revalidated::{Revalidated, Revalidations};
pub use runs::{RunFilms, RunLayout};
pub use scenes::ScenePacks;
pub use tiles::{LayerInfo, Tile, TileRepository};

#[cfg(not(target_arch = "wasm32"))]
pub use native::{DiskChunks, HttpGet, Kept};
