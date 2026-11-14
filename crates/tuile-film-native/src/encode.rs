// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What every encoder backend opens a session with, and what it hands
//! back: the profile's settings as one [`Spec`] (`tuile_farm::compress`
//! is where the profiles are), pictures in ([`Frame`]) and pictures out
//! ([`Encoded`]), and the film they make ([`Package`]: faststart, BT.709
//! said, composition offsets).
//!
//! The backends — NVENC on an NVIDIA card, VideoToolbox on a Mac — differ
//! in how they encode, never in what they are asked: a profile means the
//! same spec on either.

use std::path::PathBuf;

use tuile_farm::compress::{Profile, VideoCodec};
use tuile_mp4::film::{FilmWriter, Written};
use tuile_mp4::{annex_b_to_avcc, nal_units};

use crate::av1::{av1c_of, obus};

/// The codec of a stream, as the encode loop and the film's sample entry
/// see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Out {
    H264,
    Hevc,
    Av1,
}

/// How pictures are handed to the encoder, rows tightly packed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// Planar 4:2:0, 8 bits: Y, then U, then V.
    Iyuv,
    /// Y, then U and V interleaved, 8 bits.
    Nv12,
    /// As NV12, 16-bit little-endian samples, the 10 bits in the high ones.
    P010,
}

impl Input {
    /// Bits a sample.
    pub fn bit_depth(self) -> u8 {
        match self {
            Input::P010 => 10,
            Input::Iyuv | Input::Nv12 => 8,
        }
    }

    /// Bytes of one tightly packed picture.
    pub fn bytes(self, (width, height): (usize, usize)) -> usize {
        let chroma = 2 * width.div_ceil(2) * height.div_ceil(2);
        let samples = width * height + chroma;
        if self == Input::P010 {
            2 * samples
        } else {
            samples
        }
    }

    /// The picture laid out at the buffer's `pitch` (bytes a row): the
    /// rows the encoder reads.
    pub fn pitched(self, tight: &[u8], (width, height): (usize, usize), pitch: usize) -> Vec<u8> {
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        // Each plane: its rows' bytes, its rows, and the pitch they go at.
        let planes: Vec<(usize, usize, usize)> = match self {
            Input::Iyuv => vec![
                (width, height, pitch),
                (cw, ch, pitch / 2),
                (cw, ch, pitch / 2),
            ],
            Input::Nv12 => vec![(width, height, pitch), (2 * cw, ch, pitch)],
            Input::P010 => vec![(2 * width, height, pitch), (4 * cw, ch, pitch)],
        };
        let total: usize = planes.iter().map(|&(_, rows, p)| rows * p).sum();
        if planes.iter().all(|&(row, _, p)| row == p) {
            return tight.to_vec();
        }
        let mut out = vec![0u8; total];
        let (mut from, mut to) = (0, 0);
        for (row, rows, p) in planes {
            for r in 0..rows {
                out[to + r * p..to + r * p + row]
                    .copy_from_slice(&tight[from + r * row..from + (r + 1) * row]);
            }
            from += row * rows;
            to += p * rows;
        }
        out
    }
}

/// How the bitrate is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rate {
    /// A constant quality (NVENC's `targetQuality`), under a ceiling in bits
    /// a second.
    Quality { quality: u8, max: u32 },
    /// A mean bitrate and its ceiling, bits a second.
    Vbr { average: u32, max: u32 },
}

/// Everything an encode session is opened with.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub out: Out,
    /// Bits a sample of the stream: 8, or 10 (HEVC, AV1).
    pub bit_depth: u8,
    pub input: Input,
    pub size: (usize, usize),
    pub fps: u32,
    /// 1 (fastest) to 7 (best).
    pub preset: u8,
    pub rate: Rate,
    /// Frames between two IDRs.
    pub gop: u32,
    pub b_frames: u8,
    pub lookahead: u8,
    pub spatial_aq: bool,
    pub temporal_aq: bool,
    pub multipass: bool,
}

impl Spec {
    /// A profile's spec for a `width`×`height` film at `fps`, its pictures
    /// handed as `input`; `rate` in place of the profile's own.
    pub fn of(
        profile: &Profile,
        input: Input,
        size: (usize, usize),
        fps: u32,
        rate: Option<Rate>,
    ) -> Self {
        let (_, max) = profile.bitrates(size.0 as u32, size.1 as u32, f64::from(fps));
        Self {
            out: match profile.codec {
                VideoCodec::H264 => Out::H264,
                VideoCodec::Hevc => Out::Hevc,
            },
            bit_depth: profile.bit_depth,
            input,
            size,
            fps: fps.max(1),
            preset: profile.preset,
            rate: rate.unwrap_or(Rate::Quality {
                quality: profile.quality,
                max,
            }),
            gop: profile.gop_frames(fps.max(1)),
            b_frames: profile.b_frames,
            lookahead: profile.lookahead,
            spatial_aq: profile.spatial_aq,
            temporal_aq: profile.temporal_aq,
            multipass: profile.multipass,
        }
    }

    /// Buffers in an encoder's pool: every picture the encoder may hold
    /// before it hands one back — its lookahead, its B-frames — and a few
    /// more.
    pub fn buffers(&self) -> usize {
        usize::from(self.lookahead) + usize::from(self.b_frames) + 4
    }
}

/// One picture for the encoder: tightly packed in its spec's input layout,
/// and the frame of the film it is shown as.
pub struct Frame {
    pub data: Vec<u8>,
    pub shown: u64,
    /// An IDR forced on this picture, beside the GOP's own: where a film
    /// is to be cut on a sync sample without decoding
    /// (`tuile_farm::concat::join_pieces`).
    pub key: bool,
}

/// One picture out of the encoder, in decoding order: Annex B (H.264,
/// HEVC) or a temporal unit (AV1), and the frame it is shown as.
pub struct Encoded {
    pub data: Vec<u8>,
    pub key: bool,
    pub shown: u64,
}

/// Encoded pictures into a film laid out for a viewer: its sample entry
/// made from the first picture's parameter sets (or AV1 sequence header),
/// each picture a sample shown where the encoder said.
pub struct Package {
    path: PathBuf,
    out: Out,
    size: (u16, u16),
    fps: u32,
    writer: Option<FilmWriter>,
}

impl Package {
    pub fn at(
        path: impl Into<PathBuf>,
        out: Out,
        size: (usize, usize),
        fps: u32,
    ) -> Result<Self, String> {
        Ok(Self {
            path: path.into(),
            out,
            size: (
                u16::try_from(size.0).map_err(|e| e.to_string())?,
                u16::try_from(size.1).map_err(|e| e.to_string())?,
            ),
            fps,
            writer: None,
        })
    }

    pub fn take(&mut self, encoded: Encoded) -> Result<(), String> {
        let (width, height) = self.size;
        let data = encoded.data.as_slice();
        let sample = match self.out {
            Out::H264 => {
                if self.writer.is_none() {
                    let units = nal_units(data);
                    let of = |kind: u8| {
                        units
                            .iter()
                            .find(|u| u.first().is_some_and(|b| b & 0x1f == kind))
                            .map(|u| u.to_vec())
                    };
                    let (sps, pps) = of(7)
                        .zip(of(8))
                        .ok_or("NVENC: the first picture carries no parameter sets")?;
                    self.open(
                        tuile_mp4::film::avc1(width, height, &sps, &pps)
                            .map_err(|e| e.to_string())?,
                    )?;
                }
                annex_b_to_avcc(data)
            }
            Out::Hevc => {
                let (parameters, units) = tuile_mp4::spatial::entry_units(data);
                if self.writer.is_none() {
                    let record = tuile_mp4::spatial::hevc_record(&parameters)
                        .map_err(|e| format!("NVENC: {e}"))?;
                    self.open(
                        tuile_mp4::film::hvc1(width, height, &record).map_err(|e| e.to_string())?,
                    )?;
                }
                let mut sample = Vec::with_capacity(data.len());
                for unit in units {
                    sample.extend((unit.len() as u32).to_be_bytes());
                    sample.extend_from_slice(unit);
                }
                sample
            }
            Out::Av1 => {
                if self.writer.is_none() {
                    let record = obus(data)
                        .into_iter()
                        .find(|(kind, _)| *kind == 1)
                        .and_then(|(_, header)| av1c_of(header))
                        .ok_or("NVENC: the first picture carries no sequence header this reads")?;
                    self.open(
                        tuile_mp4::film::av01(width, height, &record).map_err(|e| e.to_string())?,
                    )?;
                }
                // The binding says a sample does not carry the temporal
                // delimiter an encoder heads each unit with.
                data.strip_prefix(&[0x12, 0x00]).unwrap_or(data).to_vec()
            }
        };
        let writer = self.writer.as_mut().ok_or("the film was not opened")?;
        writer
            .push(&sample, encoded.key, encoded.shown)
            .map_err(|e| e.to_string())
    }

    fn open(&mut self, entry: tuile_mp4::film::Entry) -> Result<(), String> {
        self.writer =
            Some(FilmWriter::create(&self.path, entry, self.fps).map_err(|e| e.to_string())?);
        Ok(())
    }

    pub fn finish(self) -> Result<Written, String> {
        self.writer
            .ok_or("a film of no picture")?
            .finish()
            .map_err(|e| e.to_string())
    }
}

/// What a render node writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeCodec {
    /// H.264, delivered as the render node writes it.
    H264,
    /// AV1, likewise.
    Av1,
    /// The HEVC Main10 mezzanine the final stage reads
    /// ([`Profile::mezzanine`]).
    Mezzanine,
}

/// The encode a render node's single-view film of `size` at `fps` gets,
/// its pictures handed as `input`: the mezzanine's profile, or H.264 and
/// AV1 as a render node writes them — the bitrate asked for (`bitrate` bits
/// a second, its ceiling twice that), else the H.264 profile's by the
/// picture's size. The same on every backend.
pub fn node_spec(
    codec: NodeCodec,
    bitrate: u32,
    input: Input,
    size: (usize, usize),
    fps: u32,
) -> Spec {
    let asked = |profile: &Profile| match bitrate {
        0 => {
            let (average, max) = profile.bitrates(size.0 as u32, size.1 as u32, f64::from(fps));
            Rate::Vbr { average, max }
        }
        bits => Rate::Vbr {
            average: bits,
            max: bits.saturating_mul(2),
        },
    };
    match codec {
        NodeCodec::Mezzanine => {
            let profile = Profile::mezzanine();
            let rate = (bitrate > 0).then(|| asked(&profile));
            Spec::of(&profile, input, size, fps, rate)
        }
        NodeCodec::H264 => {
            let profile = Profile::h264();
            // A render node's own H.264: the profile's B-frames and
            // lookahead, at the bitrate asked for, no second pass.
            let mut spec = Spec::of(&profile, input, size, fps, Some(asked(&profile)));
            spec.multipass = false;
            spec.preset = 4;
            spec
        }
        NodeCodec::Av1 => {
            let profile = Profile::h264();
            let mut spec = Spec::of(&profile, input, size, fps, Some(asked(&profile)));
            spec.out = Out::Av1;
            spec.b_frames = 0;
            spec.lookahead = 0;
            spec.multipass = false;
            spec.preset = 4;
            spec
        }
    }
}

/// An I420 picture (8 bits, planar) as P010 (16-bit little-endian samples,
/// ten bits in the high ones, chroma interleaved), each sample's eight bits
/// made ten exactly: `v` becomes `4v`, limited range staying limited range
/// (16–235 → 64–940). No dither: there is nothing between two eight-bit
/// steps to say. What NVENC does when it is handed eight bits for a
/// ten-bit stream, done here for an encoder that takes ten bits only.
pub fn i420_to_p010(i420: &[u8], (width, height): (usize, usize)) -> Vec<u8> {
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    let (luma, chroma) = (width * height, cw * ch);
    let ten = |v: u8| (u16::from(v) << 8).to_le_bytes();
    let mut out = Vec::with_capacity(Input::P010.bytes((width, height)));
    for &v in &i420[..luma] {
        out.extend(ten(v));
    }
    let (u, v) = (
        &i420[luma..luma + chroma],
        &i420[luma + chroma..luma + 2 * chroma],
    );
    for (&u, &v) in u.iter().zip(v) {
        out.extend(ten(u));
        out.extend(ten(v));
    }
    out
}

/// An I420 picture as NV12: the luma as it is, the two chromas
/// interleaved, U first.
pub fn i420_to_nv12(i420: &[u8], (width, height): (usize, usize)) -> Vec<u8> {
    let (luma, chroma) = (width * height, width.div_ceil(2) * height.div_ceil(2));
    let mut out = Vec::with_capacity(luma + 2 * chroma);
    out.extend_from_slice(&i420[..luma]);
    let (u, v) = (
        &i420[luma..luma + chroma],
        &i420[luma + chroma..luma + 2 * chroma],
    );
    for (&u, &v) in u.iter().zip(v) {
        out.extend([u, v]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each plane's rows at the buffer's pitch, the planes one after the
    /// other — and a buffer as wide as the picture takes it as it is.
    #[test]
    fn a_picture_is_laid_out_at_the_buffer_s_pitch() {
        let size = (4, 2);
        let nv12: Vec<u8> = (0..Input::Nv12.bytes(size) as u8).collect();
        assert_eq!(Input::Nv12.pitched(&nv12, size, 4), nv12);
        let wide = Input::Nv12.pitched(&nv12, size, 8);
        assert_eq!(wide.len(), 8 * 3);
        assert_eq!(&wide[..4], &[0, 1, 2, 3]);
        assert_eq!(&wide[8..12], &[4, 5, 6, 7]);
        assert_eq!(&wide[16..20], &[8, 9, 10, 11], "the chroma's row");
        let p010 = vec![1u8; Input::P010.bytes(size)];
        assert_eq!(Input::P010.pitched(&p010, size, 16).len(), 16 * 3);
        let iyuv: Vec<u8> = (0..Input::Iyuv.bytes(size) as u8).collect();
        let wide = Input::Iyuv.pitched(&iyuv, size, 8);
        assert_eq!(wide.len(), 8 * 2 + 2 * 4);
        assert_eq!(&wide[16..18], &[8, 9], "U at half the pitch");
        assert_eq!(&wide[20..22], &[10, 11], "then V");
    }

    /// The mezzanine is the profile's; a render node's own H.264 keeps the
    /// bitrate it is asked for, and its size's when asked for none.
    #[test]
    fn a_render_node_s_encode_follows_its_codec_and_its_picture_s_size() {
        let mezzanine = node_spec(NodeCodec::Mezzanine, 0, Input::Iyuv, (1920, 1080), 30);
        assert_eq!(
            (mezzanine.out, mezzanine.bit_depth, mezzanine.input),
            (Out::Hevc, 10, Input::Iyuv)
        );
        assert_eq!((mezzanine.gop, mezzanine.b_frames), (30, 0));
        assert!(matches!(
            mezzanine.rate,
            Rate::Quality {
                max: 60_000_000,
                ..
            }
        ));
        let mobile = node_spec(NodeCodec::H264, 0, Input::Iyuv, (720, 1280), 30);
        assert_eq!(
            mobile.rate,
            Rate::Vbr {
                average: 3_000_000,
                max: 5_000_000
            }
        );
        assert_eq!(mobile.b_frames, 3);
        let asked = node_spec(NodeCodec::H264, 45_000_000, Input::Iyuv, (3840, 2160), 30);
        assert_eq!(
            asked.rate,
            Rate::Vbr {
                average: 45_000_000,
                max: 90_000_000
            }
        );
        let av1 = node_spec(NodeCodec::Av1, 0, Input::Iyuv, (1920, 1080), 30);
        assert_eq!((av1.out, av1.b_frames, av1.lookahead), (Out::Av1, 0, 0));
    }

    /// Eight bits made ten: four times the value, in the high bits of each
    /// 16-bit sample; the chroma interleaved, U first.
    #[test]
    fn eight_bits_become_ten_exactly() {
        let (w, h) = (4usize, 2usize);
        let mut i420 = vec![16u8, 235, 128, 0, 255, 1, 2, 3];
        i420.extend([100u8, 101]); // U
        i420.extend([200u8, 201]); // V
        let p010 = i420_to_p010(&i420, (w, h));
        assert_eq!(p010.len(), Input::P010.bytes((w, h)));
        let samples: Vec<u16> = p010
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b) >> 6)
            .collect();
        assert_eq!(&samples[..8], &[64, 940, 512, 0, 1020, 4, 8, 12]);
        assert_eq!(&samples[8..], &[400, 800, 404, 804], "U, V, U, V");
    }
}
