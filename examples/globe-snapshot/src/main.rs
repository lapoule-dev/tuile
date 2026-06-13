// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Renders one patch of the real globe — Cesium World Terrain (quantized-mesh)
//! draped with Bing imagery, both via ion — to a PNG. End-to-end proof of the
//! terrain + imagery pipeline on live data.
//!
//! ```text
//! CESIUM_ION_TOKEN=... cargo run -p globe-snapshot -- [out.png] [z] [x] [y]
//! ```
//! The token is read from the environment and never stored.

use anyhow::{bail, Context, Result};
use glam::{Mat4, Vec3};
use std::sync::Arc;
use tuile_cesium_ion::{AssetEndpoint, IonClient, IonTerrainSource, ReqwestHttp};
use tuile_core::fetch::HttpFetcher;
use tuile_core::geo::{ecef_to_geodetic, enu_frame};
use tuile_core::raster::{self, GeoRect, ImageryProvider};
use tuile_terrain::{decode, to_decoded, GeographicTilingScheme, TileCoord};
use tuile_wgpu::{prepare, GpuContext, TileRenderer, ViewUniform, DEPTH_FORMAT, TEXTURE_FORMAT};

const SIZE: u32 = 1024;

#[tokio::main]
async fn main() -> Result<()> {
    let token = std::env::var("CESIUM_ION_TOKEN")
        .context("set CESIUM_ION_TOKEN (read from env, never stored)")?;
    let mut args = std::env::args().skip(1);
    let out = args
        .next()
        .unwrap_or_else(|| "target/test-renders/globe.png".into());
    let z: u32 = args.next().map_or(10, |s| s.parse().expect("z"));
    let x: u64 = args.next().map_or(1083, |s| s.parse().expect("x"));
    let y: u64 = args.next().map_or(773, |s| s.parse().expect("y"));
    let coord = TileCoord::new(z, x, y);

    // --- Terrain (Cesium World Terrain = ion asset 1) ---
    let ion = IonClient::new(Arc::new(ReqwestHttp::default()), token.clone());
    let terrain = IonTerrainSource::new(ion, 1);
    let tile_bytes = terrain
        .fetch_tile(coord)
        .await
        .context("fetch .terrain tile")?;
    let qm = decode(&tile_bytes).context("decode quantized-mesh")?;
    let rect = GeographicTilingScheme::default().tile_rect(coord);
    // Skirts off for a clean single-tile snapshot (no neighbours to crack against).
    let mut content = to_decoded(&qm, &rect, 0.0);
    eprintln!(
        "terrain {z}/{x}/{y}: {} verts, height {:.0}..{:.0} m, center {:?}",
        qm.vertex_count(),
        qm.header.min_height,
        qm.header.max_height,
        content.local_origin_ecef,
    );

    // --- Bing imagery (ion asset 2) ---
    let ion2 = IonClient::new(Arc::new(ReqwestHttp::default()), token);
    let bing_ep = match ion2.asset_endpoint(2).await.context("bing endpoint")? {
        AssetEndpoint::Imagery(e) => e,
        _ => bail!("asset 2 is not imagery"),
    };
    let opts = &bing_ep.options;
    let meta_url = tuile_bing::BingMetadata::metadata_url(
        opts.url.as_deref().context("bing url")?,
        opts.map_style.as_deref().unwrap_or("Aerial"),
        opts.key.as_deref().context("bing key")?,
    );
    let bing = tuile_bing::BingImageryProvider::from_metadata_url(
        Arc::new(HttpFetcher::default()),
        &meta_url,
    )
    .await
    .context("bing metadata")?;

    // --- Drape: pick the imagery tile covering the terrain extent, fetch, attach ---
    let georect = GeoRect {
        west: rect.west,
        south: rect.south,
        east: rect.east,
        north: rect.north,
    };
    let attachment = raster::single_tile_attachment(&content, &georect, &bing.tiling_scheme());
    eprintln!(
        "imagery tile {:?} covers the terrain extent",
        attachment.imagery
    );
    let texture = bing
        .fetch_tile(attachment.imagery)
        .await
        .context("fetch bing tile")?;
    eprintln!("bing texture {}x{}", texture.width, texture.height);
    raster::drape_single(&mut content, &attachment, texture);

    // --- Render ---
    let gpu = GpuContext::headless()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let prepared = prepare(&gpu, &content, content.local_origin_ecef);
    let renderer = TileRenderer::new(&gpu, TEXTURE_FORMAT);

    // Oblique ENU camera framing the tile, to show the relief.
    let center = content.local_origin_ecef;
    let frame = enu_frame(ecef_to_geodetic(center));
    let (east, north, up) = (frame.col(0), frame.col(1), frame.col(2));
    let bs = qm.header.bounding_sphere_radius.max(1000.0);
    let eye = center + (east * 0.3 - north * 0.8 + up * 0.6).normalize() * bs * 2.2;
    let view = ViewUniform {
        view_proj: {
            let v = Mat4::look_at_rh((eye - center).as_vec3(), Vec3::ZERO, up.as_vec3());
            let p = Mat4::perspective_rh(50f32.to_radians(), 1.0, 1.0, 1.0e9);
            (p * v).to_cols_array()
        },
        sun_dir: {
            let d = (-up * 0.5 - east * 0.5 - north * 0.4).normalize().as_vec3();
            [d.x, d.y, d.z, 0.0]
        },
        // Low ambient floor so the relief shading reads, imagery still bright.
        params: [0.55, 0.0, 0.0, 0.0],
    };
    renderer.set_view(&gpu.queue, &view);

    let pixels = render_to_png(&gpu, &renderer, &prepared);
    image::RgbaImage::from_raw(SIZE, SIZE, pixels)
        .context("image")?
        .save(&out)
        .with_context(|| format!("save {out}"))?;
    eprintln!("wrote {out}");
    Ok(())
}

fn render_to_png(
    gpu: &GpuContext,
    renderer: &TileRenderer,
    tile: &tuile_wgpu::PreparedTile,
) -> Vec<u8> {
    let extent = wgpu::Extent3d {
        width: SIZE,
        height: SIZE,
        depth_or_array_layers: 1,
    };
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("globe color"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TEXTURE_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let depth = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("globe depth"),
        size: extent,
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
            label: Some("globe"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &color_view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.05,
                        g: 0.07,
                        b: 0.12,
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
        renderer.render(&mut pass, std::iter::once(tile), false);
    }

    let bytes_per_row = SIZE * 4;
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("globe readback"),
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
        extent,
    );
    gpu.queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    slice.get_mapped_range().to_vec()
}
