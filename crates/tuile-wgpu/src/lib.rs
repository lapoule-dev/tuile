// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

mod compress;
mod context;
mod encode;
mod host;
mod overlay;
mod prepare;
mod pump;
mod readback;
mod renderer;
mod sky;
mod surface;
mod targets;
#[cfg(test)]
mod webgl2;
mod world_overlay;

pub use compress::{
    adapter_plan, caps_of, compression_named, format_name, plan_features, plan_for,
    stored_features, texture_caps, EncodePace, EncodeStats, ImageryCompression,
};
pub use context::{
    ContextError, GpuContext, ImageryTextures, DEPTH_FORMAT, SAMPLES, TEXTURE_FORMAT,
};
pub use host::{view_proj_from_world, DepthDirection, HostFrame, HostPass, TargetLayout};
pub use overlay::{OverlayRenderer, OverlayVertex};
pub use prepare::{
    prepare, prepare_drape, PreparedDrape, PreparedMesh, PreparedTile, Vertex, STITCH_ROWS,
};
pub use pump::{ContentPump, Drawn, Resolution, STITCH_SLIDE_FRAMES, UPLOAD_SHARE_PERMILLE};
pub use readback::{MappedFrame, Readback, ReadbackRing};
pub use renderer::{sky_wgsl, LineVertex, TileRenderer, ViewUniform};
pub use sky::{
    ecef_of, Atmosphere, AtmosphereConfig, Bodies, Lights, SkyFrame, SkyView, LIGHT_INTERVAL,
    LIGHT_SUN_STEP,
};
pub use surface::{
    draw_to_surface, draw_to_texture, preferred_format, surface_format, FrameOnSurface,
    FrameOnTexture, FramePass, Presented, SurfaceFormat,
};
pub use targets::FrameTargets;
pub use world_overlay::{Picture, WorldOverlayError, WorldOverlayRenderer, WorldOverlayView};
