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
pub mod from_store;
mod look;
mod mesh;
mod ranges;
mod slice;
mod survey;

pub use camera::{FrameCamera, NEAR_FRACTION};
pub use cursor::{Cursor, FilmError, FrameDiff, TileKey};
pub use look::{Imagery, Look};
pub use mesh::{texture, texture_of_span, Mesh};
pub use ranges::{block_plan, coalesce, file_reads, Fetch};
pub use slice::slice;
pub use survey::{cameras, frame_tiles, CameraSample, TileInfo};
pub use tuile_pack::{
    blob_start, fb, refs_of, BakedView, Content, Fnv1a, ImageryPlacement, Pack, PackError,
    StoreTile, TileRefs, PREAMBLE,
};
