// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A pre-baked film, rendered in a browser — the page and its workers, in
//! Rust.
//!
//! [`start_page`] is the page: it lists what the buckets hold, shows a pack's
//! camera and tiles, and renders a scene by handing contiguous slices of its
//! frames to workers and joining what they send back into one mp4.
//!
//! [`start_slice`] is a worker: its own WebGPU device and an
//! `OffscreenCanvas` it presents every frame to, and an encoder.
//!
//! Encoding is progressive: the browser's H.264 encoder (WebCodecs) when it
//! has one for the picture size, and otherwise rav1e, compiled into this
//! module and fed I420 planes the GPU converted. WebCodecs is called by name
//! rather than through `web-sys`, whose bindings for it are still behind an
//! unstable flag — a stable toolchain is the rule here — so the JavaScript
//! that remains is two files that load this module and call one function.
//!
//! Imagery never enters the wasm heap as pixels: the pack's PNG goes to the
//! browser's own decoder (`createImageBitmap`, off-thread) and from there
//! straight into a GPU texture.

#[cfg(target_arch = "wasm32")]
mod encode;
#[cfg(target_arch = "wasm32")]
mod js;
#[cfg(target_arch = "wasm32")]
mod page;
#[cfg(target_arch = "wasm32")]
mod slice;
#[cfg(target_arch = "wasm32")]
mod soft;
#[cfg(target_arch = "wasm32")]
mod source;
#[cfg(target_arch = "wasm32")]
mod store;
#[cfg(target_arch = "wasm32")]
mod worker;

#[cfg(target_arch = "wasm32")]
pub use page::start_page;
#[cfg(target_arch = "wasm32")]
pub use slice::start_slice;
#[cfg(target_arch = "wasm32")]
pub use soft::{SoftEncoder, SoftPacket};
#[cfg(target_arch = "wasm32")]
pub use worker::{FilmMuxer, FilmWorker, FrameStats, PackView, Preloaded};
