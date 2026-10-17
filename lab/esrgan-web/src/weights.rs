// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The weights file the page downloads: the network's tensors in forward order, in one of three encodings.
//!
//! - `F32`: the published values, raw little-endian;
//! - `F16`: every value rounded to IEEE half (round to nearest even);
//! - `Q8`: each convolution weight as signed 8-bit integers with one symmetric f32 scale per output channel,
//!   biases and PReLU slopes kept in f32 (they are 0.6 % of the parameters).
//!
//! The file starts with an 8-byte header: `ESRW`, a version byte (1), the encoding byte, two zero bytes.
//! Every encoding decodes to the same flat f32 layout `Compact::from_flat` reads; the quantisation is weight-only,
//! and the network computes on what the decoder gives back.

use burn::tensor::f16;

const MAGIC: &[u8; 4] = b"ESRW";
const VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    F32 = 0,
    F16 = 1,
    Q8 = 2,
}

impl Encoding {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "f32" => Some(Self::F32),
            "f16" => Some(Self::F16),
            "q8" => Some(Self::Q8),
            _ => None,
        }
    }
}

/// One tensor of the flat layout: its length and, for a convolution weight, its output channels.
#[derive(Clone, Copy, Debug)]
struct Part {
    len: usize,
    conv_out: Option<usize>,
}

/// The tensors `Compact::from_flat` takes, in its order.
fn layout(feat: usize, num_conv: usize, scale: usize) -> Vec<Part> {
    let mut parts = Vec::new();
    for i in 0..num_conv + 2 {
        let (cin, cout) = match i {
            0 => (3, feat),
            i if i == num_conv + 1 => (feat, 3 * scale * scale),
            _ => (feat, feat),
        };
        parts.push(Part { len: cout * cin * 9, conv_out: Some(cout) });
        parts.push(Part { len: cout, conv_out: None });
        if i <= num_conv {
            parts.push(Part { len: cout, conv_out: None });
        }
    }
    parts
}

/// The flat f32 weights, encoded.
pub fn encode(flat: &[f32], encoding: Encoding, feat: usize, num_conv: usize, scale: usize) -> Result<Vec<u8>, String> {
    let parts = layout(feat, num_conv, scale);
    let total: usize = parts.iter().map(|p| p.len).sum();
    if total != flat.len() {
        return Err(format!("{} weights, the network has {total}", flat.len()));
    }
    let mut out = Vec::with_capacity(8 + flat.len() * 4);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&[VERSION, encoding as u8, 0, 0]);
    let mut at = 0;
    for part in parts {
        let values = &flat[at..at + part.len];
        at += part.len;
        match (encoding, part.conv_out) {
            (Encoding::F16, _) => values.iter().for_each(|&v| out.extend_from_slice(&f16::from_f32(v).to_le_bytes())),
            (Encoding::Q8, Some(cout)) => {
                let per = part.len / cout;
                let scales: Vec<f32> = values.chunks(per).map(|row| row.iter().fold(0f32, |m, v| m.max(v.abs())) / 127.0).collect();
                scales.iter().for_each(|s| out.extend_from_slice(&s.to_le_bytes()));
                for (row, &s) in values.chunks(per).zip(&scales) {
                    out.extend(row.iter().map(|&v| if s == 0.0 { 0u8 } else { ((v / s).round().clamp(-127.0, 127.0) as i8) as u8 }));
                }
            }
            _ => values.iter().for_each(|v| out.extend_from_slice(&v.to_le_bytes())),
        }
    }
    Ok(out)
}

/// The encoded weights, back to the flat f32 layout.
pub fn decode(bytes: &[u8], feat: usize, num_conv: usize, scale: usize) -> Result<Vec<f32>, String> {
    if bytes.len() < 8 || &bytes[..4] != MAGIC || bytes[4] != VERSION {
        return Err("not an ESRW v1 weights file".into());
    }
    let encoding = match bytes[5] {
        0 => Encoding::F32,
        1 => Encoding::F16,
        2 => Encoding::Q8,
        e => return Err(format!("unknown encoding {e}")),
    };
    let mut body = &bytes[8..];
    let mut take = |n: usize| -> Result<&[u8], String> {
        if body.len() < n {
            return Err(format!("weights file ends {} bytes short", n - body.len()));
        }
        let (head, rest) = body.split_at(n);
        body = rest;
        Ok(head)
    };
    let f32s = |b: &[u8]| -> Vec<f32> { b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect() };
    let mut flat = Vec::new();
    for part in layout(feat, num_conv, scale) {
        match (encoding, part.conv_out) {
            (Encoding::F16, _) => {
                flat.extend(take(part.len * 2)?.chunks_exact(2).map(|c| f16::from_le_bytes([c[0], c[1]]).to_f32()))
            }
            (Encoding::Q8, Some(cout)) => {
                let scales = f32s(take(cout * 4)?);
                let q = take(part.len)?;
                let per = part.len / cout;
                for (row, s) in q.chunks(per).zip(scales) {
                    flat.extend(row.iter().map(|&b| f32::from(b as i8) * s));
                }
            }
            _ => flat.extend(f32s(take(part.len * 4)?)),
        }
    }
    if !body.is_empty() {
        return Err(format!("{} bytes left over in the weights file", body.len()));
    }
    Ok(flat)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A small network keeps the test fast: feat 8, 2 body convolutions, scale 2.
    const SHAPE: (usize, usize, usize) = (8, 2, 2);

    fn flat() -> Vec<f32> {
        let n: usize = layout(SHAPE.0, SHAPE.1, SHAPE.2).iter().map(|p| p.len).sum();
        // values spread like a trained conv's: mostly small, a few large, signs mixed
        (0..n).map(|i| ((i as f32 * 0.618).sin() * 0.3) * if i % 97 == 0 { 8.0 } else { 1.0 }).collect()
    }

    fn roundtrip(e: Encoding) -> (Vec<f32>, Vec<f32>) {
        let w = flat();
        let bytes = encode(&w, e, SHAPE.0, SHAPE.1, SHAPE.2).unwrap();
        (w, decode(&bytes, SHAPE.0, SHAPE.1, SHAPE.2).unwrap())
    }

    #[test]
    fn f32_is_exact() {
        let (w, d) = roundtrip(Encoding::F32);
        assert_eq!(w, d);
    }

    #[test]
    fn f16_is_within_half_an_ulp() {
        let (w, d) = roundtrip(Encoding::F16);
        for (a, b) in w.iter().zip(&d) {
            // f16 has an 11-bit significand: relative error at most 2^-11 for normal values
            assert!((a - b).abs() <= a.abs() * 2f32.powi(-11) + 6e-8, "{a} -> {b}");
        }
    }

    #[test]
    fn q8_is_within_half_a_step_per_channel_and_keeps_biases_exact() {
        let w = flat();
        let (_, d) = roundtrip(Encoding::Q8);
        let mut at = 0;
        for part in layout(SHAPE.0, SHAPE.1, SHAPE.2) {
            let (a, b) = (&w[at..at + part.len], &d[at..at + part.len]);
            at += part.len;
            match part.conv_out {
                None => assert_eq!(a, b, "biases and slopes stay f32"),
                Some(cout) => {
                    for (ra, rb) in a.chunks(part.len / cout).zip(b.chunks(part.len / cout)) {
                        let step = ra.iter().fold(0f32, |m, v| m.max(v.abs())) / 127.0;
                        for (x, y) in ra.iter().zip(rb) {
                            assert!((x - y).abs() <= step * 0.5 + 1e-7, "{x} -> {y}, step {step}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn q8_is_a_quarter_of_f32() {
        let w = flat();
        let f = encode(&w, Encoding::F32, SHAPE.0, SHAPE.1, SHAPE.2).unwrap().len();
        let q = encode(&w, Encoding::Q8, SHAPE.0, SHAPE.1, SHAPE.2).unwrap().len();
        assert!((q as f64) < 0.3 * f as f64, "q8 {q} bytes, f32 {f}");
    }

    #[test]
    fn truncated_or_foreign_files_are_refused() {
        let bytes = encode(&flat(), Encoding::Q8, SHAPE.0, SHAPE.1, SHAPE.2).unwrap();
        assert!(decode(&bytes[..bytes.len() - 1], SHAPE.0, SHAPE.1, SHAPE.2).is_err());
        assert!(decode(b"PK\x03\x04....", SHAPE.0, SHAPE.1, SHAPE.2).is_err());
    }
}
