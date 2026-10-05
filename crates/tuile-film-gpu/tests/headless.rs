// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The film pipeline on a real GPU, against numbers worked out by hand.
//!
//! A flat square of one colour, seen face-on with the sun straight behind the
//! camera: every pixel on it must come out as the look's formula says, every
//! pixel off it as the sky. Skipped when the machine has no adapter.

use glam::Vec3;
use tuile_film::{BakedView, FrameCamera, Imagery, Look, TileKey};
use tuile_film_gpu::{FilmGpu, Settings, TileMesh};

/// The WGS84 equatorial radius: the square sits on the ground at null island.
const A: f64 = 6_378_137.0;

fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
    pollster::block_on(async {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .ok()?;
        adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("film test"),
                ..Default::default()
            })
            .await
            .ok()
    })
}

fn le<T: Copy + bytemuck::Pod>(v: &[T]) -> Vec<u8> {
    bytemuck::cast_slice(v).to_vec()
}

/// A square in the plane x = 0 of its tile, 400 m a side, facing +X.
fn square() -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
    let p = [
        0.0f32, -200.0, -200.0, 0.0, 200.0, -200.0, 0.0, 200.0, 200.0, 0.0, -200.0, 200.0,
    ];
    let n = [1.0f32, 0.0, 0.0].repeat(4);
    let uv = [0.0f32, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0];
    let i = [0u32, 1, 2, 0, 2, 3];
    (le(&p), le(&n), le(&uv), le(&i))
}

fn view() -> BakedView {
    BakedView {
        position: [A + 1000.0, 0.0, 0.0],
        direction: [-1.0, 0.0, 0.0],
        up: [0.0, 0.0, 1.0],
        viewport_px: [64.0, 48.0],
        fovy_rad: 0.8,
    }
}

fn read(
    film: &FilmGpu,
    buffer_of: impl FnOnce(&mut wgpu::CommandEncoder) -> (wgpu::Buffer, u64),
) -> Vec<u8> {
    let mut encoder = film.device().create_command_encoder(&Default::default());
    let (buffer, _) = buffer_of(&mut encoder);
    film.queue().submit([encoder.finish()]);
    let slice = buffer.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    film.device()
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    slice.get_mapped_range().to_vec()
}

fn oetf(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.003_130_8 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

fn eotf(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

struct Rendered {
    rgba: Vec<u8>,
    i420: Vec<u8>,
    width: u32,
}

fn render(look: Look, albedo_srgb: [u8; 4], supersample: u32) -> Option<Rendered> {
    render_squares(look, &[(0.0, albedo_srgb)], supersample)
}

/// Squares of one colour each, centred `y` metres east of the nadir.
fn render_squares(look: Look, squares: &[(f64, [u8; 4])], supersample: u32) -> Option<Rendered> {
    let (device, queue) = device()?;
    let (width, height) = (64, 48);
    let mut film = FilmGpu::new(
        device,
        queue,
        Settings {
            width,
            height,
            supersample,
            look,
        },
    );
    let (p, n, uv, i) = square();
    let mut keys = Vec::new();
    for (at, (y, albedo_srgb)) in squares.iter().enumerate() {
        let texture = film.create_albedo(4, 4);
        film.write_rgba(&texture, &albedo_srgb.repeat(16));
        let key = TileKey {
            id: at as u64 + 1,
            drape: 0,
        };
        film.enter(
            key,
            &TileMesh {
                origin_ecef: [A, *y, 0.0],
                positions: &p,
                normals: &n,
                uvs: &uv,
                indices: &i,
                index_count: 6,
                base_color_factor: [1.0; 4],
            },
            Some(texture),
        )
        .expect("enter");
        keys.push(key);
    }
    let camera = FrameCamera::of(&view(), width as f32 / height as f32);
    let mut encoder = film.render(&camera, &keys).expect("render");
    film.encode_i420(&mut encoder).expect("i420");
    film.queue().submit([encoder.finish()]);

    let padded = (width * 4).div_ceil(256) * 256;
    let rgba = read(&film, |e| {
        let buffer = film.device().create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(padded * height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        e.copy_texture_to_buffer(
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
        (buffer, 0)
    });
    let rgba = rgba
        .chunks(padded as usize)
        .flat_map(|row| row[..(width * 4) as usize].to_vec())
        .collect();
    let i420 = read(&film, |e| {
        let size = film.i420_planes().size();
        let buffer = film.device().create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        e.copy_buffer_to_buffer(film.i420_planes(), 0, &buffer, 0, size);
        (buffer, size)
    });
    Some(Rendered { rgba, i420, width })
}

fn pixel(r: &Rendered, x: u32, y: u32) -> [u8; 3] {
    let at = ((y * r.width + x) * 4) as usize;
    [r.rgba[at], r.rgba[at + 1], r.rgba[at + 2]]
}

fn close(found: [u8; 3], wanted: [f32; 3]) -> bool {
    found
        .iter()
        .zip(wanted)
        .all(|(f, w)| (f32::from(*f) - w * 255.0).abs() <= 2.0)
}

/// The sun straight down the camera's line of sight, so cos θ = 1.
fn look() -> Look {
    Look {
        to_sun: Vec3::X,
        imagery: Imagery::Decoded,
        ..Look::default()
    }
}

#[test]
fn the_square_is_shaded_by_the_look_and_the_rest_is_sky() {
    let Some(r) = render(look(), [90, 60, 30, 255], 2) else {
        eprintln!("no GPU adapter: skipped");
        return;
    };
    let look = look();
    let expected = |albedo: [u8; 3]| {
        let mut out = [0.0; 3];
        for c in 0..3 {
            let a = eotf(f32::from(albedo[c]) / 255.0);
            let radiance = a * (look.world[c] + look.sun[c]);
            out[c] = oetf(radiance * look.exposure_scale());
        }
        out
    };
    let centre = pixel(&r, 32, 24);
    let wanted = expected([90, 60, 30]);
    assert!(
        close(centre, wanted),
        "centre {centre:?}, wanted {:?}",
        wanted.map(|c| c * 255.0)
    );

    let sky = look.world * look.exposure_scale();
    let sky = [oetf(sky.x), oetf(sky.y), oetf(sky.z)];
    for (x, y) in [(0, 0), (63, 0), (0, 47), (63, 47)] {
        let corner = pixel(&r, x, y);
        assert!(
            close(corner, sky),
            "corner ({x},{y}) {corner:?} is not sky {:?}",
            sky.map(|c| c * 255.0)
        );
    }
}

#[test]
fn i420_luma_matches_the_picture() {
    let Some(r) = render(look(), [90, 60, 30, 255], 1) else {
        eprintln!("no GPU adapter: skipped");
        return;
    };
    let c = pixel(&r, 32, 24).map(|v| f32::from(v) / 255.0);
    let y = 16.0 + 219.0 * (0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]);
    let found = r.i420[(24 * r.width + 32) as usize];
    assert!((f32::from(found) - y).abs() <= 1.0, "Y {found}, wanted {y}");
    assert_eq!(r.i420.len(), (64 * 48 * 3 / 2) as usize);
}

/// Two tiles, two colours: each tile's pixels must be shaded with its own
/// texture, which only holds if binning gives each tile its own run of the
/// pixel list.
#[test]
fn each_tile_resolves_its_own_pixels() {
    let squares = [(-250.0, [220, 30, 30, 255]), (250.0, [30, 30, 220, 255])];
    let Some(r) = render_squares(look(), &squares, 1) else {
        eprintln!("no GPU adapter: skipped");
        return;
    };
    // Camera right is +Y: the west square is on the left.
    let left = pixel(&r, 18, 24);
    let right = pixel(&r, 46, 24);
    assert!(left[0] > 2 * left[2], "left {left:?} should be red");
    assert!(right[2] > 2 * right[0], "right {right:?} should be blue");
}

/// Lit as stored, the texture's sRGB values are taken for linear ones: the
/// square comes out at its stored value times the light, with no curve
/// undone first.
#[test]
fn imagery_lit_as_stored_skips_the_srgb_decode() {
    let look = Look {
        imagery: Imagery::AsStored,
        ..look()
    };
    let Some(r) = render(look, [30, 15, 5, 255], 1) else {
        eprintln!("no GPU adapter: skipped");
        return;
    };
    let mut wanted = [0.0; 3];
    for (c, v) in [30u8, 15, 5].into_iter().enumerate() {
        let radiance = f32::from(v) / 255.0 * (look.world[c] + look.sun[c]);
        wanted[c] = oetf(radiance * look.exposure_scale());
    }
    let centre = pixel(&r, 32, 24);
    assert!(
        close(centre, wanted),
        "centre {centre:?}, wanted {:?}",
        wanted.map(|c| c * 255.0)
    );
}
