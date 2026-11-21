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
use crate::encode::Encode;
use crate::surface::SurfaceFormat;

/// The multisampled colour target and its depth buffer.
pub struct FrameTargets {
    /// Draw into this; resolve into the surface texture.
    pub colour: wgpu::TextureView,
    pub depth: wgpu::TextureView,
    /// The depth alone, viewed for a shader to read sample by sample
    /// (the mist pass, [`crate::MistRenderer`]) — where the device can
    /// bind a multisampled depth, which WebGL2 cannot; there, `None`.
    pub depth_binding: Option<wgpu::TextureView>,
    format: wgpu::TextureFormat,
    size: (u32, u32),
    /// The surface's format, when the frame reaches it through an encoding
    /// pass rather than straight from the resolve: see [`crate::encode`].
    surface: Option<wgpu::TextureFormat>,
    pub(crate) encode: Option<Encode>,
}

impl FrameTargets {
    /// `size` is in **device** pixels — what the surface is configured with,
    /// not what a stylesheet would call a point.
    pub fn new(gpu: &GpuContext, format: wgpu::TextureFormat, size: (u32, u32)) -> Self {
        let size = (size.0.max(1), size.1.max(1));
        let readable = crate::MistRenderer::supported(gpu);
        let depth = texture(
            gpu,
            DEPTH_FORMAT,
            size,
            "tuile frame depth",
            if readable {
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING
            } else {
                wgpu::TextureUsages::RENDER_ATTACHMENT
            },
        );
        let depth_binding = readable.then(|| {
            depth.create_view(&wgpu::TextureViewDescriptor {
                label: Some("tuile frame depth (read)"),
                aspect: wgpu::TextureAspect::DepthOnly,
                ..Default::default()
            })
        });
        Self {
            colour: attachment(gpu, format, size, "tuile frame colour"),
            depth: depth.create_view(&wgpu::TextureViewDescriptor::default()),
            depth_binding,
            format,
            size,
            surface: None,
            encode: None,
        }
    }

    /// The targets for drawing onto a surface of `format`: those of
    /// [`FrameTargets::new`], plus — where the surface can neither be sRGB
    /// nor be viewed as sRGB — the texture the frame is resolved into and the
    /// pass that encodes it into the surface.
    pub fn for_surface(gpu: &GpuContext, format: SurfaceFormat, size: (u32, u32)) -> Self {
        let mut targets = Self::new(gpu, format.render, size);
        if format.encode {
            targets.surface = Some(format.surface);
            targets.encode = Some(Encode::new(
                gpu,
                format.render,
                format.surface,
                targets.size,
            ));
        }
        targets
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
        *self = match self.surface {
            Some(surface) => Self::for_surface(
                gpu,
                SurfaceFormat {
                    surface,
                    render: self.format,
                    encode: true,
                },
                size,
            ),
            None => Self::new(gpu, self.format, size),
        };
        true
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The colour format the frame is drawn in — and so the format the
    /// drawable it resolves into is viewed as.
    pub fn format(&self) -> wgpu::TextureFormat {
        self.format
    }
}

fn attachment(
    gpu: &GpuContext,
    format: wgpu::TextureFormat,
    size: (u32, u32),
    label: &str,
) -> wgpu::TextureView {
    texture(
        gpu,
        format,
        size,
        label,
        wgpu::TextureUsages::RENDER_ATTACHMENT,
    )
    .create_view(&wgpu::TextureViewDescriptor::default())
}

fn texture(
    gpu: &GpuContext,
    format: wgpu::TextureFormat,
    size: (u32, u32),
    label: &str,
    usage: wgpu::TextureUsages,
) -> wgpu::Texture {
    gpu.device.create_texture(&wgpu::TextureDescriptor {
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
        usage,
        view_formats: &[],
    })
}
