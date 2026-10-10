// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! PROBE (exploration, not a suite): scenes that reproduce, on made-up
//! tiles and with no tile source, what `docs/explorations/
//! seams-poles-islands-colour.md` describes. Each prints what it measures
//! and writes its pictures into `TUILE_EXPLORE_OUT` (a temporary directory
//! otherwise). Nothing here asserts a fix: the numbers are the result.
//!
//! ```text
//! TUILE_EXPLORE_OUT=<dir> cargo test --release -p tuile-film-native \
//!     --test exploration -- --nocapture --test-threads 1
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use glam::DVec3;
use tuile_core::content::DecodedTexture;
use tuile_core::geo::{ecef_to_geodetic, enu_frame, geodetic_to_ecef, Geodetic, WGS84_A};
use tuile_core::raster::{self, ImageryCoord, ImageryLayer, TilingScheme};
use tuile_film::{BakedView, FrameCamera, Look, TileKey};
use tuile_film_gpu::{FilmGpu, Settings, TileMesh};
use tuile_terrain::{
    skirt_height, to_decoded, upsample, GeographicTilingScheme, Header, QuantizedMesh, TileCoord,
};

fn out_dir() -> PathBuf {
    let dir = std::env::var_os("TUILE_EXPLORE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("tuile-explore"));
    std::fs::create_dir_all(&dir).expect("out dir");
    dir
}

fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
    pollster::block_on(async {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance.request_adapter(&Default::default()).await.ok()?;
        adapter
            .request_device(&wgpu::DeviceDescriptor {
                required_limits: adapter.limits(),
                ..Default::default()
            })
            .await
            .ok()
    })
}

fn picture(film: &FilmGpu) -> Vec<u8> {
    let (width, height) = (film.settings().width, film.settings().height);
    let padded = (width * 4).div_ceil(256) * 256;
    let buffer = film.device().create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(padded * height),
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
                rows_per_image: Some(height),
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
        .flat_map(|row| row[..(width * 4) as usize].to_vec())
        .collect()
}

fn save(name: &str, rgba: &[u8], width: u32, height: u32) -> PathBuf {
    let path = out_dir().join(name);
    image::save_buffer(&path, rgba, width, height, image::ExtendedColorType::Rgba8).expect("png");
    path
}

// ---------------------------------------------------------------- seams

const LON0: f64 = -2.86;
const LAT0: f64 = 52.51;

/// The ground the made-up terrain source measures: metres over the
/// ellipsoid, hills a couple of kilometres across with smaller ones on them.
fn relief(lon: f64, lat: f64) -> f64 {
    let x = lon * WGS84_A * LAT0.to_radians().cos();
    let y = lat * WGS84_A;
    let tau = std::f64::consts::TAU;
    300.0
        + 120.0 * (tau * x / 1700.0).sin() * (tau * y / 2300.0).cos()
        + 35.0 * (tau * x / 430.0 + 1.0).sin() * (tau * y / 510.0).sin()
}

fn tile_at(level: u32, lon_deg: f64, lat_deg: f64) -> TileCoord {
    let across = (2u64 << level) as f64;
    let up = (1u64 << level) as f64;
    TileCoord::new(
        level,
        ((lon_deg + 180.0) / 360.0 * across) as u64,
        ((lat_deg + 90.0) / 180.0 * up) as u64,
    )
}

/// A terrain tile as a source would hold it: the relief on a grid of
/// `steps` spacings a side, with its four edge lists.
fn measured(coord: TileCoord, steps: usize) -> QuantizedMesh {
    let rect = GeographicTilingScheme::default().tile_rect(coord);
    let n = steps + 1;
    let (mut u, mut v, mut metres) = (Vec::new(), Vec::new(), Vec::new());
    for j in 0..n {
        for i in 0..n {
            let (fu, fv) = (i as f64 / steps as f64, j as f64 / steps as f64);
            u.push(fu);
            v.push(fv);
            metres.push(relief(
                rect.west + fu * rect.width(),
                rect.south + fv * rect.height(),
            ));
        }
    }
    let low = metres.iter().copied().fold(f64::INFINITY, f64::min);
    let high = metres.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let mut indices = Vec::new();
    for j in 0..steps {
        for i in 0..steps {
            let a = (j * n + i) as u32;
            let (b, c, d) = (a + 1, a + n as u32, a + n as u32 + 1);
            indices.extend_from_slice(&[a, b, c, c, b, d]);
        }
    }
    let (clon, clat) = rect.center();
    let centre = geodetic_to_ecef(Geodetic {
        lon: clon,
        lat: clat,
        height: (low + high) / 2.0,
    });
    let line = |f: &dyn Fn(usize) -> usize| (0..n).map(|k| f(k) as u32).collect::<Vec<u32>>();
    QuantizedMesh {
        header: Header {
            center: centre.to_array(),
            min_height: low as f32,
            max_height: high as f32,
            bounding_sphere_center: centre.to_array(),
            bounding_sphere_radius: 5000.0,
            horizon_occlusion: [0.0; 3],
        },
        u,
        v,
        height: metres.iter().map(|m| (m - low) / (high - low)).collect(),
        indices,
        normals: None,
        edges: [
            line(&|k| k * n),
            line(&|k| k),
            line(&|k| k * n + steps),
            line(&|k| (n - 1) * n + k),
        ],
        metadata_available: None,
    }
}

struct Built {
    origin: [f64; 3],
    positions: Vec<u8>,
    uvs: Vec<u8>,
    indices: Vec<u8>,
    index_count: u32,
}

/// The mesh of `target`, from the terrain tile `from` — itself or an
/// ancestor — exactly as `tuile_film::from_store::terrain_mesh` builds it
/// from a store's bytes, the probes' switches included.
fn built(source: &QuantizedMesh, from: TileCoord, target: TileCoord) -> Built {
    let mut mesh = source.clone();
    let mut at = from;
    while at.level < target.level {
        let shift = target.level - at.level - 1;
        let child = TileCoord::new(at.level + 1, target.x >> shift, target.y >> shift);
        mesh = upsample(&mesh, at, child).expect("covers");
        at = child;
    }
    let scheme = GeographicTilingScheme::default();
    let rect = scheme.tile_rect(target);
    let deep = if std::env::var_os("TUILE_PROBE_SKIRT_OF_SOURCE").is_some() {
        skirt_height(&scheme.tile_rect(from))
    } else {
        skirt_height(&rect)
    };
    let content = to_decoded(&mesh, &rect, deep);
    let made = &content.meshes[0];
    Built {
        origin: content.local_origin_ecef.to_array(),
        positions: bytemuck::cast_slice(&made.positions).to_vec(),
        uvs: bytemuck::cast_slice(made.uvs.as_ref().expect("uvs")).to_vec(),
        indices: bytemuck::cast_slice(&made.indices).to_vec(),
        index_count: made.indices.len() as u32,
    }
}

fn children(of: TileCoord) -> [TileCoord; 4] {
    of.children()
}

/// What a frame draws: each tile, and the terrain tile its surface is from.
fn drawn(two_surfaces: Option<bool>) -> Vec<(TileCoord, TileCoord)> {
    let a = tile_at(13, LON0, LAT0);
    let [sw, se, nw, ne] = children(a);
    let mut tiles = vec![(sw, a), (nw, a)];
    tiles.extend(children(ne).map(|t| (t, a)));
    let [s0, s1, s2, s3] = children(se);
    tiles.extend([(s0, a), (s2, a), (s3, a)]);
    tiles.extend(children(s1).map(|t| (t, a)));
    let b = TileCoord::new(13, a.x + 1, a.y);
    match two_surfaces {
        None => {}
        // The neighbouring source tile drawn as itself: it has its skirts.
        Some(false) => tiles.push((b, b)),
        // …or as its children, cut from it: none has.
        Some(true) => tiles.extend(children(b).map(|t| (t, b))),
    }
    tiles
}

/// `side` is −1 for an eye west of the meridian the two source tiles share,
/// +1 for one east of it: a step is looked into from its lower side only.
fn seam_camera(side: f64) -> BakedView {
    // South of the tile, beside that meridian, half a kilometre over the
    // hills and looking north along it, down a little.
    let a = tile_at(13, LON0, LAT0);
    let rect = GeographicTilingScheme::default().tile_rect(a);
    let at = Geodetic {
        lon: rect.east + side * 0.12 * rect.width(),
        lat: rect.south - 0.10 * rect.height(),
        height: 900.0,
    };
    let frame = enu_frame(at);
    let (east, north, up) = (frame.x_axis, frame.y_axis, frame.z_axis);
    let direction = (north * 0.90 - east * side * 0.08 - up * 0.42).normalize();
    BakedView {
        position: geodetic_to_ecef(at).to_array(),
        direction: direction.to_array(),
        up: up.to_array(),
        viewport_px: [1920.0, 1080.0],
        fovy_rad: 0.8,
    }
}

fn seam_frame(two_surfaces: Option<bool>, side: f64) -> Option<Vec<u8>> {
    let (device, queue) = device()?;
    let (width, height) = (1920, 1080);
    let mut film = FilmGpu::new(
        device,
        queue,
        Settings {
            width,
            height,
            supersample: 2,
            look: Look::default(),
        },
    );
    let a = tile_at(13, LON0, LAT0);
    let b = TileCoord::new(13, a.x + 1, a.y);
    // Two source tiles, each measured on its own grid: along the meridian
    // they share, their vertices do not fall at the same places.
    let (source_a, source_b) = (measured(a, 32), measured(b, 20));
    let mut keys = Vec::new();
    for (n, (tile, from)) in drawn(two_surfaces).into_iter().enumerate() {
        let mesh = built(if from == a { &source_a } else { &source_b }, from, tile);
        // Dark ground, so that the sky through a gap cannot be taken for it.
        let texture = film.create_albedo(4, 4);
        let shade = 60 + 12 * (tile.level as u8 - 13) + 10 * ((tile.x + tile.y) % 2) as u8;
        film.write_rgba(&texture, &[shade / 2, shade, shade / 3, 255].repeat(16));
        let key = TileKey {
            id: n as u64 + 1,
            drape: 0,
        };
        film.enter(
            key,
            &TileMesh {
                origin_ecef: mesh.origin,
                positions: &mesh.positions,
                normals: &[],
                uvs: &mesh.uvs,
                indices: &mesh.indices,
                index_count: mesh.index_count,
                base_color_factor: [1.0; 4],
            },
            Some(texture),
        )
        .expect("enter");
        keys.push(key);
    }
    let camera = FrameCamera::of(&seam_camera(side), width as f32 / height as f32);
    let encoder = film.render(&camera, &keys).expect("render");
    film.queue().submit([encoder.finish()]);
    Some(picture(&film))
}

fn with_probes<T>(set: &[(&str, &str)], run: impl FnOnce() -> T) -> T {
    const ALL: [&str; 3] = [
        "TUILE_PROBE_UPSAMPLED_SKIRTS",
        "TUILE_PROBE_SKIRT_OF_SOURCE",
        "TUILE_PROBE_SKIRT_SCALE",
    ];
    for name in ALL {
        std::env::remove_var(name);
    }
    for (name, value) in set {
        std::env::set_var(name, value);
    }
    let out = run();
    for name in ALL {
        std::env::remove_var(name);
    }
    out
}

/// Pixels that are sky for the most part, and pixels the sky touches: the
/// ground is dark and the sky is not, so brightness tells.
fn holes(rgba: &[u8], closed: &[u8]) -> (u32, u32) {
    let (mut whole, mut touched) = (0, 0);
    for (p, q) in rgba.chunks_exact(4).zip(closed.chunks_exact(4)) {
        let lift = (0..3)
            .map(|c| i32::from(p[c]) - i32::from(q[c]))
            .min()
            .unwrap_or(0);
        touched += u32::from(lift > 20);
        whole += u32::from(p[0] > 200 && p[1] > 200 && p[2] > 200 && lift > 20);
    }
    (whole, touched)
}

#[test]
fn seams_where_tiles_are_cut_from_an_ancestor() {
    let a = tile_at(13, LON0, LAT0);
    let rect = GeographicTilingScheme::default().tile_rect(a);
    // How far the two source tiles part along their meridian, in metres:
    // each interpolates the relief between its own vertices.
    let along = |steps: usize, t: f64| {
        let k = (t * steps as f64).floor().min(steps as f64 - 1.0);
        let (t0, t1) = (k / steps as f64, (k + 1.0) / steps as f64);
        let h = |t: f64| relief(rect.east, rect.south + t * rect.height());
        h(t0) + (h(t1) - h(t0)) * (t - t0) / (t1 - t0)
    };
    let steps: Vec<f64> = (0..=4000)
        .map(|i| (along(32, i as f64 / 4000.0) - along(20, i as f64 / 4000.0)).abs())
        .collect();
    println!(
        "source tiles of level 13 part along their shared meridian by {:.2} m at most, {:.2} m on \
         average; a level-13 skirt is {:.1} m deep, a level-14 one {:.1}, level 15 {:.1}, level 16 {:.1}",
        steps.iter().copied().fold(0.0, f64::max),
        steps.iter().sum::<f64>() / steps.len() as f64,
        skirt_height(&rect),
        skirt_height(&rect) / 2.0,
        skirt_height(&rect) / 4.0,
        skirt_height(&rect) / 8.0,
    );

    for (name, scene) in [
        ("one-surface", None),
        ("two-surfaces-neighbour-itself", Some(false)),
        ("two-surfaces-neighbour-cut", Some(true)),
    ] {
        for (eye, side) in [("eye-west", -1.0), ("eye-east", 1.0)] {
            // With nothing drawn east of the meridian, an eye east of it
            // looks at the tile's open side: that is no seam.
            if scene.is_none() && side > 0.0 {
                continue;
            }
            // Everything closed: skirts everywhere, far deeper than any step.
            let Some(closed) = with_probes(
                &[
                    ("TUILE_PROBE_UPSAMPLED_SKIRTS", "1"),
                    ("TUILE_PROBE_SKIRT_SCALE", "40"),
                ],
                || seam_frame(scene, side),
            ) else {
                eprintln!("no adapter: skipped");
                return;
            };
            for (variant, probes) in [
                ("main", &[][..]),
                (
                    "cut-tiles-skirted",
                    &[("TUILE_PROBE_UPSAMPLED_SKIRTS", "1")][..],
                ),
                (
                    "cut-tiles-skirted-at-source-depth",
                    &[
                        ("TUILE_PROBE_UPSAMPLED_SKIRTS", "1"),
                        ("TUILE_PROBE_SKIRT_OF_SOURCE", "1"),
                    ][..],
                ),
            ] {
                let frame = with_probes(probes, || seam_frame(scene, side)).expect("adapter");
                let (whole, touched) = holes(&frame, &closed);
                let file = format!("seams-synthetic-{name}-{eye}-{variant}.png");
                save(&file, &frame, 1920, 1080);
                println!("seams,{name},{eye},{variant},{whole},{touched},{file}");
            }
        }
    }
}

// ------------------------------------------------------------ the factor

#[test]
fn the_film_multiplies_a_drape_by_the_tiles_base_colour() {
    let Some((device, queue)) = device() else {
        eprintln!("no adapter: skipped");
        return;
    };
    let mut film = FilmGpu::new(
        device,
        queue,
        Settings {
            width: 64,
            height: 48,
            supersample: 1,
            look: Look::default(),
        },
    );
    let p: [f32; 12] = [
        0.0, -200.0, -200.0, 0.0, 200.0, -200.0, 0.0, 200.0, 200.0, 0.0, -200.0, 200.0,
    ];
    let uv: [f32; 8] = [0.0, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0];
    let i: [u32; 6] = [0, 1, 2, 0, 2, 3];
    let view = BakedView {
        position: [WGS84_A + 1000.0, 0.0, 0.0],
        direction: [-1.0, 0.0, 0.0],
        up: [0.0, 0.0, 1.0],
        viewport_px: [64.0, 48.0],
        fovy_rad: 0.8,
    };
    let camera = FrameCamera::of(&view, 64.0 / 48.0);
    let mut shown = Vec::new();
    // White as a drape leaves it where every layer covers, and the slate a
    // bake of the globe writes into every tile of a pack.
    for (n, factor) in [[1.0f32; 4], [0.16, 0.20, 0.24, 1.0]]
        .into_iter()
        .enumerate()
    {
        let texture = film.create_albedo(4, 4);
        film.write_rgba(&texture, &[120u8, 120, 120, 255].repeat(16));
        let key = TileKey {
            id: n as u64 + 1,
            drape: 0,
        };
        film.enter(
            key,
            &TileMesh {
                origin_ecef: [WGS84_A, 0.0, 0.0],
                positions: bytemuck::cast_slice(&p),
                normals: &[],
                uvs: bytemuck::cast_slice(&uv),
                indices: bytemuck::cast_slice(&i),
                index_count: 6,
                base_color_factor: factor,
            },
            Some(texture),
        )
        .expect("enter");
        let encoder = film.render(&camera, &[key]).expect("render");
        film.queue().submit([encoder.finish()]);
        let rgba = picture(&film);
        let at = (24 * 64 + 32) * 4;
        shown.push([rgba[at], rgba[at + 1], rgba[at + 2]]);
    }
    let linear = |v: u8| {
        let v = f64::from(v) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    let stops: Vec<f64> = (0..3)
        .map(|c| (linear(shown[1][c]) / linear(shown[0][c])).log2())
        .collect();
    println!(
        "a grey drape (120, 120, 120) under the default look: factor white shows {:?}, the pack's \
         slate (0.16, 0.20, 0.24) shows {:?} — {:.2} / {:.2} / {:.2} stops (R G B)",
        shown[0], shown[1], stops[0], stops[1], stops[2]
    );
}

// ------------------------------------------------------------- the poles

/// One web-mercator tile painted by the latitude of each of its rows: a
/// band a degree, a dark line on each whole degree, a red one on 85.
fn graticule(scheme: &TilingScheme, coord: ImageryCoord) -> DecodedTexture {
    let side = 256u32;
    let (_, y0, _, y1) = scheme.tile_extent(coord);
    let mut rgba8 = Vec::with_capacity((side * side * 4) as usize);
    for row in 0..side {
        let y = y0 + (y1 - y0) * (f64::from(row) + 0.5) / f64::from(side);
        let lat = scheme.projection.from_normalized(0.5, y).lat.to_degrees();
        let texel = paint(lat);
        for _ in 0..side {
            rgba8.extend_from_slice(&texel);
        }
    }
    DecodedTexture {
        width: side,
        height: side,
        rgba8,
    }
}

fn paint(lat_deg: f64) -> [u8; 4] {
    let within = lat_deg - lat_deg.floor();
    if (lat_deg - 85.0).abs() < 0.06 {
        [230, 30, 30, 255]
    } else if !(0.04..=0.96).contains(&within) {
        [20, 20, 20, 255]
    } else if (lat_deg.floor() as i64) % 2 == 0 {
        [110, 150, 190, 255]
    } else {
        [190, 200, 160, 255]
    }
}

#[test]
fn imagery_over_the_polar_cap() {
    let scheme = TilingScheme::web_mercator();
    // The northernmost row of terrain at level 3: 67.5° to the pole, 22.5°
    // of longitude.
    let terrain = TileCoord::new(3, 8, 7);
    let rect = GeographicTilingScheme::default().tile_rect(terrain);
    let tile = raster::GeoRect {
        west: rect.west,
        south: rect.south,
        east: rect.east,
        north: rect.north,
    };
    let side = 512u32;
    let mut sheet = vec![255u8; (side * 3 * side * 4) as usize];
    let mut put = |column: u32, drape: &[u8]| {
        for row in 0..side {
            let from = (row * side * 4) as usize;
            let to = ((row * side * 3 + column * side) * 4) as usize;
            sheet[to..to + (side * 4) as usize]
                .copy_from_slice(&drape[from..from + (side * 4) as usize]);
        }
    };
    // What is there: the same painting, straight in latitude.
    let mut truth = Vec::with_capacity((side * side * 4) as usize);
    for row in 0..side {
        let lat = 90.0 - 22.5 * (f64::from(row) + 0.5) / f64::from(side);
        for _ in 0..side {
            truth.extend_from_slice(&paint(lat));
        }
    }
    put(0, &truth);
    for (column, level) in [(1u32, 4u32), (2, 7)] {
        // The imagery tiles under the terrain tile at this level, placed as
        // the loader places them: the grid's top row said to reach the pole.
        let (across, rows) = scheme.tiles_at(level);
        let x0 = ((rect.west + std::f64::consts::PI) / std::f64::consts::TAU * across as f64)
            .round() as u64;
        let columns = across / 16;
        let mut layers = Vec::new();
        for y in 0..rows {
            for x in x0..x0 + columns {
                let coord = ImageryCoord { level, x, y };
                let mut covers = scheme.tile_rect(coord);
                if covers.north <= tile.south {
                    continue;
                }
                let texture = graticule(&scheme, coord);
                let texture = raster::reproject_tile_to_geographic(&texture, &scheme, coord)
                    .unwrap_or(texture);
                if y == 0 {
                    covers.north = std::f64::consts::FRAC_PI_2;
                }
                layers.push(ImageryLayer::placed(
                    coord,
                    Arc::new(texture),
                    &tile,
                    &covers,
                ));
            }
            if scheme.tile_rect(ImageryCoord { level, x: x0, y }).south <= tile.south {
                break;
            }
        }
        let top = scheme.tile_rect(ImageryCoord { level, x: x0, y: 0 });
        println!(
            "imagery level {level}: its top row holds {:.3}° to {:.3}° and is laid over {:.3}° to 90°, \
             stretched {:.1} times; {} layers on the terrain tile",
            top.south.to_degrees(),
            top.north.to_degrees(),
            top.south.to_degrees(),
            (90.0 - top.south.to_degrees()) / (top.north.to_degrees() - top.south.to_degrees()),
            layers.len(),
        );
        let drape = raster::bake_layers(&layers, [0.16, 0.20, 0.24, 1.0], (side, side));
        // Where the red line of 85° is drawn.
        let reds: Vec<f64> = (0..side)
            .filter(|row| {
                let p = &drape.rgba8[((row * side + side / 2) * 4) as usize..];
                p[0] > 150 && p[1] < 90
            })
            .map(|row| 90.0 - 22.5 * (f64::from(row) + 0.5) / f64::from(side))
            .collect();
        if let (Some(north), Some(south)) = (reds.first(), reds.last()) {
            println!("  the line of 85° is drawn from {south:.2}° to {north:.2}°");
        }
        put(column, &drape.rgba8);
    }
    let path = save("pole-drape-truth-level4-level7.png", &sheet, side * 3, side);
    println!("pole: {}", path.display());
}

#[test]
fn the_mesh_and_the_maths_at_the_pole() {
    let terrain = TileCoord::new(3, 8, 7);
    let rect = GeographicTilingScheme::default().tile_rect(terrain);
    let steps = 8usize;
    let n = steps + 1;
    let (mut u, mut v) = (Vec::new(), Vec::new());
    for j in 0..n {
        for i in 0..n {
            u.push(i as f64 / steps as f64);
            v.push(j as f64 / steps as f64);
        }
    }
    let mut indices = Vec::new();
    for j in 0..steps {
        for i in 0..steps {
            let a = (j * n + i) as u32;
            indices.extend_from_slice(&[
                a,
                a + 1,
                a + n as u32,
                a + n as u32,
                a + 1,
                a + n as u32 + 1,
            ]);
        }
    }
    let (clon, clat) = rect.center();
    let centre = geodetic_to_ecef(Geodetic {
        lon: clon,
        lat: clat,
        height: 0.0,
    });
    let line = |f: &dyn Fn(usize) -> usize| (0..n).map(|k| f(k) as u32).collect::<Vec<u32>>();
    let mesh = QuantizedMesh {
        header: Header {
            center: centre.to_array(),
            min_height: 0.0,
            max_height: 1.0,
            bounding_sphere_center: centre.to_array(),
            bounding_sphere_radius: 2.0e6,
            horizon_occlusion: [0.0; 3],
        },
        height: vec![0.0; u.len()],
        u,
        v,
        indices,
        normals: None,
        edges: [
            line(&|k| k * n),
            line(&|k| k),
            line(&|k| k * n + steps),
            line(&|k| (n - 1) * n + k),
        ],
        metadata_available: None,
    };
    let surface = mesh.indices.len() / 3;
    let content = to_decoded(&mesh, &rect, skirt_height(&rect));
    let made = &content.meshes[0];
    let place = |i: u32| {
        let p = made.positions[i as usize];
        content.local_origin_ecef + DVec3::new(f64::from(p[0]), f64::from(p[1]), f64::from(p[2]))
    };
    let area = |t: &[u32]| {
        (place(t[1]) - place(t[0]))
            .cross(place(t[2]) - place(t[0]))
            .length()
            / 2.0
    };
    let flat = |from: usize, to: usize| {
        made.indices[from * 3..to * 3]
            .chunks_exact(3)
            .filter(|t| area(t) < 1.0)
            .count()
    };
    let all = made.indices.len() / 3;
    println!(
        "polar tile 3/8/7 on a {steps}×{steps} grid: {} of {surface} surface triangles have no area \
         (under 1 m²), {} of {} skirt triangles; its skirt is {:.0} m deep, as at the equator, on a \
         tile {:.0} km wide at its south edge and nothing at its north",
        flat(0, surface),
        flat(surface, all),
        all - surface,
        skirt_height(&rect),
        rect.width() * WGS84_A * rect.south.cos() / 1000.0,
    );
    // The north skirt's vertices: said to stand a hair north of 90°.
    let north_skirt = made.positions.len() - n;
    let over = ecef_to_geodetic(place(north_skirt as u32));
    println!(
        "  a north-skirt vertex of the meridian {:.2}° comes out at longitude {:.2}°, latitude {:.5}°",
        rect.west.to_degrees(),
        over.lon.to_degrees(),
        over.lat.to_degrees()
    );
    for (name, p) in [
        (
            "the pole itself",
            DVec3::new(0.0, 0.0, 6_356_752.314_245 + 1000.0),
        ),
        (
            "a millimetre off the axis",
            DVec3::new(0.001, 0.0, 6_356_752.314_245 + 1000.0),
        ),
    ] {
        let g = ecef_to_geodetic(p);
        println!(
            "  1000 m over {name}: ecef_to_geodetic gives latitude {:.4}°, height {:.1} m",
            g.lat.to_degrees(),
            g.height
        );
    }
}
