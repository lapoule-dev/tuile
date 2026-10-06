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

/// A texture of noise that is the same every run.
fn noise(side: u32, seed: u32) -> Vec<u8> {
    let mut state = seed | 1;
    (0..side * side * 4)
        .map(|i| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            // Opaque: an imagery tile is.
            if i % 4 == 3 {
                255
            } else {
                (state >> 8) as u8
            }
        })
        .collect()
}

/// A drape composed on the GPU is the drape the bake composes: the same
/// layers, in the same order, at the same size, give the same texels — to
/// within one step of a byte, which is how far two machines' arithmetic may
/// part on a value that falls on a boundary.
#[test]
fn a_drape_composed_on_the_gpu_is_the_bakes() {
    use std::sync::Arc;
    use tuile_core::content::DecodedTexture;
    use tuile_core::raster::{bake_layers, ImageryCoord, ImageryLayer};
    use tuile_film_gpu::DrapeLayer;

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

    // A floor under everything, then three tiles of a finer level laid in
    // quarters, with a quarter the floor alone covers; placements that land
    // between texels, so the filtering is exercised and not only the masks.
    let side = 96u32;
    let stack: [(u32, u32, [f32; 4], [f32; 2], [f32; 2]); 4] = [
        (16, 1, [0.0, 0.0, 1.0, 1.0], [0.21, 0.37], [0.4, 0.33]),
        (32, 2, [0.0, 0.0, 0.5, 0.5], [0.0, 0.0], [2.0, 2.0]),
        (32, 3, [0.5, 0.0, 1.0, 0.5], [-1.0, 0.0], [2.0, 2.0]),
        (24, 4, [0.0, 0.5, 0.5, 1.0], [0.013, -0.97], [1.93, 1.97]),
    ];
    let base = [0.2, 0.4, 0.6, 1.0];

    let mut cpu_layers = Vec::new();
    let mut gpu_layers = Vec::new();
    for (texels, seed, coverage, translation, scale) in stack {
        let rgba8 = noise(texels, seed);
        let texture = film.create_imagery(texels, texels);
        film.write_rgba(&texture, &rgba8);
        gpu_layers.push(DrapeLayer {
            texture,
            coverage,
            translation,
            scale,
            grade: Default::default(),
        });
        cpu_layers.push(ImageryLayer {
            coord: ImageryCoord {
                level: 0,
                x: 0,
                y: 0,
            },
            texture: Arc::new(DecodedTexture {
                width: texels,
                height: texels,
                rgba8,
            }),
            coverage,
            translation,
            scale,
        });
    }
    let wanted = bake_layers(&cpu_layers, base, (side, side));

    let albedo = film.create_albedo(side, side);
    film.compose(&albedo, base, gpu_layers);
    let row = (side * 4).next_multiple_of(256);
    // The drape is recorded, and read back, in one submission.
    let mut pending = film.device().create_command_encoder(&Default::default());
    film.record_pending(&mut pending);
    film.queue().submit([pending.finish()]);
    let got = read(&film, |encoder| {
        let buffer = film.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: u64::from(row * side),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            albedo.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(side),
                },
            },
            albedo.size(),
        );
        (buffer, 0)
    });

    let (mut apart, mut worst) = (0usize, 0u8);
    for y in 0..side as usize {
        for x in 0..side as usize * 4 {
            let (g, c) = (
                got[y * row as usize + x],
                wanted.rgba8[y * side as usize * 4 + x],
            );
            let d = g.abs_diff(c);
            apart += usize::from(d > 0);
            worst = worst.max(d);
        }
    }
    let values = (side * side * 4) as usize;
    assert!(worst <= 1, "a texel is {worst} away from the bake's");
    assert!(
        apart * 100 <= values,
        "{apart} of {values} values differ from the bake's"
    );
    // And it is not the base colour everywhere: the layers were written.
    let base_only = wanted
        .rgba8
        .chunks(4)
        .filter(|p| p[..3] == [51, 102, 153])
        .count();
    assert!(
        base_only < (side * side) as usize / 2,
        "{base_only} texels are bare"
    );
}

/// A level's grade is applied in linear light to every texel of the layer,
/// as the reference does it: black point, gain, contrast, saturation.
#[test]
fn a_layer_is_composed_at_its_levels_grade() {
    use tuile_film_gpu::{DrapeLayer, LayerGrade};

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
    // Flat tiles, so that the filtering has nothing to say and what comes
    // back is the grade alone; dark, middling, and bright enough to clip.
    let side = 32u32;
    let row = (side * 4).next_multiple_of(256);
    let luma = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let grades = [
        // A gain alone, one channel left as it is.
        LayerGrade {
            gain: [2.0, 0.5, 1.0],
            ..LayerGrade::IDENTITY
        },
        // Everything at once.
        LayerGrade {
            black: [0.01, 0.004, -0.006],
            gain: [2.4, 2.1, 1.6],
            contrast: 1.2,
            pivot: 0.2,
            saturation: 1.3,
        },
    ];
    for (n, grade) in grades.into_iter().enumerate() {
        let (mut worst, mut moved) = (0u8, 0usize);
        for stored in [[12u8, 40, 7], [90, 130, 201], [230, 250, 66]] {
            let rgba8: Vec<u8> = (0..side * side)
                .flat_map(|_| [stored[0], stored[1], stored[2], 255])
                .collect();
            let texture = film.create_imagery(side, side);
            film.write_rgba(&texture, &rgba8);
            let albedo = film.create_albedo(side, side);
            film.compose(
                &albedo,
                [0.0, 0.0, 0.0, 1.0],
                vec![DrapeLayer {
                    texture,
                    coverage: [0.0, 0.0, 1.0, 1.0],
                    translation: [0.0, 0.0],
                    scale: [1.0, 1.0],
                    grade,
                }],
            );
            let mut pending = film.device().create_command_encoder(&Default::default());
            film.record_pending(&mut pending);
            film.queue().submit([pending.finish()]);
            let got = read(&film, |encoder| {
                let buffer = film.device().create_buffer(&wgpu::BufferDescriptor {
                    label: Some("readback"),
                    size: u64::from(row * side),
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                encoder.copy_texture_to_buffer(
                    albedo.as_image_copy(),
                    wgpu::TexelCopyBufferInfo {
                        buffer: &buffer,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(row),
                            rows_per_image: Some(side),
                        },
                    },
                    albedo.size(),
                );
                (buffer, 0)
            });
            // The reference, as the grade is defined.
            let mut c = [0.0f32; 3];
            for i in 0..3 {
                c[i] =
                    (eotf(f32::from(stored[i]) / 255.0) - grade.black[i]).max(0.0) * grade.gain[i];
            }
            let by = (luma(c).max(1e-5) / grade.pivot).powf(grade.contrast - 1.0);
            c = c.map(|v| v * by);
            let y = luma(c);
            c = c.map(|v| (y + (v - y) * grade.saturation).max(0.0));
            let wanted = c.map(|v| (oetf(v.min(1.0)) * 255.0).round() as u8);
            for y in 0..side as usize {
                for x in 0..side as usize {
                    let out = &got[y * row as usize + x * 4..][..3];
                    for i in 0..3 {
                        worst = worst.max(out[i].abs_diff(wanted[i]));
                        moved += usize::from(out[i] != stored[i]);
                    }
                    if n == 0 {
                        // The channel whose gain is one is the stored byte.
                        assert_eq!(out[2], stored[2]);
                    }
                }
            }
        }
        assert!(
            worst <= 1,
            "grade {n}: worst channel {worst} from the reference"
        );
        assert!(moved > (side * side) as usize, "grade {n} did nothing");
    }
}
