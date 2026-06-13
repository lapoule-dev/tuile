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
//!
//! The crate never creates a window or a surface; the host owns the event
//! loop and the render pass.

mod context;
mod prepare;
mod pump;
mod renderer;

pub use context::{ContextError, GpuContext, DEPTH_FORMAT, TEXTURE_FORMAT};
pub use prepare::{prepare, PreparedMesh, PreparedTile, Vertex};
pub use pump::ContentPump;
pub use renderer::{LineVertex, TileRenderer, ViewUniform};
