// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **No patch of ground carries two surfaces.**
//!
//! This is the artefact, reproduced. A selected tile that has not arrived is
//! drawn by the nearest ancestor the consumer holds — and that ancestor spans
//! *all* of its descendants, including the siblings that did arrive. Both write
//! depth, at nearly the same place over real relief, and the depth test picks a
//! winner per pixel.
//!
//! On screen it is unmistakable once you know what you are looking at: sharp
//! imagery and blurry imagery interleaved in **ragged organic outlines that
//! follow the terrain rather than the tile grid**, because the boundary is the
//! curve where a coarse tessellation crosses a fine one. Worst over islands and
//! mountains, where refinement is deep and uneven and the two states coexist
//! most. It has been mistaken, in this repository, for a coverage gap, an LOD
//! front, and a texture-budget problem. It is none of those.
//!
//! The fixture is the smallest thing that produces it: four children of one
//! tile, three of them arrived, one not. The one that has not arrived is what
//! puts the ancestor on screen, and the ancestor then covers the three that did.
//!
//! Every surface here carries a flat colour, so the question "which surface won
//! this pixel" is answered by reading the pixel. The terrain has relief — a
//! smooth bump sampled coarsely by the ancestor and finely by the children — for
//! the reason the artefact needs it: on a bare ellipsoid a coarse chord always
//! falls *inside* the fine surface and the fine one wins everywhere, which is
//! why a flat fixture would report success and prove nothing.
//!
//! A machine without a usable adapter skips, with a printed reason. Set
//! `TUILE_REQUIRE_GPU=1` to turn that skip into a failure.

use std::collections::VecDeque;
use std::sync::Arc;
use std::task::{Context, Poll};

use glam::{DVec3, Mat4, Vec3};

use tuile_core::content::{DecodedTexture, DecodedTileContent, TileContent};
use tuile_core::geo::{geodetic_to_ecef, Geodetic};
use tuile_core::protocol::{ClientMessage, GeometryStream, ServerMessage, StreamError};
use tuile_core::raster::{ImageryCoord, ImageryLayer};
use tuile_core::source::TileId;
use tuile_core::traversal::TraversalStats;
use tuile_terrain::{
    skirt_height, to_decoded, GeoRect, GeographicTilingScheme, Header, QuantizedMesh, TileCoord,
};
use tuile_wgpu::{
    ContentPump, GpuContext, TileRenderer, ViewUniform, DEPTH_FORMAT, SAMPLES, TEXTURE_FORMAT,
};

const SIZE: u32 = 512;

/// How much relief the fixture carries, metres.
///
/// Large enough that a coarse tessellation and a fine one visibly disagree at
/// this framing, small enough to be terrain rather than a wall.
const RELIEF: f64 = 2_000.0;

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

/// The one surface every mesh in this fixture samples, in metres above the
/// ellipsoid.
///
/// A product of sines over the *whole planet's* longitude and latitude, so that
/// an ancestor and its children sample the same function and differ only in how
/// finely. Two full periods across the ancestor's own rectangle: enough that a
/// coarse tessellation cuts above the surface in some places and below it in
/// others, which is exactly the condition that makes the depth test flip from
/// pixel to pixel.
fn ground(lon: f64, lat: f64) -> f64 {
    const PERIODS_PER_RADIAN: f64 = 120.0;
    RELIEF * (lon * PERIODS_PER_RADIAN).sin() * (lat * PERIODS_PER_RADIAN).sin()
}

/// A tile tessellated `steps × steps`, following [`ground`].
fn relief_mesh(rect: &GeoRect, steps: usize) -> QuantizedMesh {
    let (lon, lat) = rect.center();
    let c = geodetic_to_ecef(Geodetic {
        lon,
        lat,
        height: 0.0,
    });
    let n = steps + 1;
    let (mut u, mut v, mut height) = (Vec::new(), Vec::new(), Vec::new());
    for row in 0..n {
        for col in 0..n {
            let fu = col as f64 / steps as f64;
            let fv = row as f64 / steps as f64;
            u.push(fu);
            v.push(fv);
            // Normalised into the header's own range, which `to_decoded`
            // undoes — so every mesh here must declare the same range or the
            // same normalised value would mean different metres.
            let metres = ground(
                rect.west + fu * rect.width(),
                rect.south + fv * rect.height(),
            );
            height.push((metres + RELIEF) / (2.0 * RELIEF));
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
            min_height: -RELIEF as f32,
            max_height: RELIEF as f32,
            bounding_sphere_center: [c.x, c.y, c.z],
            bounding_sphere_radius: 1.0e6,
            horizon_occlusion: [0.0; 3],
        },
        u,
        v,
        height,
        indices,
        normals: None,
        edges: [
            (0..n).map(|row| at(0, row)).collect(),
            (0..n).map(|col| at(col, 0)).collect(),
            (0..n).map(|row| at(n - 1, row)).collect(),
            (0..n).map(|col| at(col, n - 1)).collect(),
        ],
        metadata_available: None,
    }
}

/// One flat-coloured layer over a whole tile.
fn painted(colour: [u8; 3], key: u64) -> ImageryLayer {
    ImageryLayer {
        coord: ImageryCoord {
            level: 1,
            x: key,
            y: 0,
        },
        texture: Arc::new(DecodedTexture {
            width: 2,
            height: 2,
            rgba8: [[colour[0], colour[1], colour[2], 255u8]; 4].concat(),
        }),
        coverage: [0.0, 0.0, 1.0, 1.0],
        translation: [0.0, 0.0],
        scale: [1.0, 1.0],
    }
}

fn content(tile: TileId, steps: usize, colour: [u8; 3], key: u64) -> DecodedTileContent {
    let rect = rect_of(tile);
    let mut c = to_decoded(&relief_mesh(&rect, steps), &rect, skirt_height(&rect));
    for mesh in &mut c.meshes {
        mesh.material.base_color_texture = None;
    }
    c.imagery = vec![painted(colour, key)];
    c
}

const ANCESTOR_COLOUR: [u8; 3] = [230, 30, 30];
const CHILD_COLOUR: [u8; 3] = [30, 230, 30];

fn parent_of(id: TileId) -> Option<TileId> {
    let (z, x, y) = id.terrain_coord();
    (z > 0).then(|| TileId::from_terrain(z - 1, x / 2, y / 2))
}

/// Renders the fixture straight down on `look_at` and returns the resolved
/// frame, RGBA8.
fn render(gpu: &GpuContext, pump: &mut ContentPump, eye: DVec3, look_at: DVec3) -> Vec<u8> {
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
    let msaa = make(
        "two-surfaces msaa",
        TEXTURE_FORMAT,
        wgpu::TextureUsages::RENDER_ATTACHMENT,
        SAMPLES,
    );
    let msaa_view = msaa.create_view(&wgpu::TextureViewDescriptor::default());
    let target = make(
        "two-surfaces target",
        TEXTURE_FORMAT,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        1,
    );
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let depth = make(
        "two-surfaces depth",
        DEPTH_FORMAT,
        wgpu::TextureUsages::RENDER_ATTACHMENT,
        SAMPLES,
    );
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());

    let renderer = TileRenderer::new(gpu, TEXTURE_FORMAT);
    pump.rebase(&gpu.queue, eye);

    let forward = (look_at - eye).normalize();
    let up = DVec3::Z.cross(forward).normalize_or(DVec3::X);
    let view_m = Mat4::look_to_rh(Vec3::ZERO, forward.as_vec3(), up.as_vec3());
    let proj = Mat4::perspective_rh(40f32.to_radians(), 1.0, 100.0, 40_000_000.0);
    renderer.set_view(
        &gpu.queue,
        &ViewUniform {
            view_proj: (proj * view_m).to_cols_array(),
            // Ambient only, no atmosphere: the question is which surface won,
            // not how it is lit.
            params: [1.0, 0.0, 0.0, 0.0],
            ..Default::default()
        },
    );

    let (drawn, _) = pump.resolve(&gpu.queue, parent_of);
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("two-surfaces"),
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
        // Exactly the order the renderer guarantees: fallbacks are backdrop,
        // chosen tiles are geometry. See `tuile_wgpu::Drawn`.
        renderer.render_background(&mut pass, drawn.fallback.into_iter());
        renderer.render(&mut pass, drawn.exact.into_iter(), false);
    }

    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("two-surfaces readback"),
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

/// Where the ancestor's surface won a pixel: more red than green.
fn ancestor_pixels(pixels: &[u8], lo: u32, hi: u32) -> u64 {
    let mut count = 0;
    for row in lo..hi {
        for col in lo..hi {
            let i = ((row * SIZE + col) * 4) as usize;
            let (r, g) = (i32::from(pixels[i]), i32::from(pixels[i + 1]));
            if r > g + 40 {
                count += 1;
            }
        }
    }
    count
}

/// **An ancestor drawn for one missing child must not appear over the children
/// that arrived.**
///
/// Three of four children have their own surface; the fourth has not arrived, so
/// the ancestor is drawn to cover it. The camera looks straight down on a corner
/// well inside one of the three that *did* arrive.
///
/// Every red pixel there is a fragment where the ancestor's coarse surface beat a
/// child's fine one — ground that carries two surfaces and shows whichever
/// happened to win. There should be none.
///
/// This is the test that was missing. The consumer-side unit tests assert that
/// the walk draws no ancestor *when every selected tile has a surface*, which is
/// true and not the situation on screen: one late sibling is enough to put the
/// ancestor back, and it then covers all three of the others.
#[test]
fn an_ancestor_drawn_for_a_late_sibling_does_not_show_over_the_others() {
    let Some(gpu) = gpu() else { return };

    let ancestor = TileId::from_terrain(8, 130, 90);
    let (az, ax, ay) = ancestor.terrain_coord();
    let children: Vec<TileId> = (0..4)
        .map(|i| TileId::from_terrain(az + 1, ax * 2 + (i & 1), ay * 2 + (i >> 1)))
        .collect();
    // The one that has not arrived, and the reason the ancestor is on screen.
    let late = children[3];
    let arrived: Vec<TileId> = children.iter().copied().filter(|c| *c != late).collect();

    let mut script = vec![ServerMessage::Select {
        tiles: children.iter().map(|c| (*c, 0.0)).collect(),
        stats: TraversalStats::default(),
    }];
    // Coarse: two quads across the whole ancestor, which is what makes its
    // surface disagree with the children's.
    script.push(ServerMessage::Content {
        tile: ancestor,
        content: TileContent::Decoded(content(ancestor, 2, ANCESTOR_COLOUR, 0)),
    });
    for (n, child) in arrived.iter().enumerate() {
        script.push(ServerMessage::Content {
            tile: *child,
            content: TileContent::Decoded(content(*child, 16, CHILD_COLOUR, n as u64 + 1)),
        });
    }

    let mut stream = Scripted(VecDeque::from(script));
    // Straight down on the middle of the first arrived child, from close enough
    // that it fills the frame — so every pixel counted is over ground that child
    // covers.
    let rect = rect_of(arrived[0]);
    let (lon, lat) = rect.center();
    let look_at = geodetic_to_ecef(Geodetic {
        lon,
        lat,
        height: 0.0,
    });
    let eye = look_at + look_at.normalize() * 30_000.0;
    let mut pump = ContentPump::new(eye);
    pump.pump(&mut stream, &gpu, 32);

    let pixels = render(&gpu, &mut pump, eye, look_at);
    // The middle of the frame only: the edges are where the child runs out and
    // the ancestor is legitimately the only surface there.
    let (lo, hi) = (SIZE * 3 / 8, SIZE * 5 / 8);
    let intruding = ancestor_pixels(&pixels, lo, hi);
    let area = u64::from(hi - lo) * u64::from(hi - lo);

    assert_eq!(
        intruding,
        0,
        "{intruding} of {area} pixels over a child that HAS its own surface were \
         won by the ancestor drawn for its late sibling — two surfaces on one \
         patch of ground, decided per pixel. That is the interleaving of sharp \
         and blurry imagery in ragged outlines that follow the relief."
    );
}
