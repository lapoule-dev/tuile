// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! # tuile-wgpu
//!
//! Reference wgpu render backend for `tuile-core`: correct and readable
//! before fast — this is the core's shop window, not a game engine
//! (`docs/11-crate-wgpu.md`).
//!
//! - [`GpuContext`] — device/queue + shared layouts ([`GpuContext::headless`]
//!   for tests and offscreen rendering)
//! - [`prepare`]/[`PreparedTile`] — decoded content → GPU resources
//!   (interleaved buffers, mipmapped sRGB textures)
//! - [`TileRenderer`] — one minimal PBR pipeline (+ wireframe when the
//!   device supports it, + debug lines for bounding volumes)
//! - [`ContentPump`] — the async→frame bridge over any
//!   [`tuile_core::protocol::GeometryStream`]
//! - [`OverlayRenderer`] — pixel-space triangles over the scene, for the
//!   on-screen controls a host builds with `tuile-ui`
//!
//! The crate never creates a window or a surface; the host owns the event
//! loop and the render pass.

mod context;
mod overlay;
mod prepare;
mod pump;
mod readback;
mod renderer;
mod surface;
mod targets;

pub use context::{ContextError, GpuContext, DEPTH_FORMAT, SAMPLES, TEXTURE_FORMAT};
pub use overlay::{OverlayRenderer, OverlayVertex};
pub use prepare::{prepare, PreparedMesh, PreparedTile, Vertex};
pub use pump::{ContentPump, Resolution, UPLOADS_PER_FRAME};
pub use readback::Readback;
pub use renderer::{LineVertex, TileRenderer, ViewUniform};
pub use surface::{draw_to_surface, preferred_format, FrameOnSurface, Presented};
pub use targets::FrameTargets;
