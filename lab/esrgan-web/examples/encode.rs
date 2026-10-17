// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! `data/weights.bin` (raw f32, from `prepare.py`) to the three files the page can load:
//! `data/weights.{f32,f16,q8}.esrw`.
//!
//!     cargo run --release --example encode

use esrgan_web::weights::{Encoding, encode};
use esrgan_web::{FEAT, NUM_CONV, SCALE};

fn main() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
    let raw = std::fs::read(dir.join("weights.bin")).expect("data/weights.bin: run prepare.py first");
    let flat: Vec<f32> = raw.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    for (name, e) in [("f32", Encoding::F32), ("f16", Encoding::F16), ("q8", Encoding::Q8)] {
        let bytes = encode(&flat, e, FEAT, NUM_CONV, SCALE).expect("encode");
        let path = dir.join(format!("weights.{name}.esrw"));
        std::fs::write(&path, &bytes).expect("write");
        println!("{} {} bytes", path.display(), bytes.len());
    }
}
