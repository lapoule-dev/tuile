// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Segments into one film, in Rust, without decoding a picture.
//!
//! Every segment of a render is H.264 from the same encoder with the same
//! settings, so they share one sequence and one picture parameter set. Joining
//! them is then bookkeeping, not video: copy each segment's samples in order
//! into one track, shifting their timestamps by the length of what came
//! before, and keep the sync flags and composition offsets as they are. That is
//! what `ffmpeg -f concat -c copy` did, and it was the only thing ffmpeg was
//! still doing after the encode.
//!
//! Parameter sets that differ are refused rather than papered over: a segment
//! encoded at another size or profile would decode as garbage from its first
//! frame, and the join would not know.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use mp4::{
    AvcConfig, MediaConfig, Mp4Config, Mp4Reader, Mp4Sample, Mp4Writer, TrackConfig, TrackType,
};

#[derive(Debug, thiserror::Error)]
pub enum ConcatError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Mp4 {
        path: PathBuf,
        #[source]
        source: mp4::Error,
    },
    #[error("{0}: no H.264 video track")]
    NoVideo(PathBuf),
    #[error("{path}: {what} differs from the first segment's — not the same encode")]
    Mismatch { path: PathBuf, what: &'static str },
    #[error("nothing to join")]
    Empty,
}

/// What a film's single video track is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Format {
    width: u16,
    height: u16,
    timescale: u32,
    sps: Vec<u8>,
    pps: Vec<u8>,
}

fn open(path: &Path) -> Result<(Mp4Reader<BufReader<File>>, u32, Format), ConcatError> {
    let io = |source| ConcatError::Io { path: path.to_path_buf(), source };
    let mp4 = |source| ConcatError::Mp4 { path: path.to_path_buf(), source };
    let file = File::open(path).map_err(io)?;
    let size = file.metadata().map_err(io)?.len();
    let reader = Mp4Reader::read_header(BufReader::new(file), size).map_err(mp4)?;
    let (id, track) = reader
        .tracks()
        .iter()
        .find(|(_, t)| {
            matches!(t.track_type(), Ok(TrackType::Video))
                && t.box_type().map(|b| b.to_string() == "avc1").unwrap_or(false)
        })
        .ok_or_else(|| ConcatError::NoVideo(path.to_path_buf()))?;
    let format = Format {
        width: track.width(),
        height: track.height(),
        timescale: track.timescale(),
        sps: track.sequence_parameter_set().map_err(mp4)?.to_vec(),
        pps: track.picture_parameter_set().map_err(mp4)?.to_vec(),
    };
    let id = *id;
    Ok((reader, id, format))
}

/// What a film holds, read from its index — no picture decoded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Probe {
    pub frames: u64,
    /// Seconds, from the video track's own timestamps.
    pub seconds: f64,
}

/// Frames and duration of a film's video track.
pub fn probe(path: &Path) -> Result<Probe, ConcatError> {
    let (mut reader, id, format) = open(path)?;
    let mp4 = |source| ConcatError::Mp4 { path: path.to_path_buf(), source };
    let count = reader.sample_count(id).map_err(mp4)?;
    let mut end = 0u64;
    // The track's end is its last sample's end in decode order; the header's
    // duration can be padded by the muxer, and padding is what this measures.
    if count > 0 {
        if let Some(last) = reader.read_sample(id, count).map_err(mp4)? {
            end = last.start_time + u64::from(last.duration);
        }
    }
    Ok(Probe { frames: u64::from(count), seconds: end as f64 / f64::from(format.timescale.max(1)) })
}

/// Frames in a film: the sample count of its video track.
pub fn count_frames(path: &Path) -> Result<u64, ConcatError> {
    Ok(probe(path)?.frames)
}

/// Does the film last what its frames and its cadence announce?
///
/// The join copies samples and imposes no cadence: it inherits the segments'.
/// 7200 frames labelled 24 fps make 300 seconds, and nothing else in the chain
/// notices — which is how a five-minute film came out of a two-minute flight
/// once, `JOB_FPS` having never reached the render. Re-labelling here would
/// hide that upstream fault; this only reports it.
///
/// The tolerance is in FRAMES, not a percentage: containers round their
/// timestamps (25 ms of padding was measured on a remux, a frame and a half),
/// and a percentage is too loose on a long shot and too strict on a short one.
pub fn check_cadence(probe: Probe, fps: f64) -> Result<(), String> {
    let expected = probe.frames as f64 / fps;
    let drift = (probe.seconds - expected).abs() * fps;
    if drift > 2.0 {
        return Err(format!(
            "{:.3} s for {} frames at {fps} fps, {expected:.3} s expected — {drift:.1} frames off",
            probe.seconds, probe.frames
        ));
    }
    Ok(())
}

/// Joins `segments`, in the order given, into `out`. Returns the frame count.
pub fn concat(segments: &[PathBuf], out: &Path) -> Result<u64, ConcatError> {
    let first = segments.first().ok_or(ConcatError::Empty)?;
    let (_, _, format) = open(first)?;

    let out_io = |source| ConcatError::Io { path: out.to_path_buf(), source };
    let out_mp4 = |source| ConcatError::Mp4 { path: out.to_path_buf(), source };
    let file = File::create(out).map_err(out_io)?;
    let brand = |s: &str| s.parse().map_err(out_mp4);
    let config = Mp4Config {
        major_brand: brand("isom")?,
        minor_version: 512,
        compatible_brands: vec![brand("isom")?, brand("iso2")?, brand("avc1")?, brand("mp41")?],
        timescale: 1000,
    };
    let mut writer = Mp4Writer::write_start(BufWriter::new(file), &config).map_err(out_mp4)?;
    writer
        .add_track(&TrackConfig {
            track_type: TrackType::Video,
            timescale: format.timescale,
            language: "und".into(),
            media_conf: MediaConfig::AvcConfig(AvcConfig {
                width: format.width,
                height: format.height,
                seq_param_set: format.sps.clone(),
                pic_param_set: format.pps.clone(),
            }),
        })
        .map_err(out_mp4)?;

    // Where the next segment starts, in the track's timescale.
    let mut offset = 0u64;
    let mut frames = 0u64;
    for path in segments {
        let (mut reader, id, this) = open(path)?;
        let mismatch = |what| ConcatError::Mismatch { path: path.clone(), what };
        if (this.width, this.height) != (format.width, format.height) {
            return Err(mismatch("picture size"));
        }
        if this.timescale != format.timescale {
            return Err(mismatch("timescale"));
        }
        if this.sps != format.sps {
            return Err(mismatch("sequence parameter set"));
        }
        if this.pps != format.pps {
            return Err(mismatch("picture parameter set"));
        }
        let mp4 = |source| ConcatError::Mp4 { path: path.clone(), source };
        let count = reader.sample_count(id).map_err(mp4)?;
        let mut end = 0u64;
        // Sample ids are 1-based in the format.
        for sample_id in 1..=count {
            let Some(sample) = reader.read_sample(id, sample_id).map_err(mp4)? else {
                continue;
            };
            end = end.max(sample.start_time + u64::from(sample.duration));
            writer
                .write_sample(
                    1,
                    &Mp4Sample {
                        start_time: offset + sample.start_time,
                        duration: sample.duration,
                        rendering_offset: sample.rendering_offset,
                        is_sync: sample.is_sync,
                        bytes: sample.bytes,
                    },
                )
                .map_err(out_mp4)?;
            frames += 1;
        }
        offset += end;
    }
    writer.write_end().map_err(out_mp4)?;
    Ok(frames)
}

/// A synthetic H.264 segment: real container, placeholder NAL payloads. Enough
/// for everything here, which never decodes a picture.
#[doc(hidden)]
pub fn write_test_segment(
    path: &Path,
    frames: u32,
    sps: &[u8],
    tag: u8,
) -> Result<(), ConcatError> {
    let io = |source| ConcatError::Io { path: path.to_path_buf(), source };
    let mp4 = |source| ConcatError::Mp4 { path: path.to_path_buf(), source };
    let file = File::create(path).map_err(io)?;
    let brand = |s: &str| s.parse().map_err(mp4);
    let config = Mp4Config {
        major_brand: brand("isom")?,
        minor_version: 512,
        compatible_brands: vec![brand("isom")?, brand("avc1")?],
        timescale: 1000,
    };
    let mut w = Mp4Writer::write_start(BufWriter::new(file), &config).map_err(mp4)?;
    w.add_track(&TrackConfig {
        track_type: TrackType::Video,
        timescale: 15360,
        language: "und".into(),
        media_conf: MediaConfig::AvcConfig(AvcConfig {
            width: 160,
            height: 90,
            seq_param_set: sps.to_vec(),
            pic_param_set: vec![0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0],
        }),
    })
    .map_err(mp4)?;
    for i in 0..frames {
        // The payload carries the segment's tag and the frame's index, so a
        // join that reorders or drops samples changes the bytes.
        let bytes = vec![0, 0, 0, 3, 0x65, tag, i as u8].into();
        w.write_sample(
            1,
            &Mp4Sample {
                start_time: u64::from(i) * 256,
                duration: 256,
                rendering_offset: if i % 3 == 1 { 512 } else { 0 },
                is_sync: i % 10 == 0,
                bytes,
            },
        )
        .map_err(mp4)?;
    }
    w.write_end().map_err(mp4)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x0c, 0xac, 0xd9, 0x42];

    #[test]
    fn samples_keep_their_order_flags_and_offsets() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.mp4");
        let b = dir.path().join("b.mp4");
        write_test_segment(&a, 12, SPS, 0xa).expect("a");
        write_test_segment(&b, 7, SPS, 0xb).expect("b");
        let out = dir.path().join("film.mp4");
        assert_eq!(concat(&[a, b], &out).expect("concat"), 19);
        assert_eq!(count_frames(&out).expect("count"), 19);

        let (mut reader, id, _) = open(&out).expect("open film");
        let mut previous_end = 0;
        for n in 1..=19u32 {
            let s = reader.read_sample(id, n).expect("read").expect("sample");
            let (tag, index) = if n <= 12 { (0xa, n - 1) } else { (0xb, n - 13) };
            assert_eq!(&s.bytes[5..], &[tag, index as u8], "sample {n} payload");
            assert_eq!(s.start_time, previous_end, "sample {n} is not contiguous");
            assert_eq!(s.is_sync, index % 10 == 0, "sample {n} sync flag");
            assert_eq!(s.rendering_offset, if index % 3 == 1 { 512 } else { 0 }, "sample {n} cts");
            previous_end = s.start_time + u64::from(s.duration);
        }
    }

    #[test]
    fn a_film_at_the_wrong_cadence_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.mp4");
        // 30 samples of 256 ticks at 15360 Hz: exactly 60 fps, 0.5 s.
        write_test_segment(&a, 30, SPS, 1).expect("a");
        let p = probe(&a).expect("probe");
        assert_eq!(p.frames, 30);
        assert!((p.seconds - 0.5).abs() < 1e-9, "{}", p.seconds);
        assert!(check_cadence(p, 60.0).is_ok());
        assert!(check_cadence(p, 24.0).is_err(), "60 fps passed for 24");
    }

    #[test]
    fn another_encode_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.mp4");
        let b = dir.path().join("b.mp4");
        write_test_segment(&a, 3, SPS, 1).expect("a");
        write_test_segment(&b, 3, &[0x67, 0x42, 0x00, 0x1e], 2).expect("b");
        let err = concat(&[a, b], &dir.path().join("f.mp4")).expect_err("must refuse");
        assert!(matches!(err, ConcatError::Mismatch { what: "sequence parameter set", .. }), "{err}");
    }
}
