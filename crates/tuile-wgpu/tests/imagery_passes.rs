// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A drape wider than one draw can bind is **redrawn**, not coarsened.
//!
//! The device says how many textures a fragment shader may bind at once — 16 on
//! the WebGPU baseline and on WebGL2, 128 on Metal. A mosaic needing more than
//! that used to lose the surplus: the layer list was truncated at the slot count
//! and the loader coarsened the whole mosaic a level until it fit. Coarsening is
//! not a small loss of sharpness. It makes neighbouring geometry tiles disagree
//! about which imagery level they drape, and that disagreement is exactly what
//! makes them visibly differ in colour.
//!
//! The reference implementation does not coarsen; it redraws. Each pass consumes
//! as many layers as the device will bind, then the same geometry is submitted
//! again with `LESS_OR_EQUAL` depth — so it is not rejected by the depth its own
//! first pass wrote — and alpha-blended over what is already there, starting
//! from a transparent colour so a fragment this batch does not cover keeps the
//! pass beneath it.
//!
//! Two claims, and a fix that gets either one wrong is worse than no fix:
//!
//! | claim | what breaking it looks like |
//! |---|---|
//! | a layer past the first batch reaches the screen | the sharpest imagery silently missing |
//! | ground the later batch does not cover keeps the earlier pass | holes, or untextured ground painted over the mosaic |
//!
//! Both are asserted below against read-back pixels, on a fixture built to have
//! exactly one layer more than the device will bind.
//!
//! A machine without a usable adapter skips, with a printed reason. Set
//! `TUILE_REQUIRE_GPU=1` to turn that skip into a failure.

use glam::{DVec3, Mat4, Vec3};
use std::sync::Arc;
use tuile_core::content::{DecodedMesh, DecodedTexture, DecodedTileContent, MaterialDesc};
use tuile_core::raster::{ImageryCoord, ImageryLayer};
use tuile_wgpu::{
    prepare, GpuContext, TileRenderer, ViewUniform, DEPTH_FORMAT, SAMPLES, TEXTURE_FORMAT,
};

const SIZE: u32 = 256;

/// A GPU, or a stated reason there is none.
fn gpu() -> Option<GpuContext> {
    match pollster::block_on(GpuContext::headless()) {
        Ok(gpu) => Some(gpu),
        Err(e) => {
            assert!(
                std::env::var("TUILE_REQUIRE_GPU").is_err(),
                "TUILE_REQUIRE_GPU is set and there is no usable adapter: {e}"
            );
            eprintln!("skipping GPU test: no usable adapter ({e})");
            None
        }
    }
}

/// A unit quad in the z = 0 plane, facing the camera, with uv over `[0, 1]²`.
///
/// Deliberately not terrain. What is being measured is which *layer* reaches a
/// fragment, and a quantized mesh would bring its own relief, its own skirts and
/// its own reprojection into a question that has nothing to do with any of them.
fn quad() -> DecodedTileContent {
    DecodedTileContent {
            withheld_drape: None,
        meshes: vec![DecodedMesh {
            positions: vec![
                [-1.0, -1.0, 0.0],
                [1.0, -1.0, 0.0],
                [1.0, 1.0, 0.0],
                [-1.0, 1.0, 0.0],
            ],
            normals: Some(vec![[0.0, 0.0, 1.0]; 4]),
            uvs: Some(vec![[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]]),
            indices: vec![0, 1, 2, 0, 2, 3],
            material: MaterialDesc {
                base_color_factor: [1.0, 1.0, 1.0, 1.0],
                base_color_texture: None,
            },
        }],
        textures: Vec::new(),
        imagery: Vec::new(),
        local_origin_ecef: DVec3::ZERO,
        transform_local: Mat4::IDENTITY,
    }
}

/// A layer of one flat colour over `coverage`, in the tile's own uv space.
fn layer(index: u32, colour: [u8; 3], coverage: [f32; 4]) -> ImageryLayer {
    ImageryLayer {
        // Distinct per layer: the shared-imagery pool is keyed by coord, so two
        // layers claiming the same one would be the same texture and the test
        // would compare a colour with itself.
        coord: ImageryCoord {
            level: 1,
            x: u64::from(index),
            y: 0,
        },
        texture: Arc::new(DecodedTexture {
            width: 2,
            height: 2,
            rgba8: [[colour[0], colour[1], colour[2], 255u8]; 4].concat(),
        }),
        coverage,
        // Maps the covered part of the tile onto the whole texture. Immaterial
        // for a flat colour, and written correctly anyway so the fixture does
        // not quietly depend on the placement being ignored.
        translation: [-coverage[0] / (coverage[2] - coverage[0]), 0.0],
        scale: [1.0 / (coverage[2] - coverage[0]), 1.0],
    }
}

/// Renders the quad head-on and returns the resolved frame, RGBA8.
fn render(gpu: &GpuContext, content: &DecodedTileContent) -> Vec<u8> {
    let make = |label, format, usage, samples| {
        gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: SIZE,
                height: SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: samples,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        })
    };
    // Multisampled, because the pipelines are built for `SAMPLES` and a
    // single-sample pass will not accept them.
    let msaa = make(
        "passes msaa",
        TEXTURE_FORMAT,
        wgpu::TextureUsages::RENDER_ATTACHMENT,
        SAMPLES,
    );
    let msaa_view = msaa.create_view(&wgpu::TextureViewDescriptor::default());
    let target = make(
        "passes target",
        TEXTURE_FORMAT,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        1,
    );
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let depth = make(
        "passes depth",
        DEPTH_FORMAT,
        wgpu::TextureUsages::RENDER_ATTACHMENT,
        SAMPLES,
    );
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());

    let renderer = TileRenderer::new(gpu, TEXTURE_FORMAT);
    let view_m = Mat4::look_at_rh(Vec3::new(0.0, 0.0, 2.0), Vec3::ZERO, Vec3::Y);
    let proj = Mat4::perspective_rh(60f32.to_radians(), 1.0, 0.1, 10.0);
    renderer.set_view(
        &gpu.queue,
        &ViewUniform {
            view_proj: (proj * view_m).to_cols_array(),
            // Ambient only: the question is which layer won, not how it is lit.
            params: [1.0, 0.0, 0.0, 0.0],
            ..Default::default()
        },
    );

    let tile = prepare(gpu, content, DVec3::ZERO);
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("passes"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &msaa_view,
                depth_slice: None,
                resolve_target: Some(&view),
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: Some(wgpu::Operations {
                    // Cleared to 0, and never read back: the mark says "a
                    // surface that owns this ground drew here", which is only
                    // true within one frame.
                    load: wgpu::LoadOp::Clear(0),
                    store: wgpu::StoreOp::Discard,
                }),
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        renderer.render(&mut pass, std::iter::once(&tile), false);
    }

    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("passes readback"),
        size: (SIZE * SIZE * 4) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(SIZE * 4),
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
    let pixels = slice.get_mapped_range().to_vec();
    readback.unmap();
    pixels
}

/// The pixel at the middle of the frame's left or right half.
///
/// Both are well inside the quad and well away from the uv = 0.5 boundary the
/// second pass stops at, so neither reads a multisampled edge.
fn sample(pixels: &[u8], fraction_across: f32) -> [u8; 3] {
    let col = (SIZE as f32 * fraction_across) as u32;
    let row = SIZE / 2;
    let i = ((row * SIZE + col) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2]]
}

fn close(got: [u8; 3], want: [u8; 3]) -> bool {
    const TOLERANCE: i32 = 12;
    (0..3).all(|c| (i32::from(got[c]) - i32::from(want[c])).abs() <= TOLERANCE)
}

const RED: [u8; 3] = [220, 20, 20];
const GREEN: [u8; 3] = [20, 220, 20];

/// **A layer past the first batch reaches the screen, and only where it covers.**
///
/// The fixture is built to be exactly one layer too wide: `slots` layers of red
/// over the whole quad, then one green layer over its left half — which lands in
/// the second pass, whatever the device's slot count turns out to be.
///
/// Reverting the redraw makes the left half red: the surplus layer is packed
/// into a table nothing samples, and the assertion below fails naming it. That
/// is the whole reason the green layer covers half rather than all of the quad —
/// a full-width green layer would still be "the last one wins" if the passes
/// were merged, and the test would pass for the wrong reason.
#[test]
fn a_layer_past_the_first_batch_is_drawn_where_it_covers() {
    let Some(gpu) = gpu() else { return };
    let slots = gpu.imagery_slots;

    let mut content = quad();
    content.imagery = (0..slots)
        .map(|i| layer(i, RED, [0.0, 0.0, 1.0, 1.0]))
        .chain(std::iter::once(layer(slots, GREEN, [0.0, 0.0, 0.5, 1.0])))
        .collect();
    assert_eq!(
        content.imagery.len(),
        slots as usize + 1,
        "the fixture must be exactly one layer wider than one draw can bind"
    );

    let pixels = render(&gpu, &content);
    let left = sample(&pixels, 0.25);
    let right = sample(&pixels, 0.75);

    assert!(
        close(left, GREEN),
        "the layer past the first batch of {slots} did not reach the screen: \
         the left half is {left:?}, expected green {GREEN:?}"
    );
    assert!(
        close(right, RED),
        "the second pass painted over ground its own layer does not cover: \
         the right half is {right:?}, expected the first pass's red {RED:?}"
    );
}

/// **A drape that fits in one draw is unchanged by the machinery above.**
///
/// The regression the multi-pass path could plausibly cause, and the one nobody
/// would look for: every tile on the globe carries fewer layers than the device
/// binds, so if a single-pass tile were drawn twice, or its one pass given the
/// later-pass flag, the whole globe would go dark or transparent and the test
/// above would still pass.
#[test]
fn a_drape_that_fits_in_one_draw_is_drawn_exactly_once() {
    let Some(gpu) = gpu() else { return };

    let mut content = quad();
    content.imagery = vec![
        layer(0, RED, [0.0, 0.0, 1.0, 1.0]),
        layer(1, GREEN, [0.0, 0.0, 0.5, 1.0]),
    ];

    let pixels = render(&gpu, &content);
    assert!(
        close(sample(&pixels, 0.25), GREEN),
        "two layers in one pass: the later one must win where it covers"
    );
    assert!(
        close(sample(&pixels, 0.75), RED),
        "two layers in one pass: the earlier one must survive where the later \
         does not cover"
    );
}

/// **Content with no imagery at all still draws its base colour.**
///
/// The empty case is where a pass-splitting loop goes wrong quietly: `chunks`
/// over an empty slice yields nothing, a mesh ends up with no bind group, and
/// the shell behind the globe — which carries no imagery by construction —
/// stops being drawn. That is bare ground, which `CLAUDE.md` calls the one
/// forbidden output.
#[test]
fn content_with_no_imagery_still_draws() {
    let Some(gpu) = gpu() else { return };

    let mut content = quad();
    content.meshes[0].material.base_color_factor = [0.0, 1.0, 0.0, 1.0];
    let pixels = render(&gpu, &content);

    let middle = sample(&pixels, 0.5);
    assert!(
        middle[1] > 100,
        "a tile with no imagery drew nothing: the middle of the frame is \
         {middle:?}, and the clear colour is black"
    );
}
