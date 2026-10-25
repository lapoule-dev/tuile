// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

pub mod av1;
pub mod compress;
pub mod discs;
pub mod encode;
pub mod engine;
pub mod extract;
pub mod meter;
#[cfg(all(target_os = "linux", feature = "nvenc"))]
pub mod nvenc;
pub mod observe;
pub mod reference;
pub mod render;
pub mod seams;
pub mod serve;
pub mod setup;
pub mod sink;
pub mod source;
pub mod timelapse;
#[cfg(all(target_os = "macos", feature = "videotoolbox"))]
pub mod videotoolbox;

pub use discs::DiscEnergy;
pub use meter::LightMeter;
#[cfg(all(target_os = "linux", feature = "nvenc"))]
pub use nvenc::{NvencCodec, NvencFilm};
pub use observe::{FrameOut, ImageryIn, Observer, Origin, SkyOut, TileIn, Timings};
pub use reference::{Reference, SetAgainst};
pub use render::{render, AbsentTiles, Order, Tone};
pub use seams::SeamLog;
pub use sink::{Av1Film, Nothing, Pictures, Sink};
pub use source::{Counting, Film, Reads, Sources};
#[cfg(all(target_os = "macos", feature = "videotoolbox"))]
pub use videotoolbox::{VtCodec, VtFilm};
#[cfg(all(target_os = "macos", feature = "videotoolbox"))]
pub mod spatial;
#[cfg(all(target_os = "macos", feature = "videotoolbox"))]
pub use spatial::SpatialFilm;

/// What goes wrong here is told to a person, not matched on.
pub type Error = Box<dyn std::error::Error + Send + Sync>;
