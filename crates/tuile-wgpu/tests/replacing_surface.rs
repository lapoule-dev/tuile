// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **A surface that replaces the ground is never fought for a pixel by it.**
//!
//! A 3D Tiles mesh of the whole surface — buildings, trees and the ground
//! under them — drawn over a globe is a second surface of the same ground,
//! from another source. The two never agree to the centimetre, and a depth
//! test between them flips from pixel to pixel: z-fighting, in shimmering
//! patches that follow the relief. So the replacing surface is drawn first
//! and marks what it drew, and the globe draws only where it did not — and
//! there, exactly as it would without it, so no ground is ever left bare.
//!
//! The fixture is the artefact's own condition: the same relief tessellated
//! coarsely by one surface and finely by the other, so each rises above the
//! other in turn. Every surface carries a flat colour, and reading a pixel
//! says which one won it.
//!
//! A machine without a usable adapter skips, with a printed reason. Set
//! `TUILE_REQUIRE_GPU=1` to turn that skip into a failure.

mod support;

use std::sync::Arc;

use glam::{DVec3, Vec3};

use support::{exact, gpu, mesh, select, Scripted};
use tuile_core::budget::FrameDeadline;
use tuile_core::content::{DecodedTexture, DecodedTileContent};
use tuile_core::geo::{geodetic_to_ecef, Geodetic};
use tuile_core::raster::{ImageryCoord, ImageryLayer};
use tuile_core::source::TileId;
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
        cut: 0,
    }
}

/// One flat-coloured layer over a whole tile.
fn painted(colour: [u8; 3], key: u64) -> ImageryLayer {
    ImageryLayer {
        source: 0,
        coord: ImageryCoord {
            level: 1,
            x: key,
            y: 0,
        },
        texture: Arc::new(DecodedTexture::from_rgba8(
            2,
            2,
            [[colour[0], colour[1], colour[2], 255u8]; 4].concat(),
        )),
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

/// Draws `replacing` as a surface that replaces the ground and `ground` as
/// the globe's own surface, in the renderer's order, and returns the
/// resolved frame, RGBA8.
fn draw(
    gpu: &GpuContext,
    eye: DVec3,
    look_at: DVec3,
    replacing: &[&tuile_wgpu::PreparedTile],
    ground: &[&tuile_wgpu::PreparedTile],
) -> Vec<u8> {
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
        "replacing msaa",
        TEXTURE_FORMAT,
        wgpu::TextureUsages::RENDER_ATTACHMENT,
        SAMPLES,
    );
    let msaa_view = msaa.create_view(&wgpu::TextureViewDescriptor::default());
    let target = make(
        "replacing target",
        TEXTURE_FORMAT,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        1,
    );
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let depth = make(
        "replacing depth",
        DEPTH_FORMAT,
        wgpu::TextureUsages::RENDER_ATTACHMENT,
        SAMPLES,
    );
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());

    let renderer = TileRenderer::new(gpu, TEXTURE_FORMAT);

    let forward = (look_at - eye).normalize();
    let up = DVec3::Z.cross(forward).normalize_or(DVec3::X);
    let view_m = glam::camera::rh::view::look_to_mat4(Vec3::ZERO, forward.as_vec3(), up.as_vec3());
    let proj =
        glam::camera::rh::proj::directx::perspective(40f32.to_radians(), 1.0, 100.0, 40_000_000.0);
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

    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("replacing"),
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
        renderer.paint_replacing(
            &mut pass,
            std::iter::empty(),
            replacing.iter().copied(),
            std::iter::empty(),
            ground.iter().copied(),
            std::iter::empty(),
            None,
            false,
        );
    }

    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("replacing readback"),
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
    let pixels = slice.get_mapped_range().expect("map range").to_vec();
    readback.unmap();
    pixels
}

const REPLACING_COLOUR: [u8; 3] = [230, 30, 30];
const GROUND_COLOUR: [u8; 3] = [30, 230, 30];

/// Pixels in the square `lo..hi` whose colour is `which`'s: more red than
/// green for the replacing surface, the other way round for the ground.
fn won_by(pixels: &[u8], lo: u32, hi: u32, replacing: bool) -> u64 {
    let mut count = 0;
    for row in lo..hi {
        for col in lo..hi {
            let i = ((row * SIZE + col) * 4) as usize;
            let (r, g) = (i32::from(pixels[i]), i32::from(pixels[i + 1]));
            if (replacing && r > g + 40) || (!replacing && g > r + 40) {
                count += 1;
            }
        }
    }
    count
}

/// **Where the replacing surface is, it is all there is; where it is not,
/// the ground is.**
///
/// The replacing surface covers one tile, tessellated coarsely and a few
/// metres lower; the ground covers that tile's parent — four times the area
/// — finely, over the same relief, so by depth alone the ground would win
/// most of the shared tile. Looking
/// straight down on the shared tile, every pixel of its middle must be the
/// replacing surface's and none the ground's; and the ground must still
/// cover what lies beyond it.
#[test]
fn a_replacing_surface_takes_every_pixel_it_draws_and_no_other() {
    let Some(gpu) = gpu() else { return };

    let parent = TileId::from_terrain(8, 130, 90);
    let (pz, px, py) = parent.terrain_coord();
    let shared = TileId::from_terrain(pz + 1, px * 2, py * 2);

    let mut ground = ContentPump::new(DVec3::ZERO);
    let mut replacing = ContentPump::new(DVec3::ZERO);
    ground.pump(
        &mut Scripted::new([
            mesh(parent, 1, content(parent, 24, GROUND_COLOUR, 0)),
            select(vec![exact(parent)], 1),
        ]),
        &gpu,
        FrameDeadline::UNBOUNDED,
    );
    // Lowered under the ground by a few metres, as two sources of the same
    // ground routinely are: depth alone would give every pixel to the ground.
    let mut lowered = content(shared, 3, REPLACING_COLOUR, 1);
    let down = -lowered.local_origin_ecef.normalize() * 8.0;
    for m in &mut lowered.meshes {
        for p in &mut m.positions {
            *p = (glam::DVec3::new(f64::from(p[0]), f64::from(p[1]), f64::from(p[2])) + down)
                .as_vec3()
                .to_array();
        }
    }
    replacing.pump(
        &mut Scripted::new([mesh(shared, 1, lowered), select(vec![exact(shared)], 1)]),
        &gpu,
        FrameDeadline::UNBOUNDED,
    );

    let rect = rect_of(shared);
    let (lon, lat) = rect.center();
    let look_at = geodetic_to_ecef(Geodetic {
        lon,
        lat,
        height: 0.0,
    });
    let eye = look_at + look_at.normalize() * 30_000.0;
    ground.rebase(&gpu.queue, eye);
    replacing.rebase(&gpu.queue, eye);
    let (on_top, _) = replacing.resolve(&gpu);
    let on_top = on_top.exact.clone();
    let (under, _) = ground.resolve(&gpu);
    let pixels = draw(&gpu, eye, look_at, &on_top, &under.exact);

    let (lo, hi) = (SIZE * 3 / 8, SIZE * 5 / 8);
    let area = u64::from(hi - lo) * u64::from(hi - lo);
    let fought = won_by(&pixels, lo, hi, false);
    assert_eq!(
        fought, 0,
        "{fought} of {area} pixels over the replacing surface were won by the \
         ground beneath it — two surfaces of one patch of ground, decided per \
         pixel by depth: z-fighting"
    );
    assert_eq!(won_by(&pixels, lo, hi, true), area, "all of it replaced");

    // Farther out: the ground is still there past what the replacing surface
    // covers, from a height that shows both.
    let far = look_at + look_at.normalize() * 150_000.0;
    ground.rebase(&gpu.queue, far);
    replacing.rebase(&gpu.queue, far);
    let (on_top, _) = replacing.resolve(&gpu);
    let on_top = on_top.exact.clone();
    let (under, _) = ground.resolve(&gpu);
    let pixels = draw(&gpu, far, look_at, &on_top, &under.exact);
    assert!(
        won_by(&pixels, 0, SIZE, false) > 0,
        "the ground beyond the replacing surface is drawn"
    );
    assert!(won_by(&pixels, 0, SIZE, true) > 0);
}
