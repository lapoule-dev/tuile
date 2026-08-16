// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The textures a frame is drawn into, before it reaches a surface.
//!
//! Every wgpu host writes these two by hand and writes them the same way, and
//! the one detail that is easy to get wrong is the one that fails loudly: the
//! sample count of the colour target, the depth target and the pipelines must
//! agree exactly, or the first draw call fails validation rather than merely
//! looking wrong. Four separate test harnesses in this repository were silently
//! failing that check for weeks, each having been written before MSAA reached
//! the renderer, and each reporting a wgpu validation panic instead of whatever
//! it was actually there to measure.
//!
//! Owning the pair here is what makes that impossible to get wrong: the count
//! comes from [`crate::SAMPLES`], which is the same constant the pipelines are
//! built against.
//!
//! The surface itself is **not** here. This crate creates no window and no
//! surface — the host owns those, and owns the resolve into them.

use crate::context::{GpuContext, DEPTH_FORMAT, SAMPLES};

/// The multisampled colour target and its depth buffer.
pub struct FrameTargets {
    /// Draw into this; resolve into the surface texture.
    pub colour: wgpu::TextureView,
    pub depth: wgpu::TextureView,
    format: wgpu::TextureFormat,
    size: (u32, u32),
}

impl FrameTargets {
    /// `size` is in **device** pixels — what the surface is configured with,
    /// not what a stylesheet would call a point.
    pub fn new(gpu: &GpuContext, format: wgpu::TextureFormat, size: (u32, u32)) -> Self {
        let size = (size.0.max(1), size.1.max(1));
        Self {
            colour: attachment(gpu, format, size, "tuile frame colour"),
            depth: attachment(gpu, DEPTH_FORMAT, size, "tuile frame depth"),
            format,
            size,
        }
    }

    /// Rebuilds both targets for a new size, and says whether it had to.
    ///
    /// Cheap to call every frame: a resize that is not a resize does nothing,
    /// which means a host can simply hand over the current window size instead
    /// of tracking whether it changed.
    pub fn resize(&mut self, gpu: &GpuContext, size: (u32, u32)) -> bool {
        let size = (size.0.max(1), size.1.max(1));
        if size == self.size {
            return false;
        }
        *self = Self::new(gpu, self.format, size);
        true
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }
}

fn attachment(
    gpu: &GpuContext,
    format: wgpu::TextureFormat,
    size: (u32, u32),
    label: &str,
) -> wgpu::TextureView {
    gpu.device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            // The one number that must match the pipelines. See the module doc.
            sample_count: SAMPLES,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default())
}
