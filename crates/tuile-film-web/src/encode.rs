// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A film's frames, encoded: by the browser when it can, by this module when
//! it cannot.
//!
//! Encoding is progressive. The browser's own H.264 encoder (WebCodecs)
//! reads the canvas directly and is used whenever it exists for the film's
//! size. When it does not — most stop at 4096×2304 — the frame is read back
//! as I420, converted on the GPU, and encoded to AV1 by rav1e, compiled into
//! this same module.

use std::cell::RefCell;
use std::rc::Rc;

use js_sys::{ArrayBuffer, Uint8Array};
use wasm_bindgen::prelude::*;
use web_sys::OffscreenCanvas;

use crate::js::{call, construct, get, has, number, object, settled, sleep, string, text};
use crate::soft::SoftEncoder;
use crate::worker::FilmWorker;

/// High profile from level 4.0 up to 6.2, then Main and Baseline: the first
/// the browser accepts wins. Level 5.1 stops at 4096×2304; 6.x reaches 8K,
/// on the browsers and machines that have such an encoder.
const CODECS: [&str; 9] = [
    "avc1.640028",
    "avc1.640032",
    "avc1.640033",
    "avc1.640034",
    "avc1.64003c",
    "avc1.64003d",
    "avc1.64003e",
    "avc1.4d0033",
    "avc1.42e033",
];

/// One encoded frame: its place in the film, and whether a decoder can start
/// from it.
pub struct Chunk {
    pub index: u32,
    pub key: bool,
    pub data: Vec<u8>,
}

/// The browser's encoder configuration for this size, or `None` when it has
/// no H.264 encoder for it.
pub async fn browser_config(width: u32, height: u32, fps: u32, bitrate: f64) -> Option<JsValue> {
    if !has("VideoEncoder") {
        return None;
    }
    let encoder = get(&js_sys::global(), "VideoEncoder");
    for codec in CODECS {
        let config: JsValue = object(&[
            ("codec", codec.into()),
            ("width", width.into()),
            ("height", height.into()),
            ("bitrate", bitrate.into()),
            ("framerate", fps.into()),
            ("avc", object(&[("format", "avc".into())]).into()),
            ("latencyMode", "quality".into()),
        ])
        .into();
        // A size the browser refuses to even consider throws; that is a no.
        let Ok(asked) = call(&encoder, "isConfigSupported", &[config.clone()]) else {
            continue;
        };
        if let Ok(answer) = settled(asked).await {
            if get(&answer, "supported").as_bool() == Some(true) {
                return Some(config);
            }
        }
    }
    None
}

type Output = Closure<dyn FnMut(JsValue, JsValue)>;
type Failed = Closure<dyn FnMut(JsValue)>;

/// The browser's encoder, fed the canvas.
struct Browser {
    encoder: JsValue,
    codec: String,
    fps: u32,
    record: Rc<RefCell<Option<Vec<u8>>>>,
    chunks: Rc<RefCell<Vec<Chunk>>>,
    failure: Rc<RefCell<Option<String>>>,
    // Held for as long as the encoder may call them.
    _output: Output,
    _failed: Failed,
}

/// The bytes of a `BufferSource`: an `ArrayBuffer`, or a view onto one.
fn bytes_of(source: &JsValue) -> Vec<u8> {
    if let Some(buffer) = source.dyn_ref::<ArrayBuffer>() {
        return Uint8Array::new(buffer).to_vec();
    }
    let buffer = get(source, "buffer");
    let (offset, length) = (
        number(source, "byteOffset") as u32,
        number(source, "byteLength") as u32,
    );
    Uint8Array::new_with_byte_offset_and_length(&buffer, offset, length).to_vec()
}

impl Browser {
    fn new(config: JsValue, fps: u32) -> Result<Self, String> {
        let record = Rc::new(RefCell::new(None));
        let chunks = Rc::new(RefCell::new(Vec::new()));
        let failure = Rc::new(RefCell::new(None));
        let (record_in, chunks_in) = (record.clone(), chunks.clone());
        let output: Output = Closure::new(move |chunk: JsValue, meta: JsValue| {
            // The first chunk carries the stream's description: its avcC.
            let description = get(&get(&meta, "decoderConfig"), "description");
            if !description.is_undefined() && !description.is_null() && record_in.borrow().is_none()
            {
                *record_in.borrow_mut() = Some(bytes_of(&description));
            }
            let data = Uint8Array::new_with_length(number(&chunk, "byteLength") as u32);
            let _ = call(&chunk, "copyTo", &[data.clone().into()]);
            chunks_in.borrow_mut().push(Chunk {
                index: (number(&chunk, "timestamp") * f64::from(fps) / 1e6).round() as u32,
                key: string(&chunk, "type") == "key",
                data: data.to_vec(),
            });
        });
        let failure_in = failure.clone();
        let failed: Failed = Closure::new(move |e: JsValue| {
            *failure_in.borrow_mut() = Some(text(e));
        });
        let init = object(&[
            ("output", output.as_ref().clone()),
            ("error", failed.as_ref().clone()),
        ]);
        let encoder = construct("VideoEncoder", &[init.into()]).map_err(text)?;
        call(&encoder, "configure", &[config.clone()]).map_err(text)?;
        Ok(Self {
            encoder,
            codec: string(&config, "codec"),
            fps,
            record,
            chunks,
            failure,
            _output: output,
            _failed: failed,
        })
    }

    fn check(&self) -> Result<(), String> {
        match self.failure.borrow().as_ref() {
            Some(why) => Err(format!("the browser's encoder failed: {why}")),
            None => Ok(()),
        }
    }

    async fn frame(&self, canvas: &OffscreenCanvas, index: u32, n: u32) -> Result<(), String> {
        self.check()?;
        let fps = f64::from(self.fps);
        let timing = object(&[
            ("timestamp", (f64::from(index) * 1e6 / fps).round().into()),
            ("duration", (1e6 / fps).round().into()),
        ]);
        let frame =
            construct("VideoFrame", &[canvas.clone().into(), timing.into()]).map_err(text)?;
        // A key frame first, then one every two seconds of film.
        let options = object(&[("keyFrame", (n % (2 * self.fps) == 0).into())]);
        let encoded = call(&self.encoder, "encode", &[frame.clone(), options.into()]);
        let _ = call(&frame, "close", &[]);
        encoded.map_err(text)?;
        // The encoder is not to be handed frames faster than it takes them:
        // each is a full picture held until it is encoded.
        while number(&self.encoder, "encodeQueueSize") > 4.0 {
            sleep(1).await;
            self.check()?;
        }
        Ok(())
    }

    async fn finish(&self) -> Result<(), String> {
        let flushed = call(&self.encoder, "flush", &[]).map_err(text)?;
        settled(flushed).await.map_err(text)?;
        let _ = call(&self.encoder, "close", &[]);
        self.check()
    }
}

/// rav1e, fed I420 read back from the GPU. Its packets are numbered from 0
/// in the order frames went in, so the film's index of the first frame it
/// was given places them.
struct Soft {
    encoder: SoftEncoder,
    base: Option<u32>,
    chunks: Vec<Chunk>,
}

impl Soft {
    fn drain(&mut self) -> Result<(), String> {
        let base = self.base.unwrap_or(0);
        while let Some(mut packet) = self.encoder.next_packet().map_err(text)? {
            self.chunks.push(Chunk {
                index: base + packet.index,
                key: packet.key,
                data: packet.take(),
            });
        }
        Ok(())
    }
}

enum Kind {
    Browser(Browser),
    Soft(Soft),
}

/// Whichever encoder this film's size gets.
pub struct Encoder(Kind);

/// A slice of a film, encoded.
pub struct Encoded {
    pub codec: String,
    /// The stream's configuration record: an `avcC`, or an `av1C`.
    pub record: Vec<u8>,
    pub chunks: Vec<Chunk>,
}

impl Encoder {
    /// The browser's encoder when it has one for this size, rav1e otherwise.
    pub async fn open(width: u32, height: u32, fps: u32, bitrate: f64) -> Result<Self, String> {
        match browser_config(width, height, fps, bitrate).await {
            Some(config) => Ok(Self(Kind::Browser(Browser::new(config, fps)?))),
            None => Ok(Self(Kind::Soft(Soft {
                encoder: SoftEncoder::new(width, height, fps, bitrate as u32).map_err(text)?,
                base: None,
                chunks: Vec::new(),
            }))),
        }
    }

    /// Readies a renderer for this encoder: rav1e needs each frame's planes.
    pub fn prepare(&self, film: &mut FilmWorker) {
        if matches!(self.0, Kind::Soft(_)) {
            film.enable_i420();
        }
    }

    /// Takes the frame `film` has just rendered: `index` is its place in the
    /// film, `n` its place in this slice.
    pub async fn frame(
        &mut self,
        film: &FilmWorker,
        canvas: &OffscreenCanvas,
        index: u32,
        n: u32,
    ) -> Result<(), String> {
        match &mut self.0 {
            Kind::Browser(browser) => browser.frame(canvas, index, n).await,
            Kind::Soft(soft) => {
                soft.base.get_or_insert(index);
                let planes = film.read_i420().await.map_err(text)?;
                soft.encoder.push(&planes).map_err(text)?;
                soft.drain()
            }
        }
    }

    pub async fn finish(self) -> Result<Encoded, String> {
        match self.0 {
            Kind::Browser(browser) => {
                browser.finish().await?;
                let record = browser
                    .record
                    .borrow_mut()
                    .take()
                    .ok_or("the encoder gave no avcC description")?;
                let chunks = std::mem::take(&mut *browser.chunks.borrow_mut());
                Ok(Encoded {
                    codec: browser.codec.clone(),
                    record,
                    chunks,
                })
            }
            Kind::Soft(mut soft) => {
                soft.encoder.finish();
                soft.drain()?;
                Ok(Encoded {
                    codec: "av01 (rav1e)".into(),
                    record: soft.encoder.config(),
                    chunks: soft.chunks,
                })
            }
        }
    }
}
