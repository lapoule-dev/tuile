// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The discs' instrument on a real GPU: the sun's and the Moon's light, as
//! the film's frames draw it, measured against what the air lets through
//! ([`tuile_film_native::discs`]). Skipped when the machine has no adapter.

use glam::DVec3;
use tuile_atmosphere::{AtmosphereParams, Moonlight};
use tuile_film::{BakedView, FrameCamera, FrameLight, Look};
use tuile_film_gpu::{FilmGpu, Settings};
use tuile_film_native::discs::{measure, Body, DiscLight};
use tuile_film_native::SkyOut;

const A: f64 = 6_378_137.0;
const W: u32 = 96;
const H: u32 = 64;

fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
    pollster::block_on(async {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .ok()?;
        adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("discs test"),
                ..Default::default()
            })
            .await
            .ok()
    })
}

/// Up at null island, 1 000 m over it, is +X; north +Z. A body
/// `elevation` degrees up, due south.
fn south_at(elevation: f64) -> DVec3 {
    let e = elevation.to_radians();
    DVec3::new(e.sin(), 0.0, -e.cos())
}

/// What a film draws of the sky about `at`, `across` disc diameters high,
/// under the sun along `sun` and the Moon, if any: its light, and the
/// discs measured in it.
fn shoot(
    sun: DVec3,
    moon: Option<Moonlight>,
    at: DVec3,
    fovy: f64,
    supersample: u32,
) -> Option<Vec<DiscLight>> {
    let (device, queue) = device()?;
    let look = Look {
        to_sun: sun.as_vec3(),
        atmosphere: Some(AtmosphereParams::default()),
        ..Look::default()
    };
    let mut film = FilmGpu::new(
        device,
        queue,
        Settings {
            width: W,
            height: H,
            supersample,
            look,
        },
    );
    if moon.is_some() {
        let mut light = FrameLight::of(&look);
        light.illumination.moon = moon;
        film.set_light(light);
    }
    let position = DVec3::new(A + 1000.0, 0.0, 0.0);
    let up = (DVec3::X - at * at.x).normalize();
    let view = BakedView {
        position: position.to_array(),
        direction: at.to_array(),
        up: up.to_array(),
        viewport_px: [f64::from(W), f64::from(H)],
        fovy_rad: fovy,
    };
    let camera = FrameCamera::of(&view, W as f32 / H as f32);
    let mut encoder = film.render(&camera, &[]).expect("render");
    let texture = film.write_linear(&mut encoder).clone();
    film.queue().submit([encoder.finish()]);
    let linear = read_floats(&film, &texture);
    let (uniform, model) = film.sky_seen().expect("an atmosphere");
    Some(measure(
        0,
        &linear,
        W,
        H,
        &camera,
        SkyOut {
            uniform: &uniform,
            model,
        },
    ))
}

fn read_floats(film: &FilmGpu, texture: &wgpu::Texture) -> Vec<f32> {
    let size = texture.size();
    let row = size.width * 16;
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
        .flat_map(|r| {
            r[..row as usize]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The field of view that draws the sun `across` pixels wide on the
/// test's picture.
fn fovy_for(across: f64) -> f64 {
    let rho = tuile_atmosphere::hillaire::SUN_ANGULAR_RADIUS;
    2.0 * ((2.0 * rho / across) * f64::from(H) / 2.0).atan()
}

fn of(rows: &[DiscLight], body: Body) -> DiscLight {
    *rows
        .iter()
        .find(|r| r.body == body)
        .expect("the body measured")
}

/// A disc carries the light the ground is lit by, whatever its size on the
/// picture: a high sun three, eight and fifty pixels across sends, within
/// a hundredth, its irradiance through the air — its limb law normalised
/// over the disc, its edge an area the pixel covers. Today's limb law was
/// not normalised (its mean, 0.8: defect 2 of the study), and a sun a few
/// pixels across was a step at each pixel.
#[test]
fn a_sun_carries_the_light_that_lights_the_ground_at_any_size() {
    let sun = south_at(30.0);
    for across in [3.0, 8.0, 50.0] {
        let Some(rows) = shoot(sun, None, sun, fovy_for(across), 2) else {
            return;
        };
        let disc = of(&rows, Body::Sun);
        println!("{across} px: {disc:?} ratio {}", disc.ratio());
        assert!(!disc.cut);
        assert!((disc.across_px - across).abs() < 0.1, "{}", disc.across_px);
        assert!(
            (disc.ratio() - 1.0).abs() < 0.01,
            "a sun {across} px across carries {} of its light",
            disc.ratio()
        );
    }
}

/// A sun half set on the planet's horizon shows its upper half, each of
/// its pixels as bright as the air along it lets it be: the half of its
/// light the upper half sends. Today the half left by the horizon was
/// dimmed again by the share of the disc over it (defect 1 of the study):
/// a quarter of the light, not a half.
#[test]
fn a_half_set_sun_shows_its_upper_half_at_its_own_brightness() {
    // From 1 000 m the horizon of the air's sphere lies 1.016° under the
    // eye's horizontal.
    let sun = south_at(-1.016);
    let Some(rows) = shoot(sun, None, sun, fovy_for(50.0), 2) else {
        return;
    };
    let disc = of(&rows, Body::Sun);
    println!("{disc:?} ratio {}", disc.ratio());
    assert!(!disc.cut);
    assert!(
        (disc.ratio() - 1.0).abs() < 0.02,
        "a half-set sun carries {} of what its upper half sends",
        disc.ratio()
    );
}

/// The Moon too, normalised already, carries its light at any size: its
/// edge no longer a smoothstep over the outer 4 % of its radius, which took
/// 4 % of it.
#[test]
fn a_full_moon_carries_its_light_at_any_size() {
    let sun = south_at(-30.0);
    let to_moon = DVec3::new(0.5f64.sqrt(), 0.5f64.sqrt(), 0.0);
    let moon = Moonlight {
        to_moon_ecef: to_moon,
        phase_angle: 0.05,
        angular_radius: 0.0045,
        moon_rgb: [0.0; 3],
    };
    for across in [3.0, 8.0, 50.0] {
        let Some(rows) = shoot(sun, Some(moon), to_moon, fovy_for(across), 2) else {
            return;
        };
        let disc = of(&rows, Body::Moon);
        println!("{across} px: {disc:?} ratio {}", disc.ratio());
        assert!(!disc.cut);
        assert!(
            (disc.ratio() - 1.0).abs() < 0.01,
            "a full Moon about {across} px across carries {} of its light",
            disc.ratio()
        );
    }
}

/// The nearer body hides the farther: a Moon wider than the sun, over it,
/// leaves nothing of the sun's disc. Today both were added — the sun shone
/// through the Moon in a total eclipse (defect 6).
#[test]
fn the_moon_hides_the_sun_behind_it() {
    let sun = south_at(30.0);
    let moon = Moonlight {
        to_moon_ecef: sun,
        phase_angle: std::f32::consts::PI,
        angular_radius: 0.0050,
        moon_rgb: [0.0; 3],
    };
    let Some(rows) = shoot(sun, Some(moon), sun, fovy_for(50.0), 2) else {
        return;
    };
    let disc = of(&rows, Body::Sun);
    println!("{disc:?} ratio {}", disc.ratio());
    assert!(
        disc.ratio().abs() < 0.01,
        "the sun behind the Moon shows {} of its light",
        disc.ratio()
    );
}
