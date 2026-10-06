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
    /// Time spent off the render's own thread, in seconds, by name: what
    /// the render's own timings cannot see.
    fn spent(&self) -> Vec<(String, f64)> {
        Vec::new()
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
///
/// The encoder runs on its own thread, a few pictures behind the render,
/// so that drawing a frame and encoding the one before it overlap.
pub struct Av1Film {
    path: PathBuf,
    bitrate: u32,
    pictures: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    encoder: Option<std::thread::JoinHandle<Result<(Vec<u8>, f64), String>>>,
    size: (usize, usize),
    encoding_seconds: f64,
}

/// Pictures waiting for the encoder before the render waits for it.
const WAITING: usize = 4;

fn drain(ctx: &mut Context<u8>, muxer: &mut Muxer) -> Result<(), String> {
    loop {
        match ctx.receive_packet() {
            Ok(packet) => muxer
                .push(
                    packet.input_frameno,
                    packet.data,
                    packet.frame_type == FrameType::KEY,
                )
                .map_err(|e| e.to_string())?,
            Err(EncoderStatus::Encoded) => continue,
            Err(EncoderStatus::NeedMoreData | EncoderStatus::LimitReached) => return Ok(()),
            Err(e) => return Err(format!("AV1 encoder: {e:?}")),
        }
    }
}

impl Av1Film {
    pub fn at(path: impl Into<PathBuf>, bitrate: u32) -> Self {
        Self {
            path: path.into(),
            bitrate,
            pictures: None,
            encoder: None,
            size: (0, 0),
            encoding_seconds: 0.0,
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
        // A picture cut in tiles is encoded on as many cores.
        enc.tiles = if width * height >= 1280 * 720 { 8 } else { 2 };
        let mut ctx: Context<u8> = Config::new()
            .with_encoder_config(enc)
            .with_threads(0)
            .new_context()
            .map_err(|e| format!("AV1 encoder: {e:?}"))?;
        let mut muxer = Muxer::with(
            u16::try_from(width)?,
            u16::try_from(height)?,
            fps,
            Codec::Av1(ctx.container_sequence_header()),
        )?;
        let (w, h) = (width as usize, height as usize);
        self.size = (w, h);
        let (pictures, waiting) = std::sync::mpsc::sync_channel::<Vec<u8>>(WAITING);
        self.pictures = Some(pictures);
        self.encoder = Some(std::thread::spawn(move || {
            let (luma, chroma) = (w * h, (w / 2) * (h / 2));
            let mut busy = 0.0f64;
            for i420 in waiting {
                let began = std::time::Instant::now();
                let mut frame = ctx.new_frame();
                frame.planes[0].copy_from_raw_u8(&i420[..luma], w, 1);
                frame.planes[1].copy_from_raw_u8(&i420[luma..luma + chroma], w / 2, 1);
                frame.planes[2].copy_from_raw_u8(&i420[luma + chroma..], w / 2, 1);
                ctx.send_frame(frame)
                    .map_err(|e| format!("AV1 encoder: {e:?}"))?;
                drain(&mut ctx, &mut muxer)?;
                busy += began.elapsed().as_secs_f64();
            }
            let began = std::time::Instant::now();
            ctx.flush();
            drain(&mut ctx, &mut muxer)?;
            let film = muxer.finish().map_err(|e| e.to_string())?;
            Ok((film, busy + began.elapsed().as_secs_f64()))
        }));
        Ok(())
    }

    fn wants_i420(&self) -> bool {
        true
    }

    fn picture(&mut self, _index: u32, _frame: u32, _: &[u8], i420: &[u8]) -> Result<(), Error> {
        let (w, h) = self.size;
        if i420.len() != w * h + 2 * (w / 2) * (h / 2) {
            return Err(format!("a {w}×{h} I420 picture is not {} bytes", i420.len()).into());
        }
        let pictures = self.pictures.as_ref().ok_or("the film was not opened")?;
        if pictures.send(i420.to_vec()).is_err() {
            // The encoder stopped: say why, not that a channel closed.
            self.pictures = None;
            return match self.encoder.take().map(std::thread::JoinHandle::join) {
                Some(Ok(Err(why))) => Err(why.into()),
                _ => Err("the encoder stopped".into()),
            };
        }
        Ok(())
    }

    fn close(&mut self) -> Result<(), Error> {
        self.pictures = None;
        let encoder = self.encoder.take().ok_or("the film was not opened")?;
        let (film, seconds) = encoder.join().map_err(|_| "the encoder panicked")??;
        self.encoding_seconds = seconds;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.path, film)?;
        Ok(())
    }

    fn spent(&self) -> Vec<(String, f64)> {
        vec![(
            "encoding AV1, on its own thread".into(),
            self.encoding_seconds,
        )]
    }
}
