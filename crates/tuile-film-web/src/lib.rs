// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A pre-baked film, rendered in a browser worker.
//!
//! Each worker owns a slice of the film: its own copy of the pack, its own
//! WebGPU device and an `OffscreenCanvas` it presents every frame to. The JS
//! shim around it turns each presented canvas into a `VideoFrame` for
//! WebCodecs — the encoder stays on the JS side because its `web-sys` bindings
//! are still behind an unstable flag, and a stable toolchain is the rule here.
//!
//! The page joins the slices with [`FilmMuxer`], which refuses a slice whose
//! encoder produced other parameter sets.
//!
//! Imagery never enters the wasm heap as pixels: the pack's PNG goes to the
//! browser's own decoder (`createImageBitmap`, off-thread) and from there
//! straight into a GPU texture.

#[cfg(target_arch = "wasm32")]
mod source;
#[cfg(target_arch = "wasm32")]
mod worker;

#[cfg(target_arch = "wasm32")]
pub use worker::{FilmMuxer, FilmWorker, FrameStats, PackView};
