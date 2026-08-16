// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Bulk multi-tile globe renders on live data: Cesium World Terrain
//! (quantized-mesh) draped with Bing imagery, both via ion, selected by the
//! core SSE traversal and pulled through the async `GeometryServer` in **bulk**
//! mode (`drive_until_complete` — wait until the whole selection is resident),
//! then rendered to a high-resolution PNG.
//!
//! ```text
//! cargo run -p globe-bulk -- hemisphere [out.png] [size]
//! cargo run -p globe-bulk -- pyrenees   [out.png] [size]
//! ```
//! The ion token is read from `CESIUM_ION_TOKEN` (a local `.env` is loaded if
//! present) and never stored.

use anyhow::{bail, Context, Result};
use glam::{DVec2, DVec3, Mat4};
use std::sync::Arc;
use tuile_bing::{BingImageryProvider, BingMetadata};
use tuile_cesium_ion::{AssetEndpoint, IonClient, IonTerrainSource};
use tuile_core::drive::drive_until_complete;
use tuile_core::geo::{ecef_to_geodetic, enu_frame, geodetic_to_ecef, Geodetic, WGS84_A};
use tuile_core::report::GeometryReport;
use tuile_core::runtime::in_process_with;
use tuile_core::source::{TileId, TileLoader, TileTree};
use tuile_core::traversal::{Config, ViewState};
use tuile_core::TileContent;
use tuile_native_fetchers::NativeHttp;
use tuile_planetary::{globe_on, GlobeOptions};
use tuile_wgpu::{prepare, GpuContext, PreparedTile, TileRenderer, ViewUniform};
use tuile_wgpu::{DEPTH_FORMAT, TEXTURE_FORMAT};

/// A camera preset: the ECEF view for the traversal, plus the rebased f32
/// view-projection and lighting for the render.
struct Preset {
    eye: DVec3,
    target: DVec3,
    up: DVec3,
    fovy_deg: f32,
    near: f32,
    far: f32,
    render_origin: DVec3,
    ambient: f32,
    max_sse: f64,
}

impl Preset {
    fn hemisphere() -> Self {
        let r = WGS84_A;
        // Look at the whole Earth from above Africa/Europe.
        let dir = geodetic_to_ecef(Geodetic {
            lon: 12.0_f64.to_radians(),
            lat: 25.0_f64.to_radians(),
            height: 0.0,
        })
        .normalize();
        let eye = dir * (r * 2.8);
        Self {
            eye,
            target: DVec3::ZERO,
            up: DVec3::Z,
            fovy_deg: 45.0,
            near: 1.0e6,
            far: 3.0e7,
            render_origin: DVec3::ZERO,
            ambient: 0.5,
            // Cesium's globe default (Globe.maximumScreenSpaceError = 2), far
            // tighter than the 3D Tiles default of 16 — this is what makes the
            // terrain refine deep enough to look sharp.
            max_sse: 2.0,
        }
    }

    fn pyrenees() -> Self {
        let center = geodetic_to_ecef(Geodetic {
            lon: 1.0_f64.to_radians(),
            lat: 42.7_f64.to_radians(),
            height: 0.0,
        });
        let f = enu_frame(ecef_to_geodetic(center));
        let (east, north, up) = (f.col(0), f.col(1), f.col(2));
        // Oblique view from the south, elevated, looking north into the range.
        let eye = center + up * 220_000.0 - north * 320_000.0 + east * 30_000.0;
        let target = center + north * 60_000.0;
        Self {
            eye,
            target,
            up,
            fovy_deg: 40.0,
            near: 1.0e3,
            far: 2.0e6,
            render_origin: center,
            ambient: 0.5,
            max_sse: 2.0,
        }
    }

    fn pole() -> Self {
        // Look down onto the north pole — the geographic-tiling singularity.
        let pole = geodetic_to_ecef(Geodetic {
            lon: 0.0,
            lat: 90.0_f64.to_radians(),
            height: 0.0,
        });
        let eye = pole + DVec3::Z * (WGS84_A * 0.4);
        Self {
            eye,
            target: pole,
            up: DVec3::X,
            fovy_deg: 45.0,
            near: 1.0e4,
            far: 1.0e7,
            render_origin: pole,
            ambient: 0.5,
            max_sse: 2.0,
        }
    }

    /// The ECEF view the traversal selects against.
    fn view_state(&self, size: u32) -> ViewState {
        ViewState::perspective(
            self.eye,
            self.target - self.eye,
            self.up,
            DVec2::splat(size as f64),
            (self.fovy_deg as f64).to_radians(),
        )
    }

    /// The rebased f32 uniforms the renderer draws with.
    fn uniform(&self) -> ViewUniform {
        let v = Mat4::look_at_rh(
            (self.eye - self.render_origin).as_vec3(),
            (self.target - self.render_origin).as_vec3(),
            self.up.as_vec3(),
        );
        let p = Mat4::perspective_rh(self.fovy_deg.to_radians(), 1.0, self.near, self.far);
        // Headlight: the light travels along the camera's view direction, so
        // it sits behind the camera and lights exactly what we look at.
        let headlight = (self.target - self.eye).normalize().as_vec3();
        ViewUniform {
            view_proj: (p * v).to_cols_array(),
            sun_dir: [headlight.x, headlight.y, headlight.z, 0.0],
            params: [self.ambient, 0.0, 0.0, 0.0],
            // Off by default: these renders are for judging geometry and
            // imagery, and haze only makes two frames harder to compare.
            // TUILE_AIR=<strength> turns it on, which is how the viewer's own
            // look gets reproduced somewhere it can be inspected offline.
            atmosphere: match std::env::var("TUILE_AIR").ok().and_then(|v| v.parse().ok()) {
                Some(strength) if strength > 0.0 => tuile_atmosphere::AerialPerspective::new(
                    self.eye,
                    self.render_origin,
                    &tuile_atmosphere::Sun::from_direction(self.eye.normalize()),
                    strength,
                ),
                _ => Default::default(),
            },
        }
    }
}

/// Resolves the Cesium-ion globe sources (World Terrain + Bing imagery) and
/// crosses them through the backend-agnostic `tuile-planetary`. The app — not
/// planetary — decides ion and the native HTTP transport here.
async fn ion_globe(
    token: String,
    no_imagery: bool,
) -> Result<(Box<dyn TileTree>, Arc<dyn TileLoader>)> {
    // One pooled, cached native transport drives both ion and Bing.
    let http = Arc::new(NativeHttp::shared().await.context("native http cache")?);
    let terrain = IonTerrainSource::new(IonClient::new(Arc::clone(&http), token.clone()), 1);
    let layer = terrain.layer().await.context("terrain layer.json")?;

    let ion2 = IonClient::new(Arc::clone(&http), token);
    let endpoint = match ion2.asset_endpoint(2).await.context("bing endpoint")? {
        AssetEndpoint::Imagery(e) => e,
        _ => bail!("ion asset 2 is not imagery"),
    };
    let o = &endpoint.options;
    let meta_url = BingMetadata::metadata_url(
        o.url.as_deref().context("bing url")?,
        o.map_style.as_deref().unwrap_or("Aerial"),
        o.key.as_deref().context("bing key")?,
    );
    let bing = BingImageryProvider::from_metadata_url(Arc::clone(&http), &meta_url)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // No cache wrappers: a bulk run visits each tile once, so a store would
    // only pay the cost of writing entries nothing comes back for.
    let (tree, loader, _detail, _heights) = globe_on(
        terrain,
        bing,
        layer,
        GlobeOptions {
            no_imagery,
            ..Default::default()
        },
        tuile_core::offload::threaded(),
    );
    Ok((tree, loader))
}

/// Logs to stderr; `RUST_LOG` overrides. Default shows tile streaming
/// (`tuile_planetary=debug`) plus app-level info.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tuile_planetary=debug".into()),
        )
        .without_time()
        .with_target(false)
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    init_tracing();
    let token = std::env::var("CESIUM_ION_TOKEN")
        .context("set CESIUM_ION_TOKEN (env or .env; never stored)")?;

    let mut args = std::env::args().skip(1);
    let name = args.next().unwrap_or_else(|| "hemisphere".into());
    // A "-geo" suffix renders terrain only (no imagery) — the geometry debug
    // view, to inspect relief/skirts/cracks before bringing textures in.
    let (preset_name, drape) = match name.strip_suffix("-geo") {
        Some(base) => (base, false),
        None => (name.as_str(), true),
    };
    let preset = match preset_name {
        "hemisphere" => Preset::hemisphere(),
        "pyrenees" => Preset::pyrenees(),
        "pole" => Preset::pole(),
        other => bail!("unknown preset '{other}' (use: hemisphere|pyrenees|pole [-geo])"),
    };
    let out = args
        .next()
        .unwrap_or_else(|| format!("target/test-renders/{name}.png"));
    let size: u32 = args.next().map_or(4096, |s| s.parse().expect("size"));
    // Tile selection is driven by THIS viewport, decoupled from the render
    // size: the SSE metric scales with viewport height, so tying it to an 8K
    // target explodes the tile count. We traverse at a fixed budget and render
    // larger — i.e. supersample (smoother edges, imagery stays crisp via the
    // mosaic) without pulling four times the tiles.
    const SSE_VIEWPORT: u32 = 2048;
    if let Some(parent) = std::path::Path::new(&out).parent() {
        std::fs::create_dir_all(parent).ok();
    }

    // Resolve the ion sources HERE (the app decides ion + transport), then
    // hand the abstract terrain × imagery to the backend-agnostic planetary.
    let (tree, loader) = ion_globe(token, !drape).await?;

    // --- Tree + loader → generic geometry server, driven in bulk ---
    let config = Config {
        maximum_screen_space_error: preset.max_sse,
        // Fat pipe: many terrain tiles loading at once, each fanning out its
        // imagery mosaic fetches concurrently on top.
        maximum_simultaneous_fetches: 64,
        resident_budget_bytes: 2 << 30,
        ..Config::default()
    };
    let (mut stream, server) = in_process_with(tree, loader, config);
    tokio::spawn(server.run());

    tracing::info!("bulk-loading selection for {name}…");
    let frame = drive_until_complete(&mut stream, vec![preset.view_state(SSE_VIEWPORT)])
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    tracing::info!(
        "selection stable: {} tiles, {} decoded, {} errors (visited {}, culled {})",
        frame.selected.len(),
        frame.contents.len(),
        frame.errors.len(),
        frame.stats.visited,
        frame.stats.culled,
    );
    for (tile, msg) in &frame.errors {
        tracing::warn!("tile {tile:?}: {msg}");
    }

    // JSON view of the generated geometry, next to the PNG. Render-agnostic
    // and built the same way a streaming consumer would (fold each tile in).
    let mut report = GeometryReport::new();
    for (tile, content) in &frame.contents {
        if let TileContent::Decoded(d) = content {
            report.add(*tile, d);
        }
    }
    let json_path = std::path::Path::new(&out).with_extension("json");
    std::fs::write(&json_path, report.to_json_pretty()).context("write report json")?;
    tracing::info!(
        "geometry: {} verts, {} tris ({} degenerate), {} textures → {}",
        report.totals.vertices,
        report.totals.triangles,
        report.totals.degenerate_triangles,
        report.totals.textures,
        json_path.display(),
    );

    // --- Render every loaded tile, rebased to the preset's render origin ---
    let gpu = GpuContext::headless()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let renderer = TileRenderer::new(&gpu, TEXTURE_FORMAT);
    renderer.set_view(&gpu.queue, &preset.uniform());

    // Render ONLY the selected frontier — not every resident tile. Coarse
    // ancestors stay resident as REPLACE stand-ins during convergence; drawing
    // them too would overlap the fine tiles (z-fighting / ghosting).
    let selected: std::collections::HashSet<TileId> =
        frame.selected.iter().map(|(t, _)| *t).collect();
    let prepared: Vec<PreparedTile> = frame
        .contents
        .iter()
        .filter(|(t, _)| selected.contains(t))
        .filter_map(|(_, c)| match c {
            TileContent::Decoded(d) => Some(prepare(&gpu, d, preset.render_origin)),
            _ => None,
        })
        .collect();
    tracing::info!("rendering {} tiles at {size}×{size}…", prepared.len());

    let pixels = render_to_png(&gpu, &renderer, &prepared, size);
    image::RgbaImage::from_raw(size, size, pixels)
        .context("image from pixels")?
        .save(&out)
        .with_context(|| format!("save {out}"))?;
    tracing::info!("wrote {out}");
    Ok(())
}

fn render_to_png(
    gpu: &GpuContext,
    renderer: &TileRenderer,
    tiles: &[PreparedTile],
    size: u32,
) -> Vec<u8> {
    let extent = wgpu::Extent3d {
        width: size,
        height: size,
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
                    // Space black.
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.0,
                        g: 0.0,
                        b: 0.0,
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
        renderer.render(&mut pass, tiles.iter(), false);
    }

    let bytes_per_row = size * 4;
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("globe readback"),
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
