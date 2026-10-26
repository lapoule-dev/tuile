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
use crate::host::{HostFrame, HostPass, TargetLayout};
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
    /// When given, the drawn frame is copied into a slot of the ring,
    /// tagged with the number beside it, and mapped once submitted: a
    /// recording that never waits for its pictures
    /// ([`crate::ReadbackRing`]).
    pub record: Option<(&'a mut crate::ReadbackRing, u64)>,
}

/// The attachments the frame's own pass used, for a pass that follows it on
/// the same encoder.
///
/// A second pass over the same picture — one that needs a depth attachment
/// the frame's own cannot be, such as overlays tested against overlays alone
/// — draws into `colour` and resolves into `resolve` exactly as the frame's
/// pass did, loading rather than clearing. Every attachment of a pass shares
/// its sample count, so the multisampled target is the one to draw into and
/// not the drawable.
pub struct FramePass<'a> {
    /// The multisampled colour target the frame was drawn into.
    pub colour: &'a wgpu::TextureView,
    /// What it resolves into.
    pub resolve: &'a wgpu::TextureView,
    /// The frame in device pixels.
    pub size: (u32, u32),
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
/// `after` runs on the same encoder once the frame's pass has closed, with the
/// attachments it used ([`FramePass`]): that is where a second pass belongs. A
/// host with none passes `|_, _| {}`.
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
    after: impl FnOnce(&mut wgpu::CommandEncoder, &FramePass<'_>),
) -> Presented {
    let FrameOnSurface {
        gpu,
        surface,
        targets,
        clear,
        trace,
        record,
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
    // Viewed in the format the frame is drawn in, which is the surface's own
    // format everywhere but a browser: there the canvas is linear and the
    // frame is resolved into an sRGB view of it. See [`surface_format`].
    //
    // Where the surface can be neither, the frame is resolved into a texture
    // of its own and encoded into the surface by one more pass.
    let surface_view = targets.encode.as_ref().map(|_| {
        frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default())
    });
    let mut encoder = encode_frame(
        gpu,
        targets,
        &frame.texture,
        surface_view.as_ref(),
        clear,
        paint,
        after,
    );
    // Before the frame is handed to the compositor: afterwards the texture
    // belongs to the surface again and there is nothing left to copy.
    if let Some(trace) = trace {
        trace.capture(gpu, &mut encoder, &frame.texture, targets.size());
    }
    let record = record.map(|(ring, tag)| {
        ring.capture(gpu, &mut encoder, &frame.texture, targets.size(), tag);
        ring
    });
    gpu.queue.submit([encoder.finish()]);
    if let Some(ring) = record {
        ring.submitted();
    }
    before_present();
    gpu.queue.present(frame);
    Presented::Yes
}

/// Where a frame drawn off screen goes: a texture the host owns.
///
/// The same frame as [`FrameOnSurface`]'s, pass for pass — the multisampled
/// target, its depth, the resolve — with a texture in place of the drawable.
/// That is the whole difference between a window and a headless run, and it
/// is meant to be: a test that draws through a renderer of its own is testing
/// a renderer nobody runs.
pub struct FrameOnTexture<'a> {
    pub gpu: &'a GpuContext,
    /// Resolved into: one sample, the targets' size and
    /// [format](FrameTargets::format), `RENDER_ATTACHMENT` — and `COPY_SRC`
    /// when it is to be read back.
    pub texture: &'a wgpu::Texture,
    pub targets: &'a FrameTargets,
    pub clear: wgpu::Color,
    /// When given, the drawn frame is copied into it before the submit; the
    /// caller reads it back with [`Readback::take`].
    pub capture: Option<&'a mut Readback>,
}

/// Draws one frame into a texture: [`draw_to_surface`] without a surface to
/// acquire, so without the ways acquiring one fails — and without a present.
/// `paint` receives the same pass.
pub fn draw_to_texture(
    into: FrameOnTexture<'_>,
    paint: impl FnOnce(&mut wgpu::RenderPass<'_>),
    after: impl FnOnce(&mut wgpu::CommandEncoder, &FramePass<'_>),
) {
    let FrameOnTexture {
        gpu,
        texture,
        targets,
        clear,
        capture,
    } = into;
    let mut encoder = encode_frame(gpu, targets, texture, None, clear, paint, after);
    if let Some(capture) = capture {
        capture.capture(gpu, &mut encoder, texture, targets.size());
    }
    gpu.queue.submit([encoder.finish()]);
}

/// The frame's one render pass, into `targets` and resolved into `drawable`
/// — or into the targets' own resolve texture and encoded into `encoded`,
/// where the drawable can be neither sRGB nor viewed as sRGB. Returns the
/// encoder, for whatever the host copies before submitting it.
fn encode_frame(
    gpu: &GpuContext,
    targets: &FrameTargets,
    drawable: &wgpu::Texture,
    encoded: Option<&wgpu::TextureView>,
    clear: wgpu::Color,
    paint: impl FnOnce(&mut wgpu::RenderPass<'_>),
    after: impl FnOnce(&mut wgpu::CommandEncoder, &FramePass<'_>),
) -> wgpu::CommandEncoder {
    let drawable_view;
    let resolve = match targets.encode.as_ref() {
        Some(encode) => &encode.resolved,
        None => {
            drawable_view = drawable.create_view(&wgpu::TextureViewDescriptor {
                format: Some(targets.format()),
                ..Default::default()
            });
            &drawable_view
        }
    };
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    // Our own frame is a host like any other: the pass a host's frame gets is
    // this one. It lends its depth's stencil, so the pass is drawn straight
    // into it, and it asks for a fresh frame.
    HostPass::new(gpu, TargetLayout::own(targets.format())).encode(
        gpu,
        &mut encoder,
        HostFrame {
            // Draw into the multisampled target and resolve into the
            // drawable: the samples are averaged on the way out, which is
            // where the staircase on the limb disappears.
            colour: &targets.colour,
            resolve: Some(resolve),
            depth: &targets.depth,
            size: targets.size(),
            clear: Some(clear),
        },
        paint,
    );
    // A pass of its own, on the same encoder and before the frame is
    // encoded: what needs a depth attachment the frame's own pass cannot
    // offer. See [`FramePass`].
    after(
        &mut encoder,
        &FramePass {
            colour: &targets.colour,
            resolve,
            size: targets.size(),
        },
    );
    if let (Some(encode), Some(view)) = (targets.encode.as_ref(), encoded) {
        encode.encode(&mut encoder, view);
    }
    encoder
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

/// The formats a host configures a surface with, and draws in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceFormat {
    /// What the surface is configured with.
    pub surface: wgpu::TextureFormat,
    /// What the frame is drawn in: the pipelines and [`FrameTargets`] are
    /// built for it. When it differs from [`SurfaceFormat::surface`] it is the
    /// sRGB counterpart of it: viewed as such through the configuration's
    /// `view_formats`, or — when [`SurfaceFormat::encode`] — reached through
    /// an encoding pass.
    pub render: wgpu::TextureFormat,
    /// The surface is linear and cannot be viewed as sRGB: the frame is
    /// resolved into a texture of its own and the transfer curve applied by
    /// one more pass ([`FrameTargets::for_surface`] builds it).
    pub encode: bool,
}

impl SurfaceFormat {
    /// The `view_formats` the surface configuration must carry.
    pub fn view_formats(&self) -> Vec<wgpu::TextureFormat> {
        if self.render == self.surface || self.encode {
            Vec::new()
        } else {
            vec![self.render]
        }
    }
}

/// [`preferred_format`], for surfaces that offer no sRGB format at all.
///
/// A browser's canvas is one: WebGPU configures it as `bgra8unorm` or
/// `rgba8unorm` and nothing else, and drawing into that linear format writes
/// every colour without its transfer curve — the washed-out globe
/// [`preferred_format`] exists to prevent. The answer there is to keep the
/// linear surface and draw into its sRGB *view*.
///
/// A view in another format needs `SURFACE_VIEW_FORMATS`, which `downlevel`
/// (the adapter's) says whether the device has. OpenGL ES without an sRGB
/// colour space for its window (the Android emulator's) has neither an sRGB
/// format nor the view: there the curve is applied by an encoding pass.
pub fn surface_format(
    caps: &wgpu::SurfaceCapabilities,
    downlevel: wgpu::DownlevelFlags,
) -> SurfaceFormat {
    let surface = preferred_format(caps);
    let render = surface.add_srgb_suffix();
    SurfaceFormat {
        surface,
        render,
        encode: render != surface
            && !downlevel.contains(wgpu::DownlevelFlags::SURFACE_VIEW_FORMATS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(formats: &[wgpu::TextureFormat]) -> wgpu::SurfaceCapabilities {
        wgpu::SurfaceCapabilities {
            formats: formats.to_vec(),
            ..Default::default()
        }
    }

    /// Where an sRGB format is on offer it is used as it is: the desktop and
    /// the phone configure exactly what they always did.
    #[test]
    fn an_offered_srgb_format_is_drawn_in_directly() {
        let chosen = surface_format(
            &caps(&[
                wgpu::TextureFormat::Bgra8Unorm,
                wgpu::TextureFormat::Bgra8UnormSrgb,
            ]),
            wgpu::DownlevelFlags::empty(),
        );
        assert_eq!(chosen.surface, wgpu::TextureFormat::Bgra8UnormSrgb);
        assert_eq!(chosen.render, chosen.surface);
        assert!(chosen.view_formats().is_empty());
        assert!(!chosen.encode);
    }

    /// A canvas that only offers linear formats is drawn into through its
    /// sRGB view, never written linear.
    #[test]
    fn a_linear_only_surface_is_drawn_through_its_srgb_view() {
        let chosen = surface_format(
            &caps(&[
                wgpu::TextureFormat::Bgra8Unorm,
                wgpu::TextureFormat::Rgba16Float,
            ]),
            wgpu::DownlevelFlags::SURFACE_VIEW_FORMATS,
        );
        assert_eq!(chosen.surface, wgpu::TextureFormat::Bgra8Unorm);
        assert_eq!(chosen.render, wgpu::TextureFormat::Bgra8UnormSrgb);
        assert!(chosen.render.is_srgb());
        assert_eq!(chosen.view_formats(), [wgpu::TextureFormat::Bgra8UnormSrgb]);
        assert!(!chosen.encode);
    }

    /// OpenGL ES without an sRGB window (the Android emulator's): no sRGB
    /// format and no view of one. Still drawn in sRGB — through an encoding
    /// pass, with nothing asked of the surface's configuration that the
    /// device would refuse.
    #[test]
    fn a_linear_surface_without_views_is_encoded_by_a_pass() {
        let chosen = surface_format(
            &caps(&[
                wgpu::TextureFormat::Rgba8Unorm,
                wgpu::TextureFormat::Bgra8Unorm,
            ]),
            wgpu::DownlevelFlags::empty(),
        );
        assert_eq!(chosen.surface, wgpu::TextureFormat::Rgba8Unorm);
        assert_eq!(chosen.render, wgpu::TextureFormat::Rgba8UnormSrgb);
        assert!(chosen.encode);
        assert!(chosen.view_formats().is_empty());
        // An sRGB surface needs no pass, whatever the device lacks.
        let srgb = surface_format(
            &caps(&[wgpu::TextureFormat::Rgba8UnormSrgb]),
            wgpu::DownlevelFlags::empty(),
        );
        assert!(!srgb.encode);
    }
}
