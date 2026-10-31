// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The headless viewer's screen: one texture per eye, read back after every
//! frame and scanned for bare ground.
//!
//! Drawn into exactly as a window's surface is — the same targets, the same
//! pass, the same resolve ([`tuile_wgpu::draw_to_texture`]) — and then copied
//! back, so a test looks at the picture the frame made rather than at what the
//! bookkeeping says it made. What each eye's scan finds is published where
//! every other number the engine counts is: `eye_ground_pixels` and
//! `eye_black_pixels` in `tuile_core::metrics`.

use std::cell::Cell;

use tuile_camera::GlobeCamera;
use tuile_tape::scan::Bare;
use tuile_wgpu::{GpuContext, Readback};

/// How many pixels of an eye are scanned, about: see [`Offscreen::read`].
const SAMPLES: f64 = 262_144.0;

/// The format an eye is drawn in: the renderer's own sRGB target.
pub(crate) const FORMAT: wgpu::TextureFormat = tuile_wgpu::TEXTURE_FORMAT;

/// One eye's picture.
pub(crate) struct Eye {
    texture: wgpu::Texture,
    readback: Readback,
    /// The last frame read back: tightly packed RGBA8, sRGB.
    pub(crate) pixels: Option<Vec<u8>>,
    /// What the scan of it found.
    pub(crate) bare: Bare,
}

/// The textures a headless frame is drawn into.
pub(crate) struct Offscreen {
    pub(crate) eyes: Vec<Eye>,
    size: (u32, u32),
    count: usize,
    released: bool,
    /// A frame asked for and not drawn yet: what a window's
    /// `request_redraw` is, with the loop being whoever steps the session.
    redraw: Cell<bool>,
}

impl Offscreen {
    pub(crate) fn new(gpu: &GpuContext, size: (u32, u32), eyes: usize) -> Self {
        let size = (size.0.max(1), size.1.max(1));
        let count = eyes.max(1);
        Self {
            eyes: (0..count).map(|_| eye(gpu, size)).collect(),
            size,
            count,
            released: false,
            redraw: Cell::new(true),
        }
    }

    pub(crate) fn request_redraw(&self) {
        self.redraw.set(true);
    }

    /// Whether a frame was asked for since the last call.
    pub(crate) fn take_redraw(&self) -> bool {
        self.redraw.replace(false)
    }

    pub(crate) fn is_released(&self) -> bool {
        self.released
    }

    pub(crate) fn resize(&mut self, gpu: &GpuContext, size: (u32, u32)) {
        let size = (size.0.max(1), size.1.max(1));
        if size != self.size || self.eyes.is_empty() {
            self.size = size;
            if !self.released {
                self.eyes = (0..self.count).map(|_| eye(gpu, size)).collect();
            }
        }
    }

    /// Lets go of every eye's texture, as a phone lets go of its surface.
    pub(crate) fn release(&mut self) {
        self.eyes.clear();
        self.released = true;
    }

    pub(crate) fn restore(&mut self, gpu: &GpuContext, size: (u32, u32)) {
        self.released = false;
        self.size = (size.0.max(1), size.1.max(1));
        self.eyes = (0..self.count).map(|_| eye(gpu, self.size)).collect();
    }

    /// Draws `paint` into every eye, each with its own view set first.
    pub(crate) fn draw(
        &mut self,
        gpu: &GpuContext,
        targets: &tuile_wgpu::FrameTargets,
        views: &[tuile_wgpu::ViewUniform],
        set_view: impl Fn(&tuile_wgpu::ViewUniform),
        clear: wgpu::Color,
        // The frame's pass and what follows it, together: one frame is drawn
        // in two, and splitting them into two arguments only to count them
        // separately is not a clearer signature.
        draw: (
            impl Fn(&mut wgpu::RenderPass<'_>),
            impl Fn(&mut wgpu::CommandEncoder, &tuile_wgpu::FramePass<'_>),
        ),
    ) {
        let (paint, after) = draw;
        for (eye, view) in self.eyes.iter_mut().zip(views) {
            // Written before the eye's submit, so the eye's pass reads it:
            // a queue write lands at the start of the next submission.
            set_view(view);
            tuile_wgpu::draw_to_texture(
                tuile_wgpu::FrameOnTexture {
                    gpu,
                    texture: &eye.texture,
                    targets,
                    clear,
                    capture: Some(&mut eye.readback),
                },
                &paint,
                &after,
            );
        }
    }

    /// Reads every eye back and scans it against the camera that drew it —
    /// the picture's rectangle of it, whose shape the camera was drawn for:
    /// the bars around it (a held aspect, the credits bar) are not the
    /// camera's.
    ///
    /// **Blocks on the GPU**, every frame: a headless run is there to look
    /// at its pictures, and a picture not read back is not looked at.
    pub(crate) fn read(
        &mut self,
        gpu: &GpuContext,
        cameras: &[GlobeCamera],
        picture: crate::frame_host::Letterbox,
    ) {
        let m = tuile_core::metrics::metrics();
        let size = self.size;
        // About a quarter of a million rays an eye, whatever the frame: every
        // pixel of a small one, every fourth of a phone's — a hole is
        // thousands of pixels, and the scan runs on the frame's own thread.
        let step = ((f64::from(size.0) * f64::from(size.1) / SAMPLES)
            .sqrt()
            .ceil() as usize)
            .max(1);
        for (index, (eye, camera)) in self.eyes.iter_mut().zip(cameras).enumerate() {
            eye.pixels = eye.readback.take(gpu, size);
            eye.bare = eye.pixels.as_ref().map_or(Bare::default(), |pixels| {
                let (cropped, w, h) = crop(pixels, size, picture);
                tuile_tape::scan::bare_ground(&cropped, w, h, &frame_of(camera), step)
            });
            m.eye_ground_pixels.set(index, eye.bare.ground);
            m.eye_black_pixels.set(index, eye.bare.black);
        }
    }

    pub(crate) fn size(&self) -> (u32, u32) {
        self.size
    }
}

/// The picture's rectangle of a frame of `size`, tightly packed RGBA8, and
/// its size. The whole frame when the picture is the frame.
fn crop(
    pixels: &[u8],
    size: (u32, u32),
    picture: crate::frame_host::Letterbox,
) -> (std::borrow::Cow<'_, [u8]>, u32, u32) {
    let x = picture.x.min(size.0);
    let y = picture.y.min(size.1);
    let w = picture.width.min(size.0 - x);
    let h = picture.height.min(size.1 - y);
    if (x, y, w, h) == (0, 0, size.0, size.1) || w == 0 || h == 0 {
        return (std::borrow::Cow::Borrowed(pixels), size.0, size.1);
    }
    let stride = size.0 as usize * 4;
    let mut out = Vec::with_capacity(w as usize * h as usize * 4);
    for row in y..y + h {
        let start = row as usize * stride + x as usize * 4;
        out.extend_from_slice(&pixels[start..start + w as usize * 4]);
    }
    (std::borrow::Cow::Owned(out), w, h)
}

/// A camera, as a tape and a scan speak of one.
pub(crate) fn frame_of(camera: &GlobeCamera) -> tuile_tape::Frame {
    tuile_tape::Frame {
        position: camera.position.to_array(),
        direction: camera.direction.to_array(),
        up: camera.up.to_array(),
        fovy: camera.fovy,
    }
}

fn eye(gpu: &GpuContext, size: (u32, u32)) -> Eye {
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("tuile headless eye"),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    Eye {
        texture,
        readback: Readback::default(),
        pixels: None,
        bare: Bare::default(),
    }
}
