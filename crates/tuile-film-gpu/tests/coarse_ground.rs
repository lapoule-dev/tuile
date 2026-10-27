// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The air over coarse ground, on a real GPU.
//!
//! A zoom-out draws the Earth with tiles of the first levels, whose flat
//! triangles are a few degrees across. A flat triangle between three
//! vertices on the ground is a chord: inside it, it lies kilometres under
//! the ground its vertices are on — up to 12 km for a facet of 5°. The air
//! between the eye and the ground is read at the ground the triangle means,
//! its vertices' heights carried across it; read at the chord, the last
//! kilometres of every ray under a facet's middle were spent in sea-level
//! air, and a sea seen from orbit came out with a blotch of haze in each
//! facet and a darker lattice through its vertices (video-54's last
//! seconds).
//!
//! The measure is the one the diagnosis took on the films: the spread of
//! the picture's luma once what varies smoothly across it is taken away.
//!
//! Every test writes what it drew to `target/test-renders/` to be looked
//! at. Skipped when the machine has no adapter; `TUILE_REQUIRE_GPU=1` turns
//! the skip into a failure.

use glam::{DVec3, Vec3};
use image::{ImageBuffer, Luma};
use tuile_atmosphere::AtmosphereParams;
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};
use tuile_film::{BakedView, FrameCamera, Haze, Look, TileKey};
use tuile_film_gpu::{FilmGpu, Settings, TileMesh};

const A: f64 = tuile_core::geo::WGS84_A;

fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
    let found = pollster::block_on(async {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .ok()?;
        adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("coarse ground test"),
                ..Default::default()
            })
            .await
            .ok()
    });
    if found.is_none() {
        assert!(
            std::env::var("TUILE_REQUIRE_GPU").is_err(),
            "TUILE_REQUIRE_GPU is set and there is no usable adapter"
        );
        eprintln!("SKIP: no adapter");
    }
    found
}

/// The adapter the tests run on, as `(backend, name)`: the same default
/// adapter [`device`] asks for.
fn adapter() -> Option<(wgpu::Backend, String)> {
    pollster::block_on(async {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .ok()?;
        let info = adapter.get_info();
        Some((info.backend, info.name))
    })
}

fn le<T: Copy + bytemuck::Pod>(v: &[T]) -> Vec<u8> {
    bytemuck::cast_slice(v).to_vec()
}

/// A picture's size.
#[derive(Clone, Copy)]
struct Size {
    width: u32,
    height: u32,
}

/// The pictures of a zoom-out's last frames.
const ORBIT: Size = Size {
    width: 256,
    height: 192,
};

/// The near ground's: small, as the frozen pictures they are held to.
const NEAR: Size = Size {
    width: 128,
    height: 96,
};

fn film(look: Look, size: Size) -> Option<FilmGpu> {
    let (device, queue) = device()?;
    Some(FilmGpu::new(
        device,
        queue,
        Settings {
            width: size.width,
            height: size.height,
            supersample: 1,
            look,
        },
    ))
}

/// A tile of the ellipsoid about null island, `side` degrees a side, cut
/// into `cells` × `cells` quads of two flat triangles each; its vertices on
/// the ground at `height` metres, its normals the ellipsoid's, its texture
/// one colour.
fn coarse_tile(film: &mut FilmGpu, side: f64, cells: u32, height: f64, rgb: [u8; 3]) -> TileKey {
    let origin = geodetic_to_ecef(Geodetic {
        lon: 0.0,
        lat: 0.0,
        height,
    });
    let (mut p, mut n, mut uv) = (Vec::new(), Vec::new(), Vec::new());
    for row in 0..=cells {
        for column in 0..=cells {
            let (u, v) = (
                f64::from(column) / f64::from(cells),
                f64::from(row) / f64::from(cells),
            );
            let g = Geodetic {
                lon: (side * (u - 0.5)).to_radians(),
                lat: (side * (v - 0.5)).to_radians(),
                height,
            };
            p.extend((geodetic_to_ecef(g) - origin).as_vec3().to_array());
            n.extend(enu_frame(g).z_axis.as_vec3().to_array());
            uv.extend([u as f32, 1.0 - v as f32]);
        }
    }
    let texture = film.create_albedo(4, 4);
    film.write_rgba(&texture, &[rgb[0], rgb[1], rgb[2], 255].repeat(16));
    let key = TileKey { id: 7, drape: 0 };
    let i = grid_triangles(cells);
    film.enter(
        key,
        &TileMesh {
            origin_ecef: origin.to_array(),
            positions: &le(&p),
            normals: &le(&n),
            uvs: &le(&uv),
            indices: &le(&i),
            index_count: i.len() as u32,
            base_color_factor: [1.0; 4],
        },
        Some(texture),
    )
    .expect("enter");
    key
}

/// The two triangles of each quad of a grid of `cells` × `cells`, its
/// vertices row by row from the south-west.
fn grid_triangles(cells: u32) -> Vec<u32> {
    let at = |row: u32, column: u32| row * (cells + 1) + column;
    let mut i = Vec::new();
    for row in 0..cells {
        for column in 0..cells {
            let (sw, se, ne, nw) = (
                at(row, column),
                at(row, column + 1),
                at(row + 1, column + 1),
                at(row + 1, column),
            );
            i.extend([sw, se, ne, sw, ne, nw]);
        }
    }
    i
}

/// A hill 2 500 m high on ground meshed a kilometre apart, around null
/// island: fine relief, whose flat triangles lie at most four centimetres
/// under the ground their vertices mean. Its own facets light it (no
/// normals), and its texture is a ramp, so that a pixel moved shows.
fn hill(film: &mut FilmGpu) -> TileKey {
    let cells = 40u32;
    let step = 0.009f64; // degrees: a kilometre
    let origin = geodetic_to_ecef(Geodetic {
        lon: 0.0,
        lat: 0.0,
        height: 0.0,
    });
    let (mut p, mut uv) = (Vec::new(), Vec::new());
    for row in 0..=cells {
        for column in 0..=cells {
            let east = f64::from(column) - f64::from(cells) / 2.0;
            let north = f64::from(row) - f64::from(cells) / 2.0;
            let height = 2_500.0 * (-(east * east + north * north) / 64.0).exp();
            let g = Geodetic {
                lon: (east * step).to_radians(),
                lat: (north * step).to_radians(),
                height,
            };
            p.extend((geodetic_to_ecef(g) - origin).as_vec3().to_array());
            uv.extend([
                column as f32 / cells as f32,
                1.0 - row as f32 / cells as f32,
            ]);
        }
    }
    let mut texels = Vec::new();
    for y in 0..16u8 {
        for x in 0..16u8 {
            texels.extend([40 + 8 * x, 60 + 6 * y, 30, 255]);
        }
    }
    let texture = film.create_albedo(16, 16);
    film.write_rgba(&texture, &texels);
    let key = TileKey { id: 8, drape: 0 };
    let i = grid_triangles(cells);
    film.enter(
        key,
        &TileMesh {
            origin_ecef: origin.to_array(),
            positions: &le(&p),
            normals: &[],
            uvs: &le(&uv),
            indices: &le(&i),
            index_count: i.len() as u32,
            base_color_factor: [1.0; 4],
        },
        Some(texture),
    )
    .expect("enter");
    key
}

/// Straight down on null island from `height` metres, north up.
fn overhead(height: f64, fovy: f64, size: Size) -> BakedView {
    BakedView {
        position: [A + height, 0.0, 0.0],
        direction: [-1.0, 0.0, 0.0],
        up: [0.0, 0.0, 1.0],
        viewport_px: [f64::from(size.width), f64::from(size.height)],
        fovy_rad: fovy,
    }
}

/// 5 km up, 30 km south of the hill, looking north and 8° down at it.
fn towards_the_hill(size: Size) -> BakedView {
    let down = 8f64.to_radians();
    let eye = geodetic_to_ecef(Geodetic {
        lon: 0.0,
        lat: (-0.27f64).to_radians(),
        height: 5_000.0,
    });
    BakedView {
        position: eye.to_array(),
        direction: [-down.sin(), 0.0, down.cos()],
        up: [down.cos(), 0.0, down.sin()],
        viewport_px: [f64::from(size.width), f64::from(size.height)],
        fovy_rad: 0.7,
    }
}

fn draw(film: &mut FilmGpu, view: &BakedView, selection: &[TileKey]) -> Vec<u8> {
    let aspect = (view.viewport_px[0] / view.viewport_px[1]) as f32;
    let camera = FrameCamera::of(view, aspect);
    let encoder = film.render(&camera, selection).expect("render");
    film.queue().submit([encoder.finish()]);
    let output = film.output().clone();
    read_texture(film, &output)
}

fn read_texture(film: &FilmGpu, texture: &wgpu::Texture) -> Vec<u8> {
    let size = texture.size();
    let row = size.width * 4;
    let padded = row.div_ceil(256) * 256;
    let buffer = film.device().create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(padded * size.height),
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
                bytes_per_row: Some(padded),
                rows_per_image: Some(size.height),
            },
        },
        size,
    );
    film.queue().submit([encoder.finish()]);
    let slice = buffer.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    film.device()
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let bytes = slice.get_mapped_range().expect("map range").to_vec();
    bytes
        .chunks(padded as usize)
        .flat_map(|r| r[..row as usize].to_vec())
        .collect()
}

fn picture(rgba: &[u8], size: Size) -> image::RgbaImage {
    image::RgbaImage::from_raw(size.width, size.height, rgba.to_vec()).expect("image")
}

fn save(picture: &image::RgbaImage, name: &str) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-renders");
    std::fs::create_dir_all(&dir).expect("create test-renders");
    let path = dir.join(name);
    picture.save(&path).expect("save png");
    eprintln!("written to {}", path.display());
}

/// The facets' pattern: the standard deviation of the picture's luma less
/// its Gaussian blur of `sigma` pixels — what varies faster than `sigma`
/// across the frame — over the box `2·sigma` in from every edge, where the
/// blur reads no edge; and the box's mean luma, to know it is ground. In
/// 8-bit levels, Rec. 709's weights on the stored (encoded) values, as the
/// diagnosis measured the films.
fn pattern(picture: &image::RgbaImage, sigma: f32) -> (f64, f64) {
    let (width, height) = picture.dimensions();
    let luma: ImageBuffer<Luma<f32>, Vec<f32>> = ImageBuffer::from_fn(width, height, |x, y| {
        let [r, g, b, _] = picture.get_pixel(x, y).0.map(f32::from);
        Luma([0.2126 * r + 0.7152 * g + 0.0722 * b])
    });
    let smooth = image::imageops::blur(&luma, sigma);
    let inner = (2.0 * sigma).ceil() as u32;
    let (mut n, mut sum, mut sum_sq, mut level) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for y in inner..height - inner {
        for x in inner..width - inner {
            let at = luma.get_pixel(x, y).0[0];
            let fine = f64::from(at - smooth.get_pixel(x, y).0[0]);
            n += 1.0;
            sum += fine;
            sum_sq += fine * fine;
            level += f64::from(at);
        }
    }
    let mean = sum / n;
    ((sum_sq / n - mean * mean).max(0.0).sqrt(), level / n)
}

/// Up at null island is +X; north +Z. A sun `elevation` degrees up, due
/// south: towards the frame's bottom.
fn sun_at(elevation: f64) -> Vec3 {
    let e = elevation.to_radians();
    DVec3::new(e.sin(), 0.0, -e.cos()).as_vec3()
}

/// A dark sea, as the imagery has one.
const SEA: [u8; 3] = [14, 36, 52];

/// Blur for the measure: well under a facet's side in the pictures below
/// (some 70 pixels), well over the steps of the picture's own gradients.
const SIGMA: f32 = 12.0;

/// The physical atmosphere over a sea of facets of 5°, seen straight down
/// from 1 500 km — the last frames of a zoom-out. Inside, the facets lie
/// up to 12 km under the ground their vertices are on; the air must not
/// see it.
///
/// Measured on an Apple M2 (Metal): 1.67 levels with the air read at the
/// chord, 0.19 read at the ground the triangles mean — the quantisation of
/// a smooth picture. The film's own (video-54, frame 5400, its ocean, as the
/// diagnosis measured it): 1.62.
#[test]
fn a_sea_of_coarse_facets_seen_from_orbit_through_the_air_shows_none_of_them() {
    let look = Look {
        to_sun: sun_at(40.0),
        atmosphere: Some(AtmosphereParams::default()),
        ..Look::default()
    };
    let Some(mut film) = film(look, ORBIT) else {
        return;
    };
    let key = coarse_tile(&mut film, 20.0, 4, 0.0, SEA);
    let drawn = picture(
        &draw(&mut film, &overhead(1_500_000.0, 0.9, ORBIT), &[key]),
        ORBIT,
    );
    save(&drawn, "film-coarse-sea-from-orbit.png");
    let (spread, level) = pattern(&drawn, SIGMA);
    eprintln!("pattern {spread:.3} levels over a mean luma of {level:.1}");
    assert!(level > 20.0, "the sea is drawn, lit: mean luma {level}");
    assert!(
        spread < 0.6,
        "the facets show through the air: a pattern of {spread:.3} levels"
    );
}

/// The look's closed-form haze ([`Look::haze`], when there is no physical
/// atmosphere) over a sea of facets of 1°, from 80 km: its air thins by e
/// every 1 200 m, and a chord a few hundred metres under the ground was
/// read in air a third as dense again.
///
/// Measured on an Apple M2 (Metal): 1.80 levels read at the chord, 0.47 at
/// the ground the triangles mean — the quantisation of the steep gradient
/// of a wide lens.
#[test]
fn a_sea_of_coarse_facets_under_the_looks_haze_shows_none_of_them() {
    let look = Look {
        to_sun: sun_at(40.0),
        haze: Some(Haze {
            density: 2.0e-4,
            ..Haze::default()
        }),
        ..Look::default()
    };
    let Some(mut film) = film(look, ORBIT) else {
        return;
    };
    let key = coarse_tile(&mut film, 4.0, 4, 0.0, SEA);
    let drawn = picture(
        &draw(&mut film, &overhead(80_000.0, 1.9, ORBIT), &[key]),
        ORBIT,
    );
    save(&drawn, "film-coarse-sea-haze.png");
    let (spread, level) = pattern(&drawn, SIGMA);
    eprintln!("pattern {spread:.3} levels over a mean luma of {level:.1}");
    assert!(level > 20.0, "the sea is drawn, lit: mean luma {level}");
    assert!(
        spread < 0.8,
        "the facets show through the haze: a pattern of {spread:.3} levels"
    );
}

/// Near ground and relief are drawn as they were before the air read the
/// ground the triangles mean: the hill in a clear noon, under a low sun,
/// in twilight through the physical atmosphere, and through the look's
/// haze — each held to its picture drawn by main before the change
/// (7b0ac83), frozen in `tests/frozen/`.
///
/// Within a level, on at most one pixel in two hundred. The ground the
/// hill's triangles mean is at most four centimetres over the triangles,
/// and the air over four centimetres differs by a few hundred-thousandths
/// of itself — a thousandth of a level: only a value at the edge of a level
/// may round the other way, by one. Measured: 1 level, on 1 to 14 pixels
/// of 12 288 (0.01 to 0.11 %). Relief flattened under the air (the air
/// read on the sphere, under the hill) moved a third of the noon's pixels,
/// by up to 30 levels.
///
/// Frozen on an Apple M2 through Metal: a picture holds for the adapter it
/// was drawn on only — on any other, the differences are printed and the
/// comparison is skipped, saying so.
#[test]
fn near_ground_and_relief_are_drawn_as_they_were() {
    let looks = [
        ("noon", sun_at(60.0), None),
        ("low-sun", sun_at(6.0), None),
        ("twilight", sun_at(-3.0), None),
        (
            "haze",
            sun_at(30.0),
            Some(Haze {
                density: 6.0e-5,
                ..Haze::default()
            }),
        ),
    ];
    // All drawn and written first, to be looked at whatever the comparison.
    let mut pictures = Vec::new();
    for (what, to_sun, haze) in looks {
        let look = Look {
            to_sun,
            haze,
            atmosphere: haze.is_none().then(AtmosphereParams::default),
            ..Look::default()
        };
        let Some(mut film) = film(look, NEAR) else {
            return;
        };
        let key = hill(&mut film);
        let drawn = picture(&draw(&mut film, &towards_the_hill(NEAR), &[key]), NEAR);
        save(&drawn, &format!("film-hill-{what}.png"));
        let (_, level) = pattern(&drawn, 4.0);
        assert!(level > 20.0, "{what}: the hill is drawn, mean luma {level}");
        pictures.push((what, drawn));
    }
    let (backend, name) = adapter().unwrap_or((wgpu::Backend::Noop, String::new()));
    let frozen_here = backend == wgpu::Backend::Metal && name.starts_with("Apple");
    // Every look compared before any fails: what moved is read in one run.
    let mut failed = Vec::new();
    for (what, drawn) in pictures {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/frozen/hill-{what}.png"));
        let frozen = image::open(&path).expect("frozen picture").to_rgba8();
        let (mut moved, mut most) = (0usize, 0u8);
        for (now, then) in drawn.pixels().zip(frozen.pixels()) {
            let apart = (0..3)
                .map(|c| now.0[c].abs_diff(then.0[c]))
                .max()
                .unwrap_or(0);
            moved += usize::from(apart > 0);
            most = most.max(apart);
        }
        let share = moved as f64 / f64::from(NEAR.width * NEAR.height);
        eprintln!(
            "{what}: {moved} pixels moved ({:.3} %), by at most {most} on {backend:?} {name}",
            100.0 * share
        );
        if frozen_here && !(most <= 1 && share <= 0.005) {
            failed.push(format!(
                "{what}: {moved} pixels moved ({:.3} %), by up to {most} levels",
                100.0 * share
            ));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("; "));
    if !frozen_here {
        eprintln!("SKIP: the near ground is frozen on an Apple GPU, not on {backend:?} {name}");
    }
}
