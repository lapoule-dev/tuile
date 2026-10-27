// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The recorded pictures as a film: an mp4 by the film's own encoders,
//! with, if asked, the frame, the altitude, the heading and the instant
//! written into each picture's corner.

use tuile_film_native::Sink;

use crate::Row;

/// The encoder a film is made with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// H.264 by the Mac's own encoder (VideoToolbox): plays everywhere.
    H264,
    /// AV1 by rav1e: Rust alone, any machine.
    Av1,
}

impl Codec {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "h264" => Ok(Self::H264),
            "av1" => Ok(Self::Av1),
            _ => Err(format!("a codec is h264 or av1, not {text:?}")),
        }
    }

    /// H.264 where the machine has an encoder for it, AV1 elsewhere.
    pub fn native() -> Self {
        if cfg!(target_os = "macos") {
            Self::H264
        } else {
            Self::Av1
        }
    }
}

/// A film being written, a picture at a time.
pub struct Film {
    sink: Box<dyn Sink>,
    /// The film's size, once open; the size it opens at.
    size: Option<(u32, u32)>,
    wanted: Option<(u32, u32)>,
    fps: u32,
    burn_in: bool,
    frames: u32,
}

impl Film {
    /// A film at `path`, of `size` — the first picture's when `None`;
    /// every picture is fitted into it, its shape kept.
    pub fn create(
        path: &std::path::Path,
        codec: Codec,
        fps: u32,
        burn_in: bool,
        size: Option<(u32, u32)>,
    ) -> Result<Self, String> {
        let sink: Box<dyn Sink> = match codec {
            #[cfg(target_os = "macos")]
            Codec::H264 => Box::new(tuile_film_native::VtFilm::at(
                path,
                0,
                tuile_film_native::VtCodec::H264,
            )),
            #[cfg(not(target_os = "macos"))]
            Codec::H264 => return Err("H.264 is encoded by a Mac's VideoToolbox; use av1".into()),
            Codec::Av1 => Box::new(tuile_film_native::Av1Film::at(path, 0)),
        };
        Ok(Self {
            sink,
            size: None,
            wanted: size.map(|(w, h)| (w & !1, h & !1)),
            fps: fps.max(1),
            burn_in,
            frames: 0,
        })
    }

    /// Appends `rgba` of `size`, the picture of `row`, fitted to the first
    /// picture's size: a film has one.
    pub fn push(&mut self, row: &Row, rgba: &[u8], size: (u32, u32)) -> Result<(), String> {
        // Even sides: 4:2:0 halves both.
        let even = (size.0 & !1, size.1 & !1);
        if self.size.is_none() {
            let open = self.wanted.unwrap_or(even);
            self.sink
                .open(open.0, open.1, self.fps)
                .map_err(|e| e.to_string())?;
            self.size = Some(open);
        }
        let open = self.size.unwrap_or(even);
        // A window resized during the session: its pictures are fitted
        // into the film's size, their shape kept, bars around them.
        let mut rgba = if even == open {
            crop(rgba, size, even)
        } else {
            fit(rgba, size, open)
                .ok_or_else(|| format!("frame {}: a picture of the wrong length", row.frame))?
        };
        let even = open;
        if self.burn_in {
            burn(&mut rgba, even, &caption(row));
        }
        let i420 = i420(&rgba, even);
        self.sink
            .picture(self.frames, self.frames, &rgba, &i420)
            .map_err(|e| e.to_string())?;
        self.frames += 1;
        Ok(())
    }

    /// Closes the film; says how many pictures it holds.
    pub fn finish(mut self) -> Result<u32, String> {
        if self.size.is_some() {
            self.sink.close().map_err(|e| e.to_string())?;
        }
        Ok(self.frames)
    }
}

fn fit(rgba: &[u8], size: (u32, u32), into: (u32, u32)) -> Option<Vec<u8>> {
    let picture = image::RgbaImage::from_raw(size.0, size.1, rgba.to_vec())?;
    let k = (f64::from(into.0) / f64::from(size.0)).min(f64::from(into.1) / f64::from(size.1));
    let (w, h) = (
        ((f64::from(size.0) * k).round() as u32).clamp(1, into.0),
        ((f64::from(size.1) * k).round() as u32).clamp(1, into.1),
    );
    let small = image::imageops::resize(&picture, w, h, image::imageops::FilterType::Triangle);
    let mut out = image::RgbaImage::from_pixel(into.0, into.1, image::Rgba([0, 0, 0, 255]));
    image::imageops::overlay(
        &mut out,
        &small,
        i64::from((into.0 - w) / 2),
        i64::from((into.1 - h) / 2),
    );
    Some(out.into_raw())
}

fn crop(rgba: &[u8], size: (u32, u32), to: (u32, u32)) -> Vec<u8> {
    if size == to {
        return rgba.to_vec();
    }
    let (w, tw) = (size.0 as usize, to.0 as usize);
    (0..to.1 as usize)
        .flat_map(|y| rgba[y * w * 4..y * w * 4 + tw * 4].iter().copied())
        .collect()
}

/// Planar BT.709, limited range, 4:2:0 — what the film's encoders take.
pub fn i420(rgba: &[u8], (w, h): (u32, u32)) -> Vec<u8> {
    let (w, h) = (w as usize, h as usize);
    let (cw, ch) = (w / 2, h / 2);
    let mut out = vec![0u8; w * h + 2 * cw * ch];
    let (luma, chroma) = out.split_at_mut(w * h);
    let (u_plane, v_plane) = chroma.split_at_mut(cw * ch);
    let px = |x: usize, y: usize| {
        let i = (y * w + x) * 4;
        (
            f32::from(rgba[i]),
            f32::from(rgba[i + 1]),
            f32::from(rgba[i + 2]),
        )
    };
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = px(x, y);
            let yv = 16.0 + (0.2126 * r + 0.7152 * g + 0.0722 * b) * 219.0 / 255.0;
            luma[y * w + x] = yv.round().clamp(0.0, 255.0) as u8;
        }
    }
    for y in 0..ch {
        for x in 0..cw {
            let mut s = (0.0, 0.0, 0.0);
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (r, g, b) = px(2 * x + dx, 2 * y + dy);
                s = (s.0 + r / 4.0, s.1 + g / 4.0, s.2 + b / 4.0);
            }
            let (r, g, b) = s;
            let u = 128.0 + (-0.1146 * r - 0.3854 * g + 0.5 * b) * 224.0 / 255.0;
            let v = 128.0 + (0.5 * r - 0.4542 * g - 0.0458 * b) * 224.0 / 255.0;
            u_plane[y * cw + x] = u.round().clamp(0.0, 255.0) as u8;
            v_plane[y * cw + x] = v.round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// What is written in a picture's corner.
pub fn caption(row: &Row) -> String {
    let time = if row.instant.is_finite() {
        tuile_atmosphere::clock::format_rfc3339(row.instant)
    } else {
        String::new()
    };
    format!(
        "F{} ALT {:.0}M HDG {:.0} {}",
        row.frame, row.altitude, row.heading, time
    )
    .to_uppercase()
}

/// Glyphs three pixels wide and five high, a row a nibble: digits, the
/// few letters the caption uses, and its punctuation.
fn glyph(c: char) -> [u8; 5] {
    match c {
        '0' => [7, 5, 5, 5, 7],
        '1' => [2, 6, 2, 2, 7],
        '2' => [7, 1, 7, 4, 7],
        '3' => [7, 1, 7, 1, 7],
        '4' => [5, 5, 7, 1, 1],
        '5' => [7, 4, 7, 1, 7],
        '6' => [7, 4, 7, 5, 7],
        '7' => [7, 1, 1, 1, 1],
        '8' => [7, 5, 7, 5, 7],
        '9' => [7, 5, 7, 1, 7],
        'A' => [2, 5, 7, 5, 5],
        'D' => [6, 5, 5, 5, 6],
        'F' => [7, 4, 6, 4, 4],
        'G' => [7, 4, 5, 5, 7],
        'H' => [5, 5, 7, 5, 5],
        'L' => [4, 4, 4, 4, 7],
        'M' => [5, 7, 7, 5, 5],
        'T' => [7, 2, 2, 2, 2],
        'Z' => [7, 1, 2, 4, 7],
        'B' => [6, 5, 6, 5, 6],
        'C' => [7, 4, 4, 4, 7],
        'E' => [7, 4, 6, 4, 7],
        'I' => [7, 2, 2, 2, 7],
        'K' => [5, 5, 6, 5, 5],
        'N' => [6, 5, 5, 5, 5],
        'O' => [7, 5, 5, 5, 7],
        'P' => [7, 5, 7, 4, 4],
        'R' => [7, 5, 6, 5, 5],
        'S' => [7, 4, 7, 1, 7],
        'U' => [5, 5, 5, 5, 7],
        'W' => [5, 5, 7, 7, 5],
        'X' => [5, 5, 2, 5, 5],
        'Y' => [5, 5, 2, 2, 2],
        ':' => [0, 2, 0, 2, 0],
        '-' => [0, 0, 7, 0, 0],
        '.' => [0, 0, 0, 0, 2],
        '+' => [0, 2, 7, 2, 0],
        _ => [0; 5],
    }
}

/// Writes `text` into the top-left corner of `rgba`, white on a dark band.
pub fn burn(rgba: &mut [u8], (w, h): (u32, u32), text: &str) {
    let (w, h) = (w as usize, h as usize);
    let k = (h / 240).max(1);
    let (band_w, band_h) = ((text.chars().count() * 4 + 2) * k, 7 * k);
    for y in 0..band_h.min(h) {
        for x in 0..band_w.min(w) {
            let i = (y * w + x) * 4;
            for c in &mut rgba[i..i + 3] {
                *c /= 3;
            }
        }
    }
    for (n, c) in text.chars().enumerate() {
        let g = glyph(c);
        for (row, bits) in g.iter().enumerate() {
            for col in 0..3 {
                if bits & (4 >> col) == 0 {
                    continue;
                }
                for dy in 0..k {
                    for dx in 0..k {
                        let (x, y) = ((1 + n * 4 + col) * k + dx, (1 + row) * k + dy);
                        if x < w && y < h {
                            let i = (y * w + x) * 4;
                            rgba[i..i + 3].copy_from_slice(&[255, 255, 255]);
                        }
                    }
                }
            }
        }
    }
}
