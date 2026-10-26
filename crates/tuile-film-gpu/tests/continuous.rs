// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A film's frame is a function of its own camera and instant: the light,
//! the meter and the sky are worked out on the GPU for each frame from that
//! frame alone — the viewer's own pass — so that
//!
//! - a climb from 10 km to 36 000 km at sunset keeps its limb in every
//!   frame, and its light never jumps from one frame to the next;
//! - two segments of a film rendered by two processes join without a seam:
//!   a frame is the same to the byte whether its segment began with it or
//!   long before it.
//!
//! The ground is a globe of plain grey terrain. With `TUILE_FILM_CLIMB_TRACE`
//! set to a directory, the climb's cameras, exposure, lumas, limb and
//! pictures go to `trace.mcap` there.

use glam::DVec3;
use tuile_atmosphere::{AtmosphereParams, ReferenceIllumination, Sun};
use tuile_core::geo::{ecef_to_geodetic, enu_frame, geodetic_to_ecef, Geodetic};
use tuile_film::{BakedView, FrameCamera, FrameLight, Look, TileKey};
use tuile_film_gpu::{FilmGpu, Settings, TileMesh};
use tuile_terrain::{fill_content, GeographicTilingScheme, TileCoord};

const WIDTH: u32 = 384;
const HEIGHT: u32 = 256;
/// Over Europe, where the sun sets at the instant: lon, lat, degrees.
const EUROPE: (f64, f64) = (5.0, 46.0);

fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
    pollster::block_on(async {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .ok()?;
        adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .ok()
    })
}

fn le<T: Copy + bytemuck::Pod>(v: &[T]) -> Vec<u8> {
    bytemuck::cast_slice(v).to_vec()
}

/// A film under the physical atmosphere, its ground every tile of level 3
/// over the Earth, grey.
fn film() -> Option<FilmGpu> {
    let (device, queue) = device()?;
    let mut film = FilmGpu::new(
        device,
        queue,
        Settings {
            width: WIDTH,
            height: HEIGHT,
            supersample: 1,
            look: Look {
                atmosphere: Some(AtmosphereParams::default()),
                ..Look::default()
            },
        },
    );
    let scheme = GeographicTilingScheme::default();
    let level = 3;
    let mut id = 1;
    for y in 0..(1u32 << level) {
        for x in 0..(2u32 << level) {
            let rect = scheme.tile_rect(TileCoord::new(level, u64::from(x), u64::from(y)));
            let content = fill_content(&rect, [Some(0.0); 4], 0.0);
            for mesh in &content.meshes {
                let texture = film.create_albedo(4, 4);
                film.write_rgba(&texture, &[52u8, 52, 52, 255].repeat(16));
                let normals = mesh.normals.clone().unwrap_or_default();
                let uvs = mesh.uvs.clone().unwrap_or_else(|| vec![[0.0; 2]; mesh.positions.len()]);
                film.enter(
                    TileKey { id, drape: 0 },
                    &TileMesh {
                        origin_ecef: content.local_origin_ecef.to_array(),
                        positions: &le(&mesh.positions),
                        normals: &le(&normals),
                        uvs: &le(&uvs),
                        indices: &le(&mesh.indices),
                        index_count: mesh.indices.len() as u32,
                        base_color_factor: [1.0; 4],
                    },
                    Some(texture),
                )
                .expect("enter");
                id += 1;
            }
        }
    }
    Some(film)
}

fn keys(film_tiles: u64) -> Vec<TileKey> {
    (1..=film_tiles).map(|id| TileKey { id, drape: 0 }).collect()
}

fn sunset_over_europe() -> f64 {
    let (lon, lat) = EUROPE;
    Sun::next_sunset(
        1_782_043_200.0,
        Geodetic {
            lon: lon.to_radians(),
            lat: lat.to_radians(),
            height: 0.0,
        },
    )
    .expect("the sun sets over Europe in June")
}

/// The climb: from 10 km to 36 000 km over Europe in `frames` frames,
/// looking north at the horizon, the field narrowing from 40° to 20°.
fn climb(frames: usize) -> Vec<BakedView> {
    (0..frames)
        .map(|i| {
            let t = i as f64 / (frames - 1) as f64;
            let height = 10_000.0 * 3_600f64.powf(t);
            let r = tuile_core::geo::WGS84_A;
            let dip = (r / (r + height)).acos();
            let g = Geodetic {
                lon: EUROPE.0.to_radians(),
                lat: EUROPE.1.to_radians(),
                height,
            };
            let enu = enu_frame(g);
            let forward = enu.y_axis * dip.cos() - enu.z_axis * dip.sin();
            let up = (enu.z_axis - forward * enu.z_axis.dot(forward)).normalize();
            BakedView {
                position: geodetic_to_ecef(g).to_array(),
                direction: forward.to_array(),
                up: up.to_array(),
                viewport_px: [f64::from(WIDTH), f64::from(HEIGHT)],
                fovy_rad: (40.0 * (1.0 - 0.5 * t)).to_radians(),
            }
        })
        .collect()
}

/// One frame of `view` at `instant`: the picture, read back.
fn shoot(film: &mut FilmGpu, view: &BakedView, instant: f64, tiles: &[TileKey]) -> Vec<u8> {
    let sun = Sun::at_unix_seconds(instant).direction_ecef;
    let mut light = FrameLight::new(
        ReferenceIllumination::provisional(sun, [1.0; 3], [0.5; 3]),
        Default::default(),
    );
    light.instant = Some(instant);
    film.set_light(light);
    let camera = FrameCamera::of(view, WIDTH as f32 / HEIGHT as f32);
    let encoder = film.render(&camera, tiles).expect("render");
    film.queue().submit([encoder.finish()]);
    let output = film.output().clone();
    read(film, &output)
}

fn read(film: &FilmGpu, texture: &wgpu::Texture) -> Vec<u8> {
    let row = (WIDTH * 4).div_ceil(256) * 256;
    let buffer = film.device().create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(row * HEIGHT),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = film.device().create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(HEIGHT),
            },
        },
        texture.size(),
    );
    film.queue().submit([encoder.finish()]);
    buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    film.device()
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let data = buffer.slice(..).get_mapped_range().expect("mapped").to_vec();
    let mut out = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
    for y in 0..HEIGHT as usize {
        out.extend_from_slice(&data[y * row as usize..][..(WIDTH * 4) as usize]);
    }
    out
}

fn luma(rgba: &[u8], x: u32, y: u32) -> f64 {
    let i = ((y * WIDTH + x) * 4) as usize;
    let [r, g, b] = [rgba[i], rgba[i + 1], rgba[i + 2]].map(f64::from);
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// The limb's light, as the viewer's climb measures it: over the central
/// columns, the luma over the horizon above each column's darkest, summed.
fn limb_energy(rgba: &[u8]) -> f64 {
    let columns = WIDTH * 9 / 20..WIDTH * 11 / 20;
    let mut sum = 0.0;
    for x in columns.clone() {
        let column: Vec<f64> = (0..=HEIGHT / 2).map(|y| luma(rgba, x, y)).collect();
        let darkest = column.iter().copied().fold(f64::INFINITY, f64::min);
        sum += column.iter().map(|l| l - darkest).sum::<f64>();
    }
    sum / columns.len() as f64
}

#[test]
fn a_films_limb_stays_from_ten_kilometres_to_geostationary_height() {
    let Some(mut film) = film() else {
        eprintln!("skipping GPU test: no usable adapter");
        return;
    };
    let tiles = keys(2 * 64);
    let instant = sunset_over_europe();
    let views = climb(181);
    let mut tape = std::env::var_os("TUILE_FILM_CLIMB_TRACE").map(|dir| {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).expect("trace dir");
        tuile_tape::Tape::recording(dir.join("trace.mcap")).expect("a trace")
    });
    let mut energies = Vec::new();
    for view in &views {
        let picture = shoot(&mut film, view, instant, &tiles);
        let energy = limb_energy(&picture);
        let height = ecef_to_geodetic(DVec3::from_array(view.position)).height;
        if let Some(tape) = tape.as_mut() {
            tape.push(tuile_tape::Frame {
                position: view.position,
                direction: view.direction,
                up: view.up,
                fovy: view.fovy_rad,
            });
            tape.push_json(
                "/limb",
                "tuile.Limb",
                r#"{"type":"object","additionalProperties":{"type":"number"}}"#,
                &format!(
                    "{{\"height\":{height},\"energy\":{energy},\"drawn\":{}}}",
                    u8::from(energy >= 60.0)
                ),
            );
            tape.push_image(&picture, WIDTH, HEIGHT);
        }
        energies.push((height, energy));
    }
    if let Some(tape) = tape {
        tape.finish().expect("the trace is written");
    }
    for (n, (height, energy)) in energies.iter().enumerate() {
        assert!(
            *energy >= 60.0,
            "the film's limb is gone at frame {n}, {height:.0} m: {energy:.1} levels·pixels"
        );
    }
    let worst = energies
        .windows(2)
        .map(|w| (w[1].1 / w[0].1).ln().abs())
        .fold(0.0f64, f64::max);
    eprintln!("the film's climb: worst limb step {:.1} %", 100.0 * (worst.exp() - 1.0));
    assert!(worst <= 0.25f64.ln_1p(), "the film's limb jumps by {:.1} %", 100.0 * (worst.exp() - 1.0));
}

/// Two farm segments in two films: the first renders frames 0 to 29, the
/// second begins at frame 20, as a segment rendered by another process. The
/// frames they share are the same to the byte: nothing a frame is lit by
/// was kept from the frames before it.
#[test]
fn two_segments_join_without_a_seam() {
    let (Some(mut first), Some(mut second)) = (film(), film()) else {
        eprintln!("skipping GPU test: no usable adapter");
        return;
    };
    let tiles = keys(2 * 64);
    let sunset = sunset_over_europe();
    // A climb at a clock six hundred times fast.
    let views = climb(30);
    let instant = |n: usize| sunset - 600.0 + 600.0 * n as f64 / 60.0;
    let mut shared = Vec::new();
    for (n, view) in views.iter().enumerate() {
        let picture = shoot(&mut first, view, instant(n), &tiles);
        if n >= 20 {
            shared.push(picture);
        }
    }
    for (k, view) in views.iter().enumerate().skip(20) {
        let picture = shoot(&mut second, view, instant(k), &tiles);
        assert!(
            picture == shared[k - 20],
            "frame {k} differs between the segment that drew frames 0 to {k} and the one that began at frame 20"
        );
    }
}
