// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A layer laid over a range of a film's mezzanine
//! (`tuile_farm::composite`): the range decoded, the layer laid over each
//! of its pictures, the pictures encoded again by the render's own encoder
//! into a segment the join splices in.
//!
//! ```text
//! the render's segments, the range's runs ─► decode (NVDEC / VideoToolbox) ─► I420, 8 bits ─┐
//! the layer: a still (decoded once) or a clip (decoded alongside, to the film's cadence) ────┤
//!                                                                                            ▼
//!                       each view: cover, centred, zoomed, faded ─► the render's encoder (film_sink)
//! ```
//!
//! Everything is laid in Y'CbCr, BT.709, limited range — the film's own
//! — on I420 pictures: a still is converted once from R'G'B', a clip comes
//! out of its decoder in it. Each output sample takes the layer's sample at
//! its place by a bilinear filter, then the film's and the layer's are
//! mixed by the layer's opacity. Rows are shared between the machine's
//! threads.
//!
//! The pictures go to the render's own sink ([`crate::setup::film_sink`])
//! with the render's codec and bitrate: the same encoder settings, so the
//! same stream description, which the join requires.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::sync_channel;
use std::time::Instant;

use tuile_farm::composite::{opacity, zoom_at};

use crate::decode::Picture;
use crate::Error;

/// An I420 picture: Y at full size, U and V at half.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub size: (usize, usize),
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl Image {
    /// A tightly packed I420 buffer of `size`, its planes apart.
    pub fn of_i420(i420: &[u8], size: (usize, usize)) -> Self {
        let (w, h) = size;
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let (luma, chroma) = (w * h, cw * ch);
        Self {
            size,
            y: i420[..luma].to_vec(),
            u: i420[luma..luma + chroma].to_vec(),
            v: i420[luma + chroma..luma + 2 * chroma].to_vec(),
        }
    }

    /// The planes one after the other, as an encoder takes them.
    pub fn to_i420(&self) -> Vec<u8> {
        [self.y.as_slice(), &self.u, &self.v].concat()
    }

    /// An R'G'B' picture (8 bits a channel, rows packed) in BT.709 Y'CbCr,
    /// limited range, its chroma the mean of each two by two block. An odd
    /// width or height loses its last column or row.
    pub fn of_rgb(rgb: &[u8], (width, height): (usize, usize)) -> Self {
        let (w, h) = (width & !1, height & !1);
        let (cw, ch) = (w / 2, h / 2);
        let mut y = vec![0u8; w * h];
        let mut u = vec![0u8; cw * ch];
        let mut v = vec![0u8; cw * ch];
        let at = |x: usize, row: usize| {
            let p = &rgb[(row * width + x) * 3..][..3];
            (
                f32::from(p[0]) / 255.0,
                f32::from(p[1]) / 255.0,
                f32::from(p[2]) / 255.0,
            )
        };
        let luma = |(r, g, b): (f32, f32, f32)| 0.2126 * r + 0.7152 * g + 0.0722 * b;
        for row in 0..h {
            for x in 0..w {
                y[row * w + x] = (16.0 + 219.0 * luma(at(x, row))).round() as u8;
            }
        }
        for row in 0..ch {
            for x in 0..cw {
                let (mut cb, mut cr) = (0.0f32, 0.0f32);
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let p = at(2 * x + dx, 2 * row + dy);
                    let l = luma(p);
                    cb += (p.2 - l) / 1.8556;
                    cr += (p.0 - l) / 1.5748;
                }
                u[row * cw + x] = (128.0 + 224.0 * cb / 4.0).round().clamp(16.0, 240.0) as u8;
                v[row * cw + x] = (128.0 + 224.0 * cr / 4.0).round().clamp(16.0, 240.0) as u8;
            }
        }
        Self {
            size: (w, h),
            y,
            u,
            v,
        }
    }

    /// A still picture's bytes (JPEG, PNG), as it is stored — its EXIF
    /// orientation is its maker's to have applied.
    pub fn of_still(bytes: &[u8]) -> Result<Self, Error> {
        let rgb = image::load_from_memory(bytes)?.to_rgb8();
        let size = (rgb.width() as usize, rgb.height() as usize);
        if size.0 < 2 || size.1 < 2 {
            return Err(format!("a still of {}×{}", size.0, size.1).into());
        }
        Ok(Self::of_rgb(rgb.as_raw(), size))
    }
}

/// The sample of `plane` (`width` samples a row) at `(x, y)`, sample
/// centres on whole numbers, by a bilinear filter; past the edges, the
/// edge's.
fn sample(plane: &[u8], width: usize, height: usize, x: f32, y: f32) -> f32 {
    let x = x.clamp(0.0, (width - 1) as f32);
    let y = y.clamp(0.0, (height - 1) as f32);
    let (x0, y0) = (x.floor() as usize, y.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(width - 1), (y0 + 1).min(height - 1));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let p = |x: usize, y: usize| f32::from(plane[y * width + x]);
    let top = p(x0, y0) + (p(x1, y0) - p(x0, y0)) * fx;
    let bottom = p(x0, y1) + (p(x1, y1) - p(x0, y1)) * fx;
    top + (bottom - top) * fy
}

/// Where a layer turned `rotation` degrees clockwise to be shown takes the
/// point `(u, v)` of what is shown, in its own pictures' coordinates (`size`
/// the stored picture's); continuous coordinates, 0 at the edge.
fn unturned(rotation: u16, (w, h): (f32, f32), (u, v): (f32, f32)) -> (f32, f32) {
    match rotation {
        90 => (v, h - u),
        180 => (w - u, h - v),
        270 => (w - v, u),
        _ => (u, v),
    }
}

/// How a layer covers a view: its shown size turned, the scale from what
/// is shown to the view, and the window's top left corner in what is shown.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Cover {
    rotation: u16,
    stored: (f32, f32),
    scale: f32,
    left: f32,
    top: f32,
}

impl Cover {
    fn of(layer: (usize, usize), rotation: u16, view: (usize, usize), zoom: f64) -> Self {
        let stored = (layer.0 as f32, layer.1 as f32);
        let shown = if rotation % 180 == 90 {
            (stored.1, stored.0)
        } else {
            stored
        };
        let (vw, vh) = (view.0 as f32, view.1 as f32);
        let scale = (vw / shown.0).max(vh / shown.1) * zoom as f32;
        Self {
            rotation,
            stored,
            scale,
            left: (shown.0 - vw / scale) / 2.0,
            top: (shown.1 - vh / scale) / 2.0,
        }
    }

    /// The point of the stored picture under the view's point `(x, y)`
    /// (continuous, in the view's luma samples).
    fn at(&self, x: f32, y: f32) -> (f32, f32) {
        unturned(
            self.rotation,
            self.stored,
            (self.left + x / self.scale, self.top + y / self.scale),
        )
    }
}

/// One plane of the film to lay over: its samples, its samples a row, the
/// luma samples each of its samples spans, and the layer's plane and size
/// it is laid from.
type LaidPlane<'a> = (&'a mut Vec<u8>, usize, f32, &'a [u8], (usize, usize));

/// `layer` laid over each of `views` side-by-side views of `film` (I420,
/// `size` the whole picture's), covering it, zoomed by `zoom`, at
/// `alpha`'s opacity. Rows are shared between `threads` threads.
pub fn lay(
    film: &mut Image,
    layer: &Image,
    rotation: u16,
    views: u32,
    zoom: f64,
    alpha: f32,
    threads: usize,
) {
    if alpha <= 0.0 {
        return;
    }
    let views = views.max(1) as usize;
    let (w, h) = film.size;
    let view = (w / views, h);
    let cover = Cover::of(layer.size, rotation, view, zoom);
    let (lw, lh) = layer.size;
    let (lcw, lch) = (lw.div_ceil(2), lh.div_ceil(2));
    // One plane: its samples a row, the luma samples each of its samples
    // spans, and the layer's plane it is laid from.
    let planes: [LaidPlane<'_>; 3] = [
        (&mut film.y, w, 1.0, &layer.y, (lw, lh)),
        (&mut film.u, w.div_ceil(2), 2.0, &layer.u, (lcw, lch)),
        (&mut film.v, w.div_ceil(2), 2.0, &layer.v, (lcw, lch)),
    ];
    for (plane, row, step, from, (fw, fh)) in planes {
        let view_samples = (view.0 as f32 / step).round() as usize;
        let rows_each = (plane.len() / row).div_ceil(threads.max(1)).max(1);
        std::thread::scope(|scope| {
            for (chunk, rows) in plane.chunks_mut(rows_each * row).enumerate() {
                let cover = &cover;
                scope.spawn(move || {
                    for (r, line) in rows.chunks_mut(row).enumerate() {
                        let y = (chunk * rows_each + r) as f32 * step + step / 2.0;
                        for (x, out) in line.iter_mut().enumerate() {
                            let within = x % view_samples.max(1);
                            if x / view_samples.max(1) >= views {
                                continue;
                            }
                            let vx = within as f32 * step + step / 2.0;
                            let (sx, sy) = cover.at(vx, y);
                            let laid = sample(from, fw, fh, sx / step - 0.5, sy / step - 0.5);
                            let mixed = f32::from(*out) + (laid - f32::from(*out)) * alpha;
                            *out = mixed.round().clamp(0.0, 255.0) as u8;
                        }
                    }
                });
            }
        });
    }
}

/// What is laid over the range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerFile {
    Still(PathBuf),
    Clip(PathBuf),
}

/// A composite, everything on this machine.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    /// The range's frames: runs of the render's segments, in film order,
    /// the first starting on a key frame.
    pub film: Vec<(PathBuf, Range<usize>)>,
    pub layer: LayerFile,
    pub fps: u32,
    pub fade_seconds: f64,
    pub zoom: (f64, f64),
    pub views: u32,
    /// The render's encoder and bitrate ([`crate::setup::film_sink`]).
    pub codec: String,
    pub bitrate: u32,
    /// The CUDA device of the decoders, on a card.
    pub device: usize,
}

/// What a composite made, and where its time went.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Done {
    pub frames: u64,
    /// Waiting for the film's and the clip's pictures (their decode).
    pub decode_seconds: f64,
    /// Laying the layer.
    pub lay_seconds: f64,
    /// Handing the pictures to the encoder and waiting for it to finish.
    pub encode_seconds: f64,
}

/// Pictures decoded ahead of the composite.
const AHEAD: usize = 4;

/// The film's pictures of `runs`, decoded one run after the other on a
/// thread of its own, sent down `send`.
fn decode_runs(
    runs: &[(PathBuf, Range<usize>)],
    device: usize,
    send: &std::sync::mpsc::SyncSender<Result<Picture, String>>,
) -> Result<(), String> {
    let mut shown = 0u64;
    for (path, samples) in runs {
        let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let track = tuile_mp4::film::read_track(&mut file)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("{}: no video track", path.display()))?;
        if samples.end > track.samples.len() || !track.samples[samples.start].key {
            return Err(format!(
                "{}: samples {samples:?} do not start on a key frame of {}",
                path.display(),
                track.samples.len()
            ));
        }
        crate::decode::pictures(path, track.range(samples.clone()), device, |mut p| {
            p.shown = shown;
            shown += 1;
            send.send(Ok(p))
                .map_err(|_| "the composite stopped".to_string())
        })
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The clip at `path`, decoded on a thread of its own: each picture and
/// the second it is shown at from the clip's start, sent down `send`.
fn decode_clip(
    path: &Path,
    device: usize,
    send: &std::sync::mpsc::SyncSender<Result<(f64, Picture), String>>,
) -> Result<(), String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let track = tuile_mp4::film::read_track(&mut file)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{}: no H.264 or HEVC video track", path.display()))?;
    let mut times: Vec<i64> = {
        let mut dts = 0i64;
        track
            .samples
            .iter()
            .map(|s| {
                let pts = dts + s.composition;
                dts += i64::from(s.duration);
                pts
            })
            .collect()
    };
    times.sort_unstable();
    let origin = times.first().copied().unwrap_or(0);
    let timescale = f64::from(track.timescale.max(1));
    crate::decode::pictures(path, track, device, |p| {
        let at = times.get(p.shown as usize).copied().unwrap_or(origin);
        let seconds = (at - origin) as f64 / timescale;
        send.send(Ok((seconds, p)))
            .map_err(|_| "the composite stopped".to_string())
    })
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// `job`'s range with its layer laid over, encoded into a segment at `out`
/// by the render's own encoder. Every picture of the range is decoded,
/// laid over and encoded once; the segment holds exactly the range's
/// frames, or this fails.
pub fn composite(job: &Job, out: &Path) -> Result<Done, Error> {
    let frames: u64 = job.film.iter().map(|(_, r)| r.len() as u64).sum();
    if frames == 0 {
        return Err("a composite of no frame".into());
    }
    let fps = job.fps.max(1);
    let fade = job.fade_seconds * f64::from(fps);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let still = match &job.layer {
        LayerFile::Still(path) => Some(Image::of_still(&std::fs::read(path)?)?),
        LayerFile::Clip(_) => None,
    };
    let rotation = match &job.layer {
        LayerFile::Clip(path) => {
            let mut file = std::fs::File::open(path)?;
            tuile_mp4::film::read_track(&mut file)?
                .ok_or_else(|| format!("{}: no H.264 or HEVC video track", path.display()))?
                .rotation
        }
        LayerFile::Still(_) => 0,
    };
    let mut sink = crate::setup::film_sink(
        out.to_string_lossy().into_owned(),
        job.bitrate,
        Some(job.codec.as_str()),
    )?;
    let (mut decode_seconds, mut lay_seconds, mut encode_seconds) = (0.0, 0.0, 0.0);
    std::thread::scope(|scope| -> Result<(), Error> {
        // The channels live in here: a composite that fails lets go of
        // them, and the decoders' sends fail, before the scope waits on
        // the decoders.
        let (send_film, film_pictures) = sync_channel::<Result<Picture, String>>(AHEAD);
        let (send_clip, clip_pictures) = sync_channel::<Result<(f64, Picture), String>>(AHEAD);
        let runs = &job.film;
        let device = job.device;
        scope.spawn(move || {
            let decoded = decode_runs(runs, device, &send_film);
            if let Err(e) = &decoded {
                let _ = send_film.send(Err(e.clone()));
            }
        });
        match &job.layer {
            LayerFile::Clip(path) => {
                scope.spawn(move || {
                    let decoded = decode_clip(path, device, &send_clip);
                    if let Err(e) = &decoded {
                        let _ = send_clip.send(Err(e.clone()));
                    }
                });
            }
            LayerFile::Still(_) => drop(send_clip),
        }
        // The clip's picture now, and the next one, waiting for its time.
        let mut now: Option<Image> = None;
        let mut next: Option<(f64, Image)> = None;
        let mut clip_over = still.is_some();
        let mut opened = false;
        for t in 0..frames {
            let waited = Instant::now();
            let picture = film_pictures
                .recv()
                .map_err(|_| "the film's decoder stopped")??;
            let mut film = Image::of_i420(&picture.i420, picture.size);
            let at = t as f64 / f64::from(fps) + 1e-6;
            loop {
                if next.is_none() && !clip_over {
                    match clip_pictures.recv() {
                        Ok(got) => {
                            let (seconds, p) = got?;
                            next = Some((seconds, Image::of_i420(&p.i420, p.size)));
                        }
                        // Its last picture is held.
                        Err(_) => clip_over = true,
                    }
                }
                match next.take() {
                    Some((seconds, image)) if seconds <= at || now.is_none() => now = Some(image),
                    other => {
                        next = other;
                        break;
                    }
                }
            }
            decode_seconds += waited.elapsed().as_secs_f64();
            let laying = Instant::now();
            let layer = still
                .as_ref()
                .or(now.as_ref())
                .ok_or("a clip of no picture")?;
            lay(
                &mut film,
                layer,
                rotation,
                job.views,
                zoom_at(t, frames, job.zoom),
                opacity(t, frames, fade),
                threads,
            );
            lay_seconds += laying.elapsed().as_secs_f64();
            let encoding = Instant::now();
            if !opened {
                sink.open(film.size.0 as u32, film.size.1 as u32, fps)?;
                opened = true;
            }
            sink.picture(t as u32, t as u32, &[], &film.to_i420())?;
            encode_seconds += encoding.elapsed().as_secs_f64();
        }
        Ok(())
    })?;
    let closing = Instant::now();
    sink.close()?;
    drop(sink);
    encode_seconds += closing.elapsed().as_secs_f64();
    let written = tuile_farm::concat::count_frames(out)?;
    if written != frames {
        return Err(format!("{written} frames encoded of the range's {frames}").into());
    }
    Ok(Done {
        frames,
        decode_seconds,
        lay_seconds,
        encode_seconds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(size: (usize, usize), (y, u, v): (u8, u8, u8)) -> Image {
        let (cw, ch) = (size.0.div_ceil(2), size.1.div_ceil(2));
        Image {
            size,
            y: vec![y; size.0 * size.1],
            u: vec![u; cw * ch],
            v: vec![v; cw * ch],
        }
    }

    /// BT.709 limited range: black, white and the primaries where the
    /// recommendation puts them.
    #[test]
    fn rgb_comes_in_as_bt709_limited_range() {
        let px = |rgb: [u8; 3]| {
            let picture: Vec<u8> = rgb.repeat(4);
            let i = Image::of_rgb(&picture, (2, 2));
            (i.y[0], i.u[0], i.v[0])
        };
        assert_eq!(px([0, 0, 0]), (16, 128, 128));
        assert_eq!(px([255, 255, 255]), (235, 128, 128));
        assert_eq!(px([255, 0, 0]), (63, 102, 240));
        assert_eq!(px([0, 255, 0]), (173, 42, 26));
        assert_eq!(px([0, 0, 255]), (32, 240, 118));
        let odd = Image::of_rgb(&[0u8; 3 * 5 * 3], (5, 3));
        assert_eq!(odd.size, (4, 2));
    }

    /// Opaque and unzoomed, a layer of the view's shape is the view;
    /// transparent, the film is left as it was; half, the mean of both.
    #[test]
    fn a_layer_covers_mixes_or_leaves_the_film() {
        let layer = flat((64, 36), (200, 90, 160));
        let film = flat((64, 36), (50, 128, 128));
        let mut opaque = film.clone();
        lay(&mut opaque, &layer, 0, 1, 1.0, 1.0, 3);
        assert_eq!(opaque, layer);
        let mut none = film.clone();
        lay(&mut none, &layer, 0, 1, 1.0, 0.0, 3);
        assert_eq!(none, film);
        let mut half = film.clone();
        lay(&mut half, &layer, 0, 1, 1.0, 0.5, 3);
        assert!(half.y.iter().all(|&v| v == 125), "{:?}", &half.y[..4]);
        assert!(half.u.iter().all(|&v| v == 109));
    }

    /// A layer the view's shape, its left half dark and its right half
    /// bright, over a view twice its size: the boundary at the view's
    /// middle. Zoomed in, the boundary stays in the middle (centred);
    /// a portrait layer over a landscape view is cropped top and bottom,
    /// not squeezed.
    #[test]
    fn a_layer_covers_the_view_centred_zoomed_and_cropped() {
        let (w, h) = (32, 18);
        let mut halves = flat((w, h), (16, 128, 128));
        for row in 0..h {
            for x in w / 2..w {
                halves.y[row * w + x] = 235;
            }
        }
        let mut film = flat((64, 36), (0, 128, 128));
        lay(&mut film, &halves, 0, 1, 1.0, 1.0, 2);
        let row = &film.y[18 * 64..19 * 64];
        assert!(row[..30].iter().all(|&v| v == 16), "{row:?}");
        assert!(row[34..].iter().all(|&v| v == 235), "{row:?}");
        let mut zoomed = flat((64, 36), (0, 128, 128));
        lay(&mut zoomed, &halves, 0, 1, 1.5, 1.0, 2);
        let row = &zoomed.y[18 * 64..19 * 64];
        assert!(row[..31].iter().all(|&v| v == 16) && row[33..].iter().all(|&v| v == 235));
        // Portrait, 18×32: a band across its middle, rows 14..18.
        let mut portrait = flat((18, 32), (16, 128, 128));
        for row in 14..18 {
            for x in 0..18 {
                portrait.y[row * 18 + x] = 235;
            }
        }
        let mut wide = flat((64, 36), (0, 128, 128));
        lay(&mut wide, &portrait, 0, 1, 1.0, 1.0, 2);
        // 18 wide covers 64: ×3.56, the 4-row band 14 rows high, centred.
        let column: Vec<u8> = (0..36).map(|r| wide.y[r * 64 + 10]).collect();
        let bright = column.iter().filter(|&&v| v > 128).count();
        assert!((13..=15).contains(&bright), "{column:?}");
        assert!(column[18] > 200 && column[0] == 16 && column[35] == 16);
    }

    /// Two views side by side get the same picture at the same place.
    #[test]
    fn each_view_gets_the_layer_at_the_same_place() {
        let mut layer = flat((16, 9), (16, 100, 150));
        layer.y[4 * 16 + 3] = 235;
        let mut film = flat((128, 36), (0, 128, 128));
        lay(&mut film, &layer, 0, 2, 1.0, 1.0, 4);
        for row in 0..36 {
            assert_eq!(
                film.y[row * 128..row * 128 + 64],
                film.y[row * 128 + 64..(row + 1) * 128],
                "row {row}"
            );
        }
        for row in 0..18 {
            assert_eq!(
                film.u[row * 64..row * 64 + 32],
                film.u[row * 64 + 32..(row + 1) * 64]
            );
        }
    }

    /// A clip stored on its side, turned as its header says: the stored
    /// picture's top row shown on the right (90°), on the left (270°), at
    /// the bottom (180°).
    #[test]
    fn a_turned_clip_is_shown_upright() {
        // Stored 18 wide, 32 high: its top rows bright.
        let mut stored = flat((18, 32), (16, 128, 128));
        for x in 0..18 * 4 {
            stored.y[x] = 235;
        }
        let shown = |rotation| {
            let mut film = flat((32, 18), (0, 128, 128));
            lay(&mut film, &stored, rotation, 1, 1.0, 1.0, 1);
            let mid = &film.y[9 * 32..10 * 32];
            let col: Vec<u8> = (0..18).map(|r| film.y[r * 32 + 16]).collect();
            (mid[0] > 200, mid[31] > 200, col[0] > 200, col[17] > 200)
        };
        assert_eq!(shown(90), (false, true, false, false));
        assert_eq!(shown(270), (true, false, false, false));
        // Upright, 18×32 over 32×18: cropped to the middle rows, no top.
        assert_eq!(shown(0), (false, false, false, false));
        let mut film = flat((18, 32), (0, 128, 128));
        lay(&mut film, &stored, 180, 1, 1.0, 1.0, 1);
        assert!(film.y[31 * 18 + 9] > 200 && film.y[9] < 30);
    }
}

/// End to end on the Mac's own decoder and encoder: a render, a range
/// laid over, spliced in, decoded back.
#[cfg(all(test, target_os = "macos", feature = "videotoolbox"))]
mod on_the_machine {
    use super::*;

    /// A picture that moves: a gradient sliding by two samples a frame.
    fn moving(w: usize, h: usize, f: usize) -> Vec<u8> {
        let mut i420 = Vec::with_capacity(w * h * 3 / 2);
        for y in 0..h {
            for x in 0..w {
                i420.push((32 + (x + 2 * f) % 160 + y / 8) as u8);
            }
        }
        i420.extend(std::iter::repeat_n(128u8, 2 * (w / 2) * (h / 2)));
        i420
    }

    /// Frames `pictures` of the moving film into a mezzanine at `path`,
    /// key frames forced on `keys`.
    fn render(path: &Path, (w, h): (usize, usize), pictures: Range<usize>, keys: Vec<u64>) {
        let mut sink = crate::setup::film_sink_with_keys(
            path.to_string_lossy().into_owned(),
            0,
            Some("mezzanine"),
            keys,
        )
        .expect("sink");
        sink.open(w as u32, h as u32, 30).expect("open");
        for (i, f) in pictures.enumerate() {
            sink.picture(i as u32, i as u32, &[], &moving(w, h, f))
                .expect("picture");
        }
        sink.close().expect("close");
    }

    /// A still to lay: four quadrants of colour and a white disc.
    fn still(path: &Path, (w, h): (u32, u32)) {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            let (dx, dy) = (x as f32 - w as f32 / 2.0, y as f32 - h as f32 / 2.0);
            if dx * dx + dy * dy < (h as f32 / 5.0).powi(2) {
                image::Rgb([250, 250, 250])
            } else {
                match (x < w / 2, y < h / 2) {
                    (true, true) => image::Rgb([200, 40, 40]),
                    (false, true) => image::Rgb([40, 160, 60]),
                    (true, false) => image::Rgb([40, 60, 200]),
                    (false, false) => image::Rgb([230, 200, 40]),
                }
            }
        });
        img.save(path).expect("still");
    }

    /// I420 as R'G'B', BT.709 limited range: to look at.
    fn rgb_of(i420: &[u8], (w, h): (usize, usize)) -> image::RgbImage {
        let image = Image::of_i420(i420, (w, h));
        image::RgbImage::from_fn(w as u32, h as u32, |x, y| {
            let (x, y) = (x as usize, y as usize);
            let l = (f32::from(image.y[y * w + x]) - 16.0) / 219.0;
            let cb = (f32::from(image.u[(y / 2) * (w / 2) + x / 2]) - 128.0) / 224.0;
            let cr = (f32::from(image.v[(y / 2) * (w / 2) + x / 2]) - 128.0) / 224.0;
            let r = l + 1.5748 * cr;
            let b = l + 1.8556 * cb;
            let g = (l - 0.2126 * r - 0.0722 * b) / 0.7152;
            let q = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
            image::Rgb([q(r), q(g), q(b)])
        })
    }

    fn track(path: &Path) -> tuile_mp4::film::Track {
        let mut file = std::fs::File::open(path).expect("film");
        tuile_mp4::film::read_track(&mut file)
            .expect("read")
            .expect("track")
    }

    /// Every picture of the film at `path`, I420.
    fn decoded(path: &Path) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        crate::decode::pictures(path, track(path), 0, |p| {
            out.push(p.i420);
            Ok(())
        })
        .expect("decoded");
        out
    }

    fn mean_abs(a: &[u8], b: &[u8]) -> f64 {
        a.iter()
            .zip(b)
            .map(|(&a, &b)| (f64::from(a) - f64::from(b)).abs())
            .sum::<f64>()
            / a.len() as f64
    }

    /// A render of two segments (frames 1..=40, 41..=75), key frames
    /// forced at 21 and 51; a still laid over frames 21..=50, across both:
    /// the inserted segment is the render's stream, the join splices it in,
    /// and the film decodes back — the render's pictures outside the
    /// range, the still in its middle, a fade between.
    #[test]
    fn a_still_laid_over_a_range_across_two_segments_is_spliced_in() {
        let dir = tempfile::tempdir().expect("dir");
        let (w, h) = (320, 180);
        let (a, b) = (dir.path().join("seg0.mp4"), dir.path().join("seg1.mp4"));
        render(&a, (w, h), 0..40, vec![20]);
        render(&b, (w, h), 40..75, vec![10]);
        let picture = dir.path().join("still.png");
        still(&picture, (900, 506));
        let job = Job {
            film: vec![(a.clone(), 20..40), (b.clone(), 0..10)],
            layer: LayerFile::Still(picture),
            fps: 30,
            fade_seconds: 0.25,
            zoom: (1.0, 1.08),
            views: 1,
            codec: "mezzanine".into(),
            bitrate: 0,
            device: 0,
        };
        let inserted = dir.path().join("insert.mp4");
        let done = composite(&job, &inserted).expect("composite");
        assert_eq!(done.frames, 30);
        assert_eq!(
            track(&inserted).entry,
            track(&a).entry,
            "the render's stream"
        );
        let joined = dir.path().join("film.mp4");
        let insert = tuile_farm::concat::Insert {
            first: 21,
            last: 50,
            key: "insert".into(),
        };
        let n = tuile_farm::concat::splice(&[a, b], 1, &[(insert, inserted)], &joined, false)
            .expect("spliced");
        assert_eq!(n, 75);
        let pictures = decoded(&joined);
        assert_eq!(pictures.len(), 75);
        for f in (0..20).chain(50..75) {
            let err = mean_abs(&pictures[f], &moving(w, h, f));
            assert!(err < 3.0, "frame {} off the render by {err}", f + 1);
        }
        // The still's middle, fully over: its white disc at the centre, its
        // red quadrant top left.
        let middle = &pictures[35];
        assert!(middle[(h / 2) * w + w / 2] > 220, "the disc");
        let red = Image::of_i420(middle, (w, h));
        assert!(red.v[(h / 8) * (w / 2) + w / 8] > 180, "red: Cr high");
        // The range's first frame is still the film's own; the next ones
        // fade towards the still.
        assert!(mean_abs(&pictures[20], &moving(w, h, 20)) < 3.0);
        let towards = |f: usize| mean_abs(&pictures[f], &pictures[35]);
        assert!(towards(21) > towards(24) && towards(24) > towards(30));
        if let Ok(peek) = std::env::var("TUILE_COMPOSITE_PEEK") {
            std::fs::create_dir_all(&peek).expect("peek dir");
            for f in [19, 22, 24, 35, 49, 50] {
                rgb_of(&pictures[f], (w, h))
                    .save(Path::new(&peek).join(format!("still-frame-{:02}.png", f + 1)))
                    .expect("png");
            }
        }
    }

    /// A portrait clip shorter than the range: decoded alongside, cropped
    /// to cover the landscape view, a new picture each frame, its last
    /// picture held to the end. (Its turning by the track header is
    /// `a_turned_clip_is_shown_upright`'s.)
    #[test]
    fn a_clip_is_laid_over_upright_and_its_last_picture_held() {
        let dir = tempfile::tempdir().expect("dir");
        let (w, h) = (320, 180);
        let seg = dir.path().join("seg0.mp4");
        render(&seg, (w, h), 0..60, vec![10]);
        // The clip: 12 pictures, 96 wide and 160 high as stored, each a
        // flat grey a step lighter than the last, a bright band on top.
        let clip = dir.path().join("clip.mp4");
        let (cw, ch) = (96, 160);
        let mut sink =
            crate::setup::film_sink(clip.to_string_lossy().into_owned(), 0, Some("h264"))
                .expect("sink");
        sink.open(cw as u32, ch as u32, 30).expect("open");
        for i in 0..12usize {
            let mut i420 = vec![(40 + 10 * i) as u8; cw * ch];
            for v in &mut i420[..cw * 20] {
                *v = 235;
            }
            i420.extend(std::iter::repeat_n(128u8, cw * ch / 2));
            sink.picture(i as u32, i as u32, &[], &i420)
                .expect("picture");
        }
        sink.close().expect("close");
        let job = Job {
            film: vec![(seg.clone(), 10..40)],
            layer: LayerFile::Clip(clip.clone()),
            fps: 30,
            fade_seconds: 0.0,
            zoom: (1.0, 1.0),
            views: 1,
            codec: "mezzanine".into(),
            bitrate: 0,
            device: 0,
        };
        let inserted = dir.path().join("insert.mp4");
        assert_eq!(composite(&job, &inserted).expect("composite").frames, 30);
        let pictures = decoded(&inserted);
        // The band on top is cropped off the landscape view.
        let at = |f: usize, x: usize, y: usize| pictures[f][y * w + x];
        assert!(
            (at(0, w / 2, h / 2) as i32 - 40).abs() < 6,
            "{}",
            at(0, w / 2, h / 2)
        );
        assert!((at(5, w / 2, h / 2) as i32 - 90).abs() < 6);
        assert_eq!(pictures[11].len(), pictures[29].len());
        assert!(
            (at(29, w / 2, h / 2) as i32 - 150).abs() < 6,
            "the last held"
        );
    }

    /// The composite's time for a few seconds at 1080p and 4K, a still
    /// over a mezzanine, on this machine: `cargo test --release ...
    /// composite_bench -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn composite_bench() {
        for (w, h, seconds) in [(1920, 1080, 5usize), (3840, 2160, 5)] {
            let dir = tempfile::tempdir().expect("dir");
            let seg = dir.path().join("seg.mp4");
            let frames = seconds * 30;
            render(&seg, (w, h), 0..frames + 2, vec![1]);
            let picture = dir.path().join("still.jpg");
            still(&picture, (2 * w as u32, 2 * h as u32));
            let job = Job {
                film: vec![(seg, 1..frames + 1)],
                layer: LayerFile::Still(picture),
                fps: 30,
                fade_seconds: 0.5,
                zoom: (1.0, 1.08),
                views: 1,
                codec: "mezzanine".into(),
                bitrate: 0,
                device: 0,
            };
            let began = Instant::now();
            let done = composite(&job, &dir.path().join("i.mp4")).expect("composite");
            eprintln!(
                "composite {w}×{h}, {seconds} s ({} frames): {:.2} s — waiting on decode {:.2} s, laying {:.2} s, encode {:.2} s",
                done.frames,
                began.elapsed().as_secs_f64(),
                done.decode_seconds,
                done.lay_seconds,
                done.encode_seconds
            );
        }
    }
}
