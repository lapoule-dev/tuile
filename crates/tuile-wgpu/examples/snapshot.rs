// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Offscreen snapshot tool: loads a real tileset, runs the geometry server
//! in-process until tiles stabilize, renders one frame and writes a PNG.
//!
//! ```text
//! cargo run -p tuile-wgpu --example snapshot -- <tileset.json> <out.png> [size]
//! ```
//!
//! This is the daily debug tool behind the headless render tests: point it
//! at any local tileset and look at the result.

use futures_executor::LocalPool;
use futures_util::task::LocalSpawnExt;
use glam::{DVec2, Mat4, Vec3};
use std::sync::Arc;
use tuile_core::fetch::FsFetcher;
use tuile_core::geo::{ecef_to_geodetic, enu_frame};
use tuile_core::protocol::{ClientMessage, GeometryStream};
use tuile_core::runtime::in_process;
use tuile_core::tileset::Tileset;
use tuile_core::traversal::{Config, ViewState};
use tuile_wgpu::{
    ContentPump, GpuContext, TileRenderer, ViewUniform, DEPTH_FORMAT, TEXTURE_FORMAT,
};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .without_time()
        .with_target(false)
        .init();
    let mut args = std::env::args().skip(1);
    let tileset_path = args
        .next()
        .expect("usage: snapshot <tileset.json> <out.png> [size]");
    let out_path = args
        .next()
        .expect("usage: snapshot <tileset.json> <out.png> [size]");
    let size: u32 = args
        .next()
        .map(|s| s.parse().expect("size"))
        .unwrap_or(1024);

    // Load and parse the tileset; keep a copy for camera framing.
    let path = std::path::Path::new(&tileset_path)
        .canonicalize()
        .expect("path");
    let base_url = url::Url::from_file_path(&path).expect("file url");
    let bytes = std::fs::read(&path).expect("read tileset.json");
    let tileset = Tileset::from_json_bytes(&bytes, &base_url).expect("parse tileset");

    // Frame the root bounding volume: ENU eye, looking at the center.
    let root = tileset.tile(tileset.root());
    let center = root.bounding_volume.center();
    let radius = match root.bounding_volume {
        tuile_core::math::BoundingVolume::Sphere(s) => s.radius,
        tuile_core::math::BoundingVolume::Obb(o) => o.bounding_sphere().radius,
    };
    let frame = enu_frame(ecef_to_geodetic(center));
    let (east, north, up) = (frame.col(0), frame.col(1), frame.col(2));
    let distance = radius * 2.8;
    let eye = center + (east * 0.4 - north * 0.7 + up * 0.55).normalize() * distance;
    let render_origin = center;

    tracing::info!(
        "tileset: {} tiles, root center {:.1?}, radius {:.1} m, eye distance {:.1} m",
        tileset.len(),
        center,
        radius,
        distance,
    );

    // Run the geometry server in-process on a local executor.
    let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), Config::default());
    let mut pool = LocalPool::new();
    pool.spawner().spawn_local(server.run()).expect("spawn");

    let gpu = pollster::block_on(GpuContext::headless()).expect("no GPU adapter for snapshot");
    let renderer = TileRenderer::new(&gpu, TEXTURE_FORMAT);
    let mut pump = ContentPump::new(render_origin);

    let viewport = DVec2::new(size as f64, size as f64);
    let view_state: ViewState =
        ViewState::perspective(eye, center - eye, up, viewport, 45f64.to_radians());

    // Drive the session: feed the camera, let the server fetch+decode, pump
    // GPU uploads. Stop once the selection is fully resident and stable.
    let mut stable = 0;
    for _ in 0..600 {
        stream
            .send(ClientMessage::ViewerState {
                views: vec![view_state],
            })
            .expect("send viewer state");
        pool.run_until_stalled();
        pump.pump(&mut stream, &gpu, 64);
        if !pump.selection.is_empty() && pump.missing() == 0 {
            stable += 1;
            if stable > 3 {
                break;
            }
        } else {
            stable = 0;
        }
    }

    for err in &pump.errors {
        tracing::warn!("server error: {err}");
    }
    tracing::info!(
        "selected {} tiles, {} prepared, {} bytes GPU, {} still missing",
        pump.selection.len(),
        pump.prepared_count(),
        pump.gpu_bytes,
        pump.missing(),
    );

    // Render one frame.
    let view = ViewUniform {
        view_proj: {
            let eye_f = (eye - render_origin).as_vec3();
            let v = Mat4::look_at_rh(eye_f, Vec3::ZERO, up.as_vec3());
            let p = Mat4::perspective_rh(45f32.to_radians(), 1.0, 0.05, 1.0e9);
            (p * v).to_cols_array()
        },
        sun_dir: {
            let d = (-up * 0.6 - east * 0.4 - north * 0.3).normalize().as_vec3();
            [d.x, d.y, d.z, 0.0]
        },
        params: [0.35, 0.0, 0.0, 0.0],
        atmosphere: Default::default(),
    };
    renderer.set_view(&gpu.queue, &view);

    let pixels = render_to_png(&gpu, &renderer, &pump, size);
    image::RgbaImage::from_raw(size, size, pixels)
        .expect("image")
        .save(&out_path)
        .expect("save png");
    tracing::info!("wrote {out_path}");
}

fn render_to_png(
    gpu: &GpuContext,
    renderer: &TileRenderer,
    pump: &ContentPump,
    size: u32,
) -> Vec<u8> {
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("snapshot color"),
        size: wgpu::Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TEXTURE_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let depth = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("snapshot depth"),
        size: wgpu::Extent3d {
            width: size,
            height: size,
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
            label: Some("snapshot"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &color_view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.45,
                        g: 0.62,
                        b: 0.82,
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
        renderer.render(&mut pass, pump.visible(), false);
    }

    let bytes_per_row = size * 4;
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("snapshot readback"),
        size: u64::from(bytes_per_row * size),
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
                rows_per_image: Some(size),
            },
        },
        wgpu::Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 1,
        },
    );
    gpu.queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    slice.get_mapped_range().to_vec()
}
