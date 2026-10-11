// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **A film's tiles meet on one line, in every stage that draws them.**
//!
//! The rule is the engine's (`tuile_core::stitch`); the film renderer
//! applies it in its shaders — the visibility raster, the sun's depth pass
//! and the resolve all read a vertex through one function. Three things are
//! held here, on the scene a film showed dark hairs in (tiles cut from two
//! terrain tiles of made-up relief, meeting along the line between them):
//!
//! - stitched, the ground shows nothing through it **with no skirt at
//!   all**: not a sample of a magenta sheet laid under it, from either
//!   side, where a hundred and more pixels of it show unstitched;
//! - the pass that moves the vertices puts each one where
//!   `stitch::displaced` puts it on the CPU, to a tenth of a millimetre:
//!   the numbers themselves, read back, for every vertex of every tile;
//! - the shaders move a vertex exactly as `stitch::displaced` does on the
//!   CPU — which is what the seam instrument measures: the picture of
//!   strips applied by the GPU is the picture of meshes displaced
//!   beforehand;
//! - and the same with a low sun and shadows, where a depth pass that did
//!   not displace would shade the ground from where it is not.

#[path = "../../tuile-terrain/tests/support/mod.rs"]
mod support;

use glam::DVec3;
use support::{encoded, measured, tile_at, LAT, LON};
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};
use tuile_core::source::TileId;
use tuile_core::stitch;
use tuile_film::from_store::terrain_mesh;
use tuile_film::stitching::{self, Stitching};
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
                label: Some("film stitching"),
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
/// as its four children. Levels 14 to 16 meet along lines inside the
/// western tile, and two terrain tiles along the line between them.
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

/// How the scene's tiles are put on one line, if they are.
#[derive(Clone, Copy, PartialEq, Debug)]
enum How {
    /// Each tile's edge as its own data states it.
    Not,
    /// By the renderer's shaders, from the strips of the plan.
    Shaders,
    /// Beforehand, on the CPU, by `stitch::displaced`: the meshes entered
    /// are the displaced ones and the renderer is told nothing.
    Beforehand,
}

struct Scene {
    how: How,
    /// Whether the tiles keep their skirts.
    walls: bool,
    /// A low sun and its shadows.
    shadows: bool,
    /// −1 for an eye west of the line between the terrain tiles, +1 east.
    side: f64,
}

fn enter(film: &mut FilmGpu, key: TileKey, mesh: &Mesh) {
    film.enter(
        key,
        &TileMesh {
            origin_ecef: mesh.origin_ecef,
            positions: &mesh.positions,
            normals: &mesh.normals,
            uvs: &mesh.uvs,
            indices: &mesh.indices,
            index_count: mesh.index_count,
            base_color_factor: mesh.base_color_factor,
        },
        None,
    )
    .expect("enter");
}

fn render(scene: &Scene) -> Option<Vec<u8>> {
    let (device, queue) = device()?;
    let a = tile_at(13, LON, LAT);
    let b = TileCoord::new(13, a.x + 1, a.y);
    let mut look = Look::default();
    if scene.shadows {
        // From the west-south-west, 10° over the horizon: every step along
        // the line between the two terrain tiles casts or takes a shadow.
        let rect = GeographicTilingScheme::default().tile_rect(a);
        let frame = enu_frame(Geodetic {
            lon: rect.east,
            lat: rect.south,
            height: 0.0,
        });
        look.to_sun = (frame.x_axis * -0.94 + frame.y_axis * -0.3 + frame.z_axis * 0.18)
            .normalize()
            .as_vec3();
        look.shadow = 1.0;
    }
    let mut film = FilmGpu::new(
        device,
        queue,
        Settings {
            width: WIDTH,
            height: HEIGHT,
            supersample: 2,
            look,
        },
    );
    let (bytes_a, bytes_b) = (encoded(&measured(a, 32)), encoded(&measured(b, 20)));
    let mut keys = Vec::new();
    let mut meshes: Vec<(TileKey, u32, Mesh)> = Vec::new();
    for (tile, from) in drawn(a, b) {
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
        let mut mesh: Mesh = terrain_mesh(id, &refs, bytes, [0.1, 0.3, 0.1, 1.0]).expect("mesh");
        if !scene.walls {
            let origin = DVec3::from_array(mesh.origin_ecef);
            mesh = stitching::encoded(
                &stitch::without_skirts(&stitching::decoded(&mesh), origin),
                mesh.origin_ecef,
                mesh.base_color_factor,
            );
        }
        meshes.push((TileKey { id, drape: 0 }, from.level, mesh));
    }
    match scene.how {
        How::Not => {
            for (key, _, mesh) in &meshes {
                enter(&mut film, *key, mesh);
            }
        }
        How::Shaders => {
            let mut stitching = Stitching::default();
            for (key, source, mesh) in &meshes {
                enter(&mut film, *key, mesh);
                stitching.enter(*key, Some(*source), mesh);
            }
            let selection: Vec<TileKey> = meshes.iter().map(|m| m.0).collect();
            let again = stitching.frame(&selection);
            assert!(!again.is_empty(), "the scene has tiles to stitch");
            for tile in again {
                film.stitch(
                    &tile.key,
                    tile.mesh
                        .as_ref()
                        .map(|mesh| TileMesh {
                            origin_ecef: mesh.origin_ecef,
                            positions: &mesh.positions,
                            normals: &mesh.normals,
                            uvs: &mesh.uvs,
                            indices: &mesh.indices,
                            index_count: mesh.index_count,
                            base_color_factor: mesh.base_color_factor,
                        })
                        .as_ref(),
                    &tile.strips,
                    tile.reach,
                )
                .expect("stitch");
            }
        }
        How::Beforehand => {
            let edges: Vec<stitch::Edges> = meshes
                .iter()
                .map(|(_, _, mesh)| {
                    let decoded = stitching::decoded(mesh);
                    stitch::Edges::of_vertices(
                        DVec3::from_array(mesh.origin_ecef),
                        &decoded.positions,
                        decoded.uvs.as_deref().expect("uvs"),
                    )
                })
                .collect();
            let tiles: Vec<stitch::Tile<'_>> = meshes
                .iter()
                .zip(&edges)
                .map(|((key, source, _), edges)| {
                    let (level, x, y) = TileId(key.id).terrain_coord();
                    stitch::Tile {
                        level,
                        x,
                        y,
                        source: *source,
                        edges,
                    }
                })
                .collect();
            for ((key, _, mesh), plan) in
                meshes
                    .iter()
                    .zip(stitch::plan(&tiles, (2, 1), stitch::BAND))
            {
                let origin = DVec3::from_array(mesh.origin_ecef);
                let mut decoded = stitch::split(&stitching::decoded(mesh), origin, &plan.inserts);
                let uvs = decoded.uvs.clone().expect("uvs");
                for (p, uv) in decoded.positions.iter_mut().zip(&uvs) {
                    *p = (glam::Vec3::from(*p) + plan.offset(*uv, stitch::BAND)).to_array();
                }
                let moved = stitching::encoded(&decoded, mesh.origin_ecef, mesh.base_color_factor);
                enter(&mut film, *key, &moved);
            }
        }
    }
    keys.extend(meshes.iter().map(|m| m.0));
    let (origin, positions, indices) = sheet(a);
    // A key whose place in the tree is nobody's neighbour.
    let under = TileKey { id: 0, drape: 7 };
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

    let camera = FrameCamera::of(&eye(a, scene.side), WIDTH as f32 / HEIGHT as f32);
    let encoder = film.render(&camera, &keys).expect("render");
    film.queue().submit([encoder.finish()]);
    let rgba = picture(&film);
    if let Some(dir) = std::env::var_os("TUILE_SEAM_PICTURES") {
        let name = format!(
            "film-stitch-{:?}-{}-{}-{}.png",
            scene.how,
            if scene.side < 0.0 {
                "eye-west"
            } else {
                "eye-east"
            },
            if scene.walls { "skirts" } else { "no-skirts" },
            if scene.shadows { "low-sun" } else { "default" },
        )
        .to_lowercase();
        image::save_buffer(
            std::path::Path::new(&dir).join(name),
            &rgba,
            WIDTH,
            HEIGHT,
            image::ExtendedColorType::Rgba8,
        )
        .expect("png");
    }
    Some(rgba)
}

/// Pixels that show the sheet: the ground is green and nothing else in the
/// frame is, so a pixel with more red and more blue than green has the
/// sheet in it, were it one sample of its four.
fn through(rgba: &[u8]) -> usize {
    rgba.chunks_exact(4)
        .filter(|p| p[0] > p[1] && p[2] > p[1])
        .count()
}

/// Pixels of two pictures that differ by more than `by` in a channel.
fn apart(a: &[u8], b: &[u8], by: u8) -> usize {
    a.chunks_exact(4)
        .zip(b.chunks_exact(4))
        .filter(|(p, q)| p.iter().zip(q.iter()).any(|(x, y)| x.abs_diff(*y) > by))
        .count()
}

#[test]
fn stitched_ground_is_closed_with_no_skirt_at_all() {
    for side in [-1.0, 1.0] {
        let scene = |how, walls| Scene {
            how,
            walls,
            shadows: false,
            side,
        };
        let Some(bare) = render(&scene(How::Not, false)) else {
            eprintln!("no GPU: skipped");
            return;
        };
        let stitched = render(&scene(How::Shaders, false)).expect("a GPU");
        // The defect, first: without skirts and without stitching the
        // sheet shows along the line.
        assert!(
            through(&bare) > 100,
            "side {side}: {} pixels of the sheet — the scene has no seam to close",
            through(&bare)
        );
        // Stitched, not a sample of it, with nothing hung under the line.
        assert_eq!(
            through(&stitched),
            0,
            "side {side}: the sheet shows through stitched ground"
        );
        // With the skirts under it: nothing.
        let netted = render(&scene(How::Shaders, true)).expect("a GPU");
        assert_eq!(through(&netted), 0, "side {side}");
    }
}

#[test]
fn the_shaders_move_a_vertex_as_the_engine_does() {
    let scene = |how, shadows| Scene {
        how,
        walls: true,
        shadows,
        side: 1.0,
    };
    for shadows in [false, true] {
        let Some(by_shaders) = render(&scene(How::Shaders, shadows)) else {
            eprintln!("no GPU: skipped");
            return;
        };
        let beforehand = render(&scene(How::Beforehand, shadows)).expect("a GPU");
        let not = render(&scene(How::Not, shadows)).expect("a GPU");
        // The scene is one stitching changes: the comparison below is not
        // of two pictures that could not differ.
        assert!(
            apart(&by_shaders, &not, 8) > 300,
            "shadows {shadows}: stitching moved {} pixels",
            apart(&by_shaders, &not, 8)
        );
        // A vertex placed by the shader and one placed beforehand differ by
        // rounding; a pixel on a facet's edge may fall on either side of
        // it. Nothing more: under 1 in 10 000 pixels, and none by much.
        let (some, much) = (
            apart(&by_shaders, &beforehand, 8),
            apart(&by_shaders, &beforehand, 48),
        );
        assert!(
            some < 92 && much < 12,
            "shadows {shadows}: {some} pixels apart, {much} of them by much"
        );
    }
}

/// The scene's tiles as content, stitched on the CPU or not, and the tree
/// place of each.
fn contents(stitched: bool) -> Vec<((u32, u64, u64), tuile_core::content::DecodedTileContent)> {
    let a = tile_at(13, LON, LAT);
    let b = TileCoord::new(13, a.x + 1, a.y);
    let (bytes_a, bytes_b) = (encoded(&measured(a, 32)), encoded(&measured(b, 20)));
    let made: Vec<_> = drawn(a, b)
        .into_iter()
        .map(|(tile, from)| {
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
            let mesh = terrain_mesh(id, &refs, bytes, [1.0; 4]).expect("mesh");
            let content = tuile_core::content::DecodedTileContent {
                withheld_drape: None,
                meshes: vec![stitching::decoded(&mesh)],
                textures: Vec::new(),
                imagery: Vec::new(),
                local_origin_ecef: DVec3::from_array(mesh.origin_ecef),
                transform_local: glam::Mat4::IDENTITY,
            };
            ((tile.level, tile.x, tile.y), from.level, content)
        })
        .collect();
    if !stitched {
        return made.into_iter().map(|(at, _, c)| (at, c)).collect();
    }
    let edges: Vec<stitch::Edges> = made.iter().map(|m| stitch::Edges::of(&m.2)).collect();
    let tiles: Vec<stitch::Tile<'_>> = made
        .iter()
        .zip(&edges)
        .map(|((at, source, _), edges)| stitch::Tile {
            level: at.0,
            x: at.1,
            y: at.2,
            source: *source,
            edges,
        })
        .collect();
    made.iter()
        .zip(stitch::plan(&tiles, (2, 1), stitch::BAND))
        .map(|((at, _, content), plan)| (*at, stitch::displaced(content, &plan, stitch::BAND, 1.0)))
        .collect()
}

#[test]
fn the_instrument_finds_nothing_left_between_the_scenes_tiles() {
    use tuile_core::seam::{residuals, Drawn, Tolerance};
    let left = |stitched: bool| {
        let made = contents(stitched);
        let drawn: Vec<Drawn<'_>> = made
            .iter()
            .map(|(at, content)| Drawn {
                level: at.0,
                x: at.1,
                y: at.2,
                content,
            })
            .collect();
        let found = residuals(&drawn, (2, 1), None, Tolerance::Metres(0.01));
        (found.shared, found.over, found.metres[0])
    };
    let (shared, over, worst) = left(false);
    assert!(
        over > 0 && worst > 1.0,
        "{over} of {shared} over, worst {worst} m"
    );
    let (_, over, worst) = left(true);
    assert_eq!(over, 0, "worst {worst} m");
    assert!(worst < 1.0e-2, "{worst} m");
}

/// Reads a buffer of `f32` back.
fn floats(film: &FilmGpu, buffer: &wgpu::Buffer) -> Vec<f32> {
    let copy = film.device().create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: buffer.size(),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = film.device().create_command_encoder(&Default::default());
    encoder.copy_buffer_to_buffer(buffer, 0, &copy, 0, buffer.size());
    film.queue().submit([encoder.finish()]);
    let slice = copy.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    film.device()
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let bytes = slice.get_mapped_range().to_vec();
    bytemuck::pod_collect_to_vec(&bytes)
}

/// **The numbers.** Every vertex of every tile of the scene, as the GPU's
/// pass left it, against `stitch::displaced` — the function the seam
/// instrument measures with.
#[test]
fn the_pass_puts_every_vertex_where_the_engine_does() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU: skipped");
        return;
    };
    let mut film = FilmGpu::new(
        device,
        queue,
        Settings {
            width: 64,
            height: 64,
            supersample: 1,
            look: Look::default(),
        },
    );
    let a = tile_at(13, LON, LAT);
    let b = TileCoord::new(13, a.x + 1, a.y);
    let (bytes_a, bytes_b) = (encoded(&measured(a, 32)), encoded(&measured(b, 20)));
    let mut stitching = Stitching::default();
    let mut keys = Vec::new();
    for (tile, from) in drawn(a, b) {
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
        let mesh = terrain_mesh(id, &refs, bytes, [1.0; 4]).expect("mesh");
        let key = TileKey { id, drape: 0 };
        enter(&mut film, key, &mesh);
        stitching.enter(key, Some(from.level), &mesh);
        keys.push(key);
    }
    for tile in stitching.frame(&keys) {
        film.stitch(
            &tile.key,
            tile.mesh
                .as_ref()
                .map(|mesh| TileMesh {
                    origin_ecef: mesh.origin_ecef,
                    positions: &mesh.positions,
                    normals: &mesh.normals,
                    uvs: &mesh.uvs,
                    indices: &mesh.indices,
                    index_count: mesh.index_count,
                    base_color_factor: mesh.base_color_factor,
                })
                .as_ref(),
            &tile.strips,
            tile.reach,
        )
        .expect("stitch");
    }
    // The pass runs with the next frame recorded.
    let camera = FrameCamera::of(&eye(a, 1.0), 1.0);
    let encoder = film.render(&camera, &keys).expect("render");
    film.queue().submit([encoder.finish()]);

    let expected = contents(true);
    let (mut vertices, mut moved, mut worst, mut far) = (0usize, 0usize, 0.0f32, 0.0f32);
    for (key, (_, content)) in keys.iter().zip(&expected) {
        let cpu = &content.meshes[0].positions;
        let Some(buffer) = film.stitched_positions(key) else {
            continue;
        };
        let gpu = floats(&film, buffer);
        assert_eq!(gpu.len(), 3 * cpu.len(), "the same mesh on both sides");
        let before = contents(false);
        let as_meshed = &before
            .iter()
            .find(|(at, _)| TileId::from_terrain(at.0, at.1, at.2).0 == key.id)
            .expect("tile")
            .1
            .meshes[0]
            .positions;
        for (n, (p, q)) in cpu.iter().zip(gpu.chunks_exact(3)).enumerate() {
            let apart = (glam::Vec3::from(*p) - glam::Vec3::from_slice(q)).length();
            worst = worst.max(apart);
            vertices += 1;
            if let Some(was) = as_meshed.get(n) {
                let by = (glam::Vec3::from(*p) - glam::Vec3::from(*was)).length();
                far = far.max(by);
                moved += usize::from(by > 1.0e-3);
            }
        }
    }
    println!("{vertices} vertices, {moved} moved by more than a millimetre, by {far} m at most; GPU and CPU {worst} m apart at most");
    // Not a comparison of two things that did not move.
    assert!(
        moved > 500 && far > 1.0,
        "{moved} vertices moved, {far} m at most"
    );
    assert!(
        worst < 1.0e-4,
        "the GPU's pass and the engine are {worst} m apart"
    );
}
