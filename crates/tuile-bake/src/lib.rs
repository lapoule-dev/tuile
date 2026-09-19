// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! # tuile-bake
//!
//! The pack pipeline: a camera path in, a container out, and the same container
//! read back. Nothing here knows what will draw the result.
//!
//! # Why the session lives beside the bake, and not beside a renderer
//!
//! It used to live in `tuile-hydra`, and that had a visible consequence: **the
//! bake depended on Hydra**. The binary that writes a deliberately neutral
//! container — plain buffers, PNG, no host concepts anywhere in the schema —
//! linked a crate named after one particular OpenUSD host, and imported
//! `tuile_hydra::Session` to do work that has nothing to do with OpenUSD.
//! Anyone reading the dependency graph would conclude the pack was a Hydra
//! artefact. It is not, and the graph should not have said so.
//!
//! So the split follows what the code actually does:
//!
//! * here — the globe and its sources, the traversal, the session that resolves
//!   a camera into tiles, the writer that freezes that into a pack, and
//!   [`packed`], which reads one back. 3D Tiles and nothing else.
//! * in `tuile-hydra` — the C ABI, and only that: buffers laid out the way
//!   Hydra wants to read them, a status code instead of an unwind, and the
//!   thread-safety its worker pool requires.
//!
//! Reading a pack sits here rather than with the consumer on purpose: the code
//! that writes a format and the code that reads it drift apart the moment they
//! live in different crates with different reasons to change.
//!
//! # Why bulk and not streaming
//!
//! The rest of the project streams: tiles arrive progressively and the picture
//! sharpens. That is right for a viewer and wrong for a renderer. In
//! progressive mode the selection depends on *which tiles happened to arrive*,
//! so two machines rendering the same frame can disagree — and a farm would
//! show it as flicker between frames rendered on different nodes, a defect that
//! is close to impossible to diagnose after the fact.
//!
//! So a frame here is one blocking, converged answer
//! ([`tuile_core::drive::drive_until_complete`]): the selection is a function of
//! the camera and the sources, not of the network's mood.

mod globe;
pub mod packed;
mod session;

pub use globe::{
    duration_or, imagery_boost_cap, GlobeConfig, GlobeError, BING_AERIAL,
    CESIUM_WORLD_TERRAIN,
};
pub use packed::PackedError;
pub use session::{
    exact_traversal, EncodedTexture, Frame, FrameError, Session, SessionConfig, TileGeometry,
};
