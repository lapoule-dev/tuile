// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Per-tile decode time of imagery tiles, by decoder and setting.
//!
//! Inputs are files of length-prefixed records (u32 LE + bytes), written by
//! `decode_bench_inputs.py`: the same tiles as JPEG and as WebP. One decode
//! per tile per pass, single-threaded, `PASSES` passes; per tile the fastest
//! pass is kept (a busy machine preempts), and the mean and median of those
//! per-tile minima are reported.
//!
//!     decode-bench DIR

use std::path::Path;
use std::time::Instant;

use libwebp_sys as w;

/// Passes per measurement, the best kept: `PASSES` in the environment, 5 by
/// default. A busy machine needs more.
fn passes() -> usize {
    std::env::var("PASSES").ok().and_then(|v| v.parse().ok()).unwrap_or(5)
}

fn records(path: &Path) -> Vec<Vec<u8>> {
    let raw = std::fs::read(path).expect("read");
    let mut out = Vec::new();
    let mut at = 0;
    while at + 4 <= raw.len() {
        let n = u32::from_le_bytes(raw[at..at + 4].try_into().unwrap()) as usize;
        out.push(raw[at + 4..at + 4 + n].to_vec());
        at += 4 + n;
    }
    out
}

fn bench(name: &str, tiles: &[Vec<u8>], mut f: impl FnMut(&[u8]) -> usize) {
    // Per tile, the fastest of the passes: on a machine busy with other
    // work, a whole-pass best still carries every preemption of that pass,
    // a per-tile minimum carries almost none. Reported: the mean and the
    // median of those minima.
    let mut best = vec![f64::INFINITY; tiles.len()];
    let mut check = 0usize;
    for _ in 0..passes() {
        for (i, t) in tiles.iter().enumerate() {
            let t0 = Instant::now();
            check = check.wrapping_add(f(t));
            best[i] = best[i].min(t0.elapsed().as_secs_f64());
        }
    }
    let mean = best.iter().sum::<f64>() / tiles.len() as f64;
    let mut sorted = best.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = sorted[sorted.len() / 2];
    let bytes: usize = tiles.iter().map(Vec::len).sum();
    println!(
        "{name:46} mean {:6.3} ms  median {:6.3} ms   ({} tiles, {:.1} KiB mean, check {check})",
        mean * 1e3,
        median * 1e3,
        tiles.len(),
        bytes as f64 / tiles.len() as f64 / 1024.0
    );
}

/// libwebp's advanced API: one config, output to RGBA/RGB/YUV, into a
/// preallocated buffer when `buf` is not empty.
fn libwebp(data: &[u8], mode: w::WEBP_CSP_MODE, fancy: bool, threads: bool, buf: &mut [u8]) -> usize {
    unsafe {
        let mut config: w::WebPDecoderConfig = std::mem::zeroed();
        assert!(w::WebPInitDecoderConfig(&mut config));
        config.options.no_fancy_upsampling = i32::from(!fancy);
        config.options.use_threads = i32::from(threads);
        config.output.colorspace = mode;
        let rgba = mode == w::WEBP_CSP_MODE::MODE_RGBA;
        let rgb = mode == w::WEBP_CSP_MODE::MODE_RGB;
        if (rgba || rgb) && !buf.is_empty() {
            let bpp = if rgba { 4 } else { 3 };
            config.output.is_external_memory = 1;
            config.output.u.RGBA.rgba = buf.as_mut_ptr();
            config.output.u.RGBA.stride = 256 * bpp;
            config.output.u.RGBA.size = buf.len();
        }
        let status = w::WebPDecode(data.as_ptr(), data.len(), &mut config);
        assert_eq!(status, w::VP8StatusCode::VP8_STATUS_OK);
        let n = config.output.width as usize;
        w::WebPFreeDecBuffer(&mut config.output);
        n
    }
}

fn main() {
    let dir = std::env::args().nth(1).expect("DIR");
    let dir = Path::new(&dir);
    let jpeg = records(&dir.join("jpeg.bin"));
    println!("# JPEG source");
    bench("zune-jpeg 0.5 (RGB)", &jpeg, |d| {
        let mut dec = zune_jpeg::JpegDecoder::new(zune_core::bytestream::ZCursor::new(d));
        dec.decode().expect("zune").len()
    });
    bench("jpeg-decoder 0.3 (RGB)", &jpeg, |d| {
        jpeg_decoder::Decoder::new(d).decode().expect("jpeg-decoder").len()
    });
    let mut tj = turbojpeg::Decompressor::new().expect("tj");
    let mut out = vec![0u8; 256 * 256 * 4];
    bench("libjpeg-turbo 3.2 via turbojpeg (RGBA, into)", &jpeg, |d| {
        let h = tj.read_header(d).expect("hdr");
        let img = turbojpeg::Image {
            pixels: &mut out[..],
            width: h.width,
            pitch: h.width * 4,
            height: h.height,
            format: turbojpeg::PixelFormat::RGBA,
        };
        tj.decompress(d, img).expect("tj");
        h.width
    });
    bench("libjpeg-turbo 3.2 via turbojpeg (YUV planes)", &jpeg, |d| {
        turbojpeg::decompress_to_yuv(d).expect("tj yuv").pixels.len()
    });

    for set in [
        "webp-q90-m4",
        "webp-q90-m6",
        "webp-q90-m4-f0",
        "webp-q90-m4-seg1",
        "webp-q90-m4-sharpyuv",
    ] {
        let tiles = records(&dir.join(format!("{set}.bin")));
        println!("# {set}");
        bench("image-webp 0.2.4 (pure Rust, RGB)", &tiles, |d| {
            let mut dec = image_webp::WebPDecoder::new(std::io::Cursor::new(d)).expect("iw");
            let mut buf = vec![0u8; dec.output_buffer_size().expect("size")];
            dec.read_image(&mut buf).expect("iw read");
            buf.len()
        });
        let mut none: Vec<u8> = Vec::new();
        bench("libwebp (NEON) RGBA, fancy upsampling", &tiles, |d| {
            libwebp(d, w::WEBP_CSP_MODE::MODE_RGBA, true, false, &mut none)
        });
        bench("libwebp (NEON) RGBA, no fancy upsampling", &tiles, |d| {
            libwebp(d, w::WEBP_CSP_MODE::MODE_RGBA, false, false, &mut none)
        });
        bench("libwebp (NEON) RGBA, use_threads", &tiles, |d| {
            libwebp(d, w::WEBP_CSP_MODE::MODE_RGBA, true, true, &mut none)
        });
        let mut pre = vec![0u8; 256 * 256 * 4];
        bench("libwebp (NEON) RGBA into preallocated", &tiles, |d| {
            libwebp(d, w::WEBP_CSP_MODE::MODE_RGBA, true, false, &mut pre)
        });
        let mut pre3 = vec![0u8; 256 * 256 * 3];
        bench("libwebp (NEON) RGB into preallocated", &tiles, |d| {
            libwebp(d, w::WEBP_CSP_MODE::MODE_RGB, true, false, &mut pre3)
        });
        bench("libwebp (NEON) YUV 4:2:0 planes", &tiles, |d| {
            libwebp(d, w::WEBP_CSP_MODE::MODE_YUV, true, false, &mut none)
        });
    }
}
