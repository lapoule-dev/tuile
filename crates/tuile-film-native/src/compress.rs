// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The final stage of a film (`tuile_farm::compress`): its joined segments
//! decoded and encoded once more, by a delivery profile, into the file a
//! viewer gets.
//!
//! ```text
//! joined film (HEVC Main10 mezzanine)
//!   └─ NVDEC, pictures in the order they are shown (P010 or NV12)
//!        └─ 10 → 8 bits, dithered, for an 8-bit profile (H.264)
//!             └─ ONE NVENC session for the whole film
//!                  └─ a film laid out for a viewer (faststart, BT.709, ctts)
//! ```
//!
//! Decoding runs on a thread of its own, a few pictures ahead of the
//! encoder. A profile with a size cap whose film comes out larger is
//! encoded a second time, at the mean bitrate that fits.
//!
//! The encoder does not take ten bits down to eight (its own conversion is
//! eight to ten only), so that is done here, with a dither: each sample's
//! two dropped bits become a probability of rounding up, from a fixed
//! pattern of thresholds, so that a smooth gradient — the sky's — keeps
//! its mean instead of breaking into bands. The pattern does not move
//! from frame to frame: noise that moves costs the encoder bits.

use std::path::Path;

use crate::encode::{Input, Rate, Spec};

/// Thresholds of the dither, 0 to 3, over a tile of 64 × 64 samples:
/// interleaved gradient noise, which spreads neighbouring thresholds apart
/// (no clumps, no visible grid).
fn thresholds() -> &'static [u8; 64 * 64] {
    static TABLE: std::sync::OnceLock<[u8; 64 * 64]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u8; 64 * 64];
        for y in 0..64 {
            for x in 0..64 {
                let v = (0.067_110_56 * x as f64 + 0.005_837_15 * y as f64).fract();
                let noise = (52.982_918_9 * v).fract();
                table[y * 64 + x] = (noise * 4.0).floor().clamp(0.0, 3.0) as u8;
            }
        }
        table
    })
}

/// A P016 picture (16-bit little-endian samples, the stream's ten bits in
/// the high ones — as a decoder hands a 10-bit stream) as NV12, dithered.
/// Rows tightly packed, `width`×`height`, luma then interleaved chroma.
pub fn p016_to_nv12(p016: &[u8], (width, height): (usize, usize)) -> Vec<u8> {
    let table = thresholds();
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    let rows = [(width, height, 0usize), (2 * cw, ch, 1)];
    let mut out = Vec::with_capacity(width * height + 2 * cw * ch);
    let mut at = 0usize;
    for (row, count, plane) in rows {
        for y in 0..count {
            let line = &p016[at..at + 2 * row];
            at += 2 * row;
            let pattern = &table[((y + 32 * plane) % 64) * 64..][..64];
            for (x, sample) in line.as_chunks::<2>().0.iter().enumerate() {
                let ten = u16::from_le_bytes(*sample) >> 6;
                let eight = (ten + u16::from(pattern[x % 64])) >> 2;
                out.push(eight.min(255) as u8);
            }
        }
    }
    out
}

/// A film's video track, read for decoding: each sample in Annex B, the
/// parameter sets ahead of every key frame.
pub struct Stream {
    track: tuile_mp4::film::Track,
    /// The entry's parameter sets, start codes and all.
    sets: Vec<u8>,
    /// The same, each NAL unit apart.
    units: Vec<Vec<u8>>,
    /// Each sample's decoding and presentation times, in ticks.
    times: Vec<(i64, i64)>,
    /// The frame shown at each presentation time.
    shown: std::collections::HashMap<i64, u64>,
    /// Bytes of a NAL unit's length in a sample.
    length: usize,
    pub hevc: bool,
    /// Bits a sample of the stream.
    pub bit_depth: u8,
}

impl Stream {
    pub fn of(track: tuile_mp4::film::Track) -> Result<Self, crate::Error> {
        let mut units: Vec<Vec<u8>> = Vec::new();
        let mut start = |unit: &[u8], sets: &mut Vec<u8>| {
            sets.extend([0, 0, 0, 1]);
            sets.extend_from_slice(unit);
            units.push(unit.to_vec());
        };
        let mut sets = Vec::new();
        let (length, hevc, bit_depth) = match &track.entry {
            tuile_mp4::film::Entry::Avc1(e) => {
                for unit in e
                    .avcc
                    .sequence_parameter_sets
                    .iter()
                    .chain(&e.avcc.picture_parameter_sets)
                {
                    start(unit, &mut sets);
                }
                let depth = e.avcc.ext.as_ref().map_or(8, |x| x.bit_depth_luma);
                (usize::from(e.avcc.length_size), false, depth)
            }
            tuile_mp4::film::Entry::Hvc1(e) => {
                if e.lhvc.is_some() {
                    return Err(
                        "a spatial film's two layers are delivered as they are, not decoded".into(),
                    );
                }
                for array in &e.hvcc.arrays {
                    for unit in &array.nalus {
                        start(unit, &mut sets);
                    }
                }
                (
                    usize::from(e.hvcc.length_size_minus_one) + 1,
                    true,
                    e.hvcc.bit_depth_luma_minus8 + 8,
                )
            }
            _ => return Err("only H.264 and HEVC films are decoded here".into()),
        };
        let mut dts = 0i64;
        let times: Vec<(i64, i64)> = track
            .samples
            .iter()
            .map(|s| {
                let t = (dts, dts + s.composition);
                dts += i64::from(s.duration);
                t
            })
            .collect();
        let mut order: Vec<i64> = times.iter().map(|&(_, pts)| pts).collect();
        order.sort_unstable();
        order.dedup();
        if order.len() != times.len() {
            return Err("two samples shown at the same time".into());
        }
        let shown = order.into_iter().zip(0u64..).collect();
        Ok(Self {
            track,
            sets,
            units,
            times,
            shown,
            length,
            hevc,
            bit_depth,
        })
    }

    /// The entry's parameter sets, each NAL unit as it is (no start code).
    pub fn parameter_sets(&self) -> &[Vec<u8>] {
        &self.units
    }

    /// Bytes of a NAL unit's length in a sample.
    pub fn length_size(&self) -> usize {
        self.length
    }

    /// Ticks a second of the track's times.
    pub fn timescale(&self) -> u32 {
        self.track.timescale
    }

    /// Sample `n`'s decoding and presentation times, in ticks, and its
    /// duration.
    pub fn timing(&self, n: usize) -> (i64, i64, u32) {
        let (dts, pts) = self.times[n];
        (dts, pts, self.track.samples[n].duration)
    }

    /// The frame of the film shown at presentation time `pts` (ticks), from
    /// 0: frames counted in the order they are shown.
    pub fn shown_at(&self, pts: i64) -> Option<u64> {
        self.shown.get(&pts).copied()
    }

    /// Sample `n` (decoding order) from `file`, as the film holds it:
    /// length-prefixed NAL units.
    pub fn sample(
        &self,
        file: &mut (impl std::io::Read + std::io::Seek),
        n: usize,
    ) -> Result<Vec<u8>, crate::Error> {
        let s = self.track.samples[n];
        let mut sample = vec![0u8; s.size as usize];
        file.seek(std::io::SeekFrom::Start(s.offset))?;
        file.read_exact(&mut sample)?;
        Ok(sample)
    }

    pub fn frames(&self) -> usize {
        self.track.samples.len()
    }

    pub fn size(&self) -> Option<(usize, usize)> {
        self.track
            .width_height()
            .map(|(w, h)| (usize::from(w), usize::from(h)))
    }

    /// Sample `n` (decoding order) from `file`, in Annex B.
    pub fn access_unit(
        &self,
        file: &mut (impl std::io::Read + std::io::Seek),
        n: usize,
    ) -> Result<Vec<u8>, crate::Error> {
        let s = self.track.samples[n];
        let sample = self.sample(file, n)?;
        let mut unit = Vec::with_capacity(sample.len() + self.sets.len() + 16);
        if s.key {
            unit.extend_from_slice(&self.sets);
        }
        let mut at = 0;
        while at + self.length <= sample.len() {
            let len = sample[at..at + self.length]
                .iter()
                .fold(0usize, |n, &b| n << 8 | usize::from(b));
            at += self.length;
            let end = (at + len).min(sample.len());
            unit.extend([0, 0, 0, 1]);
            unit.extend_from_slice(&sample[at..end]);
            at = end;
        }
        Ok(unit)
    }
}

/// `profile` with the encoder settings `var` names changed — tuning, for
/// calibration runs, never a switch between paths: `TUILE_COMPRESS_QUALITY`,
/// `_PRESET`, `_B_FRAMES`, `_LOOKAHEAD`, `_SPATIAL_AQ`, `_TEMPORAL_AQ`,
/// `_MULTIPASS` (0 or 1). A setting changed is said on stderr, and the
/// file's key does not change with it: a tuned run is a trial.
pub fn tuned(
    mut profile: tuile_farm::compress::Profile,
    var: impl Fn(&str) -> Option<String>,
) -> Result<tuile_farm::compress::Profile, crate::Error> {
    let key = profile.key();
    let number = |name: &str| -> Result<Option<u8>, crate::Error> {
        match var(name) {
            Some(v) if !v.trim().is_empty() => {
                let n = v.trim().parse().map_err(|e| format!("{name}={v}: {e}"))?;
                eprintln!("compress {key}: {name}={n}");
                Ok(Some(n))
            }
            _ => Ok(None),
        }
    };
    if let Some(n) = number("TUILE_COMPRESS_QUALITY")? {
        profile.quality = n;
    }
    if let Some(n) = number("TUILE_COMPRESS_PRESET")? {
        profile.preset = n;
    }
    if let Some(n) = number("TUILE_COMPRESS_B_FRAMES")? {
        profile.b_frames = n;
    }
    if let Some(n) = number("TUILE_COMPRESS_LOOKAHEAD")? {
        profile.lookahead = n;
    }
    if let Some(n) = number("TUILE_COMPRESS_SPATIAL_AQ")? {
        profile.spatial_aq = n > 0;
    }
    if let Some(n) = number("TUILE_COMPRESS_TEMPORAL_AQ")? {
        profile.temporal_aq = n > 0;
    }
    if let Some(n) = number("TUILE_COMPRESS_MULTIPASS")? {
        profile.multipass = n > 0;
    }
    Ok(profile)
}

/// What a compression made.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Compressed {
    pub frames: u64,
    pub bytes: u64,
    /// Encodes of the whole film; 0 for a copy.
    pub passes: u32,
    pub seconds: f64,
}

/// The film at `joined` delivered at `out` as `delivery` says, on CUDA
/// device `device`; `fps` the film's.
pub fn compress(
    joined: &Path,
    out: &Path,
    delivery: &tuile_farm::compress::Delivery,
    fps: u32,
    device: usize,
) -> Result<Compressed, crate::Error> {
    let began = std::time::Instant::now();
    match delivery {
        tuile_farm::compress::Delivery::Copy => {
            // The joined film is already laid out for a viewer: copied as
            // it is.
            let frames = tuile_mp4::film::join(&[joined.to_path_buf()], out)?;
            Ok(Compressed {
                frames,
                bytes: std::fs::metadata(out)?.len(),
                passes: 0,
                seconds: began.elapsed().as_secs_f64(),
            })
        }
        tuile_farm::compress::Delivery::Encode { profile } => {
            let profile = tuned(profile.clone(), |name| std::env::var(name).ok())?;
            let (written, passes) = by_the_machine(joined, out, &profile, fps, device)?;
            Ok(Compressed {
                frames: written.frames,
                bytes: written.bytes,
                passes,
                seconds: began.elapsed().as_secs_f64(),
            })
        }
    }
}

/// The film at `joined` delivered by `profile` at `out` by this build's
/// decoder and encoder: NVDEC and NVENC on CUDA device `device`, or the
/// Mac's VideoToolbox. Returns the film and the encodes it took.
#[allow(unused_variables)]
fn by_the_machine(
    joined: &Path,
    out: &Path,
    profile: &tuile_farm::compress::Profile,
    fps: u32,
    device: usize,
) -> Result<(tuile_mp4::film::Written, u32), crate::Error> {
    #[cfg(all(target_os = "linux", feature = "nvenc"))]
    {
        gpu::encode_profile(joined, out, profile, fps, device)
    }
    #[cfg(all(target_os = "macos", feature = "videotoolbox"))]
    {
        crate::videotoolbox::compress_profile(joined, out, profile, fps)
    }
    #[cfg(not(any(
        all(target_os = "linux", feature = "nvenc"),
        all(target_os = "macos", feature = "videotoolbox")
    )))]
    {
        Err("this build decodes and encodes with no card: no compression but a copy".into())
    }
}

/// What a profile's encoder is handed from a stream of `stream_bits` bits:
/// ten bits as they are to a ten-bit profile; to an eight-bit one, eight,
/// dithered ([`p016_to_nv12`]).
pub fn input_for(stream_bits: u8, profile: &tuile_farm::compress::Profile) -> Input {
    if stream_bits > 8 && profile.bit_depth > 8 {
        Input::P010
    } else {
        Input::Nv12
    }
}

/// A film of `frames` pictures of `size` at `fps` delivered by `profile`,
/// whichever backend encodes it: `pass` encodes the whole film once by the
/// spec it is given. A film over the profile's size cap is encoded again,
/// at the mean rate that fits the cap (its ceiling half as high again,
/// never above the profile's); an encoder that still misses it is asked
/// again, its mean lowered by what it missed, up to [`PASSES`] encodes in
/// all. Returns the film written and the encodes it took.
pub fn deliver(
    profile: &tuile_farm::compress::Profile,
    input: Input,
    size: (usize, usize),
    fps: u32,
    frames: usize,
    mut pass: impl FnMut(&Spec) -> Result<tuile_mp4::film::Written, crate::Error>,
) -> Result<(tuile_mp4::film::Written, u32), crate::Error> {
    let fps = fps.max(1);
    let seconds = frames as f64 / f64::from(fps);
    let mut written = pass(&Spec::of(profile, input, size, fps, None))?;
    let mut passes = 1;
    let Some(cap) = profile.size_cap_bytes else {
        return Ok((written, passes));
    };
    let (_, ceiling) = profile.bitrates(size.0 as u32, size.1 as u32, f64::from(fps));
    let mut average = profile.capped_bitrate(seconds).ok_or("a cap of no rate")?;
    while written.bytes > cap {
        if passes == PASSES {
            return Err(format!(
                "compress {}: {} bytes after {passes} encodes, over the cap of {cap}",
                profile.key(),
                written.bytes
            )
            .into());
        }
        if passes > 1 {
            // Missed by so much: asked for that much less, and a little more.
            average = (f64::from(average) * cap as f64 / written.bytes as f64 * 0.97) as u32;
        }
        eprintln!(
            "compress {}: {} bytes over the cap of {cap}: again at {average} b/s",
            profile.key(),
            written.bytes
        );
        let rate = Rate::Vbr {
            average,
            max: ceiling.min(average.saturating_mul(3) / 2).max(average),
        };
        written = pass(&Spec::of(profile, input, size, fps, Some(rate)))?;
        passes += 1;
    }
    Ok((written, passes))
}

/// The most encodes a delivery takes to fit its cap.
pub const PASSES: u32 = 4;

#[cfg(all(target_os = "linux", feature = "nvenc"))]
mod gpu {
    use std::path::Path;
    use std::sync::mpsc::sync_channel;

    use cudarc::driver::CudaContext;
    use nvidia_video_codec_sdk::{DecodeCodec, Decoder};
    use tuile_farm::compress::Profile;

    use super::{deliver, input_for, p016_to_nv12, Stream};
    use crate::nvenc::{encode, Frame, Input, Package, Spec};

    /// Pictures decoded ahead of the encoder.
    const AHEAD: usize = 8;

    /// The film at `joined` delivered by `profile` at `out`, decoded by
    /// NVDEC and encoded by NVENC on CUDA device `device`.
    pub(super) fn encode_profile(
        joined: &Path,
        out: &Path,
        profile: &Profile,
        fps: u32,
        device: usize,
    ) -> Result<(tuile_mp4::film::Written, u32), crate::Error> {
        let mut file = std::fs::File::open(joined)?;
        let track =
            tuile_mp4::film::read_track(&mut file)?.ok_or("no video track in the joined film")?;
        let stream = Stream::of(track)?;
        let size = stream.size().ok_or("a track of no size")?;
        let input = input_for(stream.bit_depth, profile);
        deliver(profile, input, size, fps, stream.frames(), |spec| {
            pass(joined, &stream, out, spec, device)
        })
    }

    /// One encode of the whole film: decoded on a thread of its own, the
    /// pictures handed to one encoder session in the order they are shown.
    fn pass(
        joined: &Path,
        stream: &Stream,
        out: &Path,
        spec: &Spec,
        device: usize,
    ) -> Result<tuile_mp4::film::Written, crate::Error> {
        let (send, pictures) = sync_channel::<Result<Frame, String>>(AHEAD);
        let mut package = Package::at(out, spec.out, spec.size, spec.fps)?;
        let input = spec.input;
        let size = spec.size;
        std::thread::scope(|scope| -> Result<tuile_mp4::film::Written, crate::Error> {
            let decoding = scope.spawn(move || -> Result<(), String> {
                let decoded = (|| -> Result<(), crate::Error> {
                    let ctx = CudaContext::new(device).map_err(|e| format!("CUDA device {device}: {e:?}"))?;
                    let codec = if stream.hevc { DecodeCodec::Hevc } else { DecodeCodec::H264 };
                    let mut decoder = Decoder::new(ctx, codec)?;
                    let mut file = std::fs::File::open(joined)?;
                    let mut shown = 0u64;
                    let mut hand = |pictures: Vec<nvidia_video_codec_sdk::Picture>| -> Result<(), crate::Error> {
                        for picture in pictures {
                            if (picture.width as usize, picture.height as usize) != size {
                                return Err(format!(
                                    "a {}×{} picture in a {}×{} film",
                                    picture.width, picture.height, size.0, size.1
                                )
                                .into());
                            }
                            let data = match (input, picture.bit_depth > 8) {
                                (Input::P010, true) | (Input::Nv12, false) => picture.data,
                                (Input::Nv12, true) => p016_to_nv12(&picture.data, size),
                                _ => return Err("an 8-bit picture for a 10-bit input".into()),
                            };
                            if send.send(Ok(Frame { data, shown, key: false })).is_err() {
                                return Err("the encoder stopped".into());
                            }
                            shown += 1;
                        }
                        Ok(())
                    };
                    for n in 0..stream.frames() {
                        let unit = stream.access_unit(&mut file, n)?;
                        hand(decoder.decode(&unit, n as i64)?)?;
                    }
                    hand(decoder.flush()?)?;
                    if shown != stream.frames() as u64 {
                        return Err(format!("{shown} pictures decoded of {}", stream.frames()).into());
                    }
                    Ok(())
                })();
                if let Err(e) = &decoded {
                    let _ = send.send(Err(e.to_string()));
                }
                decoded.map_err(|e| e.to_string())
            });
            let encoded = encode(device, spec, pictures, |e| package.take(e));
            let decoded = decoding.join().map_err(|_| "the decoder panicked")?;
            // The encoder's failure first: a decoder told the encoder
            // stopped says nothing of why.
            encoded?;
            decoded?;
            Ok(package.finish()?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p016(values: &[u16]) -> Vec<u8> {
        values.iter().flat_map(|v| (v << 6).to_le_bytes()).collect()
    }

    /// Ten bits to eight keeps a flat area's mean — a quarter step of the
    /// eight-bit scale shows as a quarter of its samples a step up — where
    /// a plain cut would drop it, and never passes 255.
    #[test]
    fn ten_bits_dithered_to_eight_keep_their_mean() {
        let size = (64, 64);
        let (w, h) = size;
        let chroma = 2 * (w / 2) * (h / 2);
        // 513 is 128.25 in eight bits.
        let picture = p016(&vec![513u16; w * h + chroma]);
        let eight = p016_to_nv12(&picture, size);
        assert_eq!(eight.len(), w * h + chroma);
        let luma = &eight[..w * h];
        let mean = luma.iter().map(|&v| f64::from(v)).sum::<f64>() / luma.len() as f64;
        assert!((mean - 128.25).abs() < 0.02, "{mean}");
        assert!(luma.iter().all(|&v| v == 128 || v == 129));
        let top = p016_to_nv12(&p016(&vec![1023u16; w * h + chroma]), size);
        assert!(top.iter().all(|&v| v == 255));
        let exact = p016_to_nv12(&p016(&vec![512u16; w * h + chroma]), size);
        assert!(
            exact.iter().all(|&v| v == 128),
            "a whole step stays where it is"
        );
    }

    /// A gradient of a few eight-bit steps across a picture: its ten-bit
    /// ramp comes out as a ramp, not as stairs of four-sample-wide bands.
    #[test]
    fn a_smooth_gradient_stays_smooth_in_eight_bits() {
        let (w, h) = (256usize, 64usize);
        let chroma = 2 * (w / 2) * (h / 2);
        let mut values: Vec<u16> = (0..h)
            .flat_map(|_| (0..w).map(|x| 400 + (x as u16) / 16))
            .collect();
        values.extend(std::iter::repeat_n(512u16, chroma));
        let eight = p016_to_nv12(&p016(&values), (w, h));
        // Column means follow the ten-bit ramp to a tenth of a step.
        for x in (0..w).step_by(16) {
            let column: f64 = (0..h).map(|y| f64::from(eight[y * w + x])).sum::<f64>() / h as f64;
            let truth = f64::from(400 + (x as u16) / 16) / 4.0;
            assert!(
                (column - truth).abs() < 0.15,
                "column {x}: {column} for {truth}"
            );
        }
    }

    /// A film's samples read back in Annex B: start codes in place of the
    /// lengths, the parameter sets ahead of each key frame only.
    #[test]
    fn samples_are_read_back_as_annex_b_with_their_parameter_sets() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("f.mp4");
        let (sps, pps) = ([0x67u8, 0x64, 0x00, 0x28, 0xac], [0x68u8, 0xeb]);
        let entry = tuile_mp4::film::avc1(64, 48, &sps, &pps).expect("entry");
        let mut w = tuile_mp4::film::FilmWriter::create(&path, entry, 30).expect("writer");
        w.push(&[0, 0, 0, 2, 0x65, 1, 0, 0, 0, 1, 0x06], true, 0)
            .expect("push");
        w.push(&[0, 0, 0, 2, 0x41, 2], false, 1).expect("push");
        w.finish().expect("finish");
        let mut file = std::fs::File::open(&path).expect("film");
        let track = tuile_mp4::film::read_track(&mut file)
            .expect("read")
            .expect("track");
        let stream = Stream::of(track).expect("stream");
        assert_eq!(
            (stream.hevc, stream.bit_depth, stream.frames()),
            (false, 8, 2)
        );
        assert_eq!(stream.size(), Some((64, 48)));
        let first = stream.access_unit(&mut file, 0).expect("first");
        assert_eq!(
            first,
            [
                &[0u8, 0, 0, 1][..],
                &sps,
                &[0, 0, 0, 1],
                &pps,
                &[0, 0, 0, 1, 0x65, 1],
                &[0, 0, 0, 1, 0x06]
            ]
            .concat()
        );
        assert_eq!(
            stream.access_unit(&mut file, 1).expect("second"),
            [0, 0, 0, 1, 0x41, 2]
        );
    }

    /// A film under its profile's cap is encoded once at the profile's
    /// constant quality; over it, once more at the mean rate that fits the
    /// cap, under a ceiling no higher than the profile's.
    #[test]
    fn a_film_over_its_cap_is_encoded_again_at_the_rate_that_fits() {
        let mut web = tuile_farm::compress::Profile::h264();
        web.size_cap_bytes = Some(1_000_000);
        let mut specs = Vec::new();
        let (written, passes) = deliver(&web, Input::Nv12, (720, 1280), 30, 300, |spec| {
            specs.push(spec.clone());
            let bytes = if specs.len() == 1 { 3_000_000 } else { 900_000 };
            Ok(tuile_mp4::film::Written { frames: 300, bytes })
        })
        .expect("delivered");
        assert_eq!((passes, written.bytes), (2, 900_000));
        assert!(matches!(specs[0].rate, Rate::Quality { quality: 24, .. }));
        let Rate::Vbr { average, max } = specs[1].rate else {
            panic!("{:?}", specs[1].rate)
        };
        assert_eq!(Some(average), web.capped_bitrate(10.0));
        assert!(max >= average && max <= 5_000_000, "{max}");
        let mut asked = 0;
        let (_, passes) = deliver(&web, Input::Nv12, (720, 1280), 30, 300, |_| {
            asked += 1;
            Ok(tuile_mp4::film::Written {
                frames: 300,
                bytes: 999_999,
            })
        })
        .expect("delivered");
        assert_eq!((passes, asked), (1, 1));
        // An encoder that misses the mean it is asked for: asked again,
        // lower by what it missed, until the film fits.
        let mut specs = Vec::new();
        let sizes = [3_000_000u64, 1_100_000, 950_000];
        let (written, passes) = deliver(&web, Input::Nv12, (720, 1280), 30, 300, |spec| {
            specs.push(spec.clone());
            Ok(tuile_mp4::film::Written {
                frames: 300,
                bytes: sizes[specs.len() - 1],
            })
        })
        .expect("delivered");
        assert_eq!((passes, written.bytes), (3, 950_000));
        let mean = |s: &Spec| match s.rate {
            Rate::Vbr { average, .. } => average,
            Rate::Quality { .. } => panic!("a capped pass at a mean"),
        };
        assert!(
            f64::from(mean(&specs[2])) < f64::from(mean(&specs[1])) * 0.92,
            "{} after {}",
            mean(&specs[2]),
            mean(&specs[1])
        );
        assert_eq!(
            input_for(10, &tuile_farm::compress::Profile::hevc_main10()),
            Input::P010
        );
        assert_eq!(input_for(10, &web), Input::Nv12);
    }

    /// A film whose frames are shown in another order than decoded: each
    /// sample's times as the file says them, and the frame each is shown as.
    #[test]
    fn a_reordered_film_s_samples_say_the_frame_each_is_shown_as() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("b.mp4");
        let entry = tuile_mp4::film::avc1(64, 48, &[0x67, 0x64, 0, 0x28], &[0x68]).expect("entry");
        let mut w = tuile_mp4::film::FilmWriter::create(&path, entry, 30).expect("writer");
        // Decoded I P B B: shown 0 3 1 2.
        for (i, shown) in [0u64, 3, 1, 2].into_iter().enumerate() {
            w.push(&[0, 0, 0, 1, i as u8], i == 0, shown).expect("push");
        }
        w.finish().expect("finish");
        let mut file = std::fs::File::open(&path).expect("film");
        let track = tuile_mp4::film::read_track(&mut file)
            .expect("read")
            .expect("track");
        let stream = Stream::of(track).expect("stream");
        let shown: Vec<u64> = (0..4)
            .map(|n| stream.shown_at(stream.timing(n).1).expect("shown"))
            .collect();
        assert_eq!(shown, [0, 3, 1, 2]);
        let (dts, _, duration) = stream.timing(2);
        assert_eq!(dts, 2 * i64::from(duration));
        assert_eq!(
            stream.sample(&mut file, 1).expect("sample"),
            [0, 0, 0, 1, 1]
        );
        assert_eq!(
            stream.parameter_sets(),
            [vec![0x67, 0x64, 0, 0x28], vec![0x68]]
        );
        assert_eq!(stream.length_size(), 4);
    }

    /// A tuning variable changes its one setting and nothing else; a value
    /// that is not a number is refused.
    #[test]
    fn a_profile_is_tuned_by_the_variables_that_name_its_settings() {
        let base = tuile_farm::compress::Profile::h264();
        let tuned = tuned(base.clone(), |name| match name {
            "TUILE_COMPRESS_B_FRAMES" => Some("0".into()),
            "TUILE_COMPRESS_MULTIPASS" => Some("0".into()),
            _ => None,
        })
        .expect("tuned");
        assert_eq!((tuned.b_frames, tuned.multipass), (0, false));
        assert_eq!(tuned.quality, base.quality);
        assert_eq!(tuned.key(), base.key());
        assert!(super::tuned(base, |_| Some("many".into())).is_err());
    }

    /// A copy is the joined film as it is: every frame, laid out for a
    /// viewer, no encode.
    #[test]
    fn a_copy_delivers_the_joined_film_as_it_is() {
        let dir = tempfile::tempdir().expect("dir");
        let joined = dir.path().join("j.mp4");
        let entry = tuile_mp4::film::avc1(64, 48, &[0x67, 0x64, 0, 0x28], &[0x68]).expect("entry");
        let mut w = tuile_mp4::film::FilmWriter::create(&joined, entry, 30).expect("writer");
        for i in 0..3u8 {
            w.push(&[0, 0, 0, 1, i], i == 0, u64::from(i))
                .expect("push");
        }
        w.finish().expect("finish");
        let out = dir.path().join("out.mp4");
        let done =
            compress(&joined, &out, &tuile_farm::compress::Delivery::Copy, 30, 0).expect("copy");
        assert_eq!((done.frames, done.passes), (3, 0));
        assert_eq!(
            std::fs::read(&out).unwrap(),
            std::fs::read(&joined).unwrap()
        );
    }
}
