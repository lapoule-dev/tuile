// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The recording's readback: pictures come back exactly as drawn, under the
//! tag they were captured with, in RGBA whatever the texture's order, and a
//! ring with every slot in flight drops a frame rather than waiting for one.

use tuile_wgpu::{GpuContext, ReadbackRing};

fn gpu() -> Option<GpuContext> {
    match pollster::block_on(GpuContext::headless()) {
        Ok(gpu) => Some(gpu),
        Err(e) => {
            assert!(
                std::env::var("TUILE_REQUIRE_GPU").is_err(),
                "TUILE_REQUIRE_GPU is set and there is no usable adapter: {e}"
            );
            eprintln!("SKIP readback ring test: {e}");
            None
        }
    }
}

/// A texture of `format` holding `pixels` (in the texture's own channel
/// order), readable by a copy.
fn texture(
    gpu: &GpuContext,
    format: wgpu::TextureFormat,
    (w, h): (u32, u32),
    pixels: &[u8],
) -> wgpu::Texture {
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ring test"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * 4),
            rows_per_image: Some(h),
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    texture
}

/// Every pixel different, rows not a multiple of 256 bytes wide: a shear
/// or a swapped channel cannot hide.
fn pattern((w, h): (u32, u32), seed: u32) -> Vec<u8> {
    (0..w * h)
        .flat_map(|p| {
            let v = p.wrapping_mul(2_654_435_761).wrapping_add(seed);
            [v as u8, (v >> 8) as u8, (v >> 16) as u8, 255]
        })
        .collect()
}

fn capture(gpu: &GpuContext, ring: &mut ReadbackRing, texture: &wgpu::Texture, tag: u64) -> bool {
    let size = (texture.width(), texture.height());
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    let taken = ring.capture(gpu, &mut encoder, texture, size, tag);
    gpu.queue.submit([encoder.finish()]);
    ring.submitted();
    taken
}

#[test]
fn a_frame_comes_back_as_drawn_under_its_tag() {
    let Some(gpu) = gpu() else { return };
    let size = (77, 31);
    let mut ring = ReadbackRing::new(3);
    let drawn: Vec<Vec<u8>> = (0..3).map(|n| pattern(size, n)).collect();
    for (n, pixels) in drawn.iter().enumerate() {
        let t = texture(&gpu, wgpu::TextureFormat::Rgba8Unorm, size, pixels);
        assert!(capture(&gpu, &mut ring, &t, 100 + n as u64));
    }
    let frames = ring.ready(&gpu, true);
    assert_eq!(frames.len(), 3);
    for (n, frame) in frames.into_iter().enumerate() {
        assert_eq!(frame.tag, 100 + n as u64);
        assert_eq!(frame.size, size);
        let rgba = frame.read(1).expect("reads");
        assert!(rgba == drawn[n], "frame {n} did not come back as drawn");
    }
}

#[test]
fn a_bgra_surface_comes_back_in_rgba_order() {
    let Some(gpu) = gpu() else { return };
    let size = (5, 3);
    let rgba = pattern(size, 7);
    let bgra: Vec<u8> = rgba
        .chunks(4)
        .flat_map(|p| [p[2], p[1], p[0], p[3]])
        .collect();
    let t = texture(&gpu, wgpu::TextureFormat::Bgra8Unorm, size, &bgra);
    let mut ring = ReadbackRing::new(1);
    assert!(capture(&gpu, &mut ring, &t, 0));
    let frame = ring.ready(&gpu, true).pop().expect("mapped");
    assert_eq!(frame.read(1).expect("reads"), rgba);
}

#[test]
fn a_step_averages_its_block() {
    let Some(gpu) = gpu() else { return };
    // 4×2: two 2×2 blocks, each of values 10, 20, 30, 40 on every channel.
    let block = [10u8, 20, 30, 40];
    let pixels: Vec<u8> = (0..8)
        .flat_map(|p| {
            let (x, y) = (p % 4, p / 4);
            let v = block[(x % 2 + 2 * y) as usize];
            [v, v, v, 255]
        })
        .collect();
    let t = texture(&gpu, wgpu::TextureFormat::Rgba8Unorm, (4, 2), &pixels);
    let mut ring = ReadbackRing::new(1);
    assert!(capture(&gpu, &mut ring, &t, 0));
    let frame = ring.ready(&gpu, true).pop().expect("mapped");
    assert_eq!(
        frame.read(2).expect("reads"),
        vec![25, 25, 25, 255, 25, 25, 25, 255]
    );
}

/// With every slot in flight a frame is dropped and counted — never
/// waited for — and a slot read is free again.
#[test]
fn a_full_ring_drops_rather_than_waits() {
    let Some(gpu) = gpu() else { return };
    let size = (16, 16);
    let t = texture(&gpu, wgpu::TextureFormat::Rgba8Unorm, size, &pattern(size, 1));
    let mut ring = ReadbackRing::new(2);
    assert!(capture(&gpu, &mut ring, &t, 0));
    assert!(capture(&gpu, &mut ring, &t, 1));
    assert!(!capture(&gpu, &mut ring, &t, 2));
    assert_eq!(ring.dropped(), 1);
    let frames = ring.ready(&gpu, true);
    assert_eq!(frames.len(), 2);
    // Handed out but not read: still not free.
    assert!(!capture(&gpu, &mut ring, &t, 3));
    for frame in frames {
        frame.read(1).expect("reads");
    }
    assert!(capture(&gpu, &mut ring, &t, 4));
    assert_eq!(ring.ready(&gpu, true).pop().expect("mapped").tag, 4);
}

/// What a recording costs the frame: the copy recorded, the mapping asked
/// for and the finished frames collected — never a wait — held under a
/// bound on a 1920×1080 frame, read back off the render thread.
#[test]
fn recording_a_frame_costs_the_frame_little() {
    let Some(gpu) = gpu() else { return };
    let size = (1920, 1080);
    let t = texture(&gpu, wgpu::TextureFormat::Rgba8Unorm, size, &pattern(size, 3));
    let mut ring = ReadbackRing::new(6);
    // Three readers, as the viewer's recording has.
    let (frames_tx, frames_rx) = std::sync::mpsc::channel::<tuile_wgpu::MappedFrame>();
    let frames_rx = std::sync::Arc::new(std::sync::Mutex::new(frames_rx));
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let frames_rx = std::sync::Arc::clone(&frames_rx);
            std::thread::spawn(move || {
                let mut read = 0;
                loop {
                    let frame = frames_rx.lock().expect("the queue").recv();
                    let Ok(frame) = frame else { return read };
                    frame.read(2).expect("reads");
                    read += 1;
                }
            })
        })
        .collect();
    let mut costs = Vec::new();
    for n in 0..120 {
        let began = std::time::Instant::now();
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        ring.capture(&gpu, &mut encoder, &t, size, n);
        gpu.queue.submit([encoder.finish()]);
        ring.submitted();
        for frame in ring.ready(&gpu, false) {
            frames_tx.send(frame).expect("the reader");
        }
        costs.push(began.elapsed().as_secs_f64() * 1000.0);
        // A frame at 60 Hz.
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
    for frame in ring.ready(&gpu, true) {
        frames_tx.send(frame).expect("the reader");
    }
    drop(frames_tx);
    let read: u32 = readers
        .into_iter()
        .map(|r| r.join().expect("a reader"))
        .sum();
    costs.sort_by(f64::total_cmp);
    let median = costs[costs.len() / 2];
    let worst = costs[costs.len() * 95 / 100];
    eprintln!(
        "recording a 1920x1080 frame: median {median:.3} ms, p95 {worst:.3} ms on the render thread; {read} read, {} dropped, {} failed",
        ring.dropped(),
        ring.failed()
    );
    assert!(median < 2.0, "median {median:.3} ms");
    assert!(worst < 5.0, "p95 {worst:.3} ms");
    assert!(read >= 100, "{read} of 120 frames read back");
}
