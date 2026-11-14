// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A run of a film's samples decoded by the machine's own decoder — NVDEC
//! on an NVIDIA card, VideoToolbox on a Mac — into eight-bit I420
//! pictures, in the order they are shown.
//!
//! A ten-bit stream (the mezzanine, a phone's HEVC) is taken down to eight
//! bits with the compression's dither ([`crate::compress::p016_to_nv12`]):
//! what a composite lays its layer over and hands back to the render's own
//! encoder, which takes the render's eight bits.

use std::path::Path;

use crate::Error;

/// One decoded picture: I420, rows tightly packed.
pub struct Picture {
    pub i420: Vec<u8>,
    pub size: (usize, usize),
    /// Its place among the run's pictures in the order they are shown,
    /// from 0.
    pub shown: u64,
}

/// NV12 (Y, then U and V interleaved) as I420 (Y, then U, then V).
pub fn nv12_to_i420(nv12: &[u8], (width, height): (usize, usize)) -> Vec<u8> {
    let luma = width * height;
    let chroma = width.div_ceil(2) * height.div_ceil(2);
    let mut out = Vec::with_capacity(luma + 2 * chroma);
    out.extend_from_slice(&nv12[..luma]);
    let pairs = &nv12[luma..luma + 2 * chroma];
    out.extend(pairs.iter().step_by(2));
    out.extend(pairs.iter().skip(1).step_by(2));
    out
}

/// A decoded picture, NV12 (8 bits) or P016 (more), as I420.
pub fn to_i420(data: &[u8], size: (usize, usize), bit_depth: u8) -> Vec<u8> {
    if bit_depth > 8 {
        nv12_to_i420(&crate::compress::p016_to_nv12(data, size), size)
    } else {
        nv12_to_i420(data, size)
    }
}

/// The pictures of `track` (a film's video track, or a run of it that
/// starts on a key frame: [`tuile_mp4::film::Track::range`]) in the file at
/// `path`, each handed to `take` in the order shown. NVDEC on CUDA device
/// `device` in a build with `nvenc`; VideoToolbox in one with
/// `videotoolbox`. Returns the pictures handed.
#[allow(unused_variables, unused_mut)]
pub fn pictures(
    path: &Path,
    track: tuile_mp4::film::Track,
    device: usize,
    mut take: impl FnMut(Picture) -> Result<(), String>,
) -> Result<u64, Error> {
    let stream = crate::compress::Stream::of(track)?;
    #[cfg(all(target_os = "linux", feature = "nvenc"))]
    {
        gpu::pictures(path, &stream, device, take)
    }
    #[cfg(all(target_os = "macos", feature = "videotoolbox"))]
    {
        let size = stream.size().ok_or("a track of no size")?;
        let decoder = crate::videotoolbox::Decoder::of(&stream)?;
        let ten = decoder.out() == crate::encode::Input::P010;
        let mut file = std::fs::File::open(path)?;
        Ok(decoder.decode_all(&stream, &mut file, |frame| {
            take(Picture {
                i420: to_i420(&frame.data, size, if ten { 10 } else { 8 }),
                size,
                shown: frame.shown,
            })
        })?)
    }
    #[cfg(not(any(
        all(target_os = "linux", feature = "nvenc"),
        all(target_os = "macos", feature = "videotoolbox")
    )))]
    {
        Err("this build has no decoder: NVDEC (nvenc) or VideoToolbox (videotoolbox)".into())
    }
}

#[cfg(all(target_os = "linux", feature = "nvenc"))]
mod gpu {
    use std::path::Path;

    use cudarc::driver::CudaContext;
    use nvidia_video_codec_sdk::{DecodeCodec, Decoder};

    use super::{to_i420, Picture};
    use crate::compress::Stream;

    pub(super) fn pictures(
        path: &Path,
        stream: &Stream,
        device: usize,
        mut take: impl FnMut(Picture) -> Result<(), String>,
    ) -> Result<u64, crate::Error> {
        let ctx = CudaContext::new(device).map_err(|e| format!("CUDA device {device}: {e:?}"))?;
        let codec = if stream.hevc {
            DecodeCodec::Hevc
        } else {
            DecodeCodec::H264
        };
        let mut decoder = Decoder::new(ctx, codec)?;
        let mut file = std::fs::File::open(path)?;
        let mut shown = 0u64;
        let mut hand =
            |pictures: Vec<nvidia_video_codec_sdk::Picture>| -> Result<(), crate::Error> {
                for picture in pictures {
                    let size = (picture.width as usize, picture.height as usize);
                    take(Picture {
                        i420: to_i420(&picture.data, size, picture.bit_depth),
                        size,
                        shown,
                    })?;
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
        Ok(shown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NV12's interleaved chroma comes apart into I420's two planes, and a
    /// ten-bit picture comes down to eight on the way.
    #[test]
    fn nv12_and_p016_come_out_as_i420() {
        let size = (4, 2);
        let nv12 = [1, 2, 3, 4, 5, 6, 7, 8, 10, 20, 11, 21];
        assert_eq!(
            nv12_to_i420(&nv12, size),
            [1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 20, 21]
        );
        let p016: Vec<u8> = nv12
            .iter()
            .flat_map(|&v| (u16::from(v) << 8).to_le_bytes())
            .collect();
        assert_eq!(to_i420(&p016, size, 10), nv12_to_i420(&nv12, size));
    }
}
