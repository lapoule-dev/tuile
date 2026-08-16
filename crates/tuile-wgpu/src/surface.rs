// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Driving one frame onto a surface the host created.
//!
//! The crate still creates no window and no surface — that needs a raw window
//! handle and belongs to whoever owns the event loop. But *using* a surface the
//! host hands over is not the same thing, and the sequence around it is
//! identical in every host: acquire a drawable, deal with the four ways that can
//! fail, draw into the multisampled target, resolve into the drawable, submit,
//! and present.
//!
//! Two of the mistakes in that sequence are silent, which is why it is worth
//! owning rather than documenting:
//!
//! - **not handling `Outdated` / `Lost`** — the application dies at the first
//!   resize or display change, in a place that looks nothing like a resize;
//! - **not pairing the multisampled target with the drawable as its resolve
//!   target** — or worse, sizing it with a sample count the pipelines were not
//!   built for, which fails validation on the first draw. Four test harnesses in
//!   this repository were failing exactly that check for weeks, each reporting a
//!   wgpu panic rather than whatever it was there to measure. [`FrameTargets`]
//!   removed half of that trap; this removes the other half.

use crate::context::GpuContext;
use crate::readback::Readback;
use crate::targets::FrameTargets;

/// What became of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presented {
    /// Drawn and handed to the compositor.
    Yes,
    /// The surface needs reconfiguring; `on_lost` has been called and the frame
    /// was skipped. Not an error — a resize looks exactly like this.
    Reconfigured,
    /// Nothing to draw into this time — occluded, or the compositor timed out.
    /// Try again next frame.
    Skipped,
    /// The surface refused the request. Rare, and worth saying out loud.
    ///
    /// Returned rather than logged because this crate takes no logging
    /// dependency: it is the host that has a subscriber, and the host that knows
    /// whether one dropped frame matters in its session.
    Invalid,
}

/// Where a frame is going, and what it should be cleared to.
///
/// A struct rather than five more arguments: the three references are always
/// the same three, and naming them at the call site is what makes it obvious
/// that the multisampled target and the surface belong together.
pub struct FrameOnSurface<'a, 'w> {
    pub gpu: &'a GpuContext,
    pub surface: &'a wgpu::Surface<'w>,
    pub targets: &'a FrameTargets,
    pub clear: wgpu::Color,
    /// When given, the drawn frame is copied into it before the submit.
    pub trace: Option<&'a mut Readback>,
}

/// Draws one frame onto the surface and presents it.
///
/// `paint` receives a render pass whose colour attachment is the multisampled
/// target, resolving into the drawable, and whose depth attachment is the
/// matching depth buffer. Both are cleared: colour to `clear`, depth to 1.
///
/// `on_lost` reconfigures the surface. It is a callback because only the host
/// knows the configuration it wants — usage flags, present mode, size.
///
/// `before_present` is the one place a windowing library still gets a word in:
/// winit's `pre_present_notify`, which lets the compositor start its own work.
/// A host with nothing to say there passes `|| {}`.
///
/// `trace`, when given, has the drawn frame copied into it before the submit —
/// which is the only moment it can be, since afterwards the texture belongs to
/// the surface again. Reading the pixels back is left to the caller, after this
/// returns, because that is the part that blocks.
pub fn draw_to_surface(
    into: FrameOnSurface<'_, '_>,
    on_lost: impl FnOnce(),
    before_present: impl FnOnce(),
    paint: impl FnOnce(&mut wgpu::RenderPass<'_>),
) -> Presented {
    let FrameOnSurface {
        gpu,
        surface,
        targets,
        clear,
        trace,
    } = into;
    let frame = match surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
        wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
            on_lost();
            return Presented::Reconfigured;
        }
        wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Timeout => {
            return Presented::Skipped;
        }
        wgpu::CurrentSurfaceTexture::Validation => return Presented::Invalid,
    };
    let drawable = frame
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());

    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("tuile frame"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                // Draw into the multisampled target and resolve into the
                // drawable: the samples are averaged on the way out, which is
                // where the staircase on the limb disappears.
                view: &targets.colour,
                resolve_target: Some(&drawable),
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(clear),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &targets.depth,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        paint(&mut pass);
    }
    // Before the frame is handed to the compositor: afterwards the texture
    // belongs to the surface again and there is nothing left to copy.
    if let Some(trace) = trace {
        trace.capture(gpu, &mut encoder, &frame.texture, targets.size());
    }
    gpu.queue.submit([encoder.finish()]);
    before_present();
    frame.present();
    Presented::Yes
}

/// The surface format to configure with: the first sRGB one on offer.
///
/// Worth a function because getting it wrong is silent and looks like an
/// opinion. Taking `caps.formats[0]` — the obvious thing — yields a linear
/// format on most backends, every colour is then written without the transfer
/// curve, and the globe comes out washed out and flat. Nothing errors; it just
/// looks slightly wrong for ever.
pub fn preferred_format(caps: &wgpu::SurfaceCapabilities) -> wgpu::TextureFormat {
    caps.formats
        .iter()
        .copied()
        .find(wgpu::TextureFormat::is_srgb)
        .unwrap_or(caps.formats[0])
}
