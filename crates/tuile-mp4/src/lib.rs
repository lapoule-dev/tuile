// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! H.264 samples into an mp4, in memory.
//!
//! The browser's encoder (WebCodecs) hands out an `avcC` description and
//! length-prefixed access units; a native encoder hands out Annex-B. Both end
//! up here as the samples of one video track, written to a `Vec<u8>` — no
//! file, so it runs the same in a wasm worker as in a binary.
//!
//! Several encoders may feed one film, each owning a contiguous slice of it.
//! That only joins into something decodable if they all produced the same
//! parameter sets, so every slice's `avcC` is checked against the first and a
//! mismatch is an error, never a film that turns to garbage half-way.

mod av1;

use std::io::Cursor;

use mp4::{AvcConfig, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig, TrackType};

#[derive(Debug, thiserror::Error)]
pub enum Mp4Error {
    #[error("not an avcC record: {0}")]
    BadAvcc(&'static str),
    #[error("not an av1C record: {0}")]
    BadAv1c(&'static str),
    #[error("{0} differs from the first slice's: not the same encode")]
    Mismatch(&'static str),
    #[error("frame {0} arrived out of order (expected {1})")]
    OutOfOrder(u64, u64),
    #[error("the film's first sample is not a key frame")]
    NoKeyFrame,
    #[error(transparent)]
    Mp4(#[from] mp4::Error),
}

/// The sequence and picture parameter sets of an `avcC` record (ISO/IEC
/// 14496-15). Only the first of each is kept: encoders emit one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterSets {
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

impl ParameterSets {
    pub fn from_avcc(avcc: &[u8]) -> Result<Self, Mp4Error> {
        let byte = |at: usize| avcc.get(at).copied().ok_or(Mp4Error::BadAvcc("truncated"));
        let u16_at =
            |at: usize| Ok::<_, Mp4Error>(usize::from(byte(at)?) << 8 | usize::from(byte(at + 1)?));
        if byte(0)? != 1 {
            return Err(Mp4Error::BadAvcc("version is not 1"));
        }
        if (byte(4)? & 0b11) != 3 {
            return Err(Mp4Error::BadAvcc("NAL lengths are not four bytes"));
        }
        if byte(5)? & 0x1f == 0 {
            return Err(Mp4Error::BadAvcc("no sequence parameter set"));
        }
        let sps_len = u16_at(6)?;
        let sps = avcc
            .get(8..8 + sps_len)
            .ok_or(Mp4Error::BadAvcc("truncated SPS"))?;
        let at = 8 + sps_len;
        if byte(at)? == 0 {
            return Err(Mp4Error::BadAvcc("no picture parameter set"));
        }
        let pps_len = u16_at(at + 1)?;
        let pps = avcc
            .get(at + 3..at + 3 + pps_len)
            .ok_or(Mp4Error::BadAvcc("truncated PPS"))?;
        Ok(Self {
            sps: sps.to_vec(),
            pps: pps.to_vec(),
        })
    }
}

/// Annex-B (start codes) to length-prefixed NAL units, dropping parameter
/// sets and access unit delimiters — the track header carries those.
pub fn annex_b_to_avcc(annex_b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(annex_b.len() + 16);
    for nal in nal_units(annex_b) {
        let kind = nal.first().map_or(0, |b| b & 0x1f);
        if matches!(kind, 7..=9) {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// The NAL units of an Annex-B stream, without their start codes.
pub fn nal_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::with_capacity(starts.len());
    for (n, &(_, body)) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(stream.len(), |&(code, _)| code);
        // A four-byte start code leaves its leading zero on the unit before.
        while end > body && stream[end - 1] == 0 {
            end -= 1;
        }
        if end > body {
            units.push(&stream[body..end]);
        }
    }
    units
}

/// What a film is encoded with, and what its track header needs to say so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Codec {
    /// H.264, with the parameter sets of the encoder's `avcC`.
    Avc(ParameterSets),
    /// AV1, with its `av1C` record.
    Av1(Vec<u8>),
}

enum Writer {
    Avc(Mp4Writer<Cursor<Vec<u8>>>, ParameterSets),
    Av1(av1::Av1Writer),
}

/// One film being written: a single video track at a constant frame rate.
///
/// H.264 samples are length-prefixed access units; AV1 samples are temporal
/// units as an encoder emits them.
pub struct Muxer {
    writer: Writer,
    next: u64,
}

/// Ticks per frame. 512 is what FFmpeg picks for common rates and divides
/// everything a player cares about.
const TICKS: u32 = 512;

impl Muxer {
    /// An H.264 film.
    pub fn new(width: u16, height: u16, fps: u32, sets: ParameterSets) -> Result<Self, Mp4Error> {
        Self::with(width, height, fps, Codec::Avc(sets))
    }

    pub fn with(width: u16, height: u16, fps: u32, codec: Codec) -> Result<Self, Mp4Error> {
        let writer = match codec {
            Codec::Av1(config) => {
                Writer::Av1(av1::Av1Writer::new(width, height, fps, TICKS, &config)?)
            }
            Codec::Avc(sets) => {
                let brand = |s: &str| s.parse().map_err(Mp4Error::Mp4);
                let config = Mp4Config {
                    major_brand: brand("isom")?,
                    minor_version: 512,
                    compatible_brands: vec![
                        brand("isom")?,
                        brand("iso2")?,
                        brand("avc1")?,
                        brand("mp41")?,
                    ],
                    timescale: 1000,
                };
                let mut writer = Mp4Writer::write_start(Cursor::new(Vec::new()), &config)?;
                writer.add_track(&TrackConfig {
                    track_type: TrackType::Video,
                    timescale: fps * TICKS,
                    language: "und".into(),
                    media_conf: MediaConfig::AvcConfig(AvcConfig {
                        width,
                        height,
                        seq_param_set: sets.sps.clone(),
                        pic_param_set: sets.pps.clone(),
                    }),
                })?;
                Writer::Avc(writer, sets)
            }
        };
        Ok(Self { writer, next: 0 })
    }

    /// Fails unless a slice encoded as `codec` can join this film: the same
    /// codec, configured the same.
    pub fn check(&self, codec: &Codec) -> Result<(), Mp4Error> {
        match (&self.writer, codec) {
            (Writer::Avc(_, mine), Codec::Avc(sets)) if sets.sps != mine.sps => {
                Err(Mp4Error::Mismatch("sequence parameter set"))
            }
            (Writer::Avc(_, mine), Codec::Avc(sets)) if sets.pps != mine.pps => {
                Err(Mp4Error::Mismatch("picture parameter set"))
            }
            (Writer::Av1(mine), Codec::Av1(config)) if mine.config() != config.as_slice() => {
                Err(Mp4Error::Mismatch("AV1 configuration record"))
            }
            (Writer::Avc(..), Codec::Avc(_)) | (Writer::Av1(_), Codec::Av1(_)) => Ok(()),
            _ => Err(Mp4Error::Mismatch("codec")),
        }
    }

    /// Appends frame `index` (counted from 0). Frames must arrive in order:
    /// there are no B-frames to reorder in what we encode, and a gap is a
    /// lost frame, which must not pass silently.
    pub fn push(&mut self, index: u64, sample: Vec<u8>, key: bool) -> Result<(), Mp4Error> {
        if index != self.next {
            return Err(Mp4Error::OutOfOrder(index, self.next));
        }
        if index == 0 && !key {
            return Err(Mp4Error::NoKeyFrame);
        }
        match &mut self.writer {
            Writer::Av1(writer) => writer.push(&sample, key),
            Writer::Avc(writer, _) => writer.write_sample(
                1,
                &Mp4Sample {
                    start_time: index * u64::from(TICKS),
                    duration: TICKS,
                    rendering_offset: 0,
                    is_sync: key,
                    bytes: sample.into(),
                },
            )?,
        }
        self.next += 1;
        Ok(())
    }

    /// Frames written so far.
    pub fn frames(&self) -> u64 {
        self.next
    }

    pub fn finish(self) -> Result<Vec<u8>, Mp4Error> {
        match self.writer {
            Writer::Av1(writer) => Ok(writer.finish()),
            Writer::Avc(mut writer, _) => {
                writer.write_end()?;
                Ok(writer.into_writer().into_inner())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x28, 0xac, 0xd9];
    const PPS: &[u8] = &[0x68, 0xeb, 0xe3, 0xcb];

    fn avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
        let mut v = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
        v.extend((sps.len() as u16).to_be_bytes());
        v.extend(sps);
        v.push(1);
        v.extend((pps.len() as u16).to_be_bytes());
        v.extend(pps);
        v
    }

    #[test]
    fn parameter_sets_come_out_of_an_avcc() {
        let sets = ParameterSets::from_avcc(&avcc(SPS, PPS)).expect("parse");
        assert_eq!(sets.sps, SPS);
        assert_eq!(sets.pps, PPS);
        assert!(ParameterSets::from_avcc(&[1, 2, 3]).is_err());
    }

    #[test]
    fn annex_b_becomes_length_prefixed_without_parameter_sets() {
        let stream = [
            0, 0, 0, 1, 0x67, 9, 9, 0, 0, 1, 0x68, 8, 0, 0, 0, 1, 0x65, 1, 2, 3,
        ];
        assert_eq!(annex_b_to_avcc(&stream), [0, 0, 0, 4, 0x65, 1, 2, 3]);
    }

    #[test]
    fn a_film_holds_its_frames_in_order() {
        let sets = ParameterSets::from_avcc(&avcc(SPS, PPS)).expect("parse");
        let mut m = Muxer::new(64, 48, 30, sets.clone()).expect("muxer");
        m.push(0, vec![0, 0, 0, 2, 0x65, 0], true).expect("0");
        m.push(1, vec![0, 0, 0, 2, 0x41, 1], false).expect("1");
        assert!(matches!(
            m.push(3, vec![], false),
            Err(Mp4Error::OutOfOrder(3, 2))
        ));
        let other = ParameterSets {
            sps: vec![0x67, 0x42],
            pps: PPS.to_vec(),
        };
        assert!(matches!(
            m.check(&Codec::Avc(other)),
            Err(Mp4Error::Mismatch(_))
        ));
        assert!(m.check(&Codec::Avc(sets.clone())).is_ok());
        assert!(matches!(
            m.check(&Codec::Av1(vec![0x81, 0, 0, 0])),
            Err(Mp4Error::Mismatch("codec"))
        ));
        let bytes = m.finish().expect("finish");

        let size = bytes.len() as u64;
        let mut r = mp4::Mp4Reader::read_header(Cursor::new(bytes), size).expect("read");
        let (&id, track) = r.tracks().iter().next().expect("track");
        assert_eq!(track.sequence_parameter_set().expect("sps"), SPS);
        assert_eq!(r.sample_count(id).expect("count"), 2);
        let s = r.read_sample(id, 2).expect("read").expect("sample");
        assert_eq!(s.start_time, 512);
        assert!(!s.is_sync);
    }

    #[test]
    fn the_first_frame_must_be_a_key_frame() {
        let sets = ParameterSets::from_avcc(&avcc(SPS, PPS)).expect("parse");
        let mut m = Muxer::new(64, 48, 30, sets).expect("muxer");
        assert!(matches!(
            m.push(0, vec![], false),
            Err(Mp4Error::NoKeyFrame)
        ));
    }
}

#[cfg(test)]
mod av1_film {
    use super::*;

    #[test]
    fn an_av1_film_keeps_the_same_rules() {
        let record = vec![0x81, 0x05, 0x0c, 0x00];
        let mut m = Muxer::with(640, 360, 30, Codec::Av1(record.clone())).expect("muxer");
        assert!(matches!(
            m.push(0, vec![0x32, 0x00], false),
            Err(Mp4Error::NoKeyFrame)
        ));
        m.push(0, vec![0x12, 0x00, 0x0a, 0x00], true).expect("0");
        assert!(matches!(
            m.push(2, vec![], false),
            Err(Mp4Error::OutOfOrder(2, 1))
        ));
        assert!(m.check(&Codec::Av1(record)).is_ok());
        assert!(matches!(
            m.check(&Codec::Av1(vec![0x81, 0x08, 0x0c, 0x00])),
            Err(Mp4Error::Mismatch("AV1 configuration record"))
        ));
        assert_eq!(m.frames(), 1);
        assert!(m.finish().expect("finish").len() > 100);
    }
}
