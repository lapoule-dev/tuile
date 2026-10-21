// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! PNG tiles decoded two ways, timed: the `png` crate through `image` (what
//! `tuile_core::raster::decode_image` runs today) and `zune-png`.
//!
//! Both produce tightly packed RGBA8, which is what the decode job hands on,
//! and the two outputs are checked equal before any number is printed: a
//! faster decoder that decodes differently is not a candidate.
//!
//! ```text
//! cargo run --release -p tuile-decode-bench -- <png files or directories>
//! cargo build --release -p tuile-decode-bench --target wasm32-wasip1
//! node examples/decode-bench/wasi-run.mjs target/wasm32-wasip1/release/tuile-decode-bench.wasm <dir>
//! ```
//!
//! JPEG is not compared: `image` 0.25 already decodes it with `zune-jpeg`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_png::PngDecoder;

/// Today's path: `image::load_from_memory(..).to_rgba8()`.
fn with_image(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let img = image::load_from_memory(bytes)
        .expect("png decodes")
        .to_rgba8();
    (img.width(), img.height(), img.into_raw())
}

/// `zune-png`, asked for RGBA8 as the decode job wants it.
fn with_zune(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let options = DecoderOptions::default()
        .png_set_add_alpha_channel(true)
        .png_set_strip_to_8bit(true);
    let mut decoder = PngDecoder::new_with_options(ZCursor::new(bytes), options);
    decoder.decode_headers().expect("png headers");
    let (w, h) = decoder.dimensions().expect("dimensions");
    let colour = decoder.colorspace().expect("colour space");
    let pixels = decoder.decode_raw().expect("png decodes");
    let rgba = match colour {
        ColorSpace::RGBA => pixels,
        ColorSpace::LumaA => pixels
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        other => panic!("zune-png gave {other:?} with alpha requested"),
    };
    (w as u32, h as u32, rgba)
}

fn files(args: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for a in args {
        let p = Path::new(a);
        if p.is_dir() {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(p)
                .expect("readable directory")
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")))
                .collect();
            entries.sort();
            out.extend(entries);
        } else {
            out.push(p.to_path_buf());
        }
    }
    out
}

/// A decoder: PNG bytes to width, height and tight RGBA8.
type Decode = fn(&[u8]) -> (u32, u32, Vec<u8>);

fn time(runs: u32, tiles: &[Vec<u8>], f: Decode) -> f64 {
    let start = Instant::now();
    for _ in 0..runs {
        for t in tiles {
            std::hint::black_box(f(std::hint::black_box(t)));
        }
    }
    start.elapsed().as_secs_f64() * 1e3 / (runs as f64 * tiles.len() as f64)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let paths = files(&args);
    assert!(!paths.is_empty(), "give PNG files or directories of them");
    let tiles: Vec<Vec<u8>> = paths
        .iter()
        .map(|p| std::fs::read(p).expect("readable file"))
        .collect();
    let mut texels = 0u64;
    for (path, bytes) in paths.iter().zip(&tiles) {
        let a = with_image(bytes);
        let b = with_zune(bytes);
        assert_eq!((a.0, a.1), (b.0, b.1), "{}: sizes differ", path.display());
        assert!(a.2 == b.2, "{}: the two decoders disagree", path.display());
        texels += a.0 as u64 * a.1 as u64;
    }
    let bytes: usize = tiles.iter().map(Vec::len).sum();
    println!(
        "{} PNG tiles, {:.1} KiB average, {:.0} texels average — outputs identical",
        tiles.len(),
        bytes as f64 / tiles.len() as f64 / 1024.0,
        texels as f64 / tiles.len() as f64
    );
    let runs = 5;
    // One warm-up pass each.
    time(1, &tiles, with_image);
    time(1, &tiles, with_zune);
    let image_ms = time(runs, &tiles, with_image);
    let zune_ms = time(runs, &tiles, with_zune);
    let mtex = |ms: f64| texels as f64 / tiles.len() as f64 / (ms * 1e3);
    println!(
        "png 0.18 via image 0.25: {image_ms:8.3} ms/tile  {:6.1} Mtexel/s",
        mtex(image_ms)
    );
    println!(
        "zune-png 0.5:            {zune_ms:8.3} ms/tile  {:6.1} Mtexel/s",
        mtex(zune_ms)
    );
    println!("speed-up: {:.2}×", image_ms / zune_ms);
}
