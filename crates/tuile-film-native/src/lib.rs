// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A film rendered natively, from its packs and the tile store.
//!
//! The same film a browser renders, by the same parts — the pack's plan
//! (`tuile_film::Cursor`), the store's tiles made into meshes and drapes
//! (`tuile_film::from_store`), the GPU's composition and frame
//! (`tuile_film_gpu`) — on a machine's own adapter, with no page around it.
//!
//! Three things, kept apart:
//!
//! - **what is read** ([`source`]): packs and the tile store, both behind
//!   `tuile_repository::Objects`, both through a cache of chunks on disk,
//!   both counted;
//! - **the render** ([`render`]): frame after frame, tiles entering and
//!   leaving as the plan says, each picture handed to a [`Sink`];
//! - **what watches** ([`observe`]): an [`Observer`] is told of every
//!   imagery tile that comes in, every tile built, every picture that comes
//!   out. The render knows nothing of what an observer does with it;
//!   [`meter::LightMeter`] is one, and measures light.

pub mod av1;
pub mod meter;
#[cfg(all(target_os = "linux", feature = "nvenc"))]
pub mod nvenc;
pub mod observe;
pub mod render;
pub mod sink;
pub mod source;
#[cfg(all(target_os = "macos", feature = "videotoolbox"))]
pub mod videotoolbox;

pub use meter::LightMeter;
#[cfg(all(target_os = "linux", feature = "nvenc"))]
pub use nvenc::{NvencCodec, NvencFilm};
pub use observe::{FrameOut, ImageryIn, Observer, Origin, TileIn, Timings};
pub use render::{render, Order, Tone};
pub use sink::{Av1Film, Nothing, Pictures, Sink};
pub use source::{Counting, Film, Reads, Sources};
#[cfg(all(target_os = "macos", feature = "videotoolbox"))]
pub use videotoolbox::H264Film;

/// What goes wrong here is told to a person, not matched on.
pub type Error = Box<dyn std::error::Error + Send + Sync>;
