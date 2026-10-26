// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a frame's light costs: on the CPU as `LutAtmosphere::light` works
//! it out, and on the GPU as `frame_light` makes it every frame — in
//! compute, and drawn as WebGL2 draws it — with the sky-view made after it.
//!
//! GPU times are the device's own (timestamp queries) where it has them,
//! and wall time from submission to completion otherwise, less an empty
//! submission's.
//!
//! ```text
//! cargo run --release -p tuile-atmosphere --features wgpu --example frame_light_cost
//! ```

use std::time::Instant;

use tuile_atmosphere::frame_light::{FrameLightInputs, FrameLightPass};
use tuile_atmosphere::gpu::{layout, AtmosphereGpu, SkyViewPass};
use tuile_atmosphere::hillaire::{metering, Moment};
use tuile_atmosphere::{AtmosphereFrame, AtmosphereParams, LutAtmosphere, SkyLook, Sun};
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};

const RUNS: u32 = 200;

fn main() {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter =
        pollster::block_on(instance.request_adapter(&Default::default())).expect("an adapter");
    let timestamps = adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY);
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        required_features: if timestamps {
            wgpu::Features::TIMESTAMP_QUERY
        } else {
            wgpu::Features::empty()
        },
        ..Default::default()
    }))
    .expect("a device");
    let info = adapter.get_info();
    println!("{} ({:?})", info.name, info.backend);

    // The Pic du Midi at sunset, looking west.
    let at = Geodetic {
        lon: 0.1418f64.to_radians(),
        lat: 42.9365f64.to_radians(),
        height: 2_877.0,
    };
    let eye = geodetic_to_ecef(at);
    let enu = enu_frame(at);
    let forward = (-enu.x_axis * 0.998 - enu.z_axis * 0.07).normalize();
    let right = forward.cross(enu.z_axis).normalize();
    let up = right.cross(forward);
    let half = 25f64.to_radians().tan();
    let ground = Geodetic {
        height: 600.0,
        ..at
    };
    let metered = metering(
        eye,
        forward,
        right * half * 1.5,
        up * half,
        &AtmosphereFrame::at(ground),
        600.0,
    );
    let moment = Moment {
        to_sun_ecef: Sun::at_unix_seconds(1_782_071_000.0).direction_ecef,
        moon: None,
        compensation_ev: 0.0,
    };
    let look = SkyLook::default();
    let model = LutAtmosphere::of(AtmosphereParams::default());

    let start = Instant::now();
    for _ in 0..20 {
        std::hint::black_box(model.light(&model, ground, moment, &metered, look.calibration));
    }
    println!(
        "CPU   LutAtmosphere::light        {:7.3} ms a light",
        start.elapsed().as_secs_f64() * 1e3 / 20.0
    );

    for pass_kind in [SkyViewPass::Compute, SkyViewPass::Render] {
        let bind_layout = layout(&device, wgpu::ShaderStages::FRAGMENT, true);
        let mut atmosphere = AtmosphereGpu::new(&device, &bind_layout, pass_kind);
        atmosphere.hold_with(&queue, AtmosphereParams::default(), LutAtmosphere::of);
        let pass = FrameLightPass::new(&device, &atmosphere, pass_kind);
        let inputs = FrameLightInputs::new(
            &AtmosphereParams::default(),
            ground,
            eye,
            moment,
            &metered,
            look.calibration,
            look.exposure_ev,
            look.white_balance,
            [1.0; 3],
        );
        let geometry = inputs.uniform(
            &AtmosphereParams::default(),
            eye,
            glam::DMat3::IDENTITY,
            &look,
        );
        queue.write_buffer(&atmosphere.uniform, 0, bytemuck::bytes_of(&geometry));
        pass.write(&queue, &inputs);
        let time = |what: &str, record: &mut dyn FnMut(&mut wgpu::CommandEncoder)| {
            let query = timestamps.then(|| {
                let set = device.create_query_set(&wgpu::QuerySetDescriptor {
                    label: None,
                    ty: wgpu::QueryType::Timestamp,
                    count: 2,
                });
                let resolve = device.create_buffer(&wgpu::BufferDescriptor {
                    label: None,
                    size: 16,
                    usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                });
                let read = device.create_buffer(&wgpu::BufferDescriptor {
                    label: None,
                    size: 16,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                (set, resolve, read)
            });
            let mut gpu_ns = 0.0;
            let wall = Instant::now();
            for _ in 0..RUNS {
                let mut encoder = device.create_command_encoder(&Default::default());
                if let Some((set, ..)) = &query {
                    // A pass of its own that writes the first stamp…
                    drop(encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                        label: None,
                        timestamp_writes: Some(wgpu::ComputePassTimestampWrites {
                            query_set: set,
                            beginning_of_pass_write_index: Some(0),
                            end_of_pass_write_index: None,
                        }),
                    }));
                }
                record(&mut encoder);
                if let Some((set, resolve, read)) = &query {
                    // … and one after that writes the second.
                    drop(encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                        label: None,
                        timestamp_writes: Some(wgpu::ComputePassTimestampWrites {
                            query_set: set,
                            beginning_of_pass_write_index: None,
                            end_of_pass_write_index: Some(1),
                        }),
                    }));
                    encoder.resolve_query_set(set, 0..2, resolve, 0);
                    encoder.copy_buffer_to_buffer(resolve, 0, read, 0, 16);
                }
                queue.submit([encoder.finish()]);
                if let Some((_, _, read)) = &query {
                    read.slice(..).map_async(wgpu::MapMode::Read, |_| {});
                    device
                        .poll(wgpu::PollType::wait_indefinitely())
                        .expect("poll");
                    let data = read.slice(..).get_mapped_range().expect("mapped").to_vec();
                    let t: [u64; 2] = bytemuck::pod_read_unaligned(&data);
                    gpu_ns += (t[1].saturating_sub(t[0])) as f64
                        * f64::from(queue.get_timestamp_period());
                    read.unmap();
                } else {
                    device
                        .poll(wgpu::PollType::wait_indefinitely())
                        .expect("poll");
                }
            }
            let wall_ms = wall.elapsed().as_secs_f64() * 1e3 / f64::from(RUNS);
            if timestamps {
                println!(
                    "GPU   {what:28} {:7.3} ms a frame (device time), {wall_ms:.3} ms submit to done",
                    gpu_ns / 1e6 / f64::from(RUNS)
                );
            } else {
                println!("GPU   {what:28} {wall_ms:7.3} ms a frame (submit to done)");
            }
        };
        let name = match pass_kind {
            SkyViewPass::Compute => "compute",
            SkyViewPass::Render => "drawn (WebGL2's)",
        };
        time(&format!("nothing [{name}]"), &mut |_| {});
        time(&format!("light [{name}]"), &mut |e| {
            pass.record(e);
            pass.copy_into_uniform(e, &atmosphere.uniform);
        });
        time(&format!("sky-view [{name}]"), &mut |e| {
            atmosphere.sky_view_stale = true;
            atmosphere.record_sky_view(e);
        });
        time(&format!("light + sky-view [{name}]"), &mut |e| {
            pass.record(e);
            pass.copy_into_uniform(e, &atmosphere.uniform);
            atmosphere.sky_view_stale = true;
            atmosphere.record_sky_view(e);
        });
    }
}
