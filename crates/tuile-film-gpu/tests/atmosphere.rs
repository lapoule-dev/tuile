// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The physical atmosphere on a real GPU: the sky-view table against the CPU
//! reference, the ground through a day's sun, both eyes under one light.
//! Skipped when the machine has no adapter.

use glam::{DVec3, Vec3};
use tuile_atmosphere::hillaire::{sky_view_texel_direction, SKY_VIEW_SIZE};
use tuile_atmosphere::{AtmosphereFrame, AtmosphereParams, LutAtmosphere, DAYLIGHT_EV100};
use tuile_film::{BakedView, Eye, FrameCamera, FrameLight, Look, ReferenceIllumination, TileKey};
use tuile_film_gpu::{FilmGpu, Settings, Shot, TileMesh};

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
                label: Some("atmosphere test"),
                ..Default::default()
            })
            .await
            .ok()
    })
}

fn le<T: Copy + bytemuck::Pod>(v: &[T]) -> Vec<u8> {
    bytemuck::cast_slice(v).to_vec()
}

fn read_texture(film: &FilmGpu, texture: &wgpu::Texture, bytes_per_texel: u32) -> Vec<u8> {
    let size = texture.size();
    let row = size.width * bytes_per_texel;
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
    let bytes = slice.get_mapped_range().to_vec();
    bytes
        .chunks(padded as usize)
        .flat_map(|r| r[..row as usize].to_vec())
        .collect()
}

/// Up at null island is +X; north +Z. A sun `elevation` degrees up, due
/// south: behind a camera looking north.
fn sun_at(elevation: f64) -> Vec3 {
    let e = elevation.to_radians();
    Vec3::new(e.sin() as f32, 0.0, -e.cos() as f32)
}

fn atmospheric(elevation: f64) -> Look {
    Look {
        to_sun: sun_at(elevation),
        atmosphere: Some(AtmosphereParams::default()),
        ..Look::default()
    }
}

/// 1 000 m over null island, looking down, or along the horizon to the
/// north.
fn view(down: bool) -> BakedView {
    BakedView {
        position: [A + 1000.0, 0.0, 0.0],
        direction: if down {
            [-1.0, 0.0, 0.0]
        } else {
            [0.0, 0.0, 1.0]
        },
        up: if down {
            [0.0, 0.0, 1.0]
        } else {
            [1.0, 0.0, 0.0]
        },
        viewport_px: [64.0, 48.0],
        fovy_rad: 0.8,
    }
}

fn film(look: Look) -> Option<FilmGpu> {
    let (device, queue) = device()?;
    Some(FilmGpu::new(
        device,
        queue,
        Settings {
            width: 64,
            height: 48,
            supersample: 1,
            look,
        },
    ))
}

/// A grey square 400 m a side under the camera, facing up.
fn square(film: &mut FilmGpu) -> TileKey {
    let p = [
        0.0f32, -200.0, -200.0, 0.0, 200.0, -200.0, 0.0, 200.0, 200.0, 0.0, -200.0, 200.0,
    ];
    let n = [1.0f32, 0.0, 0.0].repeat(4);
    let uv = [0.0f32, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0];
    let i = [0u32, 1, 2, 0, 2, 3];
    let texture = film.create_albedo(4, 4);
    film.write_rgba(&texture, &[110u8, 110, 110, 255].repeat(16));
    let key = TileKey { id: 1, drape: 0 };
    film.enter(
        key,
        &TileMesh {
            origin_ecef: [A, 0.0, 0.0],
            positions: &le(&p),
            normals: &le(&n),
            uvs: &le(&uv),
            indices: &le(&i),
            index_count: 6,
            base_color_factor: [1.0; 4],
        },
        Some(texture),
    )
    .expect("enter");
    key
}

fn draw(film: &mut FilmGpu, down: bool, selection: &[TileKey]) -> Vec<u8> {
    let camera = FrameCamera::of(&view(down), 64.0 / 48.0);
    let encoder = film.render(&camera, selection).expect("render");
    film.queue().submit([encoder.finish()]);
    let output = film.output().clone();
    read_texture(film, &output, 4)
}

fn at(rgba: &[u8], x: u32, y: u32) -> [f32; 3] {
    let i = ((y * 64 + x) * 4) as usize;
    [rgba[i], rgba[i + 1], rgba[i + 2]].map(f32::from)
}

#[test]
fn the_sky_view_table_is_the_cpu_sky() {
    for elevation in [35.0, 3.0, -3.0] {
        let Some(mut film) = film(atmospheric(elevation)) else {
            return;
        };
        draw(&mut film, false, &[]);
        let texels = read_texture(&film, film.sky_view_texture(), 16);
        let gpu = |i: usize, j: usize| {
            let at = (j * SKY_VIEW_SIZE[0] + i) * 4;
            let float = |k: usize| {
                let b = &texels[4 * (at + k)..4 * (at + k) + 4];
                f64::from(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            };
            [float(0), float(1), float(2)]
        };
        // The CPU's own: the sphere under the eye, its height, the sun's.
        let eye = DVec3::new(A + 1000.0, 0.0, 0.0);
        let ground = tuile_core::geo::ecef_to_geodetic(eye);
        let frame = AtmosphereFrame::at(ground);
        let height = frame.height(eye);
        let mu_s = sun_at(elevation).as_dvec3().dot(frame.up(eye));
        let sun = DVec3::new((1.0 - mu_s * mu_s).max(0.0).sqrt(), 0.0, mu_s);
        let cpu_model = LutAtmosphere::new(AtmosphereParams::default());
        let (mut worst, mut checked) = (0.0f64, 0);
        for j in (0..SKY_VIEW_SIZE[1]).step_by(7) {
            for i in (0..SKY_VIEW_SIZE[0]).step_by(11) {
                let cpu =
                    cpu_model.sky_radiance(height, sky_view_texel_direction(i, j, height), sun);
                let found = gpu(i, j);
                for c in 0..3 {
                    let scale = cpu[c].abs().max(1e-4);
                    worst = worst.max((found[c] - cpu[c]).abs() / scale);
                }
                checked += 1;
            }
        }
        assert!(checked > 200);
        assert!(
            worst < 0.03,
            "{elevation}°: worst relative difference {worst}"
        );
    }
}

#[test]
fn the_ground_dims_and_warms_as_the_sun_sets_and_is_never_black() {
    let mut last: Option<[f32; 3]> = None;
    let mut skies = Vec::new();
    for elevation in [50.0, 20.0, 6.0, 2.0, -3.0, -20.0] {
        let Some(mut film) = film(atmospheric(elevation)) else {
            return;
        };
        let key = square(&mut film);
        let ground = at(&draw(&mut film, true, &[key]), 32, 24);
        let luma = 0.2126 * ground[0] + 0.7152 * ground[1] + 0.0722 * ground[2];
        assert!(luma > 30.0, "{elevation}°: ground {ground:?}");
        if let Some(before) = last {
            let was = 0.2126 * before[0] + 0.7152 * before[1] + 0.0722 * before[2];
            assert!(
                luma <= was + 1.0,
                "{elevation}°: {ground:?} after {before:?}"
            );
            // Warmer down to a sun a few degrees up; lower, its beam on
            // level ground is a few hundredths of the light and the blue
            // sky's takes over, as measured daylight turns bluer before the
            // blue hour.
            if elevation > 5.0 {
                assert!(
                    ground[0] / ground[2] >= before[0] / before[2] - 0.01,
                    "{elevation}°: warmer as the sun sets, {ground:?} after {before:?}"
                );
            }
        }
        last = Some(ground);
        // The sky over the horizon: lit, never black, and blue by day.
        let sky = at(&draw(&mut film, false, &[]), 32, 4);
        assert!(sky.iter().sum::<f32>() > 30.0, "{elevation}°: sky {sky:?}");
        if elevation > 10.0 {
            assert!(sky[2] > sky[0], "{elevation}°: sky {sky:?}");
        }
        skies.push(0.2126 * sky[0] + 0.7152 * sky[1] + 0.0722 * sky[2]);
    }
    // And the night sky is darker than the day's, as the ground is.
    assert!(
        skies.last().unwrap() + 20.0 < skies[0],
        "skies from noon to night: {skies:?}"
    );
}

#[test]
fn a_night_side_beside_a_sunset_is_never_drawn_below_the_night_sky() {
    // 70 km over the Pyrenees at a sunset of 2026-10-19, facing the glow:
    // the camera is exposed for the lit limb, and black ground lies on the
    // night side at the frame's left, some 500 km off at a grazing angle —
    // where the air leaves next to nothing of it and, unlit, scatters next
    // to nothing. It was drawn black under a sky drawn at its least.
    let to_sun = DVec3::new(-0.047_520_53, -0.983_125_53, -0.176_652_15).normalize();
    let eye = DVec3::new(4_728_002.21, 11_742.51, 4_370_014.76);
    let forward = DVec3::new(0.551_144_46, 0.246_035_83, -0.797_311_83).normalize();
    let up = DVec3::new(0.816_761_18, 0.036_414_22, 0.575_825_65).normalize();
    let right = forward.cross(up).normalize();
    assert!(to_sun.dot(right) > 0.0, "the sun is on the frame's right");
    let (width, height) = (64u32, 36u32);
    let aspect = f64::from(width) / f64::from(height);
    let Some((device, queue)) = device() else {
        return;
    };
    let mut film = FilmGpu::new(
        device,
        queue,
        Settings {
            width,
            height,
            supersample: 1,
            look: Look {
                to_sun: to_sun.as_vec3(),
                atmosphere: Some(AtmosphereParams::default()),
                ..Look::default()
            },
        },
    );
    // The ray through the frame's left, a little under the horizon, and
    // where it meets the ground.
    let (px, py) = (10u32, 20u32);
    let (nx, ny) = (
        (f64::from(px) + 0.5) / f64::from(width) * 2.0 - 1.0,
        1.0 - (f64::from(py) + 0.5) / f64::from(height) * 2.0,
    );
    let ray = (forward + right * nx * aspect + up * ny).normalize();
    let radius = 6_368_000.0f64;
    let b = eye.dot(ray);
    let c = eye.length_squared() - radius * radius;
    let hit = eye + ray * (-b - (b * b - c).max(0.0).sqrt());
    assert!((hit - eye).length() > 400_000.0, "{}", (hit - eye).length());
    let normal = hit.normalize();
    let east = DVec3::Z.cross(normal).normalize();
    let north = normal.cross(east);
    let half = 300_000.0f64;
    let corner = |a: f64, b: f64| (east * a + north * b).as_vec3();
    let p: Vec<f32> = [
        corner(-half, -half),
        corner(half, -half),
        corner(half, half),
        corner(-half, half),
    ]
    .iter()
    .flat_map(|v| v.to_array())
    .collect();
    let n: Vec<f32> = normal.as_vec3().to_array().repeat(4);
    let uv = [0.0f32, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0];
    let i = [0u32, 1, 2, 0, 2, 3];
    let texture = film.create_albedo(4, 4);
    film.write_rgba(&texture, &[0u8, 0, 0, 255].repeat(16));
    let key = TileKey { id: 9, drape: 0 };
    film.enter(
        key,
        &TileMesh {
            origin_ecef: hit.to_array(),
            positions: &le(&p),
            normals: &le(&n),
            uvs: &le(&uv),
            indices: &le(&i),
            index_count: 6,
            base_color_factor: [1.0; 4],
        },
        Some(texture),
    )
    .expect("enter");
    let view = BakedView {
        position: eye.to_array(),
        direction: forward.to_array(),
        up: up.to_array(),
        viewport_px: [f64::from(width), f64::from(height)],
        fovy_rad: std::f64::consts::FRAC_PI_2,
    };
    let camera = FrameCamera::of(&view, aspect as f32);
    let encoder = film.render(&camera, &[key]).expect("render");
    film.queue().submit([encoder.finish()]);
    let output = film.output().clone();
    let rgba = read_texture(&film, &output, 4);
    let pixel = |x: u32, y: u32| {
        let i = ((y * width + x) * 4) as usize;
        let c = [rgba[i], rgba[i + 1], rgba[i + 2]].map(f32::from);
        0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
    };
    let ground = pixel(px, py);
    // The night sky at the frame's top left, drawn at its least.
    let sky = pixel(2, 2);
    assert!(sky > 5.0, "the night sky is drawn: {sky}");
    assert!(
        ground >= 0.5 * sky,
        "night ground {ground} under a night sky of {sky}: a black band"
    );
}

#[test]
fn both_eyes_of_a_frame_share_one_light_and_one_sky() {
    let Some(mut film) = film(atmospheric(15.0)) else {
        return;
    };
    let key = square(&mut film);
    let frame = FrameCamera::of(&view(true), 64.0 / 48.0);
    let mut lights = Vec::new();
    let mut skies = Vec::new();
    // Eyes far apart (a hyperstereo baseline), so that a light worked out
    // from each eye's own place would differ between them.
    for (eye, apart) in [(Eye::Left, -40.0), (Eye::Right, 40.0)] {
        let mut camera = frame;
        camera.eye.y += apart;
        let shot = Shot {
            camera,
            frame,
            eye: Some(eye),
        };
        let encoder = film.render_shot(&shot, &[key], &[]).expect("render");
        film.queue().submit([encoder.finish()]);
        lights.push(*film.lit());
        skies.push(read_texture(&film, film.sky_view_texture(), 16));
    }
    assert_eq!(lights[0], lights[1]);
    assert_eq!(skies[0], skies[1]);
}

#[test]
fn a_frames_light_is_laid_over_the_ordered_look_never_the_last_frames() {
    let look = Look {
        exposure_ev: 0.5,
        ..Look::default()
    };
    let Some(mut film) = film(look) else {
        return;
    };
    let mut illumination = ReferenceIllumination::provisional(DVec3::X, [1.0; 3], [0.5; 3]);
    illumination.ev100 = DAYLIGHT_EV100 + 2.0;
    illumination.white = [1.2, 1.0, 0.8];
    illumination.adaptation = 1.0;
    let light = FrameLight::new(illumination, Default::default());
    film.set_light(light);
    let first = *film.lit();
    film.set_light(light);
    assert_eq!(*film.lit(), first, "the second frame's look is the first's");
    assert!((first.exposure_ev - (0.5 - 2.0 * tuile_atmosphere::EXPOSURE_FOLLOWS)).abs() < 1e-6);
}

/// What the present pass makes of a linear radiance under `look`, as it
/// stood before the atmosphere: exposure, white balance, the roll-off of
/// highlights, the sRGB curve — the look's contrast and saturation left at
/// none.
fn presented(look: &Look, radiance: Vec3) -> [f32; 3] {
    let mut linear = radiance * look.exposure_scale() * look.white_balance;
    let lit = linear.dot(Vec3::new(0.2126, 0.7152, 0.0722));
    if lit > 0.5 {
        linear *= (0.5 + 0.5 * ((lit - 0.5) / 0.5).tanh()) / lit;
    }
    linear.to_array().map(|x| {
        let x = x.clamp(0.0, 1.0);
        let v = if x <= 0.003_130_8 {
            12.92 * x
        } else {
            1.055 * x.powf(1.0 / 2.4) - 0.055
        };
        (v * 255.0).round()
    })
}

#[test]
fn without_an_atmosphere_or_a_hosts_light_the_film_is_the_looks_own() {
    // The default path: no atmosphere, no light from the host. Nothing of
    // the air is read — the look is the one ordered, the ground is lit by
    // its sun and dome alone, and the sky is its dome's one colour.
    let look = Look {
        to_sun: sun_at(45.0),
        ..Look::default()
    };
    assert!(look.atmosphere.is_none());
    let Some(mut film) = film(look) else {
        return;
    };
    let key = square(&mut film);
    let ground = at(&draw(&mut film, true, &[key]), 32, 24);
    assert_eq!(*film.lit(), look, "the look is the one ordered");
    let base = (110.0f32 / 255.0 + 0.055).powf(2.4) / 1.055f32.powf(2.4);
    let cos = sun_at(45.0).dot(Vec3::X);
    let wanted = presented(&look, base * (look.world + look.sun * cos));
    for c in 0..3 {
        assert!(
            (ground[c] - wanted[c]).abs() <= 1.0,
            "ground {ground:?} against {wanted:?}"
        );
    }
    let sky = at(&draw(&mut film, false, &[]), 32, 4);
    let wanted = presented(&look, look.world);
    for c in 0..3 {
        assert!(
            (sky[c] - wanted[c]).abs() <= 1.0,
            "sky {sky:?} against {wanted:?}"
        );
    }
}

/// A default film's pictures, byte for byte: no atmosphere, no host light —
/// what every film made before the atmosphere was is drawn as. The digest is
/// frozen from main before the sky's work (07be3d0, and the same on this
/// branch); a change here changes
/// every default film, and is a decision, never an accident.
#[test]
fn a_default_films_pictures_are_the_same_to_the_byte() {
    let look = Look {
        to_sun: sun_at(45.0),
        ..Look::default()
    };
    let Some(mut film) = film(look) else {
        return;
    };
    let key = square(&mut film);
    let down = draw(&mut film, true, &[key]);
    let sky = draw(&mut film, false, &[]);
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for b in down.iter().chain(&sky) {
        hash = (hash ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3);
    }
    println!("DEFAULT-FILM-DIGEST {hash:016x}");
    assert_eq!(format!("{hash:016x}"), FROZEN_DEFAULT_FILM);
}

const FROZEN_DEFAULT_FILM: &str = "c203a130175f25bd";
