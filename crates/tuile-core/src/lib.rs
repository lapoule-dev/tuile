// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! # tuile-core
//!
//! Render-agnostic OGC 3D Tiles engine: tileset parsing, screen-space-error
//! traversal, content decoding, and a **logical geometry server** exposed
//! through the [`protocol::GeometryStream`] trait.
//!
//! The core never depends on a renderer, an async runtime or an HTTP
//! framework, and compiles to `wasm32-unknown-unknown` — that property is
//! CI-enforced. All geospatial math is f64 until positions are rebased onto
//! a local origin (the anti-jitter protocol of `docs/01-architecture.md`).
//!
//! Module map (see `docs/10-crate-core.md`):
//! - [`tileset`] — tileset.json (1.0/1.1) serde types + flat tile arena
//! - [`implicit`] — implicit-tiling types (subtree decoding lands in M2)
//! - [`content`] — glb / b3dm → [`content::DecodedTileContent`]
//! - [`geo`] — WGS84 ↔ ECEF, regions → OBB
//! - [`math`] — f64 OBB / sphere / frustum / distances
//! - [`raster`] — imagery overlays: providers, tiling schemes, UV mapping
//! - [`traversal`] — pure SSE selection, multi-view
//! - [`protocol`] — the streaming protocol trait and in-process binding
//! - [`runtime`] — the geometry server driving it all
//! - [`tiles3d`] — the 3D Tiles binding of the source seam (tree + loader)
//! - [`fetch`] — abstract byte source ([`fetch::FsFetcher`], `HttpFetcher`)
//! - [`cache`] — resident-content LRU under a byte budget

pub mod cache;
pub mod content;
pub mod fetch;
pub mod geo;
pub mod implicit;
pub mod math;
pub mod protocol;
pub mod raster;
pub mod runtime;
pub mod source;
pub mod tiles3d;
pub mod tileset;
pub mod traversal;

pub use content::{ContentFormat, DecodedTileContent, TileContent};
pub use protocol::{ClientMessage, GeometryStream, InProcessStream, ServerMessage};
pub use runtime::{in_process, in_process_with, GeometryServer};
pub use source::{
    CompositeLoader, CompositeTileTree, LoadError, Loaded, TileId, TileLoader, TileProperties,
    TileTree,
};
pub use tiles3d::{SharedTileset, TilesetLoader, TilesetTree};
pub use tileset::Tileset;
pub use traversal::{Config, ViewState};
