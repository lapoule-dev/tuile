// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! # tuile-hydra
//!
//! The C ABI an OpenUSD/Hydra plugin calls to turn a camera into tiles.
//!
//! A **specialised façade**, in the sense `docs/01-architecture.md` means it:
//! there is no generic FFI layer, and this crate exposes what a Hydra plugin
//! consumes — one frame at a time, fully resolved, with buffers laid out the
//! way Hydra wants to read them.
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
//!
//! # Rules at the boundary
//!
//! - **No panic may cross.** An unwind through `extern "C"` aborts the process,
//!   so every entry point catches and returns a status instead.
//! - **Rust owns every buffer.** The C++ side borrows pointers that stay valid
//!   until it releases the frame, and frees nothing itself.
//! - **Calls arrive from Hydra's worker threads.** `GetPrim` and
//!   `GetChildPrimPaths` are documented as thread-safe, so anything reachable
//!   from them must be too.

mod ffi;
mod globe;
pub mod packed;
mod session;

pub use globe::{GlobeConfig, GlobeError, BING_AERIAL, CESIUM_WORLD_TERRAIN};
pub use packed::PackedError;
pub use session::{
    exact_traversal, EncodedTexture, Frame, FrameError, Session, SessionConfig, TileGeometry,
};
