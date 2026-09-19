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
//! The work itself lives in [`tuile_bake`], which knows nothing about OpenUSD.
//! That separation is not tidiness: `tuile-bake` produces a container any
//! renderer can read, and while the session lived here, the binary that wrote
//! that container linked a crate named after one particular host. The format is
//! neutral; the dependency graph said otherwise.
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

// Re-exported so the plugin side has one crate to name, and so an existing
// `tuile_hydra::Session` keeps resolving.
pub use tuile_bake::{
    exact_traversal, packed, EncodedTexture, Frame, FrameError, GlobeConfig, GlobeError,
    PackedError, Session, SessionConfig, TileGeometry, BING_AERIAL, CESIUM_WORLD_TERRAIN,
};
