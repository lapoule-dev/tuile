// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

pub mod bake;
mod cancel;
#[cfg(test)]
mod by_index_tests;
mod drawer;
mod globe;
mod ground;
pub mod local_store;
pub mod memory;
pub mod open;

pub mod packed;
mod session;
pub mod settle;
mod shed;
pub mod tape;
pub mod tiles;

pub use cancel::Cancel;
pub use drawer::{GroundDrawer, Held, Selected};
pub use globe::{
    asset_of_namespace, duration_or, source_namespace, GlobeConfig, GlobeError, ReferenceFetched,
    ReferenceImagery, Sources, SourcesHandle, TileCache, BING_AERIAL, CESIUM_WORLD_TERRAIN,
};
pub use packed::PackedError;
pub use session::{
    budget_settings, exact_budget, exact_traversal, pose_view, EncodedTexture, Finish, Frame,
    FrameError, GroundMemory, Session, SessionConfig, TileGeometry,
};
/// What a caller already holds, so a bake never fetches it twice. See
/// [`GlobeConfig::held_drape`].
pub use tuile_planetary::HeldDrape;
