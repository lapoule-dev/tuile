// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Headless orbit probe: flies a full turn over Lake Geneva and reports what
//! the streaming session actually did with its tiles.
//!
//! The viewer can show that texture keeps vanishing and coming back as the
//! camera turns, but not *why*: a live session mixes camera easing, frame
//! pacing and GPU upload budgets into one blur. This runs the same server on a
//! scripted path with no window, so the numbers mean one thing —
//!
//! - **loads** — every `Content` the server sent
//! - **distinct** — how many different tiles those covered
//! - **reloads** — content for a tile that had already been sent and dropped
//! - **evictions** — tiles the budget reclaimed
//!
//! A healthy turn loads each tile about once. Reloads mean the session is
//! paying for the same tile repeatedly, which is what the eye reads as texture
//! flickering back in. Coming back round to a bearing already visited should
//! cost nothing: the second half of the orbit revisits the first half's tiles,
//! so reloads there say the residency did not hold them.
//!
//! ```text
//! cargo run --release -p orbit-probe             # 24 steps, no images
//! cargo run --release -p orbit-probe -- --png    # also write a frame per step
//! ```
//! Reads `CESIUM_ION_TOKEN` (env or `.env`).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use glam::DVec2;
use tuile_bing::{BingImageryProvider, BingMetadata};
use tuile_camera::GlobeCamera;
use tuile_cesium_ion::{AssetEndpoint, IonClient, IonTerrainSource};
use tuile_core::content::TileContent;
use tuile_core::protocol::{ClientMessage, GeometryStream, ServerMessage};
use tuile_core::raster::CachedImagery;
use tuile_core::runtime::in_process_with;
use tuile_core::source::TileId;
use tuile_core::storage::ContentStore;
use tuile_core::traversal::Config;
use tuile_native_fetchers::NativeHttp;
use tuile_planetary::{globe, GlobeOptions};
use tuile_storage_foyer::FoyerStore;
use tuile_terrain::CachedTerrain;
use tuile_wgpu::{prepare, GpuContext, PreparedTile, TileRenderer, ViewUniform};
use tuile_wgpu::{DEPTH_FORMAT, TEXTURE_FORMAT};

/// Lake Geneva, looking at it from the south-west shore.
const LAT_DEG: f64 = 46.45;
const LON_DEG: f64 = 6.50;
const ALTITUDE_M: f64 = 6_000.0;
/// Well below the horizon, so the view always has ground in it.
const PITCH: f64 = 0.6;
/// The viewer's own lens — the probe measures what the viewer will pay, and the
/// imagery detail target goes as `tan(fovy / 2)`, so a probe on a different lens
/// measures a different globe.
const FOVY: f64 = tuile_camera::DEFAULT_GLOBE_FOVY;

/// Bearings sampled around the turn. 24 is every 15°: fine enough that
/// consecutive views overlap heavily, which is exactly the case where nothing
/// should need reloading.
const STEPS: u32 = 24;
const VIEWPORT: (f64, f64) = (1280.0, 720.0);
const PNG_SIZE: u32 = 720;

/// How long one bearing may take to settle before the probe moves on. A view
/// that never settles is itself the finding, so this is a bound, not a wait.
const MAX_SETTLE_MESSAGES: usize = 200_000;

/// Silence that suggests the server has finished answering a view.
///
/// Generous on purpose: a bearing's answer is a burst of network fetches, and
/// gaps of a second between messages are ordinary. Too short a window and the
/// probe walks away mid-burst, reporting an empty bearing — which reads as a
/// clean orbit and is worse than no measurement at all.
const QUIET: std::time::Duration = std::time::Duration::from_millis(2_500);

/// Longest a single bearing may take. Reaching it means the view never
/// converged, which is a finding rather than a failure.
const MAX_BEARING: std::time::Duration = std::time::Duration::from_secs(60);

fn camera_at(bearing: f64) -> GlobeCamera {
    GlobeCamera::from_geodetic(
        LAT_DEG.to_radians(),
        LON_DEG.to_radians(),
        ALTITUDE_M,
        bearing,
        PITCH,
        FOVY,
    )
}

/// What one bearing cost.
#[derive(Default, Debug, Clone, Copy)]
struct StepCost {
    loads: u32,
    reloads: u32,
    evictions: u32,
    selected: u32,
}

/// Per-tile history across the whole orbit.
#[derive(Default)]
struct Ledger {
    /// How many times content arrived for a tile.
    arrivals: HashMap<TileId, u32>,
    /// Tiles currently held by the server.
    resident: HashMap<TileId, ()>,
    steps: Vec<StepCost>,
}

impl Ledger {
    fn on_content(&mut self, tile: TileId, step: &mut StepCost) {
        let seen = self.arrivals.entry(tile).or_insert(0);
        *seen += 1;
        step.loads += 1;
        // Arriving again for a tile we had already been given and lost is the
        // waste this probe exists to count.
        if *seen > 1 {
            step.reloads += 1;
        }
        self.resident.insert(tile, ());
    }

    fn on_evict(&mut self, tiles: &[TileId], step: &mut StepCost) {
        for t in tiles {
            self.resident.remove(t);
            step.evictions += 1;
        }
    }

    fn total(&self) -> StepCost {
        self.steps.iter().fold(StepCost::default(), |mut a, s| {
            a.loads += s.loads;
            a.reloads += s.reloads;
            a.evictions += s.evictions;
            a.selected = a.selected.max(s.selected);
            a
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .without_time()
        .with_target(false)
        .init();

    let want_png = std::env::args().any(|a| a == "--png");
    let token = std::env::var("CESIUM_ION_TOKEN")
        .context("set CESIUM_ION_TOKEN (env or .env; never stored)")?;

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

    // The same store the viewer uses, so the numbers describe the shipping
    // configuration rather than a stripped-down one.
    let foyer = Arc::new(FoyerStore::shared("tiles").await?);
    let store = Arc::clone(&foyer) as Arc<dyn ContentStore>;
    let (tree, loader, detail, _heights) = globe(
        CachedTerrain::new(terrain, Arc::clone(&store), "ion-cwt"),
        CachedImagery::new(bing, Arc::clone(&store), "bing-aerial"),
        layer,
        GlobeOptions::default(),
    );

    let config = Config {
        maximum_screen_space_error: 2.0,
        maximum_simultaneous_fetches: 64,
        resident_budget_bytes: 3072 * 1024 * 1024,
        ..Config::default()
    };
    let sse = config.maximum_screen_space_error;
    let (mut stream, server) = in_process_with(tree, loader, config);
    let server = tokio::spawn(server.run());

    // One synchronous handshake, only when images were asked for. Blocking a
    // worker for it is fine: nothing else is in flight yet.
    let gpu = match want_png {
        true => Some(pollster::block_on(GpuContext::headless())?),
        false => None,
    };
    let renderer = gpu.as_ref().map(|g| TileRenderer::new(g, TEXTURE_FORMAT));

    let mut ledger = Ledger::default();
    let mut contents = HashMap::new();

    tracing::info!(
        "orbiting {LAT_DEG}°N {LON_DEG}°E at {ALTITUDE_M:.0} m, {STEPS} bearings, \
         SSE {sse} — each tile should load about once"
    );

    for step in 0..STEPS {
        let bearing = std::f64::consts::TAU * f64::from(step) / f64::from(STEPS);
        let camera = camera_at(bearing);
        // Drive imagery detail exactly as the viewer does, or the probe would
        // measure a different scene from the one that misbehaves.
        let target_texel = 2.0 * camera.altitude() * (camera.fovy * 0.5).tan() / VIEWPORT.1;
        detail.set_target_texel_spacing(target_texel);

        let mut cost = StepCost::default();
        let selected = settle(
            &mut stream,
            camera.view_state(DVec2::new(VIEWPORT.0, VIEWPORT.1)),
            &mut ledger,
            &mut cost,
            &mut contents,
        )
        .await?;
        cost.selected = selected.len() as u32;
        ledger.steps.push(cost);

        tracing::info!(
            "bearing {:>3.0}° | selected {:>4} | loads {:>4} | reloads {:>4} | evictions {:>4}",
            bearing.to_degrees(),
            cost.selected,
            cost.loads,
            cost.reloads,
            cost.evictions,
        );

        if let (Some(gpu), Some(renderer)) = (gpu.as_ref(), renderer.as_ref()) {
            let path = format!("orbit-{step:02}.png");
            render_step(gpu, renderer, &camera, &selected, &contents, &path)?;
        }
    }

    report(&ledger);
    report_imagery(&contents);
    // Same order as the viewer: let the server see the session end before the
    // store closes, or its flusher shouts into a channel nobody holds.
    drop(stream);
    let _ = server.await;
    if let Err(e) = foyer.close().await {
        tracing::warn!("tile store not flushed: {e}");
    }
    Ok(())
}

/// Sends a view and drains the server until it stops producing content for it,
/// returning the final selection.
async fn settle<S: GeometryStream + Unpin>(
    stream: &mut S,
    view: tuile_core::traversal::ViewState,
    ledger: &mut Ledger,
    cost: &mut StepCost,
    contents: &mut HashMap<TileId, tuile_core::DecodedTileContent>,
) -> Result<Vec<TileId>> {
    stream.send(ClientMessage::ViewerState { views: vec![view] })?;

    let mut selection: Vec<TileId> = Vec::new();
    let mut messages = 0usize;
    let deadline = std::time::Instant::now() + MAX_BEARING;

    while messages < MAX_SETTLE_MESSAGES {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let msg = match tokio::time::timeout(QUIET.min(left), stream.next_message()).await {
            Ok(Some(msg)) => msg,
            Ok(None) => break,
            Err(_) => {
                // Quiet. Settled only if the view is actually satisfied;
                // otherwise the server is still working and we wait.
                let missing = selection
                    .iter()
                    .filter(|t| !contents.contains_key(t))
                    .count();
                if !selection.is_empty() && missing == 0 {
                    break;
                }
                continue;
            }
        };
        messages += 1;
        match msg {
            ServerMessage::Select { tiles, .. } => {
                selection = tiles.iter().map(|(t, _)| *t).collect();
            }
            ServerMessage::Content { tile, content } => {
                ledger.on_content(tile, cost);
                if let TileContent::Decoded(decoded) = content {
                    contents.insert(tile, decoded);
                }
            }
            ServerMessage::Evict { tiles } => {
                for t in &tiles {
                    contents.remove(t);
                }
                ledger.on_evict(&tiles, cost);
            }
            ServerMessage::Error { tile, message } => {
                tracing::warn!("server error on {tile:?}: {message}");
            }
        }
    }
    if std::time::Instant::now() >= deadline || messages >= MAX_SETTLE_MESSAGES {
        let missing = selection
            .iter()
            .filter(|t| !contents.contains_key(t))
            .count();
        tracing::warn!(
            "bearing never went quiet after {messages} messages ({missing} of \
             {} selected tiles still missing) — the session is churning, not converging",
            selection.len()
        );
    }
    Ok(selection)
}

/// What the imagery covering this orbit costs, held once versus held per tile.
///
/// The ratio is the sharing factor, and it is the whole argument for
/// referencing imagery instead of resampling it per tile: it says how many
/// copies of the same pixels the old drape was carrying. A ratio of 1 would mean
/// no tile shares imagery with any other — worth knowing, because it would mean
/// the levels are being chosen so finely that nothing overlaps.
fn report_imagery(contents: &HashMap<TileId, tuile_core::DecodedTileContent>) {
    use std::collections::HashMap as Map;
    let mut distinct: Map<tuile_core::raster::ImageryCoord, usize> = Map::new();
    let mut drapes = 0usize;
    let mut unshared = 0usize;
    for content in contents.values() {
        unshared += content.imagery_byte_size_unshared();
        for layer in &content.imagery {
            distinct.insert(layer.coord, layer.texture.rgba8.len());
            drapes += 1;
        }
    }
    if drapes == 0 {
        tracing::info!("no imagery draped");
        return;
    }
    let shared: usize = distinct.values().sum();
    let mib = |b: usize| b as f64 / (1024.0 * 1024.0);
    tracing::info!(
        "imagery: {drapes} drapes over {} distinct tiles ({:.2}× sharing), \
         {:.1} MiB held vs {:.1} MiB if each tile owned its own",
        distinct.len(),
        drapes as f64 / distinct.len().max(1) as f64,
        mib(shared),
        mib(unshared),
    );
}

fn report(ledger: &Ledger) {
    let total = ledger.total();
    let distinct = ledger.arrivals.len() as u32;
    let amplification = f64::from(total.loads) / f64::from(distinct.max(1));

    tracing::info!("——— orbit complete ———");
    tracing::info!(
        "loads {} over {} distinct tiles ({amplification:.2}× amplification), \
         {} reloads, {} evictions",
        total.loads,
        distinct,
        total.reloads,
        total.evictions,
    );

    // The second half revisits the first half's ground. Reloads concentrated
    // there mean residency did not survive one turn — the flicker the viewer
    // shows when you rotate back.
    let half = ledger.steps.len() / 2;
    let first: u32 = ledger.steps[..half].iter().map(|s| s.reloads).sum();
    let second: u32 = ledger.steps[half..].iter().map(|s| s.reloads).sum();
    tracing::info!("reloads: {first} in the first half-turn, {second} in the second");

    let mut worst: Vec<_> = ledger
        .arrivals
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(t, n)| (*n, *t))
        .collect();
    worst.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    if worst.is_empty() {
        tracing::info!("no tile was ever loaded twice");
    } else {
        tracing::info!("{} tiles loaded more than once; worst:", worst.len());
        for (times, tile) in worst.iter().take(10) {
            let (z, x, y) = tile.terrain_coord();
            tracing::info!("  z{z} {x}/{y} loaded {times}×");
        }
    }
}

/// Renders one bearing to a PNG so the numbers can be checked against pixels.
fn render_step(
    gpu: &GpuContext,
    renderer: &TileRenderer,
    camera: &GlobeCamera,
    selection: &[TileId],
    contents: &HashMap<TileId, tuile_core::DecodedTileContent>,
    path: &str,
) -> Result<()> {
    let origin = camera.position;
    let tiles: Vec<PreparedTile> = selection
        .iter()
        .filter_map(|t| contents.get(t))
        .map(|c| prepare(gpu, c, origin))
        .collect();

    let sun = camera.direction.as_vec3();
    renderer.set_view(
        &gpu.queue,
        &ViewUniform {
            // Square frames: the probe is about tile counts, not composition.
            view_proj: camera.view_proj(origin, 1.0),
            sun_dir: [sun.x, sun.y, sun.z, 0.0],
            params: [0.5, 0.0, 0.0, 0.0],
        },
    );

    let pixels = render_to_rgba(gpu, renderer, &tiles, PNG_SIZE);
    let image =
        image::RgbaImage::from_raw(PNG_SIZE, PNG_SIZE, pixels).context("frame buffer size")?;
    image.save(path)?;
    Ok(())
}

fn render_to_rgba(
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
    let target = |format, usage| {
        gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("orbit probe"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        })
    };
    let color = target(
        TEXTURE_FORMAT,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
    );
    let depth = target(DEPTH_FORMAT, wgpu::TextureUsages::RENDER_ATTACHMENT);

    let bytes_per_row = (size * 4).div_ceil(256) * 256;
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("orbit readback"),
        size: u64::from(bytes_per_row * size),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("orbit pass"),
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
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        renderer.render(&mut pass, tiles.iter(), false);
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
                rows_per_image: Some(size),
            },
        },
        extent,
    );
    gpu.queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let data = slice.get_mapped_range();
    let mut pixels = vec![0u8; (size * size * 4) as usize];
    for row in 0..size as usize {
        let src = row * bytes_per_row as usize;
        let dst = row * size as usize * 4;
        pixels[dst..dst + size as usize * 4].copy_from_slice(&data[src..src + size as usize * 4]);
    }
    drop(data);
    readback.unmap();
    pixels
}
