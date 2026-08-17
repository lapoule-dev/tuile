// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! # tuile-terrain
//!
//! Quantized-mesh terrain decoding and geographic tiling — the data side of
//! the globe pipeline (`docs/03-roadmap.md`, MG).
//!
//! - [`decode`](mod@decode) — quantized-mesh-1.0 → [`decode::QuantizedMesh`] (normalized
//!   vertices, high-water-mark indices, oct-decoded normals, skirt edges),
//!   plus a minimal encoder for round-trip tests
//! - [`tiling`] — the EPSG:4326 geographic quadtree (Cesium World Terrain's
//!   scheme): tile → rectangle, per-level geometric error
//! - [`mesh`] — [`decode::QuantizedMesh`] + tile rectangle → a render-ready
//!   [`tuile_core::content::DecodedTileContent`] (ECEF-rebased, skirts),
//!   the same type the glTF path produces
//!
//! Pure and wasm-clean: no I/O. Fetching `.terrain` tiles (gzip, ion bearer,
//! `layer.json` availability) lives in the source/connector layer.

pub mod availability;
pub mod decode;
mod heights;
pub mod layer;
pub mod mesh;
pub mod source;
pub mod tiling;
pub mod tree;
mod upsample;

pub use availability::Availability;
pub use decode::{decode, DecodeError, Header, QuantizedMesh};
pub use heights::TerrainHeights;
pub use layer::{AvailabilityRange, LayerError, LayerJson};
pub use mesh::{globe_shell, skirt_height, surface_uvs, to_decoded};
pub use source::{CachedTerrain, TerrainSource, TerrainSourceError};
pub use tiling::{level_geometric_error, GeoRect, GeographicTilingScheme, TileCoord};
pub use tree::TerrainTree;
pub use upsample::upsample;
