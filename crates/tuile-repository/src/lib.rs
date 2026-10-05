// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Where films live.
//!
//! A film is baked into one pack or several, from source tiles — and each of
//! those is written to a bucket under a key layout. This crate is the read
//! side of those layouts, so that nothing downstream builds a key by hand:
//!
//! - [`Objects`] — bytes in a bucket, read by range. One contract, three
//!   implementations: a bucket or directory ([`tuile_farm::ObjectRunStore`]),
//!   and [`Cached`], which wraps any other and keeps each chunk it has read.
//! - [`FilmRepository`] — the films a bucket holds and what each is made of.
//!   [`ScenePacks`] reads the engine's own layout (packs keyed by scene
//!   digest); [`RunFilms`] reads run directories as an orchestrator lays
//!   them out, from a [`RunLayout`] it is configured with.
//! - [`TileRepository`] — the source tiles, by layer and coordinates.
//!
//! Every implementation of a trait here can stand in for any other: the
//! contracts below are written so a caller never needs to know which one it
//! was handed. A film from either repository is the same [`Film`], its chunks
//! ordered, their frame ranges known, their keys readable through the same
//! [`Objects`] the repository was built on.

//!
//! The contracts and the layouts compile to wasm32; the adapters that reach a
//! bucket from a native process (`native`) do not, and are not built there.

mod films;
mod objects;
mod runs;
mod scenes;
mod tiles;

#[cfg(not(target_arch = "wasm32"))]
mod native;

pub use films::{Chunk, Film, FilmRepository, FilmSummary};
pub use objects::{Entry, Listing, Objects, RepoError};
pub use runs::{RunFilms, RunLayout};
pub use scenes::ScenePacks;
pub use tiles::{LayerInfo, Tile, TileRepository};

#[cfg(not(target_arch = "wasm32"))]
pub use native::{Cached, CHUNK};
