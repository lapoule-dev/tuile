// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **A film shows nothing through its ground.**
//!
//! The film renderer has no rule about seams and is given none here: it
//! draws the meshes `tuile_film::from_store` builds from a tile store's
//! bytes, which come from the engine's one way of turning a terrain tile
//! into ground. This is the confirmation, in a picture, of what
//! `tuile-terrain` and `tuile-planetary` prove by casting rays: the scene a
//! film showed dark hairs in — tiles cut from two terrain tiles, meeting
//! along the line between them — rendered over a magenta sheet laid under
//! the ground. A magenta pixel is a ray that went between two tiles.

#[path = "../../tuile-terrain/tests/support/mod.rs"]
mod support;

use glam::DVec3;
use support::{encoded, measured, tile_at, LAT, LON};
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};
use tuile_core::source::TileId;
use tuile_film::from_store::terrain_mesh;
use tuile_film::{BakedView, FrameCamera, Look, Mesh, StoreTile, TileKey, TileRefs};
use tuile_film_gpu::{FilmGpu, Settings, TileMesh};
use tuile_terrain::{GeographicTilingScheme, TileCoord};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;

fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
    pollster::block_on(async {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .ok()?;
        adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("film seams"),
                required_limits: adapter.limits(),
                ..Default::default()
            })
            .await
            .ok()
    })
}

fn picture(film: &FilmGpu) -> Vec<u8> {
    let padded = (WIDTH * 4).div_ceil(256) * 256;
    let buffer = film.device().create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(padded * HEIGHT),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = film.device().create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        film.output().as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(HEIGHT),
            },
        },
        film.output().size(),
    );
    film.queue().submit([encoder.finish()]);
    let slice = buffer.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    film.device()
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let rows = slice.get_mapped_range().to_vec();
    rows.chunks(padded as usize)
        .flat_map(|row| row[..(WIDTH * 4) as usize].to_vec())
        .collect()
}

/// The tiles a frame draws, each with the terrain tile its surface is from:
/// the western terrain tile as tiles of levels 14, 15 and 16, the eastern
/// as its four children. None is from terrain of its own level.
fn drawn(a: TileCoord, b: TileCoord) -> Vec<(TileCoord, TileCoord)> {
    let [sw, se, nw, ne] = a.children();
    let mut tiles = vec![(sw, a), (nw, a)];
    tiles.extend(ne.children().map(|t| (t, a)));
    let [s0, s1, s2, s3] = se.children();
    tiles.extend([(s0, a), (s2, a), (s3, a)]);
    tiles.extend(s1.children().map(|t| (t, a)));
    tiles.extend(b.children().map(|t| (t, b)));
    tiles
}

/// An eye a few hundred metres over the line the two terrain tiles share,
/// `side` of it (−1 west, +1 east), looking north along it and down.
fn eye(a: TileCoord, side: f64) -> BakedView {
    let rect = GeographicTilingScheme::default().tile_rect(a);
    let at = Geodetic {
        lon: rect.east + side * 0.04 * rect.width(),
        lat: rect.south + 0.15 * rect.height(),
        height: 800.0,
    };
    let frame = enu_frame(at);
    let (east, north, up) = (frame.x_axis, frame.y_axis, frame.z_axis);
    BakedView {
        position: geodetic_to_ecef(at).to_array(),
        direction: (north * 0.75 - east * side * 0.06 - up * 0.65)
            .normalize()
            .to_array(),
        up: up.to_array(),
        viewport_px: [f64::from(WIDTH), f64::from(HEIGHT)],
        fovy_rad: 0.7,
    }
}

/// A magenta sheet two kilometres under the scene, far wider than it.
fn sheet(a: TileCoord) -> ([f64; 3], Vec<u8>, Vec<u8>) {
    let rect = GeographicTilingScheme::default().tile_rect(a);
    let under = Geodetic {
        lon: rect.east,
        lat: (rect.south + rect.north) / 2.0,
        height: -2000.0,
    };
    let frame = enu_frame(under);
    let corner = |e: f64, n: f64| -> [f32; 3] {
        let p: DVec3 = frame.x_axis * e * 30_000.0 + frame.y_axis * n * 30_000.0;
        p.as_vec3().to_array()
    };
    let positions = [
        corner(-1.0, -1.0),
        corner(1.0, -1.0),
        corner(1.0, 1.0),
        corner(-1.0, 1.0),
    ];
    (
        geodetic_to_ecef(under).to_array(),
        bytemuck::cast_slice(&positions).to_vec(),
        bytemuck::cast_slice(&[0u32, 1, 2, 0, 2, 3]).to_vec(),
    )
}

/// Pixels that show the sheet, from one side of the line. With `walls`
/// false the tiles are drawn without what hangs from their edges: only the
/// triangles of their surfaces, which a mesh lists first.
fn magenta(side: f64, walls: bool) -> Option<(u32, u32)> {
    let (device, queue) = device()?;
    let mut film = FilmGpu::new(
        device,
        queue,
        Settings {
            width: WIDTH,
            height: HEIGHT,
            supersample: 2,
            look: Look::default(),
        },
    );
    let a = tile_at(13, LON, LAT);
    let b = TileCoord::new(13, a.x + 1, a.y);
    // As a store holds them: bytes.
    let (bytes_a, bytes_b) = (encoded(&measured(a, 32)), encoded(&measured(b, 20)));
    let mut keys = Vec::new();
    let mut triangles = (0u32, 0u32);
    for (n, (tile, from)) in drawn(a, b).into_iter().enumerate() {
        let refs = TileRefs {
            terrain: StoreTile {
                level: from.level as u8,
                x: from.x as u32,
                y: from.y as u32,
                digest: 0,
            },
            skirt_height: 0.0,
            imagery: Vec::new(),
            composed_side: 0,
        };
        let id = TileId::from_terrain(tile.level, tile.x, tile.y).0;
        let bytes = if from == a { &bytes_a } else { &bytes_b };
        let mesh: Mesh = terrain_mesh(id, &refs, bytes, [0.1, 0.3, 0.1, 1.0]).expect("mesh");
        // The surface's triangles are those of the mesh cut with no skirt.
        let surface = {
            let mut cut = tuile_terrain::decode(bytes).expect("decodes");
            let mut at = from;
            while at.level < tile.level {
                let shift = tile.level - at.level - 1;
                let child = TileCoord::new(at.level + 1, tile.x >> shift, tile.y >> shift);
                cut = tuile_terrain::upsample(&cut, at, child).expect("covers");
                at = child;
            }
            cut.indices.len() as u32
        };
        triangles.0 += surface / 3;
        triangles.1 += mesh.index_count / 3;
        let key = TileKey {
            id: n as u64 + 1,
            drape: 0,
        };
        film.enter(
            key,
            &TileMesh {
                origin_ecef: mesh.origin_ecef,
                positions: &mesh.positions,
                normals: &mesh.normals,
                uvs: &mesh.uvs,
                indices: &mesh.indices,
                index_count: if walls { mesh.index_count } else { surface },
                base_color_factor: mesh.base_color_factor,
            },
            None,
        )
        .expect("enter");
        keys.push(key);
    }
    let (origin, positions, indices) = sheet(a);
    let under = TileKey { id: 999, drape: 0 };
    film.enter(
        under,
        &TileMesh {
            origin_ecef: origin,
            positions: &positions,
            normals: &[],
            uvs: &[],
            indices: &indices,
            index_count: 6,
            base_color_factor: [1.0, 0.0, 1.0, 1.0],
        },
        None,
    )
    .expect("enter");
    keys.push(under);

    let camera = FrameCamera::of(&eye(a, side), WIDTH as f32 / HEIGHT as f32);
    let encoder = film.render(&camera, &keys).expect("render");
    film.queue().submit([encoder.finish()]);
    let rgba = picture(&film);
    if let Some(dir) = std::env::var_os("TUILE_SEAM_PICTURES") {
        let name = format!(
            "film-seam-{}-{}.png",
            if side < 0.0 { "eye-west" } else { "eye-east" },
            if walls { "as-built" } else { "surfaces-only" }
        );
        let path = std::path::Path::new(&dir).join(name);
        image::save_buffer(&path, &rgba, WIDTH, HEIGHT, image::ExtendedColorType::Rgba8)
            .expect("png");
        println!(
            "{}: {} surface triangles, {} with what hangs from their edges",
            path.display(),
            triangles.0,
            triangles.1
        );
    }
    let shows = rgba
        .chunks_exact(4)
        // The ground is green and nothing else in the frame is: a pixel
        // with more red and more blue than green has the sheet in it, were
        // it one sample of its four.
        .filter(|p| p[0] > p[1] && p[2] > p[1])
        .count() as u32;
    Some((shows, triangles.1 - triangles.0))
}

#[test]
fn tiles_cut_from_two_terrain_tiles_show_nothing_between_them() {
    for (name, side) in [("west", -1.0), ("east", 1.0)] {
        let Some((bare, _)) = magenta(side, false) else {
            eprintln!("no adapter: skipped");
            return;
        };
        assert!(
            bare > 50,
            "from the {name}: without walls only {bare} pixels show the sheet, so the scene \
             opens no seam and proves nothing"
        );
        let (built, walls) = magenta(side, true).expect("an adapter, as a moment ago");
        assert!(
            walls > 0,
            "the meshes were built with nothing hung from them"
        );
        assert_eq!(
            built, 0,
            "from the {name}: {built} pixels show what is under the ground ({bare} with no \
             walls at all)"
        );
    }
}
