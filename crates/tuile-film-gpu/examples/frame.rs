// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! One frame of a pack, rendered headless to a PNG — and the numbers behind
//! it: what the imagery holds, which way the ground faces, how it is lit.
//!
//! ```bash
//! cargo run --release -p tuile-film-gpu --example frame -- pack frame out.png [scale] [decoded|stored]
//! ```
//!
//! The pack may be a prefix of one (a cache's first chunks): tiles whose
//! bytes are past the end are skipped and counted.

use glam::{DVec3, Vec3};
use tuile_film::{blob_start, texture_of_span, FrameCamera, Imagery, Look, Mesh, Pack, TileKey};
use tuile_film_gpu::{FilmGpu, Settings, TileMesh};

fn f32s(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: frame <pack> <frame> <out.png> [scale]");
    let frame: u32 = args
        .next()
        .and_then(|f| f.parse().ok())
        .expect("a frame number");
    let out = args.next().expect("an output path");
    let scale: f32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0.25);

    let bytes = std::fs::read(&path).expect("read");
    let start = blob_start(&bytes).expect("a pack");
    let pack = Pack::open_table(&bytes[..start as usize]).expect("table");
    let view = pack.view_of(frame).expect("view");
    let width = ((view.viewport_px[0] as f32 * scale) as u32 / 8).max(1) * 8;
    let height = ((view.viewport_px[1] as f32 * scale) as u32 / 8).max(1) * 8;
    // How imagery is read can be overridden; the rest is the default look.
    let look = match args.next().as_deref() {
        Some("decoded") => Look {
            imagery: Imagery::Decoded,
            ..Look::default()
        },
        Some("stored") => Look {
            imagery: Imagery::AsStored,
            ..Look::default()
        },
        _ => Look::default(),
    };

    let (device, queue) = pollster::block_on(async {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&Default::default())
            .await
            .expect("adapter");
        adapter
            .request_device(&wgpu::DeviceDescriptor {
                required_limits: adapter.limits(),
                ..Default::default()
            })
            .await
            .expect("device")
    });
    let mut film = FilmGpu::new(
        device,
        queue,
        Settings {
            width,
            height,
            supersample: 2,
            look,
        },
    );

    let eye = DVec3::from_array(view.position);
    let up = eye.normalize().as_vec3();
    let (mut keys, mut skipped) = (Vec::new(), 0);
    let (mut stored, mut texels) = ([0.0f64; 3], 0u64);
    let (mut n_up, mut n_z, mut normals, mut bare) = (0.0f64, 0.0f64, 0u64, 0u32);
    for tile in pack.frame(frame).expect("frame") {
        let Some(span) = pack.span_of(&tile) else {
            continue;
        };
        if start + span.end > bytes.len() as u64 {
            skipped += 1;
            continue;
        }
        let part = &bytes[(start + span.start) as usize..(start + span.end) as usize];
        let mesh = Mesh::of_span(&pack, &tile, span.start, part).expect("mesh");
        if mesh.normals.is_empty() {
            bare += 1;
        }
        for n in f32s(&mesh.normals).chunks_exact(3) {
            let n = Vec3::new(n[0], n[1], n[2]);
            n_up += f64::from(n.dot(up));
            n_z += f64::from(n.z);
            normals += 1;
        }
        let texture = texture_of_span(&pack, &tile, span.start, part)
            .expect("texture")
            .map(|png| {
                let rgba = image::load_from_memory(&png).expect("png").to_rgba8();
                for p in rgba.pixels() {
                    for c in 0..3 {
                        stored[c] += f64::from(p[c]);
                    }
                    texels += 1;
                }
                let t = film.create_albedo(rgba.width(), rgba.height());
                film.write_rgba(&t, &rgba);
                t
            });
        let key = TileKey::of(&tile);
        film.enter(
            key,
            &TileMesh {
                origin_ecef: mesh.origin_ecef,
                positions: &mesh.positions,
                normals: &mesh.normals,
                uvs: &mesh.uvs,
                indices: &mesh.indices,
                index_count: mesh.index_count,
                base_color_factor: mesh.base_color_factor,
            },
            texture,
        )
        .expect("enter");
        keys.push(key);
    }

    let camera = FrameCamera::of(&view, width as f32 / height as f32);
    let mut encoder = film.render(&camera, &keys).expect("render");
    let padded = (width * 4).div_ceil(256) * 256;
    let buffer = film.device().create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(padded * height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
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
    film.queue().submit([encoder.finish()]);
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    film.device()
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let data = buffer.slice(..).get_mapped_range();
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for row in data.chunks(padded as usize) {
        rgba.extend_from_slice(&row[..(width * 4) as usize]);
    }
    let mean: Vec<f64> = (0..3)
        .map(|c| {
            rgba.chunks_exact(4).map(|p| f64::from(p[c])).sum::<f64>() / f64::from(width * height)
        })
        .collect();
    image::save_buffer(&out, &rgba, width, height, image::ColorType::Rgba8).expect("save");

    let t = texels.max(1) as f64;
    println!(
        "frame {frame}: {} tiles drawn, {skipped} past the end of the file, {width}×{height}",
        keys.len()
    );
    println!(
        "imagery as stored, mean sRGB: {:.1} {:.1} {:.1}",
        stored[0] / t,
        stored[1] / t,
        stored[2] / t
    );
    println!(
        "normals: {normals} vertices ({bare} tiles without), mean n·up {:.3}, mean n·Z {:.3}; up·to_sun {:.3}, up.z {:.3}",
        n_up / normals.max(1) as f64,
        n_z / normals.max(1) as f64,
        up.dot(look.to_sun),
        up.z
    );
    println!(
        "look: world {:?}, sun {:?}, to_sun {:?}, exposure ×{:.3}, imagery {:?}",
        look.world,
        look.sun,
        look.to_sun,
        look.exposure_scale(),
        look.imagery
    );
    println!(
        "picture, mean sRGB: {:.1} {:.1} {:.1} → {out}",
        mean[0], mean[1], mean[2]
    );
}
