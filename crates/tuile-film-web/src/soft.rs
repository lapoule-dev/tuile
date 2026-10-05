// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The encoder of last resort: AV1, in this module's own wasm.
//!
//! A browser's H.264 encoder stops at a picture size — 4096×2304 for most —
//! and a pack baked larger has nothing to encode it with. rav1e is pure
//! Rust, compiles to wasm32 as it is, and takes any size. It is two orders
//! of magnitude slower than a hardware encoder, which is why it is the
//! fallback and never the first choice.

use rav1e::prelude::*;
use wasm_bindgen::prelude::*;

fn js(e: impl std::fmt::Debug) -> JsError {
    JsError::new(&format!("{e:?}"))
}

/// One encoded frame.
#[wasm_bindgen]
pub struct SoftPacket {
    data: Vec<u8>,
    /// The frame's number, from 0, in the order frames were pushed.
    pub index: u32,
    pub key: bool,
}

#[wasm_bindgen]
impl SoftPacket {
    /// The temporal unit, taken: a second call gives nothing.
    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.data)
    }
}

#[wasm_bindgen]
pub struct SoftEncoder {
    ctx: Context<u8>,
    width: usize,
    height: usize,
    flushed: bool,
}

#[wasm_bindgen]
impl SoftEncoder {
    /// An AV1 encoder for I420 frames of this size, at rav1e's fastest
    /// preset, one key frame every two seconds, no frame reordering.
    #[wasm_bindgen(constructor)]
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<SoftEncoder, JsError> {
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
        enc.bitrate = bitrate.min(i32::MAX as u32) as i32;
        // Packets out in the order frames went in, and soon after: the
        // muxer wants them in order and a worker should not sit on forty
        // frames of a film it holds in memory as raw pictures.
        enc.low_latency = true;
        enc.speed_settings.rdo_lookahead_frames = 1;
        enc.min_key_frame_interval = u64::from(2 * fps.max(1));
        enc.max_key_frame_interval = u64::from(2 * fps.max(1));
        let ctx = Config::new()
            .with_encoder_config(enc)
            .with_threads(1)
            .new_context()
            .map_err(js)?;
        Ok(SoftEncoder {
            ctx,
            width: width as usize,
            height: height as usize,
            flushed: false,
        })
    }

    /// The film's `av1C` record, for its track header.
    pub fn config(&self) -> Vec<u8> {
        self.ctx.container_sequence_header()
    }

    /// One frame: planar I420, `width × height × 3 / 2` bytes.
    pub fn push(&mut self, i420: &[u8]) -> Result<(), JsError> {
        let (w, h) = (self.width, self.height);
        let (luma, chroma) = (w * h, (w / 2) * (h / 2));
        if i420.len() != luma + 2 * chroma {
            return Err(JsError::new(&format!(
                "a {w}×{h} I420 frame is {} bytes, got {}",
                luma + 2 * chroma,
                i420.len()
            )));
        }
        let mut frame = self.ctx.new_frame();
        frame.planes[0].copy_from_raw_u8(&i420[..luma], w, 1);
        frame.planes[1].copy_from_raw_u8(&i420[luma..luma + chroma], w / 2, 1);
        frame.planes[2].copy_from_raw_u8(&i420[luma + chroma..], w / 2, 1);
        self.ctx.send_frame(frame).map_err(js)
    }

    /// No more frames: what is still inside comes out of `next_packet`.
    pub fn finish(&mut self) {
        if !self.flushed {
            self.ctx.flush();
            self.flushed = true;
        }
    }

    /// The next encoded frame, or nothing when the encoder wants more input
    /// (or, after `finish`, has given everything).
    pub fn next_packet(&mut self) -> Result<Option<SoftPacket>, JsError> {
        loop {
            match self.ctx.receive_packet() {
                Ok(packet) => {
                    return Ok(Some(SoftPacket {
                        index: packet.input_frameno as u32,
                        key: packet.frame_type == FrameType::KEY,
                        data: packet.data,
                    }))
                }
                // A frame was consumed without a packet yet: ask again.
                Err(EncoderStatus::Encoded) => continue,
                Err(EncoderStatus::NeedMoreData) | Err(EncoderStatus::LimitReached) => {
                    return Ok(None)
                }
                Err(e) => return Err(js(e)),
            }
        }
    }
}
