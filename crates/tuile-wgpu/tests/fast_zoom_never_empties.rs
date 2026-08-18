// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A violent zoom, and the one assertion that matters: **no empty tile, ever.**
//!
//! # Why a scenario and not a frame
//!
//! Black ground is forbidden outright (`CLAUDE.md`), and every attempt to fix it
//! passed the static tests and failed on a moving camera — because the failure
//! *is* movement. A selection describes a camera at least one round trip old; a
//! fast zoom-out grows the frustum's footprint faster than the selection
//! describing it, and the difference is ground nobody selected. Standing still,
//! nothing reproduces.
//!
//! So this drives the real engine — the real `GeometryServer`, the real
//! traversal, the real pump and real GPU uploads — along a camera path that
//! changes altitude by orders of magnitude between frames, and asserts on every
//! single frame that nothing the traversal selected went undrawn.
//!
//! The one thing faked is the network: the loader answers instantly from a
//! generated surface. That makes the test hermetic and, more importantly, makes
//! it *strictly harder* to pass — a real network would let the engine hide
//! behind "it had not arrived yet", which is exactly the excuse that must not
//! hold.
//!
//! # The gate
//!
//! No usable adapter means skip, with a printed reason; `TUILE_REQUIRE_GPU=1`
//! turns the skip into a failure, so a machine that is supposed to have a GPU
//! cannot quietly stop running this.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;

use glam::{DVec2, DVec3};

use tuile_core::geo::{geodetic_to_ecef, Geodetic};
use tuile_core::runtime::in_process_with;
use tuile_core::source::{LoadError, Loaded, TileId, TileLoader, TileTree};
use tuile_core::traversal::{Config, ViewState};
use tuile_terrain::{fill_content, GeographicTilingScheme, LayerJson, TileCoord};
use tuile_wgpu::{
    ContentPump, GpuContext, TileRenderer, ViewUniform, DEPTH_FORMAT, SAMPLES, TEXTURE_FORMAT,
};

/// Deepest level the synthetic terrain serves. Deep enough that a zoom from
/// orbit to a hillside crosses many levels, shallow enough that priming the
/// pinned pyramid stays quick.
const DEEPEST: u32 = 6;

/// Levels held for the whole session, as the viewer holds them.
const PINNED: u32 = 4;

fn gpu() -> Option<GpuContext> {
    match pollster::block_on(GpuContext::headless()) {
        Ok(gpu) => Some(gpu),
        Err(e) => {
            assert!(
                std::env::var("TUILE_REQUIRE_GPU").is_err(),
                "TUILE_REQUIRE_GPU is set and there is no usable adapter: {e}"
            );
            eprintln!("skipping GPU scenario: no usable adapter ({e})");
            None
        }
    }
}

/// A `layer.json` describing a globe that has every tile down to [`DEEPEST`].
///
/// Parsed rather than constructed so the tree under test is built by the same
/// code a real session builds it with — including the availability ranges,
/// which are what decide how deep refinement may go.
fn everywhere() -> LayerJson {
    let ranges: Vec<String> = (0..=DEEPEST)
        .map(|level| {
            let (x, y) = (2u64 << level, 1u64 << level);
            format!(
                r#"[{{"startX":0,"startY":0,"endX":{},"endY":{}}}]"#,
                x - 1,
                y - 1
            )
        })
        .collect();
    let doc = format!(
        r#"{{"tilejson":"2.1.0","format":"quantized-mesh-1.0","scheme":"tms",
            "projection":"EPSG:4326","tiles":["{{z}}/{{x}}/{{y}}.terrain"],
            "bounds":[-180,-90,180,90],"available":[{}]}}"#,
        ranges.join(",")
    );
    LayerJson::from_slice(doc.as_bytes()).expect("layer.json")
}

/// Answers every tile instantly with a flat surface over its own rectangle.
///
/// No network, no decode, no failure: the engine is given every excuse removed.
/// Whatever this test catches is therefore the engine's own decision-making, not
/// latency.
struct Instant;

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl TileLoader for Instant {
    async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
        let (z, x, y) = id.terrain_coord();
        let rect = GeographicTilingScheme::default().tile_rect(TileCoord::new(z, x, y));
        Ok(Loaded::Content(fill_content(&rect, [Some(0.0); 4], 0.0)))
    }
}

/// The camera, looking straight down from `altitude` over a fixed point.
fn looking_down(altitude: f64) -> (ViewState, DVec3, DVec3) {
    let ground = geodetic_to_ecef(Geodetic {
        lon: 2.17f64.to_radians(),
        lat: 42.52f64.to_radians(),
        height: 0.0,
    });
    let up = ground.normalize();
    let eye = ground + up * altitude;
    // The camera's own up must not be parallel to where it looks, or the view
    // matrix is degenerate and nothing lands on screen — which this test did,
    // reporting 100 % black while ninety-six tiles were drawn.
    let screen_up = DVec3::Z.cross(up).normalize_or(DVec3::Y);
    let view = ViewState::perspective(
        eye,
        -up,
        screen_up,
        DVec2::new(1024.0, 1024.0),
        45f64.to_radians(),
    );
    (view, eye, screen_up)
}

/// One frame of a session: drive the server, run the host's own streaming step,
/// then draw and measure.
///
/// `settle` is how many times the server future is polled before the frame is
/// drawn. One is a live frame; a large number is a camera that has been sitting
/// still. The streaming step itself is `ContentPump::advance` — the same call
/// `wgpu-viewer` makes, not a copy of it.
#[allow(clippy::too_many_arguments)]
fn frame(
    server: &mut Pin<Box<dyn Future<Output = ()>>>,
    stream: &mut tuile_core::protocol::InProcessStream,
    pump: &mut ContentPump,
    gpu: &GpuContext,
    bench: &Bench,
    view: ViewState,
    up: DVec3,
    eye: DVec3,
    settle: usize,
) -> Frame {
    let waker = futures_util::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    // The camera goes out **once**, as a host sends it once per frame. Sending
    // it on every poll instead provokes a full traversal per poll, which turned
    // a fourteen-second test into a five-minute one and measured an engine
    // nobody drives.
    let _ = server.as_mut().poll(&mut cx);
    pump.advance(stream, gpu, view, eye);
    // Then let the server work, taking this frame's share of uploads each time —
    // which is what a real session does between camera updates, on its own
    // thread.
    for _ in 1..settle {
        let _ = server.as_mut().poll(&mut cx);
        pump.pump(stream, gpu, tuile_wgpu::UPLOADS_PER_FRAME);
    }
    // Rebased once, at the end. `rebase` rewrites a uniform buffer for **every**
    // resident tile, so calling it per poll is thousands of GPU writes per frame
    // — the settle loop alone was doing tens of millions of them, and that, not
    // the engine, was the three minutes.
    pump.rebase(&gpu.queue, eye);
    let (drawn, resolution) = pump.resolve(&gpu.queue);
    Frame {
        lost: resolution.lost,
        drawn: drawn.exact.len() + drawn.fallback.len(),
        black: bench.black_fraction(gpu, &drawn, &view, up, eye),
    }
}

/// What one frame produced: the engine's own opinion, and the screen's.
///
/// Both, because they disagree — and the disagreement is the entire bug. `lost`
/// counts selected tiles that resolved to nothing; it is blind by construction
/// to ground the traversal never selected, which renders as the clear colour and
/// is exactly what a person photographs and calls a black square.
struct Frame {
    lost: usize,
    /// Reported in the failure message: "nothing was drawn" and "everything was
    /// drawn and it is still black" are different bugs.
    #[allow(dead_code)]
    drawn: usize,
    black: f64,
}

/// The offscreen bench: a target, a depth buffer, a pipeline and a readback
/// buffer, all built **once**.
///
/// Built once because building them per frame is what made this test take
/// thirty-two seconds — a `TileRenderer` compiles a pipeline, and compiling one
/// sixty times a second is the test measuring its own setup rather than the
/// engine.
struct Bench {
    target: wgpu::Texture,
    colour: wgpu::TextureView,
    msaa: wgpu::TextureView,
    depth: wgpu::TextureView,
    renderer: TileRenderer,
    readback: wgpu::Buffer,
}

/// Frame side, in pixels. Small enough to read back every frame, large enough
/// that a missing tile is thousands of pixels rather than a rounding error.
const SIZE: u32 = 128;

impl Bench {
    fn new(gpu: &GpuContext) -> Self {
        let make = |format, usage, samples| {
            gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: None,
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
        let target = make(
            TEXTURE_FORMAT,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            1,
        );
        // Multisampled, as the session is: the tile pipeline is built for
        // `SAMPLES` and a single-sample pass will not accept it. Without this
        // the whole test failed validation on every run, MSAA having reached
        // the renderer long after the test was written.
        let msaa = make(
            TEXTURE_FORMAT,
            wgpu::TextureUsages::RENDER_ATTACHMENT,
            SAMPLES,
        );
        let depth = make(
            DEPTH_FORMAT,
            wgpu::TextureUsages::RENDER_ATTACHMENT,
            SAMPLES,
        );
        Self {
            colour: target.create_view(&wgpu::TextureViewDescriptor::default()),
            msaa: msaa.create_view(&wgpu::TextureViewDescriptor::default()),
            depth: depth.create_view(&wgpu::TextureViewDescriptor::default()),
            target,
            renderer: TileRenderer::new(gpu, TEXTURE_FORMAT),
            readback: gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: (SIZE * SIZE * 4) as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }),
        }
    }

    /// Draws `tiles` from `view` and returns the fraction of the middle of the
    /// frame that came out at the clear colour — ground nothing covered.
    ///
    /// The middle half only: the edges are where the globe's limb falls, and
    /// counting those would measure framing rather than coverage.
    fn black_fraction(
        &self,
        gpu: &GpuContext,
        drawn: &tuile_wgpu::Drawn<'_>,
        view: &ViewState,
        up: DVec3,
        eye: DVec3,
    ) -> f64 {
        // The same projection the traversal culled with, so this looks at
        // exactly the volume the engine was asked about.
        let view_m =
            glam::Mat4::look_to_rh(glam::Vec3::ZERO, view.direction().as_vec3(), up.as_vec3());
        let far = (eye.length() * 2.0) as f32;
        let proj = glam::Mat4::perspective_rh(45f32.to_radians(), 1.0, 100.0, far);
        self.renderer.set_view(
            &gpu.queue,
            &ViewUniform {
                view_proj: (proj * view_m).to_cols_array(),
                // Ambient only: a grazing sun would make drawn ground read as
                // black and turn this into a lighting test.
                params: [1.0, 0.0, 0.0, 0.0],
                ..Default::default()
            },
        );

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.msaa,
                    depth_slice: None,
                    resolve_target: Some(&self.colour),
                    ops: wgpu::Operations {
                        // Black on purpose: the clear colour must be the thing
                        // being counted, or "nothing was drawn" comes out some
                        // other shade and the measurement misses it.
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth,
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
            // A fallback ancestor is backdrop, not geometry: it stands in for ground
            // that is not its own and must never win a pixel from a surface that
            // owns it. See `tuile_wgpu::Drawn`.
            self.renderer
                .render(&mut pass, drawn.exact.iter().copied(), false);
            self.renderer
                .render_fallback(&mut pass, drawn.fallback.iter().copied());
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
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

        let slice = self.readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        let data = slice.get_mapped_range();
        let (lo, hi) = (SIZE / 4, SIZE * 3 / 4);
        let (mut black, mut total) = (0u64, 0u64);
        for row in lo..hi {
            for col in lo..hi {
                let i = ((row * SIZE + col) * 4) as usize;
                total += 1;
                if data[i] < 8 && data[i + 1] < 8 && data[i + 2] < 8 {
                    black += 1;
                }
            }
        }
        drop(data);
        self.readback.unmap();
        black as f64 / total as f64
    }
}

/// **A camera that dives and climbs never leaves a tile empty.**
///
/// The altitudes below are deliberately brutal — each step is a factor of ten,
/// which no hand can produce and every wheel can. Between two such frames the
/// frustum's footprint changes by two orders of magnitude, which is the
/// condition under which every previous attempt at this failed.
///
/// The assertion is on `lost`: selected ground with nothing anywhere on its
/// ancestor chain to draw. That is the number that means *black on screen*, and
/// it must be zero on every frame, not on average.
#[test]
fn a_fast_zoom_never_leaves_a_tile_empty() {
    let Some(gpu) = gpu() else { return };

    let tree: Box<dyn TileTree> = tuile_planetary::terrain_tree(everywhere());
    let config = Config {
        maximum_screen_space_error: 2.0,
        maximum_simultaneous_fetches: 64,
        pinned_level: Some(PINNED),
        // Under memory pressure, deliberately. A session that never evicts is
        // not the session anyone runs: the viewer reclaims constantly — 19 326
        // evictions against 26 915 uploads, measured — and an eviction that
        // takes an ancestor leaves the climb below it with nothing to find.
        // Without this the harness was strictly easier than reality and passed
        // while the real globe went black.
        resident_tile_limit: 1_500,
        resident_budget_bytes: 96 * 1024 * 1024,
        ..Config::default()
    };
    let (mut stream, server) = in_process_with(tree, Arc::new(Instant), config);
    let mut server: Pin<Box<dyn Future<Output = ()>>> = Box::pin(server.run());
    let mut pump = ContentPump::new(DVec3::ZERO);
    let bench = Bench::new(&gpu);

    // Settle at altitude first: the coarse pyramid is what every fallback lands
    // on, and a session that has not got it yet is a cold start, not a zoom.
    let (view, eye, up) = looking_down(4_000_000.0);
    frame(
        &mut server,
        &mut stream,
        &mut pump,
        &gpu,
        &bench,
        view,
        up,
        eye,
        300,
    );

    // The reproduction, as described: a **continuous, aggressive zoom out**,
    // one engine step per frame, exactly as a wheel spun hard produces it.
    //
    // A hole is a black shape lasting a few hundredths of a second — three or
    // four frames — so a test that settles between altitudes cannot see one by
    // construction. This does not settle: each frame gets a single poll and a
    // single bounded upload, and the assertion runs on every one of them.
    //
    // Sixty frames from a hillside to orbit is roughly one second of wheel at
    // the fastest a hand goes, and a factor of two thousand in altitude.
    const FRAMES: usize = 16;
    let (low, high) = (2_000.0f64, 4_000_000.0f64);

    // Start settled down low: the user zooms out *from* somewhere they have
    // been looking at, not from a cold engine.
    let (view, eye, up) = looking_down(low);
    frame(
        &mut server,
        &mut stream,
        &mut pump,
        &gpu,
        &bench,
        view,
        up,
        eye,
        300,
    );

    let mut worst: (f64, f64, usize) = (0.0, low, 0);
    for i in 0..=FRAMES {
        // Geometric, so every frame multiplies the altitude by the same factor —
        // which is what a wheel does, and what makes the frustum's footprint
        // grow faster than a selection can describe it.
        let altitude = low * (high / low).powf(i as f64 / FRAMES as f64);
        let (view, eye, up) = looking_down(altitude);
        let f = frame(
            &mut server,
            &mut stream,
            &mut pump,
            &gpu,
            &bench,
            view,
            up,
            eye,
            1,
        );
        if f.black > worst.0 {
            worst = (f.black, altitude, f.lost);
        }
    }
    assert!(
        worst.0 < 0.01,
        "during a continuous zoom out, {:.1}% of a frame went bare at {:.0} km \
         (lost={}) — a black shape for a few hundredths of a second is exactly \
         the bug, and counters do not see it",
        worst.0 * 100.0,
        worst.1 / 1000.0,
        worst.2
    );

    // And the way back in, which uncovers ground just as fast.
    let mut worst_in: (f64, f64) = (0.0, high);
    for i in 0..=FRAMES {
        let altitude = high * (low / high).powf(i as f64 / FRAMES as f64);
        let (view, eye, up) = looking_down(altitude);
        let f = frame(
            &mut server,
            &mut stream,
            &mut pump,
            &gpu,
            &bench,
            view,
            up,
            eye,
            1,
        );
        if f.black > worst_in.0 {
            worst_in = (f.black, altitude);
        }
    }
    assert!(
        worst_in.0 < 0.01,
        "during a continuous zoom in, {:.1}% of a frame went bare at {:.0} km",
        worst_in.0 * 100.0,
        worst_in.1 / 1000.0
    );
}
