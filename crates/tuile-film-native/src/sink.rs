// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Where a film's pictures go.

use std::path::PathBuf;

use rav1e::prelude::*;
use tuile_mp4::{Codec, Muxer};

use crate::Error;

/// Takes the pictures of a film, in order.
pub trait Sink {
    /// Called once, before the first picture.
    fn open(&mut self, _width: u32, _height: u32, _fps: u32) -> Result<(), Error> {
        Ok(())
    }
    /// Whether [`Self::picture`] reads `i420`: converting costs a pass.
    fn wants_i420(&self) -> bool {
        false
    }
    /// One picture: `rgba` tightly packed, `i420` planar BT.709 limited
    /// range (empty unless asked for). `index` counts from 0.
    fn picture(&mut self, index: u32, frame: u32, rgba: &[u8], i420: &[u8]) -> Result<(), Error>;
    /// No more pictures.
    fn close(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// Keeps nothing: a render made to be measured.
pub struct Nothing;

impl Sink for Nothing {
    fn picture(&mut self, _: u32, _: u32, _: &[u8], _: &[u8]) -> Result<(), Error> {
        Ok(())
    }
}

/// One PNG a picture.
pub struct Pictures {
    dir: PathBuf,
    size: (u32, u32),
}

impl Pictures {
    pub fn into(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            size: (0, 0),
        }
    }
}

impl Sink for Pictures {
    fn open(&mut self, width: u32, height: u32, _fps: u32) -> Result<(), Error> {
        std::fs::create_dir_all(&self.dir)?;
        self.size = (width, height);
        Ok(())
    }

    fn picture(&mut self, _index: u32, frame: u32, rgba: &[u8], _: &[u8]) -> Result<(), Error> {
        image::save_buffer(
            self.dir.join(format!("frame-{frame:05}.png")),
            rgba,
            self.size.0,
            self.size.1,
            image::ColorType::Rgba8,
        )?;
        Ok(())
    }
}

/// The film as an mp4, AV1 by rav1e: no encoder asked of the machine.
pub struct Av1Film {
    path: PathBuf,
    bitrate: u32,
    state: Option<(Context<u8>, Muxer, (usize, usize))>,
    written: u64,
}

impl Av1Film {
    pub fn at(path: impl Into<PathBuf>, bitrate: u32) -> Self {
        Self {
            path: path.into(),
            bitrate,
            state: None,
            written: 0,
        }
    }

    fn drain(&mut self) -> Result<(), Error> {
        let Some((ctx, muxer, _)) = self.state.as_mut() else {
            return Ok(());
        };
        loop {
            match ctx.receive_packet() {
                Ok(packet) => {
                    muxer.push(
                        packet.input_frameno,
                        packet.data,
                        packet.frame_type == FrameType::KEY,
                    )?;
                    self.written += 1;
                }
                Err(EncoderStatus::Encoded) => continue,
                Err(EncoderStatus::NeedMoreData | EncoderStatus::LimitReached) => return Ok(()),
                Err(e) => return Err(format!("AV1 encoder: {e:?}").into()),
            }
        }
    }
}

impl Sink for Av1Film {
    fn open(&mut self, width: u32, height: u32, fps: u32) -> Result<(), Error> {
        let mut enc = EncoderConfig::with_speed_preset(10);
        enc.width = width as usize;
        enc.height = height as usize;
        enc.bit_depth = 8;
        enc.chroma_sampling = ChromaSampling::Cs420;
        enc.pixel_range = PixelRange::Limited;
        enc.color_description = Some(ColorDescription {
            color_primaries: ColorPrimaries::BT709,
            transfer_characteristics: TransferCharacteristics::BT709,
            matrix_coefficients: MatrixCoefficients::BT709,
        });
        enc.time_base = Rational::new(1, u64::from(fps.max(1)));
        enc.bitrate = self.bitrate.min(i32::MAX as u32) as i32;
        // Packets out in the order pictures went in: the muxer wants them
        // so, and nothing here should sit on a film's worth of raw frames.
        enc.low_latency = true;
        enc.speed_settings.rdo_lookahead_frames = 1;
        enc.min_key_frame_interval = u64::from(2 * fps.max(1));
        enc.max_key_frame_interval = u64::from(2 * fps.max(1));
        let ctx: Context<u8> = Config::new()
            .with_encoder_config(enc)
            .with_threads(0)
            .new_context()
            .map_err(|e| format!("AV1 encoder: {e:?}"))?;
        let muxer = Muxer::with(
            u16::try_from(width)?,
            u16::try_from(height)?,
            fps,
            Codec::Av1(ctx.container_sequence_header()),
        )?;
        self.state = Some((ctx, muxer, (width as usize, height as usize)));
        Ok(())
    }

    fn wants_i420(&self) -> bool {
        true
    }

    fn picture(&mut self, _index: u32, _frame: u32, _: &[u8], i420: &[u8]) -> Result<(), Error> {
        let (ctx, _, (w, h)) = self.state.as_mut().ok_or("the film was not opened")?;
        let (w, h) = (*w, *h);
        let (luma, chroma) = (w * h, (w / 2) * (h / 2));
        if i420.len() != luma + 2 * chroma {
            return Err(format!("a {w}×{h} I420 picture is not {} bytes", i420.len()).into());
        }
        let mut frame = ctx.new_frame();
        frame.planes[0].copy_from_raw_u8(&i420[..luma], w, 1);
        frame.planes[1].copy_from_raw_u8(&i420[luma..luma + chroma], w / 2, 1);
        frame.planes[2].copy_from_raw_u8(&i420[luma + chroma..], w / 2, 1);
        ctx.send_frame(frame)
            .map_err(|e| format!("AV1 encoder: {e:?}"))?;
        self.drain()
    }

    fn close(&mut self) -> Result<(), Error> {
        if let Some((ctx, ..)) = self.state.as_mut() {
            ctx.flush();
        }
        self.drain()?;
        let (_, muxer, _) = self.state.take().ok_or("the film was not opened")?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.path, muxer.finish()?)?;
        Ok(())
    }
}
