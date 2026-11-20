// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Offscreen snapshot of a globe whose far tiles are draped from a
//! **substitute** imagery layer (`docs/explorations/imagery-layer-substitution.md`).
//!
//! ```text
//! cargo run -p tuile-wgpu --example substitute_snapshot -- <out.png> [gap|off] [size]
//! ```
//!
//! Two local test layers, both a checker over the ellipsoid: the source in
//! green, its substitute in magenta, each tile's level written into its
//! brightness. Under a fallback specification of gap `G` the ground near the
//! camera is green and the far ground — tiles `G` levels or more below the
//! finest drawn — magenta; `off` renders the default (no specification:
//! green everywhere, and the substitute never asked).

use futures_executor::LocalPool;
use futures_util::task::LocalSpawnExt;
use glam::{DVec2, Vec3};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tuile_core::budget::{FallbackRule, FallbackSpec, FrameDeadline, LayerId};
use tuile_core::fetch::Fetched;
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};
use tuile_core::protocol::{ClientMessage, GeometryStream};
use tuile_core::raster::{ImageryCoord, ImageryProvider, RasterError, TilingScheme};
use tuile_core::runtime::in_process_with;
use tuile_core::traversal::{Config, ViewState};
use tuile_wgpu::{
    ContentPump, GpuContext, PreparedTile, TileRenderer, ViewUniform, DEPTH_FORMAT, TEXTURE_FORMAT,
};

/// A checker in one tint, each level a shade of it.
struct Checker {
    tint: [u8; 3],
    asked: AtomicU64,
}

#[async_trait::async_trait]
impl ImageryProvider for Checker {
    fn tiling_scheme(&self) -> TilingScheme {
        let mut s = TilingScheme::geographic();
        s.maximum_level = 18;
        s
    }

    async fn fetch_tile_bytes(
        &self,
        c: ImageryCoord,
    ) -> Result<Fetched<bytes::Bytes>, RasterError> {
        self.asked.fetch_add(1, Ordering::Relaxed);
        let shade = 0.45 + 0.55 * f64::from(c.level % 6) / 5.0;
        let mut img = image::RgbImage::new(256, 256);
        for (x, y, p) in img.enumerate_pixels_mut() {
            let on = ((x / 32) + (y / 32)) % 2 == 0;
            let k = if on { shade } else { shade * 0.7 };
            *p = image::Rgb(self.tint.map(|v| (f64::from(v) * k) as u8));
        }
        let mut png = std::io::Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png)
            .map_err(|e| RasterError::Image(e.to_string()))?;
        Ok(Fetched::undated(bytes::Bytes::from(png.into_inner())))
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .without_time()
        .with_target(false)
        .init();
    let mut args = std::env::args().skip(1);
    let usage = "usage: substitute_snapshot <out.png> [gap|off] [size]";
    let out_path = args.next().expect(usage);
    let gap = args.next().unwrap_or_else(|| "3".into());
    let size: u32 = args
        .next()
        .map(|s| s.parse().expect("size"))
        .unwrap_or(1024);

    let source = Arc::new(Checker {
        tint: [70, 200, 90],
        asked: AtomicU64::new(0),
    });
    let substitute = Arc::new(Checker {
        tint: [220, 60, 200],
        asked: AtomicU64::new(0),
    });
    let opts = tuile_planetary::GlobeOptions {
        max_level: Some(18),
        deterministic_floor: true,
        substitute: Some(tuile_planetary::SubstituteImagery(
            Arc::clone(&substitute) as Arc<dyn ImageryProvider>
        )),
        ..Default::default()
    };
    opts.imagery_slots.set_unbounded();
    let (tree, loader, _heights) = tuile_planetary::globe(
        tuile_terrain::Ellipsoid,
        tuile_planetary::Held(Arc::clone(&source) as Arc<dyn ImageryProvider>),
        tuile_terrain::Ellipsoid::layer(),
        opts,
    );
    let (mut stream, server) = in_process_with(
        tree,
        loader,
        Config::default(),
        tuile_core::budget::BudgetConfig::batch(),
    )
    .expect("the batch budget is valid");
    let mut pool = LocalPool::new();
    pool.spawner().spawn_local(server.run()).expect("spawn");
    if gap != "off" {
        let gap: u32 = gap.parse().expect("gap");
        stream
            .send(ClientMessage::Fallbacks(FallbackSpec::one(
                FallbackRule::new(LayerId(0)).with_gap(gap),
            )))
            .expect("send the fallback specification");
    }

    // 1.2 km over the Alps' latitude, looking down the valley at 14°.
    let ground = Geodetic {
        lon: 6.86f64.to_radians(),
        lat: 45.83f64.to_radians(),
        height: 0.0,
    };
    let frame = enu_frame(ground);
    let (east, north, up) = (frame.col(0), frame.col(1), frame.col(2));
    let eye = geodetic_to_ecef(Geodetic {
        height: 1_200.0,
        ..ground
    });
    let pitch = 14f64.to_radians();
    let forward = (north * pitch.cos() - up * pitch.sin()).normalize();
    let render_origin = eye;
    let viewport = DVec2::new(f64::from(size), f64::from(size));
    let view_state = ViewState::perspective(eye, forward, up, viewport, 60f64.to_radians());

    let gpu = pollster::block_on(GpuContext::headless()).expect("no GPU adapter for snapshot");
    // One sample: the offscreen target below is not multisampled.
    let renderer = TileRenderer::for_layout(
        &gpu,
        tuile_wgpu::TargetLayout {
            samples: 1,
            ..tuile_wgpu::TargetLayout::own(TEXTURE_FORMAT)
        },
    );
    let mut pump = ContentPump::new(render_origin);
    stream
        .send(ClientMessage::ViewerState {
            views: vec![view_state],
            generation: 1,
        })
        .expect("send viewer state");
    for _ in 0..4000 {
        pool.run_until_stalled();
        pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
        if pump.frame_ready && pump.generation_selected() >= 1 {
            break;
        }
    }
    for err in &pump.errors {
        tracing::warn!("server error: {err}");
    }
    let layer = &tuile_core::metrics::metrics().layer;
    tracing::info!(
        "{} drawn, ready: {}; drapes drawn by level — source {:?}, substitute {:?}; \
         tiles asked of the source {}, of the substitute {}",
        pump.draw_set().items.len(),
        pump.frame_ready,
        layer.drawn_source.occupied(),
        layer.drawn_substitute.occupied(),
        source.asked.load(Ordering::Relaxed),
        substitute.asked.load(Ordering::Relaxed),
    );

    let view = ViewUniform {
        view_proj: {
            let v =
                glam::camera::rh::view::look_at_mat4(Vec3::ZERO, forward.as_vec3(), up.as_vec3());
            let p =
                glam::camera::rh::proj::directx::perspective(60f32.to_radians(), 1.0, 0.5, 1.0e8);
            (p * v).to_cols_array()
        },
        sun_dir: {
            let d = (-up * 0.8 - east * 0.3 - north * 0.2).normalize().as_vec3();
            [d.x, d.y, d.z, 0.0]
        },
        params: [0.6, 0.0, 0.0, 0.0],
        atmosphere: Default::default(),
    };
    renderer.set_view(&gpu.queue, &view);
    let (drawn, _) = pump.resolve(&gpu);
    let pixels = render_to_png(&gpu, &renderer, &drawn.exact, size);
    image::RgbaImage::from_raw(size, size, pixels)
        .expect("image")
        .save(&out_path)
        .expect("save png");
    tracing::info!("wrote {out_path}");
}

fn render_to_png(
    gpu: &GpuContext,
    renderer: &TileRenderer,
    tiles: &[&PreparedTile],
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
                stencil_ops: Some(wgpu::Operations {
                    // Cleared to 0, and never read back: the mark says "a
                    // surface that owns this ground drew here", which is only
                    // true within one frame.
                    load: wgpu::LoadOp::Clear(0),
                    store: wgpu::StoreOp::Discard,
                }),
            }),
            ..Default::default()
        });
        renderer.render(&mut pass, tiles.iter().copied(), false);
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
    slice.get_mapped_range().expect("map range").to_vec()
}
