// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A pre-baked film, read frame by frame.
//!
//! Everything a frame needs was decided by the bake: which tiles, which
//! meshes, which imagery. What is left for a renderer is bookkeeping — which
//! tiles enter and leave between two frames — plus the camera and the look.
//! That bookkeeping lives here, free of any GPU and any I/O, so the same film
//! drives a browser worker and a headless binary.

mod camera;
mod cursor;
mod look;
mod slice;

pub use camera::{FrameCamera, NEAR_FRACTION};
pub use cursor::{Cursor, FilmError, FrameDiff, TileKey};
pub use look::Look;
pub use slice::slice;
pub use tuile_pack::{fb, BakedView, Pack};
