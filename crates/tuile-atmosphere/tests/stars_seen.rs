// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The stars as the shader draws them (`sky_seen`, the film's and the
//! viewer's one sky), read back in linear radiance: always there, dimmed by
//! the air before them, hidden by what stands before them, and shown or
//! not by the camera alone.
//!
//! Each probe is a line of sight from an eye on the Pic du Midi; the sky is
//! drawn along it, with stars and without (less `stars_seen`'s), and the
//! difference is the stars' light. A star is put where a test wants it by
//! turning the catalogue: the brightest star, Sirius, is turned onto the
//! line of sight. What the display shows is the frame's own response: its
//! exposure (the look's, metered on level ground), its white balance and
//! highlight roll-off, the sRGB curve — 8 bits.

#![cfg(feature = "wgpu")]

use glam::{DMat3, DQuat, DVec3};
use tuile_atmosphere::gpu::{layout, AtmosphereGpu, AtmosphereUniform, SkyViewPass};
use tuile_atmosphere::hillaire::{Moment, LUT_RADIUS};
use tuile_atmosphere::stars::StarTable;
use tuile_atmosphere::{AtmosphereParams, LutAtmosphere, Moon, Moonlight, SkyLook, Sun};
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};

/// The Pic du Midi's summit.
fn pic() -> Geodetic {
    Geodetic {
        lon: 0.1418f64.to_radians(),
        lat: 42.9365f64.to_radians(),
        height: 2_877.0,
    }
}

/// 2026-06-21 12:00 UTC.
const NOON: f64 = 1_782_043_200.0;

/// A star's image in a picture 540 pixels high of a 50° field.
fn sigma() -> f32 {
    let pixel = 2.0 * (25f64.to_radians()).tan() / 540.0;
    (0.8 * pixel) as f32
}

/// Towards azimuth `az` (from north, east positive) and elevation `el`,
/// degrees, from the Pic.
fn towards(az: f64, el: f64) -> DVec3 {
    let enu = enu_frame(pic());
    let (a, e) = (az.to_radians(), el.to_radians());
    enu.x_axis * a.sin() * e.cos() + enu.y_axis * a.cos() * e.cos() + enu.z_axis * e.sin()
}

/// The table's stars: J2000 direction, irradiance over the sun's, colour.
fn stars() -> Vec<(DVec3, f64, [f32; 3])> {
    StarTable::catalogue()
        .stars()
        .into_iter()
        .map(|(d, e, c)| {
            (
                DVec3::new(f64::from(d[0]), f64::from(d[1]), f64::from(d[2])).normalize(),
                e,
                c,
            )
        })
        .collect()
}

fn sirius() -> (DVec3, f64, [f32; 3]) {
    stars()
        .into_iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .expect("stars")
}

/// The turn from the Earth's axes to the catalogue's that puts Sirius
/// along `there` (ECEF).
fn sirius_along(there: DVec3) -> DMat3 {
    DMat3::from_quat(DQuat::from_rotation_arc(there.normalize(), sirius().0))
}

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    atmosphere: AtmosphereGpu,
    pipeline: wgpu::ComputePipeline,
    probe_layout: wgpu::BindGroupLayout,
}

const PROBE: &str = r"
@group(1) @binding(0) var<storage, read> probes: array<vec4f>;
@group(1) @binding(1) var<storage, read_write> results: array<vec4f>;

@compute @workgroup_size(1)
fn probe(@builtin(global_invocation_id) id: vec3u) {
    let p = probes[id.x];
    let n = normalize(p.xyz);
    // `sky_seen` takes the pixel's angle, a star's image 0.8 of it — and its
    // discs are drawn for that pixel too: the sky without the stars is the
    // same line of sight less what `sky_seen` adds of them.
    let seen = sky_seen(n, p.w / 0.8);
    results[3u * id.x] = vec4f(seen, 0.0);
    results[3u * id.x + 1u] = vec4f(seen - stars_seen(n, p.w) * atmosphere.illuminance.xyz, 0.0);
    results[3u * id.x + 2u] = vec4f(transmittance(atmosphere.sun.w, dot(atmosphere.up.xyz, n)), 0.0);
}
";

fn gpu() -> Option<Gpu> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).ok()?;
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).ok()?;
    if device.limits().max_compute_workgroups_per_dimension == 0 {
        return None;
    }
    let bgl = layout(&device, wgpu::ShaderStages::COMPUTE, true);
    let mut atmosphere = AtmosphereGpu::new(&device, &bgl, SkyViewPass::Compute);
    atmosphere.hold_with(&queue, AtmosphereParams::default(), LutAtmosphere::of);
    let storage = |binding, read_only| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    };
    let probe_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("probe"),
        entries: &[storage(0, true), storage(1, false)],
    });
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("stars probe"),
        source: wgpu::ShaderSource::Wgsl(
            format!(
                "{}{}\n{}\n{PROBE}",
                tuile_atmosphere::hillaire::wgsl_bindings(0),
                tuile_atmosphere::hillaire::WGSL,
                tuile_atmosphere::gpu::SEEN_WGSL
            )
            .into(),
        ),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("stars probe"),
        bind_group_layouts: &[Some(&bgl), Some(&probe_layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("stars probe"),
        layout: Some(&pipeline_layout),
        module: &module,
        entry_point: Some("probe"),
        compilation_options: Default::default(),
        cache: None,
    });
    Some(Gpu {
        device,
        queue,
        atmosphere,
        pipeline,
        probe_layout,
    })
}

/// One line of sight, read back: the sky with its stars and without, and
/// the air's transmittance along it — linear, pre-exposed, in the
/// renderer's units.
#[derive(Debug, Clone, Copy)]
struct Seen {
    with: [f32; 3],
    without: [f32; 3],
    transmittance: [f32; 3],
}

impl Seen {
    fn stars(&self) -> [f32; 3] {
        [0, 1, 2].map(|c| self.with[c] - self.without[c])
    }
}

/// The sky of `instant` from the Pic, the catalogue turned by `turn`.
struct Sky {
    uniform: AtmosphereUniform,
    light: tuile_atmosphere::SkyLight,
}

fn sky(instant: f64, turn: DMat3) -> Sky {
    let air = LutAtmosphere::today();
    let at = pic();
    let moon = Moon::seen_from(instant, at).map(|m| Moonlight::of(&m));
    let look = SkyLook::default();
    let light = air.light(
        &air,
        Geodetic { height: 0.0, ..at },
        Moment {
            to_sun_ecef: Sun::at_unix_seconds(instant).direction_ecef,
            moon,
            compensation_ev: 0.0,
        },
        &[],
        look.calibration,
    );
    let eye = geodetic_to_ecef(at);
    let uniform = AtmosphereUniform::new(
        &AtmosphereParams::default(),
        &light,
        eye,
        look.exposure_scale(&light),
        turn,
        &look,
    )
    .seen_from(&light, eye);
    Sky { uniform, light }
}

fn probe(gpu: &mut Gpu, sky: &Sky, lines: &[(DVec3, f32)]) -> Vec<Seen> {
    use wgpu::util::DeviceExt;
    let device = &gpu.device;
    gpu.queue
        .write_buffer(&gpu.atmosphere.uniform, 0, bytemuck::bytes_of(&sky.uniform));
    gpu.atmosphere.sky_view_stale = true;
    let probes: Vec<[f32; 4]> = lines
        .iter()
        .map(|(d, s)| {
            let d = d.normalize().as_vec3();
            [d.x, d.y, d.z, *s]
        })
        .collect();
    let input = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: None,
        contents: bytemuck::cast_slice(&probes),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let size = (lines.len() * 3 * 16) as u64;
    let output = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let read = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &gpu.probe_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: input.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: output.as_entire_binding(),
            },
        ],
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    gpu.atmosphere.record_sky_view(&mut encoder);
    {
        let mut pass = encoder.begin_compute_pass(&Default::default());
        pass.set_pipeline(&gpu.pipeline);
        pass.set_bind_group(0, &gpu.atmosphere.bind, &[]);
        pass.set_bind_group(1, &bind, &[]);
        pass.dispatch_workgroups(lines.len() as u32, 1, 1);
    }
    encoder.copy_buffer_to_buffer(&output, 0, &read, 0, size);
    gpu.queue.submit([encoder.finish()]);
    read.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let data = read.slice(..).get_mapped_range().expect("mapped").to_vec();
    let f: &[f32] = bytemuck::cast_slice(&data);
    (0..lines.len())
        .map(|i| {
            let v = |k: usize| {
                [
                    f[12 * i + 4 * k],
                    f[12 * i + 4 * k + 1],
                    f[12 * i + 4 * k + 2],
                ]
            };
            Seen {
                with: v(0),
                without: v(1),
                transmittance: v(2),
            }
        })
        .collect()
}

/// A linear, pre-exposed colour as the display shows it, 0–255 a channel,
/// not rounded: the white balance, the roll-off of highlights (`graded`),
/// the sRGB curve.
fn display(sky: &Sky, linear: [f32; 3]) -> [f64; 3] {
    let g = sky.uniform.grade;
    let c = [0, 1, 2].map(|i| f64::from(linear[i] * g[i]));
    let knee = f64::from(g[3]);
    let lit = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let c = if lit > knee {
        let bent = knee + (1.0 - knee) * ((lit - knee) / (1.0 - knee)).min(10.0).tanh();
        c.map(|v| v * bent / lit)
    } else {
        c
    };
    c.map(|x| {
        let x = x.clamp(0.0, 1.0);
        255.0
            * if x <= 0.003_130_8 {
                12.92 * x
            } else {
                1.055 * x.powf(1.0 / 2.4) - 0.055
            }
    })
}

/// The star's light over the sky's behind it, in luminance, both as the
/// camera exposes them: how much it stands out before any display.
fn contrast(seen: &Seen) -> f64 {
    let l = |c: [f32; 3]| {
        0.2126 * f64::from(c[0]) + 0.7152 * f64::from(c[1]) + 0.0722 * f64::from(c[2])
    };
    l(seen.stars()) / l(seen.without)
}

/// How far a star stands out of the sky behind it on the display, in code
/// values of its brightest channel: under a half, no picture can show it.
fn shown(sky: &Sky, seen: &Seen) -> f64 {
    let a = display(sky, seen.with);
    let b = display(sky, seen.without);
    (0..3).map(|c| a[c] - b[c]).fold(0.0, f64::max)
}

#[test]
fn the_stars_are_in_the_noon_sky_and_too_faint_for_the_camera_to_show() {
    let Some(mut gpu) = gpu() else {
        eprintln!("skipping GPU test: no usable adapter with compute");
        return;
    };
    let line = towards(110.0, 45.0);
    let sky = sky(NOON, sirius_along(line));
    let seen = probe(&mut gpu, &sky, &[(line, sigma())])[0];
    let stars = seen.stars();
    eprintln!(
        "noon: Sirius {stars:?} over a sky of {:?}, shown {:.4} code values",
        seen.without,
        shown(&sky, &seen)
    );
    assert!(
        stars.iter().all(|&s| s > 0.0),
        "Sirius is in the noon sky's radiance: {stars:?}"
    );
    assert!(
        shown(&sky, &seen) < 0.5,
        "and no 8-bit picture exposed for noon can show it"
    );
}

#[test]
fn the_stars_come_out_one_by_one_through_twilight() {
    let Some(mut gpu) = gpu() else {
        eprintln!("skipping GPU test: no usable adapter with compute");
        return;
    };
    let sunset = Sun::next_sunset(
        NOON,
        Geodetic {
            height: 0.0,
            ..pic()
        },
    )
    .expect("a sunset");
    // Sirius in the darkening east, 30° up; the brightest star of each
    // magnitude near it, as the turn carries them.
    let line = towards(110.0, 30.0);
    let turn = sirius_along(line);
    let all = stars();
    let near: Vec<_> = all
        .iter()
        .filter(|s| (turn.transpose() * s.0).dot(towards(110.0, 30.0)) > 25f64.to_radians().cos())
        .copied()
        .collect();
    let mut probed = vec![sirius()];
    for v in [0.0, 1.0, 2.0, 3.0] {
        let band = |s: &&(DVec3, f64, [f32; 3])| {
            let m = -2.5 * (s.1).log10() - 26.76;
            m >= v && m < v + 1.0
        };
        if let Some(s) = near.iter().filter(band).max_by(|a, b| a.1.total_cmp(&b.1)) {
            probed.push(*s);
        }
    }
    let lines: Vec<(DVec3, f32)> = probed
        .iter()
        .map(|s| (turn.transpose() * s.0, sigma()))
        .collect();
    let mut history: Vec<Vec<f64>> = Vec::new();
    let mut contrasts: Vec<Vec<f64>> = Vec::new();
    for k in 0..=12 {
        let t = sunset + f64::from(k) * 600.0;
        let sky = sky(t, turn);
        let seen = probe(&mut gpu, &sky, &lines);
        let row: Vec<f64> = seen.iter().map(|s| shown(&sky, s)).collect();
        contrasts.push(seen.iter().map(contrast).collect());
        // The display's floor under the sky (`least_sky`): how much of the
        // sky behind the first star it makes up — 100 % where it, not the
        // air, is the sky drawn.
        let l = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        let floor = l([0, 1, 2].map(|c| sky.uniform.least_sky[c])) / l(seen[0].without);
        eprintln!(
            "+{:3} min, floor {:5.1} % of the sky, sun {:+.1}°: {}",
            k * 10,
            100.0 * floor,
            90.0 - sky
                .light
                .illumination
                .to_sun_ecef
                .angle_between(enu_frame(pic()).z_axis)
                .to_degrees(),
            row.iter()
                .zip(&probed)
                .map(|(r, s)| format!("V{:.2}: {r:6.2}", -2.5 * s.1.log10() - 26.76))
                .collect::<Vec<_>>()
                .join("  ")
        );
        history.push(row);
    }
    // Each star stands out more against its sky as the sky darkens, never
    // less; and on the display too, until its pixel nears white, where
    // the roll-off of highlights and the night's white balance bend it.
    for j in 0..probed.len() {
        for k in 1..history.len() {
            assert!(
                contrasts[k][j] > contrasts[k - 1][j],
                "star {j} fades against its sky at step {k}: {} after {}",
                contrasts[k][j],
                contrasts[k - 1][j]
            );
            if history[k - 1][j] < 150.0 {
                assert!(
                    history[k][j] >= history[k - 1][j] - 1e-3,
                    "star {j} fades on the display at step {k}: {} after {}",
                    history[k][j],
                    history[k - 1][j]
                );
            }
        }
    }
    // Sirius is not shown at sunset and is two hours on.
    assert!(history[0][0] < 0.5, "Sirius at sunset: {}", history[0][0]);
    assert!(
        history[12][0] >= 1.0,
        "Sirius two hours on: {}",
        history[12][0]
    );
    // One by one: a brighter star is shown no later than a fainter.
    let first = |j: usize| {
        history
            .iter()
            .position(|row| row[j] >= 1.0)
            .unwrap_or(usize::MAX)
    };
    for j in 1..probed.len() {
        assert!(
            first(j - 1) <= first(j),
            "star {j} shown before a brighter one"
        );
    }
}

#[test]
fn the_air_dims_and_reddens_a_star_near_the_horizon_by_its_transmittance() {
    let Some(mut gpu) = gpu() else {
        eprintln!("skipping GPU test: no usable adapter with compute");
        return;
    };
    let night = NOON + 11.0 * 3_600.0;
    let air = LutAtmosphere::today();
    let mut got = Vec::new();
    for el in [60.0, 3.0] {
        let line = towards(110.0, el);
        let sky = sky(night, sirius_along(line));
        let seen = probe(&mut gpu, &sky, &[(line, sigma())])[0];
        // At its centre a star's image is its irradiance over 2πσ², in
        // its colour, through the air, in the renderer's units.
        let (_, e, colour) = sirius();
        let s = f64::from(sigma());
        let peak = e / (2.0 * std::f64::consts::PI * s * s);
        let mu = (90.0 - el).to_radians().cos();
        let cpu = air.transmittance(LUT_RADIUS + pic().height, mu);
        let stars = seen.stars();
        for c in 0..3 {
            let want = peak
                * f64::from(colour[c])
                * f64::from(seen.transmittance[c])
                * f64::from(sky.uniform.illuminance[c]);
            assert!(
                (f64::from(stars[c]) / want - 1.0).abs() < 2e-3,
                "{el}° channel {c}: {} against {want}",
                stars[c]
            );
            // The shader's transmittance is the tables', the sun's.
            assert!(
                (f64::from(seen.transmittance[c]) / cpu[c] - 1.0).abs() < 0.02,
                "{el}°: {:?} against the CPU's {cpu:?}",
                seen.transmittance
            );
        }
        got.push((stars, seen.transmittance));
    }
    let (high, low) = (got[0], got[1]);
    for c in 0..3 {
        let ratio = low.0[c] / high.0[c];
        let want = low.1[c] / high.1[c];
        assert!(
            ratio < 1.0 && (ratio / want - 1.0).abs() < 2e-3,
            "{c}: {ratio} {want}"
        );
    }
    // Redder: blue loses more than red.
    assert!(low.0[2] / high.0[2] < low.0[0] / high.0[0]);
}

#[test]
fn the_sun_the_moon_and_the_planet_hide_the_stars_behind_them() {
    let Some(mut gpu) = gpu() else {
        eprintln!("skipping GPU test: no usable adapter with compute");
        return;
    };
    let up = enu_frame(pic()).z_axis;
    // The sun's disc, at noon, and beside it.
    let sun = Sun::at_unix_seconds(NOON).direction_ecef;
    let aside = (sun + sun.any_orthonormal_vector() * 0.02).normalize();
    for (line, hidden) in [(sun, true), (aside, false)] {
        let sky = sky(NOON, sirius_along(line));
        let s = probe(&mut gpu, &sky, &[(line, sigma())])[0].stars();
        assert_eq!(
            s.iter().all(|&v| v == 0.0),
            hidden,
            "{} the sun: {s:?}",
            if hidden { "behind" } else { "beside" }
        );
    }
    // The Moon's disc, the first night of June 2026 it stands over 12° in
    // a sky the sun is 12° under, and a disc and a half from its centre.
    let night = (0..30 * 24)
        .map(|h| NOON - 20.0 * 86_400.0 + f64::from(h) * 3_600.0)
        .find(|&t| {
            let moon = Moon::seen_from(t, pic()).map(|m| Moonlight::of(&m));
            let sun_el = Sun::at_unix_seconds(t).direction_ecef.dot(up);
            moon.is_some_and(|m| m.to_moon_ecef.dot(up) > 0.2) && sun_el < -0.2
        })
        .expect("a moonlit night");
    let moon = Moon::seen_from(night, pic())
        .map(|m| Moonlight::of(&m))
        .expect("the Moon");
    let m = moon.to_moon_ecef;
    let rho = f64::from(moon.angular_radius) * f64::from(SkyLook::default().moon_scale);
    let beside = (m + m.any_orthonormal_vector() * 1.5 * rho).normalize();
    for (line, hidden) in [(m, true), (beside, false)] {
        let sky = sky(night, sirius_along(line));
        let s = probe(&mut gpu, &sky, &[(line, sigma())])[0].stars();
        assert_eq!(
            s.iter().all(|&v| v == 0.0),
            hidden,
            "{} the Moon: {s:?}",
            if hidden { "behind" } else { "beside" }
        );
    }
    // The planet: under the horizon.
    let below = towards(110.0, -3.0);
    let sky = sky(night, sirius_along(below));
    let s = probe(&mut gpu, &sky, &[(below, sigma())])[0].stars();
    assert!(s.iter().all(|&v| v == 0.0), "under the horizon: {s:?}");
}

#[test]
fn venus_comes_out_in_the_west_before_any_star_by_the_stars_rule() {
    let Some(mut gpu) = gpu() else {
        eprintln!("skipping GPU test: no usable adapter with compute");
        return;
    };
    let sunset = Sun::next_sunset(
        NOON,
        Geodetic {
            height: 0.0,
            ..pic()
        },
    )
    .expect("a sunset");
    let up = enu_frame(pic()).z_axis;
    let mut venus_first = None;
    let mut sirius_first = None;
    for k in 0..=12 {
        let t = sunset + f64::from(k) * 600.0;
        let lights = tuile_atmosphere::planets::lights_at(t);
        let (at, colour) = lights[1];
        let venus = DVec3::new(f64::from(at[0]), f64::from(at[1]), f64::from(at[2]));
        if venus.dot(up) < 0.03 {
            break;
        }
        // Sirius, for a star of the same sky, where Venus is not: 20° to
        // the side of it, as high.
        let beside = (venus + up.cross(venus).normalize() * 0.35).normalize();
        let sky = {
            let mut s = sky(t, sirius_along(beside));
            s.uniform = s.uniform.with_planets(&lights);
            s
        };
        let seen = probe(&mut gpu, &sky, &[(venus, sigma()), (beside, sigma())]);
        // At its centre, Venus is its irradiance over 2πσ², through the
        // air, as a star is.
        let s = f64::from(sigma());
        let peak = f64::from(at[3]) / (2.0 * std::f64::consts::PI * s * s);
        let got = seen[0].stars();
        for c in 0..3 {
            let want = peak
                * f64::from(colour[c])
                * f64::from(seen[0].transmittance[c])
                * f64::from(sky.uniform.illuminance[c]);
            assert!(
                (f64::from(got[c]) / want - 1.0).abs() < 2e-3,
                "Venus, channel {c}: {} against {want}",
                got[c]
            );
        }
        let (v, sirius) = (shown(&sky, &seen[0]), shown(&sky, &seen[1]));
        eprintln!("+{:3} min: Venus {v:6.2}, Sirius {sirius:6.2}", k * 10);
        if v >= 1.0 && venus_first.is_none() {
            venus_first = Some(k);
        }
        if sirius >= 1.0 && sirius_first.is_none() {
            sirius_first = Some(k);
        }
    }
    let venus_first = venus_first.expect("Venus shows before it sets");
    assert!(
        sirius_first.is_none_or(|s| venus_first < s),
        "Venus at step {venus_first}, Sirius at {sirius_first:?}"
    );
}
