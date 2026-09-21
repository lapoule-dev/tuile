// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Dumps rendered frames out of a trace, as PNG files.
//!
//! A trace is the ground truth of what the viewer actually drew, and sometimes
//! the question is not "where is the black" (`scan-black`) but "show me frame
//! N so I can compare it against another renderer's output over the same
//! camera" — which is exactly the imagery-seam investigation this was written
//! for.
//!
//! ```text
//! cargo run -p tuile-tape --bin trace-dump -- trace.mcap out-prefix [every-n]
//! ```

fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let at = text.find(key)? + key.len();
    let end = text[at..].find('"')? + at;
    Some(&text[at..end])
}

fn base64_decode(text: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut table = [255u8; 256];
    for (i, c) in ALPHABET.iter().enumerate() {
        table[*c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes() {
        if byte == b'=' {
            break;
        }
        let value = table[byte as usize];
        if value == 255 {
            return Err("not base64".into());
        }
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or("usage: trace-dump <trace.mcap> <out-prefix> [every-n]")?;
    let prefix = args
        .next()
        .ok_or("usage: trace-dump <trace.mcap> <out-prefix> [every-n]")?;
    let every: usize = args.next().map_or(Ok(1), |s| s.parse())?;

    let bytes = std::fs::read(&path)?;
    let mut index = 0usize;
    let mut written = 0usize;
    for message in mcap::MessageStream::new(&bytes)? {
        let message = message?;
        if message.channel.topic != "/frame" {
            continue;
        }
        index += 1;
        if !(index - 1).is_multiple_of(every) {
            continue;
        }
        let text = std::str::from_utf8(&message.data)?;
        let png = base64_decode(field(text, "\"data\":\"").ok_or("no image data")?)?;
        let out = format!("{prefix}.{index}.png");
        std::fs::write(&out, &png)?;
        written += 1;
        println!("{out}");
    }
    println!("{written} frame(s) written out of {index} in {path}");
    Ok(())
}
