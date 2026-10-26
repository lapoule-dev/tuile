// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! An interactive session, recorded: the camera on `/camera` as every tape
//! has it, and beside it, frame by frame, what else the picture depended on.
//!
//! A camera path reproduces where the eye was. A flickering sky or stars
//! that come and go depend on more: the instant the scene was lit for and
//! how fast its clock ran, the wall time the renderer's own pacing read, the
//! size of the picture, and what the atmosphere decided that frame — its
//! exposure, its white balance, its light worked out again or kept. Those
//! go on three topics of the same file, on the same timeline as the camera,
//! so a frame of the recording is everything a replay needs to draw it
//! again and everything a diagnosis needs to say why it looked as it did:
//!
//! - [`FRAME_TOPIC`] — one [`FrameContext`] a frame: wall time, scene clock,
//!   viewport, render origin;
//! - [`SKY_TOPIC`] — one [`SkyState`] a frame the atmosphere was drawn: what
//!   the renderer lit the frame with;
//! - [`INPUT_TOPIC`] — the [`InputEvents`] that arrived before the frame;
//! - [`IMAGE_TOPIC`] — **the picture the viewer drew**, as it was on the
//!   screen: `foxglove.CompressedImage`, JPEG or PNG, on the frame it
//!   shows ([`Tape::push_picture`]). What went wrong live — the tiles
//!   resident at that moment, the exposure's history, the streaming's
//!   timing — is in these pixels and in no replay; [`for_each_picture`]
//!   reads them back.
//!
//! [`read`] joins them back, one [`Recorded`] a frame. A file with only a
//! camera — any tape — reads too, its frames without context.
//!
//! Every number round-trips to the bit: `f64` is written in Rust's shortest
//! round-trip form and read back exactly.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{
    write_encoded, Frame, Tape, TapeError, IMAGE_JSON_SCHEMA, IMAGE_SCHEMA, IMAGE_TOPIC,
    NANOS_PER_FRAME,
};

/// Where a frame's [`FrameContext`] goes.
pub const FRAME_TOPIC: &str = "/viewer/frame";
/// Where a frame's [`SkyState`] goes.
pub const SKY_TOPIC: &str = "/viewer/sky";
/// Where a frame's [`InputEvents`] go.
pub const INPUT_TOPIC: &str = "/viewer/input";

const FRAME_SCHEMA: &str = "tuile.ViewerFrame";
const FRAME_JSON_SCHEMA: &str = r#"{
  "type": "object",
  "description": "one frame of an interactive session: see tuile_tape::session::FrameContext",
  "properties": {
    "version":       {"type": "integer", "const": 1},
    "wall":          {"type": "number", "description": "the wall instant the frame was drawn at, Unix seconds"},
    "instant":       {"type": "number", "description": "the scene's instant the frame was lit for, Unix seconds UTC"},
    "rate":          {"type": "number", "description": "scene seconds a wall second"},
    "offset":        {"type": "number", "description": "instant - wall, seconds"},
    "viewport":      {"type": "array", "items": {"type": "integer"}, "minItems": 2, "maxItems": 2},
    "scale":         {"type": "number", "description": "device pixels a point"},
    "render_origin": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3}
  },
  "required": ["version", "wall", "instant", "rate", "viewport", "scale", "render_origin"]
}"#;

const SKY_SCHEMA: &str = "tuile.ViewerSky";
const SKY_JSON_SCHEMA: &str = r#"{
  "type": "object",
  "description": "what the atmosphere lit one frame with: see tuile_tape::session::SkyState",
  "properties": {
    "version":       {"type": "integer", "const": 1},
    "exposure":      {"type": ["number", "null"]},
    "white_balance": {"type": "array", "items": {"type": "number"}},
    "ambient":       {"type": "array", "items": {"type": "number"}},
    "harmonic":      {"type": "array", "items": {"type": "array"}},
    "illuminance":   {"type": "array", "items": {"type": "number"}},
    "to_sun":        {"type": "array", "items": {"type": "number"}},
    "to_moon":       {"type": ["array", "null"]},
    "light_updates": {"type": "integer"},
    "view_updates":  {"type": "integer"},
    "stars_drawn":   {"type": ["integer", "null"]}
  },
  "required": ["version", "light_updates", "view_updates"]
}"#;

const INPUT_SCHEMA: &str = "tuile.ViewerInput";
const INPUT_JSON_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "version": {"type": "integer", "const": 1},
    "events":  {"type": "array", "items": {"type": "string"}}
  },
  "required": ["version", "events"]
}"#;

/// What a frame was drawn under, beside its camera.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FrameContext {
    /// The wall instant the frame was drawn at, Unix seconds: what the
    /// renderer's own pacing reads.
    pub wall: f64,
    /// The scene's instant the frame was lit for, Unix seconds UTC.
    pub instant: f64,
    /// Scene seconds a wall second: 0 for a held clock.
    pub rate: f64,
    /// `instant - wall`: how far the scene's clock is from the wall's.
    pub offset: f64,
    /// The frame, device pixels.
    pub viewport: [u32; 2],
    /// Device pixels a point.
    pub scale: f64,
    /// The origin every position on the GPU was relative to, ECEF metres.
    pub render_origin: [f64; 3],
}

/// What the atmosphere lit a frame with, as the renderer had it.
///
/// Read off the frame's uniform: every light in it is pre-exposed, so an
/// exposure that steps shows here as a step in [`Self::illuminance`] and
/// [`Self::ambient`] even when [`Self::exposure`] itself is not known.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct SkyState {
    /// The exposure's linear multiplier, when the renderer says it.
    pub exposure: Option<f64>,
    /// The white balance the picture was graded with (rgb).
    pub white_balance: [f64; 3],
    /// The sky's light averaged over every facing, pre-exposed (rgb): the
    /// harmonic's constant band.
    pub ambient: [f64; 3],
    /// The sky's first-order harmonic, pre-exposed: x, y and z, rgb each.
    pub harmonic: [[f64; 3]; 3],
    /// The sun's illuminance at the eye, pre-exposed (rgb).
    pub illuminance: [f64; 3],
    /// Towards the sun, ECEF.
    pub to_sun: [f64; 3],
    /// Towards the Moon, ECEF, when it is up anywhere.
    pub to_moon: Option<[f64; 3]>,
    /// How many times the light was worked out since the renderer began: a
    /// step between two frames is a light recomputed between them.
    pub light_updates: u64,
    /// How many times the sky-view table was made since the renderer began.
    pub view_updates: u64,
    /// Stars drawn, when the renderer counts them.
    pub stars_drawn: Option<u64>,
}

/// The input events that reached the application before a frame, each in
/// a few words (`key P pressed`, `wheel -1.0`, …).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct InputEvents {
    pub events: Vec<String>,
}

/// One frame of a recorded session.
#[derive(Debug, Clone, PartialEq)]
pub struct Recorded {
    /// From 0, as the tape's timeline counts.
    pub index: u64,
    pub camera: Frame,
    pub context: Option<FrameContext>,
    pub sky: Option<SkyState>,
    pub input: Vec<String>,
}

/// A record as it is written: the record and its version, flat.
#[derive(Serialize, Deserialize)]
struct Versioned<T> {
    version: u32,
    #[serde(flatten)]
    record: T,
}

fn json<T: Serialize>(record: T) -> String {
    serde_json::to_string(&Versioned {
        version: 1,
        record,
    })
    .unwrap_or_default()
}

impl Tape {
    /// Appends what the frame whose camera was pushed last was drawn under.
    pub fn push_context(&mut self, context: &FrameContext) {
        self.push_json(FRAME_TOPIC, FRAME_SCHEMA, FRAME_JSON_SCHEMA, &json(context));
    }

    /// Appends what the atmosphere lit the frame whose camera was pushed
    /// last with.
    pub fn push_sky(&mut self, sky: &SkyState) {
        self.push_json(SKY_TOPIC, SKY_SCHEMA, SKY_JSON_SCHEMA, &json(sky));
    }

    /// Appends the input events that arrived before the frame whose camera
    /// was pushed last. Nothing when there were none.
    pub fn push_input(&mut self, events: &[String]) {
        if events.is_empty() {
            return;
        }
        let record = InputEvents {
            events: events.to_vec(),
        };
        self.push_json(INPUT_TOPIC, INPUT_SCHEMA, INPUT_JSON_SCHEMA, &json(&record));
    }
}

impl Tape {
    /// Frames pushed so far: the index the next camera pushed will have.
    pub fn frames(&self) -> u64 {
        self.progress().0 as u64
    }

    /// Appends a picture already encoded as `format` (`jpeg`, `png`) on
    /// [`IMAGE_TOPIC`], on frame `frame` — given, not inferred: a picture
    /// read back from the GPU arrives frames after the camera it was drawn
    /// under was pushed, and its index is what keeps the two together.
    ///
    /// Silent on failure like [`Tape::push_image`]: failures are logged.
    pub fn push_picture(&mut self, frame: u64, format: &str, encoded: &[u8]) {
        let Some((writer, channel, _)) =
            self.channel_for(IMAGE_TOPIC, IMAGE_SCHEMA, IMAGE_JSON_SCHEMA)
        else {
            return;
        };
        if let Err(e) = write_encoded(writer, channel, format, encoded, frame) {
            tracing::error!("cannot write a recorded picture: {e}");
        }
    }
}

/// A picture of a recording, as it was encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    /// The frame it shows, as [`Recorded::index`] counts.
    pub index: u64,
    /// `jpeg` or `png`.
    pub format: String,
    pub data: Vec<u8>,
}

impl Picture {
    /// Its size, read from its header without decoding it.
    pub fn size(&self) -> Result<(u32, u32), TapeError> {
        image::ImageReader::new(std::io::Cursor::new(&self.data))
            .with_guessed_format()
            .map_err(|e| TapeError::NotAPath(format!("picture {}: {e}", self.index)))?
            .into_dimensions()
            .map_err(|e| TapeError::NotAPath(format!("picture {}: {e}", self.index)))
    }

    /// Decoded to tightly packed RGBA8, and its size.
    pub fn rgba(&self) -> Result<(Vec<u8>, (u32, u32)), TapeError> {
        let image = image::load_from_memory(&self.data)
            .map_err(|e| TapeError::NotAPath(format!("picture {}: {e}", self.index)))?
            .into_rgba8();
        let size = image.dimensions();
        Ok((image.into_raw(), size))
    }
}

#[derive(Deserialize)]
struct CompressedImage {
    format: String,
    data: String,
}

/// Calls `each` with every picture of the recording at `path`, in frame
/// order. Returns how many there were.
pub fn for_each_picture(
    path: impl AsRef<Path>,
    mut each: impl FnMut(Picture) -> Result<(), TapeError>,
) -> Result<u64, TapeError> {
    let bytes = std::fs::read(path.as_ref())?;
    let mut stamps = Vec::new();
    for message in mcap::MessageStream::new(&bytes)? {
        let message = message?;
        if message.channel.topic == IMAGE_TOPIC {
            stamps.push((message.log_time, message.data.into_owned()));
        }
    }
    // Written as they were read back, a few frames late and not always in
    // order: put in the order of the frames they show.
    stamps.sort_by_key(|(stamp, _)| *stamp);
    let count = stamps.len() as u64;
    for (stamp, data) in stamps {
        let image: CompressedImage = serde_json::from_slice(&data)
            .map_err(|e| TapeError::NotAPath(format!("a picture that does not read: {e}")))?;
        each(Picture {
            index: stamp / NANOS_PER_FRAME,
            format: image.format,
            data: base64_decode(&image.data)?,
        })?;
    }
    Ok(count)
}

/// The frames of the recording at `path` that have a picture, in order:
/// without decoding a picture.
pub fn pictured_frames(path: impl AsRef<Path>) -> Result<Vec<u64>, TapeError> {
    let bytes = std::fs::read(path.as_ref())?;
    let mut frames = Vec::new();
    for message in mcap::MessageStream::new(&bytes)? {
        let message = message?;
        if message.channel.topic == IMAGE_TOPIC {
            frames.push(message.log_time / NANOS_PER_FRAME);
        }
    }
    frames.sort_unstable();
    Ok(frames)
}

fn base64_decode(text: &str) -> Result<Vec<u8>, TapeError> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut table = [255u8; 256];
    for (i, c) in ALPHABET.iter().enumerate() {
        table[*c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for byte in text.bytes() {
        if byte == b'=' {
            break;
        }
        let value = table[byte as usize];
        if value == 255 {
            return Err(TapeError::NotAPath("a picture's data is not base64".into()));
        }
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

fn parse<T: for<'de> Deserialize<'de>>(topic: &str, data: &[u8]) -> Result<T, TapeError> {
    let record: Versioned<T> = serde_json::from_slice(data)
        .map_err(|e| TapeError::NotAPath(format!("a {topic} record that does not read: {e}")))?;
    if record.version != 1 {
        return Err(TapeError::NotAPath(format!(
            "a {topic} record of version {}, this reader knows 1",
            record.version
        )));
    }
    Ok(record.record)
}

/// Every frame of the session at `path`, its camera and what was recorded
/// beside it, in order.
pub fn read(path: impl AsRef<Path>) -> Result<Vec<Recorded>, TapeError> {
    let mut tape = Tape::replaying(path.as_ref())?;
    let mut frames = Vec::new();
    while let Some(camera) = tape.next_frame() {
        frames.push(Recorded {
            index: frames.len() as u64,
            camera,
            context: None,
            sky: None,
            input: Vec::new(),
        });
    }
    let bytes = std::fs::read(path.as_ref())?;
    for message in mcap::MessageStream::new(&bytes)? {
        let message = message?;
        let topic = message.channel.topic.as_str();
        if ![FRAME_TOPIC, SKY_TOPIC, INPUT_TOPIC].contains(&topic) {
            continue;
        }
        let index = (message.log_time / NANOS_PER_FRAME) as usize;
        let Some(frame) = frames.get_mut(index) else {
            return Err(TapeError::NotAPath(format!(
                "a {topic} record for frame {index}, past the camera's {} frames",
                frames.len()
            )));
        };
        match topic {
            FRAME_TOPIC => frame.context = Some(parse(topic, &message.data)?),
            SKY_TOPIC => frame.sky = Some(parse(topic, &message.data)?),
            _ => frame
                .input
                .extend(parse::<InputEvents>(topic, &message.data)?.events),
        }
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values whose last bits a rounding format would lose.
    fn awkward(n: u64) -> (Frame, FrameContext, SkyState) {
        let k = n as f64;
        let camera = Frame {
            position: [
                4_517_590.878_849_123 + k * 0.1 + 1e-9,
                11_169.471_000_000_1,
                4_487_348.409_866_7 - k / 3.0,
            ],
            direction: [-0.707_106_781_186_547_5, 0.1 / 3.0, 0.707_106_781_186_547_6],
            up: [0.0, 1.0, std::f64::consts::PI / 1e17],
            fovy: 0.785_398_163_397_448_3,
        };
        let context = FrameContext {
            wall: 1_782_043_200.123_456_7 + k / 60.0,
            instant: 1_782_043_200.0 + k * (1.0 / 3.0),
            rate: 1.0 / 3.0 * 60.0,
            offset: 0.1 + 0.2,
            viewport: [1280, 720],
            scale: 2.0,
            render_origin: camera.position,
        };
        let sky = SkyState {
            exposure: Some(1.0 / 7.0),
            white_balance: [1.0, 0.98 + 1e-15, 0.9],
            ambient: [0.1, 0.2, 0.3],
            harmonic: [[1e-30, 2.0, 3.0]; 3],
            illuminance: [5.0 / 3.0; 3],
            to_sun: [0.6, 0.0, 0.8],
            to_moon: (n % 2 == 0).then_some([0.0, 0.6, 0.8]),
            light_updates: n,
            view_updates: n / 2,
            stars_drawn: None,
        };
        (camera, context, sky)
    }

    /// Recorded, read back: every camera, context and sky to the bit, each
    /// on the frame it was recorded with — the camera held still for some
    /// frames included, which the tape stores once.
    #[test]
    fn a_session_round_trips_to_the_bit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.mcap");
        let mut tape = Tape::recording(&path).expect("records");
        let mut written = Vec::new();
        for n in 0..300u64 {
            // Held still for frames 100..150: one run on `/camera`.
            let (camera, context, sky) = awkward(if (100..150).contains(&n) { 100 } else { n });
            let context = FrameContext {
                wall: context.wall + n as f64,
                ..context
            };
            tape.push(camera);
            tape.push_context(&context);
            tape.push_sky(&sky);
            let input = if n % 7 == 0 {
                vec![format!("key {n} pressed")]
            } else {
                Vec::new()
            };
            tape.push_input(&input);
            written.push((camera, context, sky, input));
        }
        tape.finish().expect("closes");
        let read = read(&path).expect("reads");
        assert_eq!(read.len(), written.len());
        for (frame, (camera, context, sky, input)) in read.iter().zip(&written) {
            assert_eq!(frame.camera, *camera, "frame {}", frame.index);
            assert_eq!(frame.context, Some(*context), "frame {}", frame.index);
            assert_eq!(frame.sky, Some(*sky), "frame {}", frame.index);
            assert_eq!(&frame.input, input, "frame {}", frame.index);
            // Equal as values is not enough for -0.0 and NaN: the bits.
            for (a, b) in frame.camera.position.iter().zip(camera.position) {
                assert_eq!(a.to_bits(), b.to_bits());
            }
            let c = frame.context.expect("context");
            assert_eq!(c.wall.to_bits(), context.wall.to_bits());
            assert_eq!(c.instant.to_bits(), context.instant.to_bits());
        }
    }

    /// A picture read back frames after its camera was pushed, and written
    /// out of order, lands on the frame it shows; a PNG decodes back to the
    /// pixels it was made from, bit for bit.
    #[test]
    fn a_picture_lands_on_its_frame_and_decodes_to_its_pixels() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.mcap");
        let mut tape = Tape::recording(&path).expect("records");
        let pixels = |n: u64| -> Vec<u8> {
            (0..6 * 4u64)
                .flat_map(|p| [(p * 7 + n) as u8, (p * 13) as u8, n as u8, 255])
                .collect()
        };
        let png = |n: u64| {
            let mut out = Vec::new();
            image::ImageEncoder::write_image(
                image::codecs::png::PngEncoder::new(&mut out),
                &pixels(n),
                6,
                4,
                image::ExtendedColorType::Rgba8,
            )
            .expect("encodes");
            out
        };
        for n in 0..10u64 {
            tape.push(awkward(n).0);
            // Each picture arrives two frames late, the even ones before
            // the odd ones.
            if n >= 2 {
                tape.push_picture(n - 2, "png", &png(n - 2));
            }
        }
        for late in [9u64, 8] {
            tape.push_picture(late, "png", &png(late));
        }
        tape.finish().expect("closes");
        let frames = read(&path).expect("reads");
        let mut seen = Vec::new();
        for_each_picture(&path, |picture| {
            let (rgba, size) = picture.rgba().expect("decodes");
            assert_eq!(size, (6, 4));
            assert_eq!(rgba, pixels(picture.index), "picture {}", picture.index);
            // The camera of its frame is the camera it was drawn under.
            assert_eq!(frames[picture.index as usize].camera, awkward(picture.index).0);
            seen.push(picture.index);
            Ok(())
        })
        .expect("pictures");
        assert_eq!(seen, (0..10).collect::<Vec<_>>());
    }

    /// A plain tape — a camera and nothing else — reads as a session whose
    /// frames carry no context.
    #[test]
    fn a_plain_tape_reads_without_context() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("path.mcap");
        let mut tape = Tape::recording(&path).expect("records");
        for n in 0..5 {
            tape.push(awkward(n).0);
        }
        tape.finish().expect("closes");
        let read = read(&path).expect("reads");
        assert_eq!(read.len(), 5);
        assert!(read.iter().all(|f| f.context.is_none() && f.sky.is_none()));
    }
}
