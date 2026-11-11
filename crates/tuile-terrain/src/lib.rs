// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

pub mod availability;
pub mod decode;
pub mod ellipsoid;
pub mod heightfield;
mod heights;
pub mod http;
pub mod layer;
pub mod mesh;
pub mod source;
pub mod tiling;
pub mod tree;
mod upsample;

pub use availability::Availability;
pub use decode::{decode, DecodeError, Header, QuantizedMesh};
pub use ellipsoid::{Ellipsoid, Nothing};
pub use heightfield::{Grid, HeightfieldSource, Heights, Outside};
pub use heights::TerrainHeights;
pub use http::HttpTerrainSource;
pub use layer::{AvailabilityRange, LayerError, LayerJson, TileUrlError};
pub use mesh::{
    fill_content, globe_shell, skirt_depth, skirt_depth_cut, skirt_height, surface_uvs, to_decoded,
    to_decoded_with, to_ground, to_ground_with,
};
pub use source::{CachedTerrain, TerrainSource, TerrainSourceError};
pub use tiling::{level_geometric_error, GeoRect, GeographicTilingScheme, TileCoord};
pub use tree::TerrainTree;
pub use upsample::upsample;
