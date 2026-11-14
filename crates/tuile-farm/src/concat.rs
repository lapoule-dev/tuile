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
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use mp4::{AvcConfig, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig, TrackType};

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
    #[error("{0}: no H.264, HEVC or AV1 video track")]
    NoVideo(PathBuf),
    #[error("{path}: {what} differs from the first segment's — not the same encode")]
    Mismatch { path: PathBuf, what: &'static str },
    #[error("nothing to join")]
    Empty,
    #[error("{path}: {why}")]
    Film { path: PathBuf, why: String },
}

/// The video track of the film at `path`.
fn track(path: &Path) -> Result<tuile_mp4::film::Track, ConcatError> {
    let mut file = File::open(path).map_err(|source| ConcatError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    tuile_mp4::film::read_track(&mut file)
        .map_err(|e| ConcatError::Film {
            path: path.to_path_buf(),
            why: e.to_string(),
        })?
        .ok_or_else(|| ConcatError::NoVideo(path.to_path_buf()))
}

/// What a film holds, read from its index — no picture decoded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Probe {
    pub frames: u64,
    /// Seconds, from the video track's own timestamps.
    pub seconds: f64,
}

/// Frames and duration of a film's video track. The track's end is its
/// samples' durations added up; the header's duration can be padded by a
/// muxer, and padding is what this measures.
pub fn probe(path: &Path) -> Result<Probe, ConcatError> {
    let track = track(path)?;
    Ok(Probe {
        frames: track.samples.len() as u64,
        seconds: track.seconds(),
    })
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

/// Joins `segments`, in the order given, into `out`, laid out for a viewer
/// ([`tuile_mp4::film::join`]: faststart, its colour said). H.264, HEVC —
/// a spatial film's two layers included — or AV1. Returns the frame count.
pub fn concat(segments: &[PathBuf], out: &Path) -> Result<u64, ConcatError> {
    concat_into(segments, out, false)
}

/// [`concat()`], each segment's file removed once it is copied when `consume`
/// is set ([`tuile_mp4::film::join_into`]).
pub fn concat_into(segments: &[PathBuf], out: &Path, consume: bool) -> Result<u64, ConcatError> {
    if segments.is_empty() {
        return Err(ConcatError::Empty);
    }
    // Every segment readable as a film first: an error names the segment.
    for path in segments {
        track(path)?;
    }
    tuile_mp4::film::join_into(segments, out, consume).map_err(|e| match e {
        tuile_mp4::Mp4Error::Mismatch(what) => ConcatError::Mismatch {
            path: out.to_path_buf(),
            what,
        },
        other => ConcatError::Film {
            path: out.to_path_buf(),
            why: other.to_string(),
        },
    })
}

/// Frames `first..=last` of a film (numbered as its receipts number
/// them) laid over by another segment: the segment at `key`, in the store
/// the film's own segments are kept in, which holds exactly those frames
/// and starts on a key frame.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Insert {
    pub first: u64,
    pub last: u64,
    pub key: String,
}

impl Insert {
    pub fn frames(&self) -> u64 {
        self.last + 1 - self.first
    }
}

/// The film's `segments` (in film order, its frames numbered from
/// `first_frame`) joined into `out` with frames laid over: each of
/// `inserts` — its frames, and the file that holds them — takes the place
/// of the render's frames `first..=last`. Samples are copied, nothing is
/// decoded: the render's segments are cut where an insert ends, which
/// must be a key frame (the render is asked for one there), and the
/// inserted segments are taken whole. Returns the frames written.
///
/// Without inserts this is [`concat_into`], byte for byte.
pub fn splice(
    segments: &[PathBuf],
    first_frame: u64,
    inserts: &[(Insert, PathBuf)],
    out: &Path,
    consume: bool,
) -> Result<u64, ConcatError> {
    if inserts.is_empty() {
        return concat_into(segments, out, consume);
    }
    if segments.is_empty() {
        return Err(ConcatError::Empty);
    }
    let film = |path: &Path, why: String| ConcatError::Film {
        path: path.to_path_buf(),
        why,
    };
    let mut tracks = Vec::with_capacity(segments.len());
    let mut at = first_frame;
    for path in segments {
        let t = track(path)?;
        let frames = t.samples.len() as u64;
        tracks.push((path, at, t));
        at += frames;
    }
    let last_frame = at.saturating_sub(1);
    let mut sorted: Vec<&(Insert, PathBuf)> = inserts.iter().collect();
    sorted.sort_by_key(|(i, _)| i.first);
    let mut after = first_frame;
    for (insert, path) in &sorted {
        if insert.last < insert.first || insert.first < after || insert.last > last_frame {
            return Err(film(
                path,
                format!(
                    "frames {}..={} overlap another insert or lie outside the film's {first_frame}..={last_frame}",
                    insert.first, insert.last
                ),
            ));
        }
        let held = track(path)?.samples.len() as u64;
        if held != insert.frames() {
            return Err(film(
                path,
                format!(
                    "holds {held} frames for frames {}..={} ({})",
                    insert.first,
                    insert.last,
                    insert.frames()
                ),
            ));
        }
        after = insert.last + 1;
    }
    let mut pieces = Vec::new();
    for (path, a, t) in &tracks {
        let b = a + t.samples.len() as u64 - 1;
        let mut pos = *a;
        while pos <= b {
            if let Some((insert, inserted)) =
                sorted.iter().find(|(i, _)| i.first <= pos && pos <= i.last)
            {
                if pos == insert.first {
                    let n = insert.frames() as usize;
                    pieces.push(tuile_mp4::film::Piece::cut(inserted.clone(), 0..n));
                }
                pos = insert.last.min(b) + 1;
                continue;
            }
            let end = sorted
                .iter()
                .find(|(i, _)| i.first > pos)
                .map_or(b, |(i, _)| (i.first - 1).min(b));
            let (from, to) = ((pos - a) as usize, (end - a) as usize + 1);
            if from == 0 && to == t.samples.len() {
                pieces.push(tuile_mp4::film::Piece::whole(path.to_path_buf()));
            } else {
                if t.samples
                    .iter()
                    .any(|s| s.composition != t.samples[0].composition)
                {
                    return Err(film(
                        path,
                        "frames shown in another order than decoded: not cut by samples".into(),
                    ));
                }
                if !t.samples[from].key {
                    return Err(film(
                        path,
                        format!(
                            "frame {pos} is not a key frame, and the film resumes there after an insert: \
                             the render was not asked for one there"
                        ),
                    ));
                }
                pieces.push(tuile_mp4::film::Piece::cut(path.to_path_buf(), from..to));
            }
            pos = end + 1;
        }
    }
    tuile_mp4::film::join_pieces(&pieces, out, consume).map_err(|e| match e {
        tuile_mp4::Mp4Error::Mismatch(what) => ConcatError::Mismatch {
            path: out.to_path_buf(),
            what,
        },
        other => film(out, other.to_string()),
    })
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
    write_test_segment_with(path, frames, sps, tag, &|i| i % 10 == 0, true)
}

/// [`write_test_segment`], its key frames where `key` says, and its
/// frames shown in the order decoded unless `reordered`.
#[doc(hidden)]
pub fn write_test_segment_with(
    path: &Path,
    frames: u32,
    sps: &[u8],
    tag: u8,
    key: &dyn Fn(u32) -> bool,
    reordered: bool,
) -> Result<(), ConcatError> {
    let io = |source| ConcatError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mp4 = |source| ConcatError::Mp4 {
        path: path.to_path_buf(),
        source,
    };
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
                rendering_offset: if reordered && i % 3 == 1 { 512 } else { 0 },
                is_sync: key(i),
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

    /// Segments of a spatial film (MV-HEVC) are joined as one, and probed
    /// as one: the render service's segments of a film for a headset.
    #[test]
    fn spatial_segments_are_joined_as_a_spatial_film() {
        use tuile_mp4::spatial::{SpatialMuxer, StereoInfo};
        let dir = tempfile::tempdir().expect("tempdir");
        let hvcc = vec![
            1, 1, 0x60, 0, 0, 0, 0xb0, 0, 0, 0, 0, 0, 0x99, 0xf0, 0, 0xfc, 0xfd, 0xf8, 0xf8, 0, 0,
            0x0f, 0,
        ];
        let lhvc = vec![1, 0xf0, 0, 0xfc, 0xcb, 0];
        let stereo = StereoInfo {
            baseline_um: 10_000_000,
            hfov_millidegrees: 72_489,
            disparity_adjustment: 0,
        };
        let mut paths = Vec::new();
        for (tag, frames) in [(1u8, 4u8), (2, 3)] {
            let mut m = SpatialMuxer::new(64, 64, 30, &hvcc, &lhvc, stereo).expect("muxer");
            for i in 0..frames {
                m.push(&[0, 0, 0, 2, tag, i], i == 0).expect("frame");
            }
            let path = dir.path().join(format!("s{tag}.mp4"));
            std::fs::write(&path, m.finish().expect("film")).expect("written");
            paths.push(path);
        }
        let out = dir.path().join("film.mp4");
        assert_eq!(concat(&paths, &out).expect("joined"), 7);
        let p = probe(&out).expect("probe");
        assert_eq!(p.frames, 7);
        assert!(check_cadence(p, 30.0).is_ok());
        assert_eq!(probe(&paths[0]).expect("probe").frames, 4);
    }

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
        // The film lasts its frames: 19 of 256 ticks at 15 360 a second,
        // 316.7 ms — not 19 times a frame cut to a whole millisecond (304).
        let bytes = std::fs::read(&out).expect("read");
        let size = bytes.len() as u64;
        let header =
            mp4::Mp4Reader::read_header(std::io::Cursor::new(bytes), size).expect("header");
        assert_eq!(header.duration().as_millis(), 317);

        let film = std::fs::read(&out).expect("film");
        let size = film.len() as u64;
        let mut reader =
            mp4::Mp4Reader::read_header(std::io::Cursor::new(film), size).expect("open film");
        let id = *reader.tracks().keys().next().expect("track");
        let mut previous_end = 0;
        for n in 1..=19u32 {
            let s = reader.read_sample(id, n).expect("read").expect("sample");
            let (tag, index) = if n <= 12 { (0xa, n - 1) } else { (0xb, n - 13) };
            assert_eq!(&s.bytes[5..], &[tag, index as u8], "sample {n} payload");
            assert_eq!(s.start_time, previous_end, "sample {n} is not contiguous");
            assert_eq!(s.is_sync, index % 10 == 0, "sample {n} sync flag");
            assert_eq!(
                s.rendering_offset,
                if index % 3 == 1 { 512 } else { 0 },
                "sample {n} cts"
            );
            previous_end = s.start_time + u64::from(s.duration);
        }
    }

    /// The joined film plays from its first bytes and says its colour,
    /// whatever its segments said.
    #[test]
    fn the_joined_film_is_laid_out_for_a_viewer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.mp4");
        write_test_segment(&a, 3, SPS, 1).expect("a");
        let out = dir.path().join("film.mp4");
        concat(&[a], &out).expect("concat");
        let bytes = std::fs::read(&out).expect("film");
        assert_eq!(&bytes[4..8], b"ftyp");
        let ftyp = u32::from_be_bytes(bytes[0..4].try_into().unwrap()) as usize;
        assert_eq!(
            &bytes[ftyp + 4..ftyp + 8],
            b"moov",
            "the index before the pictures"
        );
        let track = tuile_mp4::film::read_track(&mut std::io::Cursor::new(&bytes))
            .expect("read")
            .expect("track");
        let mp4_atom_colour = match &track.entry {
            tuile_mp4::film::Entry::Avc1(e) => e.colr.clone(),
            _ => None,
        };
        assert_eq!(mp4_atom_colour, Some(tuile_mp4::film::bt709()));
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
        assert!(
            matches!(
                err,
                ConcatError::Mismatch {
                    what: "stream description",
                    ..
                }
            ),
            "{err}"
        );
    }
}

#[cfg(test)]
mod splices {
    use super::*;
    use sha2::Digest;

    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x0c, 0xac, 0xd9, 0x42];

    /// The join of three synthetic segments, byte for byte what it was
    /// before the splice came in: a film joined with nothing spliced is
    /// the film it always was.
    #[test]
    fn a_join_without_inserts_is_the_join_it_always_was() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths: Vec<PathBuf> = [(0xa, 12), (0xb, 7), (0xc, 21)]
            .into_iter()
            .map(|(tag, frames)| {
                let p = dir.path().join(format!("{tag}.mp4"));
                write_test_segment(&p, frames, SPS, tag).expect("segment");
                p
            })
            .collect();
        let out = dir.path().join("film.mp4");
        assert_eq!(concat(&paths, &out).expect("joined"), 40);
        let digest = sha2::Sha256::digest(std::fs::read(&out).expect("film"));
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "a59100745254499313b5ce60520d48bfbcd4eb505e2d5a942e1507d33687a3ab"
        );
    }

    /// One sample's payload: the segment's tag and the frame's index.
    fn payloads(path: &Path) -> Vec<(u8, u8, bool)> {
        let t = track(path).expect("track");
        let bytes = std::fs::read(path).expect("film");
        t.samples
            .iter()
            .map(|s| {
                let at = s.offset as usize + 5;
                (bytes[at], bytes[at + 1], s.key)
            })
            .collect()
    }

    fn plain(dir: &Path, name: &str, frames: u32, tag: u8, keys: &'static [u32]) -> PathBuf {
        let p = dir.join(name);
        write_test_segment_with(
            &p,
            frames,
            SPS,
            tag,
            &|i| i == 0 || keys.contains(&i),
            false,
        )
        .expect("segment");
        p
    }

    /// Frames 8..=14 of a film of two segments (1..=10, 11..=20) laid
    /// over by a segment of seven frames: the render's frames to 7, the
    /// insert's seven, the render's again from 15, which the render made a
    /// key frame — every sample copied, in order, timestamps contiguous.
    #[test]
    fn an_insert_spanning_two_segments_takes_their_frames_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = plain(dir.path(), "a.mp4", 10, 0xa, &[7]);
        let b = plain(dir.path(), "b.mp4", 10, 0xb, &[4]);
        let i = plain(dir.path(), "i.mp4", 7, 0x1, &[]);
        let insert = Insert {
            first: 8,
            last: 14,
            key: "i".into(),
        };
        let out = dir.path().join("film.mp4");
        let n = splice(&[a, b], 1, &[(insert, i)], &out, false).expect("spliced");
        assert_eq!(n, 20);
        let got = payloads(&out);
        let want: Vec<(u8, u8, bool)> = (0..7u8)
            .map(|f| (0xa, f, f == 0))
            .chain((0..7u8).map(|f| (0x1, f, f == 0)))
            .chain((4..10u8).map(|f| (0xb, f, f == 4)))
            .collect();
        assert_eq!(got, want);
        let t = track(&out).expect("track");
        assert!(t.samples.iter().all(|s| s.duration == 256));
        assert!((t.seconds() - 20.0 * 256.0 / 15360.0).abs() < 1e-9);
    }

    /// An insert at the film's very first frame, and one at its very last.
    #[test]
    fn inserts_at_either_end_of_the_film() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = plain(dir.path(), "a.mp4", 10, 0xa, &[3]);
        let first = plain(dir.path(), "i1.mp4", 3, 0x1, &[]);
        let last = plain(dir.path(), "i2.mp4", 2, 0x2, &[]);
        let out = dir.path().join("film.mp4");
        let inserts = [
            (
                Insert {
                    first: 9,
                    last: 10,
                    key: "2".into(),
                },
                last,
            ),
            (
                Insert {
                    first: 1,
                    last: 3,
                    key: "1".into(),
                },
                first,
            ),
        ];
        assert_eq!(splice(&[a], 1, &inserts, &out, true).expect("spliced"), 10);
        let tags: Vec<u8> = payloads(&out).iter().map(|p| p.0).collect();
        assert_eq!(tags, [1, 1, 1, 0xa, 0xa, 0xa, 0xa, 0xa, 2, 2]);
        assert!(
            !dir.path().join("a.mp4").exists(),
            "consumed once read for the last time"
        );
    }

    /// Where the film resumes after an insert on a frame that is not a
    /// key frame, the splice is refused and says which frame.
    #[test]
    fn a_film_that_resumes_on_no_key_frame_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = plain(dir.path(), "a.mp4", 10, 0xa, &[]);
        let i = plain(dir.path(), "i.mp4", 3, 0x1, &[]);
        let out = dir.path().join("film.mp4");
        let err = splice(
            std::slice::from_ref(&a),
            1,
            &[(
                Insert {
                    first: 4,
                    last: 6,
                    key: "i".into(),
                },
                i.clone(),
            )],
            &out,
            false,
        )
        .expect_err("no key at 7");
        assert!(
            err.to_string().contains("frame 7 is not a key frame"),
            "{err}"
        );
        // An insert of the wrong length, or one past the film, likewise.
        let err = splice(
            std::slice::from_ref(&a),
            1,
            &[(
                Insert {
                    first: 4,
                    last: 7,
                    key: "i".into(),
                },
                i.clone(),
            )],
            &out,
            false,
        )
        .expect_err("wrong length");
        assert!(err.to_string().contains("holds 3 frames"), "{err}");
        let err = splice(
            &[a],
            1,
            &[(
                Insert {
                    first: 9,
                    last: 11,
                    key: "i".into(),
                },
                i,
            )],
            &out,
            false,
        )
        .expect_err("past the end");
        assert!(err.to_string().contains("outside"), "{err}");
    }
}
