// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Headless render tests: offscreen target, pixel readback, PNG dumps.
//!
//! Every test writes what it rendered to `target/test-renders/*.png` so a
//! human can look at the exact frames the assertions ran on. Tests skip
//! cleanly (with a stderr note) on machines without any GPU adapter.

use glam::{dvec3, DVec3, Mat4, Vec3};
use tuile_core::content::{DecodedMesh, DecodedTexture, DecodedTileContent, MaterialDesc};
use tuile_wgpu::{prepare, GpuContext, TileRenderer, ViewUniform, DEPTH_FORMAT};

const SIZE: u32 = 256;

fn gpu() -> Option<GpuContext> {
    match pollster::block_on(GpuContext::headless()) {
        Ok(gpu) => Some(gpu),
        Err(e) => {
            eprintln!("SKIP headless render test: {e}");
            None
        }
    }
}

/// A quad in the local XY plane ([-1,1]²), normals +Z, full uv range,
/// textured with a 2×2 checker: red, green / blue, white.
fn quad_content(origin: DVec3) -> DecodedTileContent {
    DecodedTileContent {
        meshes: vec![DecodedMesh {
            positions: vec![
                [-1.0, 1.0, 0.0],
                [1.0, 1.0, 0.0],
                [-1.0, -1.0, 0.0],
                [1.0, -1.0, 0.0],
            ],
            normals: Some(vec![[0.0, 0.0, 1.0]; 4]),
            uvs: Some(vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]]),
            indices: vec![0, 2, 1, 1, 2, 3],
            material: MaterialDesc {
                base_color_factor: [1.0; 4],
                base_color_texture: Some(0),
            },
        }],
        textures: vec![DecodedTexture {
            width: 2,
            height: 2,
            rgba8: vec![
                255, 0, 0, 255, // (0,0) red
                0, 255, 0, 255, // (1,0) green
                0, 0, 255, 255, // (0,1) blue
                255, 255, 255, 255, // (1,1) white
            ],
        }],
        imagery: Vec::new(),
        local_origin_ecef: origin,
        transform_local: Mat4::IDENTITY,
    }
}

struct Frame {
    pixels: Vec<u8>, // tightly packed RGBA8, SIZE×SIZE
}

impl Frame {
    fn px(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * SIZE + x) * 4) as usize;
        [
            self.pixels[i],
            self.pixels[i + 1],
            self.pixels[i + 2],
            self.pixels[i + 3],
        ]
    }

    fn save(&self, name: &str) {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-renders");
        std::fs::create_dir_all(&dir).expect("create test-renders dir");
        let path = dir.join(name);
        image::RgbaImage::from_raw(SIZE, SIZE, self.pixels.clone())
            .expect("image from raw")
            .save(&path)
            .expect("save png");
        eprintln!(
            "rendered frame written to {}",
            path.canonicalize().expect("path").display()
        );
    }
}

/// Renders `tiles` once with the given view and reads back the pixels.
fn render_frame(
    gpu: &GpuContext,
    renderer: &TileRenderer,
    tiles: &[&tuile_wgpu::PreparedTile],
    view: &ViewUniform,
) -> Frame {
    renderer.set_view(&gpu.queue, view);

    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test color"),
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let depth = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test depth"),
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let color_view = color.create_view(&Default::default());
    let depth_view = depth.create_view(&Default::default());

    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("test pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &color_view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.0,
                        g: 0.0,
                        b: 0.05,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        renderer.render(&mut pass, tiles.iter().copied(), false);
    }

    let bytes_per_row = SIZE * 4; // 1024, already 256-aligned
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test readback"),
        size: u64::from(bytes_per_row * SIZE),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
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
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map readback"));
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let pixels = slice.get_mapped_range().to_vec();
    Frame { pixels }
}

fn looking_down_z(distance: f32) -> ViewUniform {
    let proj = Mat4::perspective_rh(60f32.to_radians(), 1.0, 0.1, 100.0);
    let view = Mat4::look_at_rh(Vec3::new(0.0, 0.0, distance), Vec3::ZERO, Vec3::Y);
    ViewUniform {
        view_proj: (proj * view).to_cols_array(),
        sun_dir: [0.0, 0.0, -1.0, 0.0],
        params: [0.25, 0.0, 0.0, 0.0],
    }
}

#[test]
fn textured_quad_renders_with_correct_colors() {
    let Some(gpu) = gpu() else { return };
    // Far-away f64 origin, camera relative to the same origin: this is the
    // rebasing protocol working end to end on the GPU.
    let origin = dvec3(6.4e6, 1.0e6, 2.0e6);
    let tile = prepare(&gpu, &quad_content(origin), origin);
    let renderer = TileRenderer::new(&gpu, wgpu::TextureFormat::Rgba8UnormSrgb);

    let frame = render_frame(&gpu, &renderer, &[&tile], &looking_down_z(2.5));
    frame.save("headless-quad.png");

    // Background untouched in the corner.
    let corner = frame.px(4, 4);
    assert!(corner[0] < 20 && corner[1] < 20, "corner = {corner:?}");

    // The quad spans ~±0.69 NDC at distance 2.5 (fov 60°): sample inside
    // each texture quadrant, away from the bilinear seams.
    // Screen top-left ↔ uv (0,0) → red.
    let tl = frame.px(SIZE / 2 - 40, SIZE / 2 - 40);
    let tr = frame.px(SIZE / 2 + 40, SIZE / 2 - 40);
    let bl = frame.px(SIZE / 2 - 40, SIZE / 2 + 40);
    let br = frame.px(SIZE / 2 + 40, SIZE / 2 + 40);
    assert!(
        tl[0] > 150 && tl[1] < 90 && tl[2] < 90,
        "top-left = {tl:?} (want red)"
    );
    assert!(tr[1] > 150 && tr[0] < 90, "top-right = {tr:?} (want green)");
    assert!(
        bl[2] > 150 && bl[0] < 90,
        "bottom-left = {bl:?} (want blue)"
    );
    assert!(
        br[0] > 150 && br[1] > 150 && br[2] > 150,
        "bottom-right = {br:?} (want white)"
    );
}

#[test]
fn depth_test_orders_overlapping_quads() {
    let Some(gpu) = gpu() else { return };
    let origin = DVec3::ZERO;
    // A red-ish textured quad at z=0 and a plain white quad behind it at
    // z=-1, shifted right so both are partially visible.
    let front = prepare(&gpu, &quad_content(origin), origin);
    let mut behind_content = quad_content(origin);
    behind_content.meshes[0].material.base_color_texture = None;
    behind_content.transform_local =
        Mat4::from_translation(Vec3::new(2.0, 0.0, -1.0)) * Mat4::from_scale(Vec3::splat(1.0));
    let behind = prepare(&gpu, &behind_content, origin);

    let renderer = TileRenderer::new(&gpu, wgpu::TextureFormat::Rgba8UnormSrgb);
    // Draw the far quad LAST: only the depth test can order them correctly.
    let frame = render_frame(&gpu, &renderer, &[&front, &behind], &looking_down_z(3.0));
    frame.save("headless-depth.png");

    // Center belongs to the front textured quad (red quadrant at its
    // top-left); a point far right belongs to the white quad.
    let front_px = frame.px(SIZE / 2 - 30, SIZE / 2 - 30);
    assert!(front_px[0] > 120, "front quad pixel = {front_px:?}");
    let white_px = frame.px(SIZE - 20, SIZE / 2);
    assert!(
        white_px[0] > 150 && white_px[1] > 150 && white_px[2] > 150,
        "behind quad pixel = {white_px:?} (want white)"
    );
}

#[test]
fn untextured_mesh_without_normals_gets_computed_normals() {
    let Some(gpu) = gpu() else { return };
    let origin = DVec3::ZERO;
    let mut content = quad_content(origin);
    content.meshes[0].normals = None;
    content.meshes[0].material.base_color_texture = None;
    content.textures.clear();

    let tile = prepare(&gpu, &content, origin);
    let renderer = TileRenderer::new(&gpu, wgpu::TextureFormat::Rgba8UnormSrgb);
    let frame = render_frame(&gpu, &renderer, &[&tile], &looking_down_z(2.5));
    frame.save("headless-normals.png");

    // Facing the light head-on: fully lit white (not just the ambient 25%).
    let center = frame.px(SIZE / 2, SIZE / 2);
    assert!(
        center[0] > 200 && center[1] > 200 && center[2] > 200,
        "center = {center:?} (computed normals should give full lambert)"
    );
}
