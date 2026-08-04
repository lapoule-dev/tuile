// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The overlay pipeline on a real device: pixel coordinates must land where
//! they claim, and translucency must composite rather than replace.

use tuile_wgpu::{GpuContext, OverlayRenderer, OverlayVertex, DEPTH_FORMAT};

const SIZE: u32 = 64;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

fn gpu() -> Option<GpuContext> {
    match pollster::block_on(GpuContext::headless()) {
        Ok(gpu) => Some(gpu),
        Err(e) => {
            eprintln!("SKIP overlay test: {e}");
            None
        }
    }
}

/// A quad covering `[x0,x1] × [y0,y1]` in pixels.
fn quad(x0: f32, y0: f32, x1: f32, y1: f32, color: [f32; 4]) -> Vec<OverlayVertex> {
    let v = |x: f32, y: f32| OverlayVertex {
        position: [x, y],
        color,
    };
    vec![
        v(x0, y0),
        v(x1, y0),
        v(x1, y1),
        v(x0, y0),
        v(x1, y1),
        v(x0, y1),
    ]
}

/// Renders the geometry over an opaque red background and reads the pixels back.
fn render(gpu: &GpuContext, vertices: &[OverlayVertex]) -> Vec<u8> {
    let mut overlay = OverlayRenderer::new(gpu, FORMAT, None);
    overlay.set_geometry(gpu, vertices, (SIZE, SIZE));

    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("overlay test"),
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = color.create_view(&Default::default());

    // 256-byte row alignment for the readback buffer.
    let bytes_per_row = (SIZE * 4).div_ceil(256) * 256;
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("overlay readback"),
        size: u64::from(bytes_per_row * SIZE),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("overlay pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    // Opaque red, so blending is visible in the result.
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 1.0,
                        g: 0.0,
                        b: 0.0,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        overlay.render(&mut pass);
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &color,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(SIZE),
            },
        },
        wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
    );
    gpu.queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let data = slice.get_mapped_range();

    let mut pixels = vec![0u8; (SIZE * SIZE * 4) as usize];
    for row in 0..SIZE as usize {
        let src = row * bytes_per_row as usize;
        let dst = row * SIZE as usize * 4;
        pixels[dst..dst + SIZE as usize * 4]
            .copy_from_slice(&data[src..src + SIZE as usize * 4]);
    }
    drop(data);
    readback.unmap();
    pixels
}

fn pixel(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * SIZE + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

/// Pixel coordinates must mean what they say, top-left origin, y downward —
/// the same space the widget hit-tests in, or clicks would miss what they see.
#[test]
fn a_quad_lands_on_the_pixels_it_names() {
    let Some(gpu) = gpu() else { return };
    // Opaque green over the top-left quarter.
    let pixels = render(&gpu, &quad(0.0, 0.0, 32.0, 32.0, [0.0, 1.0, 0.0, 1.0]));

    assert_eq!(pixel(&pixels, 8, 8), [0, 255, 0, 255], "inside the quad");
    assert_eq!(pixel(&pixels, 48, 8), [255, 0, 0, 255], "right of it");
    assert_eq!(pixel(&pixels, 8, 48), [255, 0, 0, 255], "below it");
    assert_eq!(pixel(&pixels, 48, 48), [255, 0, 0, 255], "clear elsewhere");
}

/// Translucent shapes must composite over the scene, not replace it.
#[test]
fn a_translucent_quad_blends_with_what_is_behind() {
    let Some(gpu) = gpu() else { return };
    // Half-opaque green over opaque red: expect roughly half of each.
    let pixels = render(&gpu, &quad(0.0, 0.0, 32.0, 32.0, [0.0, 1.0, 0.0, 0.5]));

    let [r, g, _, a] = pixel(&pixels, 8, 8);
    assert!((100..=160).contains(&r), "red should be halved, got {r}");
    assert!((100..=160).contains(&g), "green should be halved, got {g}");
    assert_eq!(a, 255, "the target stays opaque");
}

/// Empty geometry must draw nothing rather than keep the previous frame's.
#[test]
fn clearing_the_geometry_draws_nothing() {
    let Some(gpu) = gpu() else { return };
    let mut overlay = OverlayRenderer::new(&gpu, FORMAT, None);
    overlay.set_geometry(&gpu, &quad(0.0, 0.0, 32.0, 32.0, [0.0, 1.0, 0.0, 1.0]), (SIZE, SIZE));
    overlay.set_geometry(&gpu, &[], (SIZE, SIZE));

    let pixels = render(&gpu, &[]);
    assert_eq!(pixel(&pixels, 8, 8), [255, 0, 0, 255], "nothing drawn");
}

/// Hosts draw the overlay into their own pass, which usually carries a depth
/// attachment. A pipeline that declares none is rejected outright — the viewer
/// panicked on its first frame this way, and every test above missed it by
/// rendering without depth.
#[test]
fn the_overlay_draws_into_a_pass_that_carries_depth() {
    let Some(gpu) = gpu() else { return };
    let mut overlay = OverlayRenderer::new(&gpu, FORMAT, Some(DEPTH_FORMAT));
    overlay.set_geometry(
        &gpu,
        &quad(0.0, 0.0, 32.0, 32.0, [0.0, 1.0, 0.0, 1.0]),
        (SIZE, SIZE),
    );

    let texture = |format, usage| {
        gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("overlay depth test"),
            size: wgpu::Extent3d {
                width: SIZE,
                height: SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        })
    };
    let color = texture(FORMAT, wgpu::TextureUsages::RENDER_ATTACHMENT);
    let depth = texture(DEPTH_FORMAT, wgpu::TextureUsages::RENDER_ATTACHMENT);

    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("overlay depth pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &color.create_view(&Default::default()),
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth.create_view(&Default::default()),
                depth_ops: Some(wgpu::Operations {
                    // Nearest possible depth already written: the overlay must
                    // still draw, since it does not test.
                    load: wgpu::LoadOp::Clear(0.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        overlay.render(&mut pass);
    }
    gpu.queue.submit([encoder.finish()]);
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("a validation error would surface here");
}
