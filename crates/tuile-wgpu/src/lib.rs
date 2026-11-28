// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

mod chains;
mod compress;
mod context;
mod encode;
mod host;
mod in_place;
mod ops;
mod overlay;
mod prepare;
mod pump;
mod readback;
mod renderer;
mod sky;
mod surface;
mod targets;
mod upload;
#[cfg(test)]
mod webgl2;
mod world_overlay;

pub use compress::{
    adapter_plan, caps_of, compression_named, encode_level_named, encode_reservation, format_name,
    plan_features, plan_for, stored_features, texture_caps, EncodeStats, ImageryCompression,
    DEFAULT_ENCODE_LEVEL, MAX_ENCODE_LEVEL,
};
pub use context::{
    switchable_features, ContextError, GpuContext, ImageryTextures, DEPTH_FORMAT, SAMPLES,
    TEXTURE_FORMAT,
};
pub use host::{view_proj_from_world, DepthDirection, HostFrame, HostPass, TargetLayout};
pub use ops::UploadOps;
pub use overlay::{OverlayMask, OverlayRenderer, OverlayVertex};
pub use prepare::{
    prepare, prepare_drape, Levels, PreparedDrape, PreparedMesh, PreparedTile, Vertex, STITCH_ROWS,
};
pub use pump::{
    ContentPump, Drawn, FrameUploads, PumpTotals, Resolution, STITCH_SLIDE_FRAMES,
    UPLOAD_SHARE_PERMILLE,
};
pub use readback::{MappedFrame, Readback, ReadbackRing};
pub use renderer::{sky_wgsl, LineVertex, SkyLightRead, TileRenderer, ViewUniform};
pub use sky::{ecef_of, Atmosphere, AtmosphereConfig, Bodies, Lights, SkyFrame, SkyView};
pub use surface::{
    draw_to_surface, draw_to_texture, opaque_alpha, preferred_format, surface_format, After,
    FrameOnSurface, FrameOnTexture, FramePass, Presented, SurfaceFormat, NOTHING_AFTER,
};
pub use targets::FrameTargets;
pub use world_overlay::{Picture, WorldOverlayError, WorldOverlayRenderer, WorldOverlayView};
