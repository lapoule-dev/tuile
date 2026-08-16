// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Finds the black frames in a trace, and says when they happened.
//!
//! A trace is minutes of pictures and nobody scrubs through them looking for a
//! hole that lasts three frames. This reads both channels, measures how much of
//! each picture is bare **where ground was supposed to be**, and prints the
//! worst — with the timestamp, so the frame can be found in a viewer and the
//! camera beside it read off the same timeline.
//!
//! # Why the camera is needed to read the pictures
//!
//! Counting black pixels alone is a trap, and it caught the first version of
//! this tool. From geostationary orbit the Earth subtends 17° in a 45° field:
//! most of the frame is legitimately sky, and a naive count reported 68 % black
//! on a frame that was perfectly drawn. The number was real and meant nothing.
//!
//! So each sampled pixel is cast as a ray and tested against the globe. Pixels
//! that miss are sky and are not counted; the answer is the fraction of the
//! pixels that *should* have shown ground and did not, which is the only
//! quantity a hole moves.
//!
//! ```text
//! cargo run --release -p tuile-tape --bin scan-black -- trace.mcap
//! ```


/// A pixel this dark on every channel is the clear colour rather than dark
/// ground. Ground lit by ambient alone still lands well above it.
const BLACK: u8 = 8;

/// Sampled rather than scanned: a hole is thousands of pixels, and reading
/// every one of a two-megapixel frame times a thousand frames is minutes for an
/// answer that does not change.
const STEP: usize = 4;

/// WGS84 equatorial radius. A sphere is enough to decide whether a ray meets
/// the planet at all — the flattening is 0.3 %, far below the resolution of the
/// sampled grid, and this is a visibility test rather than a measurement.
const RADIUS: f64 = 6_378_137.0;

/// The viewport the trace was rendered at. Aspect only; the pixel counts come
/// from the images themselves.
struct Camera {
    position: [f64; 3],
    direction: [f64; 3],
    up: [f64; 3],
    fovy: f64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: scan-black <trace.mcap>")?;
    let bytes = std::fs::read(&path)?;

    // Both channels, joined on the timeline they share. A camera message is
    // written when the camera *changes*, so the one in force for a frame is the
    // last one at or before it.
    let mut cameras: Vec<(u64, Camera)> = Vec::new();
    let mut worst: Vec<(f64, u64, f64)> = Vec::new();
    let mut frames = 0usize;

    for message in mcap::MessageStream::new(&bytes)? {
        let message = message?;
        let text = std::str::from_utf8(&message.data)?;
        match message.channel.topic.as_str() {
            "/camera" => cameras.push((
                message.log_time,
                Camera {
                    position: triple(text, "position").ok_or("no position")?,
                    direction: triple(text, "direction").ok_or("no direction")?,
                    up: triple(text, "up").ok_or("no up")?,
                    fovy: number(text, "fovy").ok_or("no fovy")?,
                },
            )),
            "/frame" => {
                frames += 1;
                let Some((_, camera)) = cameras
                    .iter()
                    .rev()
                    .find(|(t, _)| *t <= message.log_time)
                else {
                    continue;
                };
                let png = base64_decode(field(text, "\"data\":\"").ok_or("no image data")?)?;
                let image = image::load_from_memory(&png)?.to_rgba8();
                let (bare, altitude) = bare_ground_fraction(&image, camera);
                worst.push((bare, message.log_time, altitude));
            }
            _ => {}
        }
    }

    worst.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!("{frames} frames in {path}");
    println!("worst frames, counting only pixels whose ray meets the globe:");
    for (bare, log_time, altitude) in worst.iter().take(12) {
        println!(
            "  {:6.2}% bare   at {:7.3}s   altitude {:9.1} km",
            bare * 100.0,
            *log_time as f64 / 1e9,
            altitude / 1000.0
        );
    }
    Ok(())
}

/// The fraction of ground-bearing pixels that came out at the clear colour, and
/// the camera's altitude.
fn bare_ground_fraction(image: &image::RgbaImage, camera: &Camera) -> (f64, f64) {
    let (w, h) = (image.width() as usize, image.height() as usize);
    let eye = camera.position;
    let altitude = norm(eye) - RADIUS;

    // The camera basis the renderer used: forward, right, up.
    let f = normalise(camera.direction);
    let r = normalise(cross(f, camera.up));
    let u = cross(r, f);
    let aspect = w as f64 / h as f64;
    let half_v = (camera.fovy * 0.5).tan();
    let half_h = half_v * aspect;

    let (mut bare, mut ground) = (0u64, 0u64);
    for y in (0..h).step_by(STEP) {
        for x in (0..w).step_by(STEP) {
            // Normalised device coordinates, y up.
            let ndc_x = (x as f64 + 0.5) / w as f64 * 2.0 - 1.0;
            let ndc_y = 1.0 - (y as f64 + 0.5) / h as f64 * 2.0;
            let dir = normalise([
                f[0] + r[0] * ndc_x * half_h + u[0] * ndc_y * half_v,
                f[1] + r[1] * ndc_x * half_h + u[1] * ndc_y * half_v,
                f[2] + r[2] * ndc_x * half_h + u[2] * ndc_y * half_v,
            ]);
            if !hits_globe(eye, dir) {
                continue; // sky, and sky is meant to be black
            }
            ground += 1;
            let p = image.get_pixel(x as u32, y as u32).0;
            if p[0] < BLACK && p[1] < BLACK && p[2] < BLACK {
                bare += 1;
            }
        }
    }
    (bare as f64 / ground.max(1) as f64, altitude)
}

/// Whether a ray from `eye` along `dir` meets the globe in front of the camera.
fn hits_globe(eye: [f64; 3], dir: [f64; 3]) -> bool {
    let b = 2.0 * dot(eye, dir);
    let c = dot(eye, eye) - RADIUS * RADIUS;
    let discriminant = b * b - 4.0 * c;
    // Behind the camera does not count: at low altitude half the rays would
    // otherwise "hit" the planet through the ground beneath the eye.
    discriminant >= 0.0 && (-b - discriminant.sqrt()) > 0.0
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn norm(a: [f64; 3]) -> f64 {
    dot(a, a).sqrt()
}

fn normalise(a: [f64; 3]) -> [f64; 3] {
    let n = norm(a).max(1e-12);
    [a[0] / n, a[1] / n, a[2] / n]
}

/// A three-number array field.
fn triple(text: &str, key: &str) -> Option<[f64; 3]> {
    let at = text.find(&format!("\"{key}\":["))? + key.len() + 4;
    let end = text[at..].find(']')? + at;
    let mut out = [0.0; 3];
    for (slot, part) in out.iter_mut().zip(text[at..end].split(',')) {
        *slot = part.trim().parse().ok()?;
    }
    Some(out)
}

/// A scalar field.
fn number(text: &str, key: &str) -> Option<f64> {
    let at = text.find(&format!("\"{key}\":"))? + key.len() + 3;
    let rest = &text[at..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    rest[..end].trim().parse().ok()
}

/// The value of a JSON string field, without a JSON parser: the document is
/// written by this crate and has five keys.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let at = text.find(key)? + key.len();
    let end = text[at..].find('"')? + at;
    Some(&text[at..end])
}

fn base64_decode(text: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
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
