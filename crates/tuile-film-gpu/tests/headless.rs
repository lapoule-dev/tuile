// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The film pipeline on a real GPU, against numbers worked out by hand.
//!
//! A flat square of one colour, seen face-on with the sun straight behind the
//! camera: every pixel on it must come out as the look's formula says, every
//! pixel off it as the sky. Skipped when the machine has no adapter.

use glam::Vec3;
use tuile_film::{BakedView, FrameCamera, Imagery, Look, OverlayDepth, OverlayMesh, TileKey};
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

    let rgba = picture(&film);
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

/// The picture the last frame submitted left, tightly packed.
fn picture(film: &FilmGpu) -> Vec<u8> {
    let (width, height) = (film.settings().width, film.settings().height);
    let padded = (width * 4).div_ceil(256) * 256;
    let rgba = read(film, |e| {
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
    rgba.chunks(padded as usize)
        .flat_map(|row| row[..(width * 4) as usize].to_vec())
        .collect()
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

/// The sun straight down the camera's line of sight, so cos θ = 1, and
/// the picture left as the lights make it: its contrast and saturation are
/// tested apart.
fn look() -> Look {
    Look {
        to_sun: Vec3::X,
        imagery: Imagery::Decoded,
        contrast: 1.0,
        saturation: 1.0,
        ..Look::default()
    }
}

/// The picture's own contrast and saturation come after the lights and the
/// exposure: luminance to a power about middle grey, then what is not
/// luminance scaled.
#[test]
fn the_pictures_contrast_and_saturation_come_after_the_exposure() {
    let graded = Look {
        contrast: 1.5,
        saturation: 0.7,
        ..look()
    };
    let Some(r) = render(graded, [90, 60, 30, 255], 2) else {
        eprintln!("no GPU adapter: skipped");
        return;
    };
    let mut lit = [0.0f32; 3];
    for c in 0..3 {
        let a = eotf(f32::from([90u8, 60, 30][c]) / 255.0);
        lit[c] = a * (graded.world[c] + graded.sun[c]) * graded.exposure_scale();
    }
    let luma = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let by = (luma(lit) / 0.18).powf(graded.contrast - 1.0);
    let lit = lit.map(|v| v * by);
    let y = luma(lit);
    let wanted = lit.map(|v| oetf(y + (v - y) * graded.saturation));
    let centre = pixel(&r, 32, 24);
    assert!(
        close(centre, wanted),
        "centre {centre:?}, wanted {:?}",
        wanted.map(|c| c * 255.0)
    );
    // And it is not what the lights alone give.
    let plain = render(look(), [90, 60, 30, 255], 2).expect("an adapter");
    assert_ne!(pixel(&plain, 32, 24), centre);
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

    // The sky is brighter than white here, and is rolled off like any
    // highlight: its luminance bent towards white, its colour kept.
    let sky = look.world * look.exposure_scale();
    let lit = 0.2126 * sky.x + 0.7152 * sky.y + 0.0722 * sky.z;
    let knee = 0.5;
    let bent = if lit > knee {
        (knee + (1.0 - knee) * ((lit - knee) / (1.0 - knee)).tanh()) / lit
    } else {
        1.0
    };
    let sky = [oetf(sky.x * bent), oetf(sky.y * bent), oetf(sky.z * bent)];
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
            field: None,
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
                    field: None,
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

/// A field is blended across the imagery tile from its four corners: a
/// texel is given what its place in the tile says, and along an edge only
/// the two corners on it count.
#[test]
fn a_layer_is_composed_through_a_field_carried_by_its_corners() {
    use tuile_film_gpu::{DrapeLayer, LayerCorner, LayerField};

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
    let side = 32u32;
    let row = (side * 4).next_multiple_of(256);
    let linear = |v: f32| {
        let v = v / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    let stored = |v: f32| {
        let v = v.clamp(0.0, 1.0);
        let s = if v <= 0.003_130_8 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (s * 255.0).round()
    };
    // A flat grey tile. The field: nothing on the left, a stop of gain on
    // the right; and on the bottom corners, a transfer curve that lifts
    // the light this grey has by another stop.
    let grey = 60u8;
    let came = linear(f32::from(grey)).log2();
    let mut lifted = [[0.0f32; 8]; 3];
    for curve in &mut lifted {
        // The same lift at every point: whatever the light, a stop.
        *curve = [1.0; 8];
    }
    assert!(
        (-8.0..=-1.0).contains(&came),
        "the grey is within the curve"
    );
    let corner = |gain: f32, curve: [[f32; 8]; 3]| LayerCorner {
        gain_stops: [gain; 3],
        curve,
        ..LayerCorner::IDENTITY
    };
    let field = LayerField {
        corners: [
            corner(0.0, [[0.0; 8]; 3]),
            corner(1.0, [[0.0; 8]; 3]),
            corner(0.0, lifted),
            corner(1.0, lifted),
        ],
        matrices: None,
    };
    let rgba8: Vec<u8> = (0..side * side)
        .flat_map(|_| [grey, grey, grey, 255])
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
            grade: Default::default(),
            field: Some(field),
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
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width: side,
                height: side,
                depth_or_array_layers: 1,
            },
        );
        (buffer, 0)
    });
    let at = |x: u32, y: u32| f32::from(got[(y * row + x * 4) as usize]);
    // Where a texel lies in the tile, as the composition places it.
    let place = |i: u32| {
        let uv = (i as f32 + 0.5) / side as f32;
        (uv * (side - 1) as f32).floor().max(0.0) / (side - 1) as f32
            + ((uv * (side - 1) as f32).fract()) / (side - 1) as f32
    };
    let mut worst = 0.0f32;
    for (x, y) in [(0, 0), (31, 0), (0, 31), (31, 31), (16, 16), (7, 25)] {
        let (u, v) = (place(x), place(y));
        // A stop of gain across, a stop of curve down.
        let expected = stored(linear(f32::from(grey)) * (u + v).exp2());
        worst = worst.max((at(x, y) - expected).abs());
    }
    assert!(worst <= 1.0, "{worst} bytes from the field blended by hand");
    // Not a flat tile any more: the corners differ, and so do the texels.
    // Two stops at the far corner, one at each of the near ones.
    assert!(
        at(31, 31) > at(0, 0) + 50.0,
        "{} against {}",
        at(31, 31),
        at(0, 0)
    );
    assert!(at(31, 0) > at(0, 0) + 20.0 && at(0, 31) > at(0, 0) + 20.0);
    // Along the left edge only the two left corners count: no gain there,
    // whatever the right ones hold.
    assert!((at(0, 0) - f32::from(grey)).abs() <= 2.0, "{}", at(0, 0));
}

#[test]
fn a_layer_is_composed_through_matrices_carried_by_its_corners() {
    use tuile_film_gpu::{DrapeLayer, LayerCorner, LayerField};

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
    let side = 32u32;
    let row = (side * 4).next_multiple_of(256);
    let linear = |v: f32| {
        let v = v / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    let stored = |v: f32| {
        let v = v.clamp(0.0, 1.0);
        let s = if v <= 0.003_130_8 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (s * 255.0).round()
    };
    // A tile of one colour. On its left corners nothing is done; on its
    // right ones red is given the blue and blue the red with a little
    // added — a matrix no gain a channel could be.
    let colour = [60u8, 120, 200];
    let same = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0f32],
    ];
    let turned = [
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [1.0, 0.0, 0.0, 0.05f32],
    ];
    let field = LayerField {
        corners: [LayerCorner::IDENTITY; 4],
        matrices: Some([same, turned, same, turned]),
    };
    let rgba8: Vec<u8> = (0..side * side)
        .flat_map(|_| [colour[0], colour[1], colour[2], 255])
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
            grade: Default::default(),
            field: Some(field),
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
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width: side,
                height: side,
                depth_or_array_layers: 1,
            },
        );
        (buffer, 0)
    });
    let at = |x: u32, y: u32, c: u32| f32::from(got[(y * row + x * 4 + c) as usize]);
    let place = |i: u32| {
        let uv = (i as f32 + 0.5) / side as f32;
        (uv * (side - 1) as f32).floor().max(0.0) / (side - 1) as f32
            + ((uv * (side - 1) as f32).fract()) / (side - 1) as f32
    };
    let lit = colour.map(|v| linear(f32::from(v)));
    let mut worst = 0.0f32;
    for (x, y) in [(0, 0), (31, 0), (0, 31), (31, 31), (16, 16), (7, 25)] {
        let u = place(x);
        for c in 0..3usize {
            // The two matrices blended across, then applied in linear light.
            let made: f32 = (0..3)
                .map(|i| (same[c][i] * (1.0 - u) + turned[c][i] * u) * lit[i])
                .sum::<f32>()
                + same[c][3] * (1.0 - u)
                + turned[c][3] * u;
            worst = worst.max((at(x, y, c as u32) - stored(made)).abs());
        }
    }
    assert!(
        worst <= 1.0,
        "{worst} bytes from the matrices blended by hand"
    );
    // On the left the colour nearly as it was — the first texel's middle
    // is a sixty-fourth of the way across — and on the right red and blue
    // turned.
    assert!((at(0, 5, 0) - 60.0).abs() <= 8.0 && (at(0, 5, 2) - 200.0).abs() <= 8.0);
    assert!(
        at(31, 5, 0) > 190.0 && at(31, 5, 2) < 110.0,
        "{} {}",
        at(31, 5, 0),
        at(31, 5, 2)
    );
}

/// A rectangle facing the camera, `height` metres over the ground at null
/// island: centred `at` metres east and north of the nadir, its half
/// extents `half`. The camera hangs at 1000 m and looks straight down, east
/// to its right and north up the picture.
#[derive(Clone, Copy)]
struct Panel {
    height: f64,
    at: [f64; 2],
    half: [f32; 2],
}

impl Panel {
    fn origin(&self) -> [f64; 3] {
        [A + self.height, self.at[0], self.at[1]]
    }

    fn corners(&self) -> [[f32; 3]; 4] {
        let [y, z] = self.half;
        [[0.0, -y, -z], [0.0, y, -z], [0.0, y, z], [0.0, -y, z]]
    }

    /// As an overlay of one colour, display-linear and premultiplied.
    fn overlay(&self, color: [f32; 4], depth: OverlayDepth) -> OverlayMesh {
        OverlayMesh {
            origin_ecef: self.origin(),
            positions: self.corners().to_vec(),
            colors: vec![color; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            depth,
        }
    }
}

/// The ground under the camera, and a ridge: a wall halfway up to it, a
/// band down the middle of the picture (columns 29 to 35).
const GROUND: Panel = Panel {
    height: 0.0,
    at: [0.0, 0.0],
    half: [200.0, 200.0],
};
const RIDGE: Panel = Panel {
    height: 500.0,
    at: [0.0, 0.0],
    half: [30.0, 200.0],
};
const GROUND_SRGB: [u8; 4] = [90, 60, 30, 255];
const RIDGE_SRGB: [u8; 4] = [30, 30, 220, 255];

/// A band across the whole picture, east to west, 250 m up: between the
/// ground and the ridge's top, so behind the ridge. `north` puts it on a
/// row: 0 is rows 23 and 24.
fn band_behind(north: f64) -> Panel {
    Panel {
        height: 250.0,
        at: [0.0, north],
        half: [400.0, 20.0],
    }
}

/// The ground and the ridge, then one frame after the other on the same
/// renderer, each with its overlays: the picture of each.
fn frames(look: Look, supersample: u32, frames: &[&[OverlayMesh]]) -> Option<Vec<Rendered>> {
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
    let mut keys = Vec::new();
    for (at, (panel, srgb)) in [(GROUND, GROUND_SRGB), (RIDGE, RIDGE_SRGB)]
        .into_iter()
        .enumerate()
    {
        let texture = film.create_albedo(4, 4);
        film.write_rgba(&texture, &srgb.repeat(16));
        let key = TileKey {
            id: at as u64 + 1,
            drape: 0,
        };
        film.enter(
            key,
            &TileMesh {
                origin_ecef: panel.origin(),
                positions: &le(&panel.corners()),
                normals: &le(&[1.0f32, 0.0, 0.0].repeat(4)),
                uvs: &le(&[0.0f32, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0]),
                indices: &le(&[0u32, 1, 2, 0, 2, 3]),
                index_count: 6,
                base_color_factor: [1.0; 4],
            },
            Some(texture),
        )
        .expect("enter");
        keys.push(key);
    }
    let camera = FrameCamera::of(&view(), width as f32 / height as f32);
    let mut out = Vec::new();
    for overlays in frames {
        let encoder = film.render_with(&camera, &keys, overlays).expect("render");
        film.queue().submit([encoder.finish()]);
        out.push(Rendered {
            rgba: picture(&film),
            i420: Vec::new(),
            width,
        });
    }
    Some(out)
}

const MAGENTA: [f32; 4] = [1.0, 0.0, 1.0, 1.0];
const CYAN: [f32; 4] = [0.0, 1.0, 1.0, 1.0];
const YELLOW: [f32; 4] = [1.0, 1.0, 0.0, 1.0];

/// An overlay is in the scene, not on the picture: a band that passes
/// behind the ridge shows before the sky and over the ground on one side,
/// is hidden by the ridge pixel for pixel, and shows again on the other
/// side; a band nearer than the ridge crosses it whole.
#[test]
fn a_band_behind_the_ridge_is_hidden_by_it_and_shows_again_past_it() {
    let behind = band_behind(0.0).overlay(MAGENTA, OverlayDepth::Terrain);
    // 700 m up, nearer than the ridge's 500, on rows 11 and 12.
    let before = Panel {
        height: 700.0,
        at: [0.0, 63.4],
        half: [100.0, 10.0],
    }
    .overlay(CYAN, OverlayDepth::Terrain);
    let Some(r) = frames(look(), 1, &[&[], &[behind, before]]) else {
        eprintln!("no GPU adapter: skipped");
        return;
    };
    let (bare, r) = (&r[0], &r[1]);
    for row in [23, 24] {
        // Before the sky, west of the ground…
        assert_eq!(pixel(r, 10, row), [255, 0, 255], "before the sky");
        // …over the ground, on either side of the ridge…
        assert_eq!(pixel(r, 24, row), [255, 0, 255], "west of the ridge");
        assert_eq!(pixel(r, 40, row), [255, 0, 255], "east of the ridge");
        // …and behind the ridge, every pixel of the ridge is the ridge's.
        for column in 30..=34 {
            assert_eq!(
                pixel(r, column, row),
                pixel(bare, column, row),
                "the ridge at column {column} does not hide the band behind it"
            );
        }
    }
    assert_ne!(pixel(bare, 32, 24), [255, 0, 255]);
    // The band nearer than the ridge is not hidden by it.
    for column in [20, 32, 44] {
        assert_eq!(pixel(r, column, 12), [0, 255, 255], "column {column}");
    }
}

/// What must stay readable is tested against nothing: of two bands behind
/// the ridge, the one never hidden crosses it.
#[test]
fn an_overlay_never_hidden_shows_through_the_ridge() {
    // Rows 35 and 36, and rows 23 and 24.
    let always = band_behind(-158.5).overlay(YELLOW, OverlayDepth::Always);
    let hidden = band_behind(0.0).overlay(MAGENTA, OverlayDepth::Terrain);
    let Some(r) = frames(look(), 1, &[&[hidden, always]]) else {
        eprintln!("no GPU adapter: skipped");
        return;
    };
    for column in [10, 24, 32, 40] {
        assert_eq!(pixel(&r[0], column, 36), [255, 255, 0], "column {column}");
    }
    assert_ne!(pixel(&r[0], 32, 24), [255, 0, 255]);
}

/// Overlays are laid over the picture once its tone is done: a colour comes
/// out as given whatever the exposure, a translucent one lets through that
/// share of what the display would have shown, and an edge that crosses a
/// pixel is filtered like the ground's.
#[test]
fn an_overlay_is_laid_over_the_toned_picture_in_its_own_colour() {
    // Over the ground west of the ridge: opaque on rows 23 and 24, and a
    // translucent square further north, on rows 14 to 17.
    let opaque = band_behind(0.0).overlay([0.5, 0.25, 0.1, 1.0], OverlayDepth::Terrain);
    let veil = Panel {
        height: 250.0,
        at: [-110.0, 105.0],
        half: [30.0, 30.0],
    }
    .overlay([0.2, 0.1, 0.0, 0.5], OverlayDepth::Terrain);
    let shapes = [opaque, veil];
    let brighter = Look {
        exposure_ev: 3.0,
        ..look()
    };
    let (Some(r), Some(bright)) = (
        frames(look(), 1, &[&[], &shapes]),
        frames(brighter, 1, &[&shapes]),
    ) else {
        eprintln!("no GPU adapter: skipped");
        return;
    };
    let (bare, r) = (&r[0], &r[1]);
    let given = [oetf(0.5), oetf(0.25), oetf(0.1)];
    assert!(close(pixel(r, 24, 24), given), "{:?}", pixel(r, 24, 24));
    // Two stops more light on the ground, and the same colour.
    assert_eq!(pixel(&bright[0], 24, 24), pixel(r, 24, 24));
    assert_ne!(pixel(&bright[0], 24, 30), pixel(r, 24, 30));

    // Half of what the ground showed, plus the veil's own light.
    let under = pixel(bare, 24, 16).map(|v| eotf(f32::from(v) / 255.0));
    let wanted = [
        oetf(under[0] * 0.5 + 0.2),
        oetf(under[1] * 0.5 + 0.1),
        oetf(under[2] * 0.5),
    ];
    assert!(
        close(pixel(r, 24, 16), wanted),
        "{:?}, wanted {:?}",
        pixel(r, 24, 16),
        wanted.map(|c| c * 255.0)
    );
    // A pixel no overlay touches is the pixel it was.
    assert_eq!(pixel(r, 24, 30), pixel(bare, 24, 30));
    assert_eq!(pixel(r, 2, 2), pixel(bare, 2, 2));

    // The opaque band's edge falls across row 22, nearly half of it: one
    // sample a pixel takes it or leaves it, sixteen give a part.
    let Some(fine) = frames(look(), 4, &[&shapes]) else {
        return;
    };
    let (edge, full, sky) = (
        pixel(&fine[0], 10, 22),
        pixel(&fine[0], 10, 24),
        pixel(&fine[0], 10, 20),
    );
    assert!(close(full, given), "{full:?}");
    assert!(
        edge[2] > full[2] + 20 && edge[2] + 20 < sky[2],
        "the edge {edge:?} is not between the band {full:?} and the sky {sky:?}"
    );
}

/// A frame without overlays is the frame it always was, to the byte — on a
/// renderer that never drew one, and on one that has just drawn some.
#[test]
fn a_frame_without_overlays_is_the_frame_it_was() {
    let shapes = [
        band_behind(0.0).overlay(MAGENTA, OverlayDepth::Terrain),
        band_behind(-158.5).overlay(YELLOW, OverlayDepth::Always),
    ];
    for supersample in [1, 2] {
        let (Some(never), Some(after)) = (
            frames(look(), supersample, &[&[]]),
            frames(look(), supersample, &[&shapes, &[]]),
        ) else {
            eprintln!("no GPU adapter: skipped");
            return;
        };
        assert_ne!(after[0].rgba, never[0].rgba, "the overlays drew nothing");
        assert_eq!(
            after[1].rgba, never[0].rgba,
            "a frame without overlays kept something of the frame before"
        );
    }
}

/// A mesh that would be read past its end is refused, by name, before
/// anything is drawn.
#[test]
fn a_faulty_overlay_is_refused() {
    let mut mesh = band_behind(0.0).overlay(MAGENTA, OverlayDepth::Terrain);
    mesh.indices[5] = 4;
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipped");
        return;
    };
    let mut film = FilmGpu::new(
        device,
        queue,
        Settings {
            width: 64,
            height: 48,
            supersample: 1,
            look: look(),
        },
    );
    let camera = FrameCamera::of(&view(), 64.0 / 48.0);
    assert!(matches!(
        film.render_with(&camera, &[], &[mesh]),
        Err(tuile_film_gpu::FilmGpuError::Overlay(_))
    ));
}
