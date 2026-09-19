// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The manifest stage: a recorded camera path plus one `Globe` prim.
//!
//! This crate is the answer to "how does a track become a render" (docs/15):
//! a tape (`tuile-tape`, MCAP) is turned into a tiny `.usda` — an animated
//! `UsdGeomCamera` written as `matrix4d` time samples, and a
//! `GenerativeProcedural` prim whose primvars configure the TuileGlobe
//! procedural. Every Hydra host renders the flight from there: `usdrecord`
//! reads it natively, Blender imports the camera and re-references the prim.
//!
//! Deliberately free of any USD dependency — the manifest is text, and text
//! is what a review reads.

pub mod stage;

pub use stage::{render_origin, write_manifest, ManifestConfig};
