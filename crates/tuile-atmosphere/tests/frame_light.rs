// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The frame's light made on the GPU is the CPU's: `LutAtmosphere::light`
//! and the look's exposure and white balance, to what f32 sums and the
//! calibration's table round apart — by day, at sunset and by moonlight,
//! from a summit, from low orbit and from geostationary height, made in
//! compute and drawn as WebGL2 draws it.

#![cfg(feature = "wgpu")]

use glam::DVec3;
use tuile_atmosphere::frame_light::{Extra, FrameLightInputs, FrameLightPass};
use tuile_atmosphere::gpu::{layout, AtmosphereGpu, AtmosphereUniform, SkyViewPass};
use tuile_atmosphere::hillaire::{metering, Moment};
use tuile_atmosphere::{
    AtmosphereFrame, AtmosphereParams, LutAtmosphere, Moon, Moonlight, SkyLook, Sun,
};
use tuile_core::geo::{ecef_to_geodetic, enu_frame, geodetic_to_ecef, Geodetic};

fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).ok()?;
    pollster::block_on(adapter.request_device(&Default::default())).ok()
}

/// A camera: where, heading from north and pitch from the horizon, degrees.
struct Shot {
    name: &'static str,
    lon: f64,
    lat: f64,
    height: f64,
    heading: f64,
    pitch: f64,
    instant: f64,
    moon: bool,
}

const PIC: (f64, f64) = (0.1418, 42.9365);
/// 2026-06-21 12:00 UTC, its sunset at the Pic, and two hours after.
const NOON: f64 = 1_782_043_200.0;

fn sunset() -> f64 {
    Sun::next_sunset(
        NOON,
        Geodetic {
            lon: PIC.0.to_radians(),
            lat: PIC.1.to_radians(),
            height: 0.0,
        },
    )
    .expect("the sun sets")
}

fn shots() -> Vec<Shot> {
    let s = sunset();
    let shot = |name, height, pitch, instant, moon| Shot {
        name,
        lon: PIC.0,
        lat: PIC.1,
        height,
        heading: 270.0,
        pitch,
        instant,
        moon,
    };
    vec![
        shot("summit noon", 2_877.0, -4.0, NOON, false),
        shot("summit sunset", 2_877.0, -4.0, s, true),
        shot("summit night", 2_877.0, 10.0, s + 7_200.0, true),
        shot("low orbit golden hour", 300_000.0, -30.0, s - 3_600.0, true),
        shot("geostationary sunset", 36_000_000.0, -80.0, s, true),
    ]
}

struct Light {
    exposure: f32,
    white: [f32; 3],
    uniform: AtmosphereUniform,
}

fn cpu(shot: &Shot, model: &LutAtmosphere, look: &SkyLook) -> Light {
    let (at, eye, moment, metered) = setting(shot);
    let light = model.light(model, at, moment, &metered, look.calibration);
    let exposure = look.exposure_scale(&light);
    Light {
        exposure,
        white: look.white_balance_under(&light),
        uniform: AtmosphereUniform::new(&AtmosphereParams::default(), &light, eye, exposure, None, look)
            .seen_from(&light, eye),
    }
}

fn setting(shot: &Shot) -> (Geodetic, DVec3, Moment, Vec<tuile_atmosphere::hillaire::Metered>) {
    let g = Geodetic {
        lon: shot.lon.to_radians(),
        lat: shot.lat.to_radians(),
        height: shot.height,
    };
    let eye = geodetic_to_ecef(g);
    let enu = enu_frame(g);
    let (h, p) = (shot.heading.to_radians(), shot.pitch.to_radians());
    let level = enu.x_axis * h.sin() + enu.y_axis * h.cos();
    let forward = level * p.cos() + enu.z_axis * p.sin();
    let right = forward.cross(enu.z_axis).normalize();
    let up = right.cross(forward);
    let half = (25f64).to_radians().tan();
    let ground = Geodetic { height: 0.0, ..g };
    let metered = metering(
        eye,
        forward,
        right * half * 1.5,
        up * half,
        &AtmosphereFrame::at(ground),
        0.0,
    );
    let moon = shot
        .moon
        .then(|| Moon::seen_from(shot.instant, ecef_to_geodetic(eye)).map(|m| Moonlight::of(&m)))
        .flatten();
    let moment = Moment {
        to_sun_ecef: Sun::at_unix_seconds(shot.instant).direction_ecef,
        moon,
        compensation_ev: 0.0,
    };
    (ground, eye, moment, metered)
}

fn gpu_light(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    atmosphere: &AtmosphereGpu,
    pass: &FrameLightPass,
    shot: &Shot,
    look: &SkyLook,
) -> Light {
    let (at, eye, moment, metered) = setting(shot);
    let inputs = FrameLightInputs::new(
        at,
        eye,
        moment,
        &metered,
        look.calibration,
        look.exposure_ev,
        look.white_balance,
        [1.0, 1.0, 1.0],
    );
    let geometry = inputs.uniform(&AtmosphereParams::default(), eye, None, look);
    queue.write_buffer(&atmosphere.uniform, 0, bytemuck::bytes_of(&geometry));
    pass.write(queue, &inputs);
    let mut encoder = device.create_command_encoder(&Default::default());
    pass.record(&mut encoder);
    pass.copy_into_uniform(&mut encoder, &atmosphere.uniform);
    queue.submit([encoder.finish()]);
    let read = pass.read(device, queue);
    // What the frame's shaders read: the uniform itself, as the copy left it.
    let drawn = read_uniform(device, queue, &atmosphere.uniform);
    assert_eq!(drawn, read.over(geometry), "{}: the uniform holds the light", shot.name);
    let e = read.extra(Extra::Exposure);
    let w = read.extra(Extra::White);
    Light {
        exposure: e[0],
        white: [w[0], w[1], w[2]],
        uniform: read.over(geometry),
    }
}

fn read_uniform(device: &wgpu::Device, queue: &wgpu::Queue, uniform: &wgpu::Buffer) -> AtmosphereUniform {
    let size = std::mem::size_of::<AtmosphereUniform>() as u64;
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    encoder.copy_buffer_to_buffer(uniform, 0, &staging, 0, size);
    queue.submit([encoder.finish()]);
    staging.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = staging.slice(..).get_mapped_range().expect("mapped").to_vec();
    bytemuck::pod_read_unaligned(&data)
}

fn close(what: &str, gpu: [f32; 4], cpu: [f32; 4], tolerance: f32, floor: f32) {
    for c in 0..3 {
        let bound = tolerance * cpu[c].abs().max(floor);
        assert!(
            (gpu[c] - cpu[c]).abs() <= bound,
            "{what}: GPU {gpu:?} against CPU {cpu:?} (±{tolerance} of max(|cpu|, {floor}))"
        );
    }
}

fn the_gpu_light_is_the_cpus(pass_kind: SkyViewPass) {
    let Some((device, queue)) = device() else {
        eprintln!("skipping GPU test: no usable adapter");
        return;
    };
    if pass_kind == SkyViewPass::Compute && device.limits().max_compute_workgroups_per_dimension == 0
    {
        eprintln!("skipping: no compute on this device");
        return;
    }
    let bind_layout = layout(&device, wgpu::ShaderStages::FRAGMENT, true);
    let mut atmosphere = AtmosphereGpu::new(&device, &bind_layout, pass_kind);
    atmosphere.hold_with(&queue, AtmosphereParams::default(), LutAtmosphere::of);
    let pass = FrameLightPass::new(&device, &atmosphere, pass_kind);
    let model = LutAtmosphere::of(AtmosphereParams::default());
    let look = SkyLook::default();
    for shot in shots() {
        let c = cpu(&shot, &model, &look);
        let g = gpu_light(&device, &queue, &atmosphere, &pass, &shot, &look);
        let ev = (g.exposure / c.exposure).log2();
        eprintln!(
            "{:28} exposure CPU {:.5} GPU {:.5} ({ev:+.4} EV); white CPU {:?} GPU {:?}",
            shot.name, c.exposure, g.exposure, c.white, g.white
        );
        assert!(ev.abs() <= 0.02, "{}: the exposure is {ev:+.4} EV off", shot.name);
        for i in 0..3 {
            assert!(
                (g.white[i] / c.white[i] - 1.0).abs() <= 0.005,
                "{}: white balance GPU {:?} CPU {:?}",
                shot.name,
                g.white,
                c.white
            );
        }
        let (gu, cu) = (g.uniform, c.uniform);
        let scale = cu.ambient[0].abs().max(cu.ambient[1].abs());
        let name = shot.name;
        close(&format!("{name} illuminance"), gu.illuminance, cu.illuminance, 0.02, 0.0);
        close(&format!("{name} ground"), gu.ground, cu.ground, 0.02, 0.0);
        close(&format!("{name} ambient"), gu.ambient, cu.ambient, 0.02, 0.0);
        close(&format!("{name} sh_x"), gu.sh_x, cu.sh_x, 0.02, scale);
        close(&format!("{name} sh_y"), gu.sh_y, cu.sh_y, 0.02, scale);
        close(&format!("{name} sh_z"), gu.sh_z, cu.sh_z, 0.02, scale);
        close(&format!("{name} floor"), gu.floor, cu.floor, 0.02, 0.0);
        close(&format!("{name} added"), gu.added, cu.added, 0.02, 0.0);
        close(&format!("{name} least"), gu.least, cu.least, 0.02, 0.0);
        close(&format!("{name} least sky"), gu.least_sky, cu.least_sky, 0.02, 0.0);
        close(&format!("{name} level"), gu.level_ref, cu.level_ref, 0.02, 0.0);
        close(&format!("{name} grade"), gu.grade, cu.grade, 0.005, 0.0);
        // The geometry is the CPU's own, untouched by the copy.
        assert_eq!(gu.sun, cu.sun, "{name}: the sun");
        assert_eq!(gu.centre, cu.centre, "{name}: the sphere");
        assert_eq!(gu.moon, cu.moon, "{name}: the Moon");
    }
}

#[test]
fn the_compute_pass_makes_the_cpus_light() {
    the_gpu_light_is_the_cpus(SkyViewPass::Compute);
}

#[test]
fn the_drawn_pass_makes_the_cpus_light() {
    the_gpu_light_is_the_cpus(SkyViewPass::Render);
}
