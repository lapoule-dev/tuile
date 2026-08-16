// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! GPU in the loop: put two tiles of different detail side by side and look at
//! the line where they meet.
//!
//! # Why this exists
//!
//! A hairline grid along tile boundaries survived three fixes in a row —
//! chained coverage edges, an outward reach on the outer ones, a half-texel
//! inset — every one of them reasoned out from the code and none of them
//! measured. Each shipped, and each was reported back from a screenshot.
//!
//! The measurement they were missing is this one. Two neighbouring tiles at
//! different levels genuinely do not meet: the coarser one's edge is a chord
//! across a span the finer one crosses in several, so the two curves part
//! company between their shared endpoints. That gap is what skirts are for, and
//! it is visible from a camera the moment nothing fills it.
//!
//! The test renders the same pair twice, with skirts and without, and asserts
//! **both** directions:
//!
//! - without skirts the seam is there — otherwise the fixture is not
//!   reproducing the defect and the test guards nothing;
//! - with skirts it is gone.
//!
//! Which makes it self-validating: it cannot pass by testing nothing.
//!
//! # The gate
//!
//! A machine without a usable adapter skips, with a printed reason. Set
//! `TUILE_REQUIRE_GPU=1` to turn that skip into a failure, which is what CI
//! should do wherever a GPU is expected.

use std::collections::VecDeque;
use std::task::{Context, Poll};

use glam::{DVec3, Mat4, Vec3};

use tuile_core::content::{DecodedTileContent, TileContent};
use tuile_core::geo::{geodetic_to_ecef, Geodetic};
use tuile_core::protocol::{ClientMessage, GeometryStream, ServerMessage, StreamError};
use tuile_core::source::TileId;
use tuile_core::traversal::TraversalStats;
use tuile_terrain::{
    skirt_height, to_decoded, GeoRect, GeographicTilingScheme, Header, QuantizedMesh, TileCoord,
};
use tuile_wgpu::{
    prepare, ContentPump, GpuContext, TileRenderer, ViewUniform, DEPTH_FORMAT, SAMPLES,
    TEXTURE_FORMAT,
};

/// Frame size. Larger than the other GPU tests: a seam is one or two pixels
/// wide, and at 256 the whole defect can hide between two samples.
const SIZE: u32 = 512;

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

/// A stream that hands the pump a scripted list of messages.
struct Scripted(VecDeque<ServerMessage>);

impl GeometryStream for Scripted {
    fn send(&self, _msg: ClientMessage) -> Result<(), StreamError> {
        Ok(())
    }

    fn poll_message(&mut self, _cx: &mut Context<'_>) -> Poll<Option<ServerMessage>> {
        match self.0.pop_front() {
            Some(msg) => Poll::Ready(Some(msg)),
            None => Poll::Pending,
        }
    }
}

fn rect_of(tile: TileId) -> GeoRect {
    let (z, x, y) = tile.terrain_coord();
    GeographicTilingScheme::default().tile_rect(TileCoord::new(z, x, y))
}

/// A tile tessellated `steps × steps` across, flat at sea level.
///
/// Flat on purpose. The gap under test is not caused by relief — it is the
/// difference between one chord and several across a curved surface, so a
/// perfectly smooth ellipsoid produces it in full. Relief would only add noise
/// to a measurement that is already unambiguous.
fn grid_mesh(rect: &GeoRect, steps: usize) -> QuantizedMesh {
    let (lon, lat) = rect.center();
    let c = geodetic_to_ecef(Geodetic {
        lon,
        lat,
        height: 0.0,
    });
    let n = steps + 1;
    let mut u = Vec::with_capacity(n * n);
    let mut v = Vec::with_capacity(n * n);
    for row in 0..n {
        for col in 0..n {
            u.push(col as f64 / steps as f64);
            v.push(row as f64 / steps as f64);
        }
    }
    let mut indices = Vec::with_capacity(steps * steps * 6);
    for row in 0..steps {
        for col in 0..steps {
            let i = (row * n + col) as u32;
            let (a, b, cc, d) = (i, i + 1, i + n as u32, i + n as u32 + 1);
            indices.extend_from_slice(&[a, b, cc, cc, b, d]);
        }
    }
    let at = |col: usize, row: usize| (row * n + col) as u32;
    QuantizedMesh {
        header: Header {
            center: [c.x, c.y, c.z],
            min_height: 0.0,
            max_height: 0.0,
            bounding_sphere_center: [c.x, c.y, c.z],
            bounding_sphere_radius: 1.0e6,
            horizon_occlusion: [0.0; 3],
        },
        u,
        v,
        height: vec![0.0; n * n],
        indices,
        normals: None,
        // Deliberately listed back to front. This is what the wire delivers —
        // the format promises no order — and stitching a wall out of an
        // unsorted list is what made the first attempt at skirts draw smears.
        edges: [
            (0..n).rev().map(|row| at(0, row)).collect(),
            (0..n).rev().map(|col| at(col, 0)).collect(),
            (0..n).rev().map(|row| at(n - 1, row)).collect(),
            (0..n).rev().map(|col| at(col, n - 1)).collect(),
        ],
        metadata_available: None,
    }
}

fn content_for(tile: TileId, steps: usize, skirts: bool) -> DecodedTileContent {
    let rect = rect_of(tile);
    let mesh = grid_mesh(&rect, steps);
    let height = if skirts { skirt_height(&rect) } else { 0.0 };
    to_decoded(&mesh, &rect, height)
}

fn on_the_ground(rect: &GeoRect, u: f64, v: f64) -> DVec3 {
    geodetic_to_ecef(Geodetic {
        lon: rect.west + u * rect.width(),
        lat: rect.south + v * rect.height(),
        height: 0.0,
    })
}

/// Renders the pair and returns how many pixels of the frame show the void
/// behind the ground.
///
/// The clear colour is the marker. Both tiles are untextured, so they sample the
/// white 1×1 and come out bright; anything that is *not* bright is a place where
/// neither tile put a fragment, which is precisely the question.
fn holes_through_the_ground(
    gpu: &GpuContext,
    pump: &mut ContentPump,
    eye: DVec3,
    look_at: DVec3,
) -> u64 {
    // The real picture, counting the clear colour.
    render_and_count(
        gpu,
        pump,
        eye,
        look_at,
        DIAGNOSTIC_OFF,
        &|px| px[0] < 8 && px[1] < 8 && px[2] < 8,
        None,
    )
}

/// The real picture.
const DIAGNOSTIC_OFF: f32 = 0.0;
/// Magenta wherever no imagery layer reached the fragment. See `shader.wgsl`.
const DIAGNOSTIC_COVERAGE: f32 = 2.0;

/// Renders in the coverage view and counts the fragments painted magenta —
/// those no imagery layer claimed at all.
fn magenta_in_the_coverage_view(
    gpu: &GpuContext,
    pump: &mut ContentPump,
    eye: DVec3,
    look_at: DVec3,
) -> u64 {
    // The shell, prepared exactly as the viewer prepares it, and rebased on the
    // eye like everything else.
    let shell = prepare(
        gpu,
        &tuile_terrain::globe_shell([0.09, 0.15, 0.24, 1.0]),
        DVec3::ZERO,
    );
    shell.rebase(&gpu.queue, eye);
    // Magenta is `(1, 0, 1)`; covered ground is green or blue, both of which
    // have far more green than red. Resolving four samples can only dilute it,
    // so the test is "leaning towards magenta", not "exactly magenta".
    render_and_count(
        gpu,
        pump,
        eye,
        look_at,
        DIAGNOSTIC_COVERAGE,
        &|px| i32::from(px[0]) > i32::from(px[1]) + 40 && px[2] > 100,
        Some(&shell),
    )
}

fn render_and_count(
    gpu: &GpuContext,
    pump: &mut ContentPump,
    eye: DVec3,
    look_at: DVec3,
    mode: f32,
    counts: &dyn Fn([u8; 4]) -> bool,
    backdrop: Option<&tuile_wgpu::PreparedTile>,
) -> u64 {
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
    // Multisampled, because the session is: the tile pipeline is built for
    // `SAMPLES` and a single-sample pass will not accept it. It also matters to
    // what is being measured — a gap narrower than a pixel survives as partial
    // coverage rather than vanishing, which is exactly how it reaches the eye.
    let msaa = make(
        "seam msaa",
        TEXTURE_FORMAT,
        wgpu::TextureUsages::RENDER_ATTACHMENT,
        SAMPLES,
    );
    let msaa_view = msaa.create_view(&wgpu::TextureViewDescriptor::default());
    let target = make(
        "seam target",
        TEXTURE_FORMAT,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        1,
    );
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let depth = make(
        "seam depth",
        DEPTH_FORMAT,
        wgpu::TextureUsages::RENDER_ATTACHMENT,
        SAMPLES,
    );
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());

    let renderer = TileRenderer::new(gpu, TEXTURE_FORMAT);
    pump.rebase(&gpu.queue, eye);

    // Rebased: the eye is the origin of render space, so the target is where it
    // sits relative to the eye.
    let forward = (look_at - eye).normalize();
    // Local up, unless the view is straight down — where it is anti-parallel to
    // the forward direction and `look_to_rh` degenerates to a matrix of NaN,
    // rendering an empty frame that a test would happily read as "no defect".
    let local_up = eye.normalize();
    let up = if local_up.cross(forward).length() > 1.0e-6 {
        local_up
    } else {
        DVec3::Z.cross(forward).normalize_or(DVec3::X)
    };
    let view_m = Mat4::look_to_rh(Vec3::ZERO, forward.as_vec3(), up.as_vec3());
    let proj = Mat4::perspective_rh(60f32.to_radians(), 1.0, 100.0, 40_000_000.0);
    renderer.set_view(
        &gpu.queue,
        &ViewUniform {
            view_proj: (proj * view_m).to_cols_array(),
            // Ambient only, and no atmosphere: this test is about whether a
            // fragment exists, not what colour it ended up.
            params: [1.0, mode, 0.0, 0.0],
            ..Default::default()
        },
    );

    // One list: at this point a tile standing in for a missing descendant is
    // drawn as ordinary geometry, like everything else.
    let drawn = pump.visible_resolved(|id: TileId| {
        let (z, x, y) = id.terrain_coord();
        (z > 0).then(|| TileId::from_terrain(z - 1, x / 2, y / 2))
    });

    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("seam pass"),
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
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        // The backstop the session draws first: a whole-planet shell, carrying
        // no imagery of its own, behind everything. Leaving it out of the
        // fixture would hide the very thing that shows through a seam.
        renderer.render(
            &mut pass,
            backdrop.into_iter().chain(drawn.into_iter()),
            false,
        );
    }

    let bytes = (SIZE * SIZE * 4) as u64;
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("seam readback"),
        size: bytes,
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
    let data = slice.get_mapped_range();

    // The middle of the frame only, where the seam is aimed. The edges are
    // where the two tiles run out and the true void begins; counting those
    // would measure the framing rather than the join.
    let (lo, hi) = (SIZE * 3 / 8, SIZE * 5 / 8);
    let mut holes = 0u64;
    for row in lo..hi {
        for col in lo..hi {
            let i = ((row * SIZE + col) * 4) as usize;
            if counts([data[i], data[i + 1], data[i + 2], data[i + 3]]) {
                holes += 1;
            }
        }
    }
    drop(data);
    readback.unmap();
    holes
}

/// The same pair of different-detail tiles, but asked the question the coverage
/// view asks: is any fragment reached by no imagery layer at all?
fn magenta_at_a_lod_boundary(gpu: &GpuContext, skirts: bool) -> u64 {
    let coarse = TileId::from_terrain(4, 17, 10);
    let fine = TileId::from_terrain(5, 33, 21);
    let mut stream = Scripted(VecDeque::from([
        ServerMessage::Select {
            tiles: vec![(coarse, 0.0), (fine, 0.0)],
            stats: TraversalStats::default(),
        },
        ServerMessage::Content {
            tile: coarse,
            content: TileContent::Decoded(textured(coarse, 2, skirts)),
        },
        ServerMessage::Content {
            tile: fine,
            content: TileContent::Decoded(textured(fine, 8, skirts)),
        },
    ]));
    let seam = on_the_ground(&rect_of(fine), 1.0, 0.5);
    let up = seam.normalize();
    let east = DVec3::Z.cross(up).normalize();
    let eye = seam + up * 60_000.0 + east * 60_000.0;
    let mut pump = ContentPump::new(eye);
    pump.pump(&mut stream, gpu, 16);
    magenta_in_the_coverage_view(gpu, &mut pump, eye, seam)
}

/// A tile carrying one imagery layer over the whole of itself, as draped ground
/// does — so a fragment with no layer means a fragment that is not this tile.
fn textured(tile: TileId, steps: usize, skirts: bool) -> DecodedTileContent {
    let mut c = content_for(tile, steps, skirts);
    for mesh in &mut c.meshes {
        mesh.material.base_color_texture = None;
    }
    c.imagery = vec![whole_tile_layer()];
    c
}

/// **A change of detail level leaves no fragment without imagery either.**
///
/// The pair that genuinely does not meet, asked the coverage question with the
/// shell behind it — which is the exact combination a camera sees: a crack, and
/// underneath it a backstop that carries no imagery of its own.
#[test]
fn a_lod_boundary_leaves_no_fragment_without_imagery() {
    let Some(gpu) = gpu() else { return };

    let bare = magenta_at_a_lod_boundary(&gpu, false);
    assert!(
        bare > 0,
        "the fixture shows no bare fragment even without skirts, so it is not \
         reproducing the defect it is meant to guard"
    );

    let skirted = magenta_at_a_lod_boundary(&gpu, true);
    assert_eq!(
        skirted, 0,
        "{skirted} fragments at the change of level were reached by no imagery \
         layer ({bare} without skirts) — the backstop is showing through"
    );
}

/// Sets the pair up and renders it, with skirts or without.
fn holes_at_a_lod_boundary(gpu: &GpuContext, skirts: bool) -> u64 {
    // A level-4 tile and the level-5 tile immediately west of its west edge:
    // the finer one crosses in several chords what the coarser one spans in
    // one, so their shared edge is two different curves between the same two
    // endpoints.
    let coarse = TileId::from_terrain(4, 17, 10);
    let fine = TileId::from_terrain(5, 33, 21);

    let mut stream = Scripted(VecDeque::from([
        ServerMessage::Select {
            tiles: vec![(coarse, 0.0), (fine, 0.0)],
            stats: TraversalStats::default(),
        },
        ServerMessage::Content {
            tile: coarse,
            content: TileContent::Decoded(content_for(coarse, 2, skirts)),
        },
        ServerMessage::Content {
            tile: fine,
            content: TileContent::Decoded(content_for(fine, 8, skirts)),
        },
    ]));

    // The middle of the edge the two actually share — which is the middle of
    // the *fine* tile's east edge, not of the coarse tile's west one. Those
    // differ: the fine tile spans half the coarse tile's latitude, so aiming at
    // the coarse tile's midpoint puts the camera on a corner where four tiles
    // meet and two of them are not in the fixture.
    let seam = on_the_ground(&rect_of(fine), 1.0, 0.5);

    // Obliquely, from over the coarse tile, at about forty-five degrees.
    //
    // Straight down would see nothing however wide the gap: the two edges lie
    // in the same meridian plane, so what separates them projects to zero width
    // from directly overhead. It takes an angle for the line of sight to pass
    // the lower edge and continue *under* the higher one, and that is also the
    // only way anyone has ever seen this on screen.
    let up = seam.normalize();
    let east = DVec3::Z.cross(up).normalize();
    const STANDOFF: f64 = 60_000.0;
    let eye = seam + up * STANDOFF + east * STANDOFF;

    let mut pump = ContentPump::new(eye);
    pump.pump(&mut stream, gpu, 16);
    holes_through_the_ground(gpu, &mut pump, eye, seam)
}

/// One flat imagery layer covering a whole tile, edge to edge.
fn whole_tile_layer() -> tuile_core::raster::ImageryLayer {
    tuile_core::raster::ImageryLayer {
        coord: tuile_core::raster::ImageryCoord {
            level: 0,
            x: 0,
            y: 0,
        },
        texture: std::sync::Arc::new(tuile_core::content::DecodedTexture {
            width: 2,
            height: 2,
            rgba8: [[200u8, 200, 200, 255]; 4].concat(),
        }),
        coverage: [0.0, 0.0, 1.0, 1.0],
        translation: [0.0, 0.0],
        scale: [1.0, 1.0],
    }
}

/// Renders two *same-level* neighbours in the coverage view and counts the
/// fragments no imagery layer reached.
fn uncovered_fragments_along_a_shared_edge(gpu: &GpuContext) -> u64 {
    // Same level on both sides, so there is no crack to find and nothing to
    // blame on geometry: whatever comes back uncovered is the texturing.
    let west = TileId::from_terrain(4, 16, 10);
    let east = TileId::from_terrain(4, 17, 10);

    let content = |tile| {
        let rect = rect_of(tile);
        let mut c = to_decoded(&grid_mesh(&rect, 8), &rect, skirt_height(&rect));
        for mesh in &mut c.meshes {
            mesh.material.base_color_texture = None;
        }
        c.imagery = vec![whole_tile_layer()];
        c
    };

    let mut stream = Scripted(VecDeque::from([
        ServerMessage::Select {
            tiles: vec![(west, 0.0), (east, 0.0)],
            stats: TraversalStats::default(),
        },
        ServerMessage::Content {
            tile: west,
            content: TileContent::Decoded(content(west)),
        },
        ServerMessage::Content {
            tile: east,
            content: TileContent::Decoded(content(east)),
        },
    ]));

    // Straight down on the middle of the edge they share, high enough that the
    // pair fills the frame from side to side.
    let seam = on_the_ground(&rect_of(east), 0.0, 0.5);
    let eye = seam + seam.normalize() * 120_000.0;
    let mut pump = ContentPump::new(eye);
    pump.pump(&mut stream, gpu, 16);
    magenta_in_the_coverage_view(gpu, &mut pump, eye, seam)
}

/// **No fragment of the ground is left without imagery, least of all at an edge.**
///
/// The coverage view paints magenta wherever every layer's mask answered zero,
/// so this counts the defect directly rather than inferring it from a colour.
///
/// The cause it guards against is subtle and cost three wrong fixes. A
/// multisampled pixel straddling a tile's edge is only partly covered, but its
/// shader inputs are interpolated at the pixel *centre* — which lies outside the
/// triangle. The uv is extrapolated past the tile, every `step` mask answers
/// zero together, and the ground falls through to its base colour: a bright
/// dashed hairline along every boundary. `@interpolate(centroid)` on the uv
/// moves the sample inside the covered area and the extrapolation cannot happen.
#[test]
fn no_fragment_of_the_ground_is_left_without_imagery() {
    let Some(gpu) = gpu() else { return };

    let uncovered = uncovered_fragments_along_a_shared_edge(&gpu);
    assert_eq!(
        uncovered, 0,
        "{uncovered} fragments on the join between two tiles were reached by no \
         imagery layer at all, and drew the bare base colour"
    );
}

/// **Two tiles of different detail leave no gap where they meet.**
///
/// Asserts in both directions, so the test cannot pass by testing nothing: the
/// gap must be there when the skirts are removed, and gone when they are not.
#[test]
fn a_lod_boundary_shows_no_seam() {
    let Some(gpu) = gpu() else { return };

    let bare = holes_at_a_lod_boundary(&gpu, false);
    assert!(
        bare > 0,
        "the fixture no longer opens a seam even without skirts, so it no \
         longer guards against one — the two tiles must actually disagree \
         about the edge they share"
    );

    let skirted = holes_at_a_lod_boundary(&gpu, true);
    assert_eq!(
        skirted, 0,
        "{skirted} pixels of void through the ground at the boundary between \
         two levels ({bare} without skirts) — the wall did not cover the crack"
    );
}
