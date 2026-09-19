// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Record a camera path, then fly it again exactly.
//!
//! Some defects only appear after a particular sequence of gestures. The one
//! that produced black ground in the globe viewer took a zoom in, a zoom out, a
//! zoom in, a rotation, a tilt, more rotation and a zoom out, in that order,
//! over one specific lake. Reproducing that by hand costs a minute and lands
//! somewhere slightly different every time, which is no basis for deciding
//! whether a change helped.
//!
//! So the camera is recorded, not the gestures: one row per frame holding the
//! eye, where it looks, which way is up, and the field of view. Those seven
//! numbers are the complete determinant of a frame, so replaying them drives
//! the render and the traversal from what the session actually used, with no
//! interpretation in between.
//!
//! # Why MCAP
//!
//! Because a session is not only a camera path. What has to be captured to
//! diagnose a render is the pose **and the picture it produced**, on one
//! timeline — two files means two clocks and no way to say which frame went
//! with which pose. MCAP is a container for exactly that: several channels, one
//! time base, and it streams, so a reader can open a file whose writer died.
//!
//! It cost something to move here from Parquet, and the thing it cost is worth
//! naming: an `f64` written to Parquet comes back bit-identical, and a globe at
//! a metre of altitude does not survive rounding — shave the last digits off an
//! ECEF position and a replay is flying a different path while claiming to
//! reproduce one. So the camera is written with Rust's shortest round-trip
//! formatting, which is exact for `f64`: every value read back is the same
//! bits. That is a property this crate must keep, and a test holds it.
//!
//! # Rendering-agnostic
//!
//! This crate knows nothing about any camera type or any renderer: [`Frame`] is
//! seven numbers. A host converts its own camera in and out, which is a line
//! each way, and keeps this out of its dependency graph.
//!
//! ```no_run
//! use tuile_tape::{Frame, Tape};
//!
//! // Record.
//! let mut tape = Tape::recording("path.mcap")?;
//! tape.push(Frame { position: [0.0; 3], direction: [0.0, 0.0, -1.0], up: [0.0, 1.0, 0.0], fovy: 1.0 });
//! tape.finish()?;                     // flushes and closes
//!
//! // Replay.
//! let mut tape = Tape::replaying("path.mcap")?;
//! while let Some(frame) = tape.next_frame() {
//!     // drive the camera from `frame`
//! }
//! # Ok::<(), tuile_tape::TapeError>(())
//! ```

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use image::ImageEncoder;

#[derive(Debug, thiserror::Error)]
pub enum TapeError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("mcap: {0}")]
    Mcap(#[from] mcap::McapError),
    #[error("not a camera path: {0}")]
    NotAPath(String),
}
pub mod path;


/// One frame's camera. ECEF metres; `direction` and `up` are unit vectors;
/// `fovy` is the vertical field of view in radians.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    pub position: [f64; 3],
    pub direction: [f64; 3],
    pub up: [f64; 3],
    pub fovy: f64,
}

/// A camera, and how many consecutive frames it was held for.
///
/// A viewer samples at its refresh rate, so a hand off the controls writes
/// sixty identical rows a second — 2599 of them at the head of the first real
/// recording, nearly half the file. Storing the run instead of the repeats
/// costs one integer and replays to the identical frame sequence, because
/// `hold` is a count of frames rather than a duration: nothing is resampled,
/// nothing is interpolated, and a replay is still frame-for-frame what was
/// flown.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Run {
    frame: Frame,
    hold: u32,
}

/// The ten `f64` columns, in the order they are written. Flat rather than
/// nested lists so the file opens cleanly in every tool that reads it.
const COLUMNS: [&str; 10] = [
    "position_x",
    "position_y",
    "position_z",
    "direction_x",
    "direction_y",
    "direction_z",
    "up_x",
    "up_y",
    "up_z",
    "fovy",
];

/// The topic the camera path is published on.
const CAMERA_TOPIC: &str = "/camera";

/// The schema name recorded alongside it.
const CAMERA_SCHEMA: &str = "tuile.Camera";

/// Frames are indexed, not timed, but a reader wants a clock. Sixty a second is
/// what a viewer runs at, so a path opened in any MCAP tool spans the wall time
/// it actually took to fly.
const NANOS_PER_FRAME: u64 = 1_000_000_000 / 60;

/// Enough for a reader to interpret the channel without asking anyone.
const CAMERA_JSON_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "position":  {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
    "direction": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
    "up":        {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
    "fovy":      {"type": "number"},
    "hold":      {"type": "integer", "description": "consecutive frames this camera was held for"}
  },
  "required": ["position", "direction", "up", "fovy"]
}"#;

/// How often the journal reaches the disk, in frames.
///
/// A session worth recording is usually one that ends badly, so what is lost
/// when it does has to be small. A few seconds of flying is a reasonable amount
/// to lose; a `write` per frame is not a reasonable amount to pay.
const BATCH: usize = 256;

/// The suffix of the crash journal that sits beside the MCAP file while a
/// recording is running. See [`Tape::recording`].
pub const JOURNAL_SUFFIX: &str = ".journal";

/// One run: ten little-endian `f64` then a little-endian `u32` hold. Fixed
/// width and no framing, which is what makes the journal readable at any byte
/// offset after a crash.
const JOURNAL_ROW: usize = COLUMNS.len() * 8 + 4;

fn journal_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(JOURNAL_SUFFIX);
    name.into()
}

/// A camera path being written, or being flown.
///
/// A struct around a private enum rather than a public one: the run-length
/// encoding is an implementation detail, and a host that could match on it
/// would be coupled to it.
pub struct Tape(Inner);

/// Recording carries an open MCAP writer and Replaying a vector of frames, and
/// the two differ in size by a wide margin. Boxed would move the writer to the
/// heap for no gain: a `Tape` is one per session and lives for its whole
/// duration, so the enum is never copied, never in an array, and the difference
/// costs a few hundred bytes once.
#[allow(clippy::large_enum_variant)]
enum Inner {
    Recording {
        /// Where the MCAP file goes. Written into as the session runs, not at
        /// the end: an hour of frames does not fit in memory, and a trace whose
        /// value is highest when the process dies must already be on disk when
        /// it does.
        destination: std::path::PathBuf,
        writer: mcap::Writer<BufWriter<File>>,
        camera_channel: u16,
        /// Déclaré à la première image écrite, et pas avant.
        ///
        /// Une tape de trajectoire n'écrit que des poses. Annoncer un canal
        /// d'images qu'elle n'utilisera jamais coûtait un second schéma dans
        /// le résumé, et donc un ordre d'itération à deux éléments — que
        /// `mcap` tire d'un `HashMap`. Avec un seul canal, l'ordre est celui
        /// qu'il y a, et le fichier est le même à chaque génération.
        image_channel: Option<u16>,
        /// The crash journal: raw, append-only, readable at any byte offset.
        /// Camera only — it is what a replay needs, and it is the part that has
        /// to survive a kill.
        journal: BufWriter<File>,
        pending: Vec<Run>,
        /// The run still being extended: the camera has not changed since it
        /// started, so its length is not known yet.
        open: Option<Run>,
        /// Frames accounted for, repeats included. Also the timeline: message
        /// timestamps are frame indices, so a camera and the picture it made
        /// carry the same stamp and no reader has to guess.
        written: u64,
    },
    Replaying {
        frames: Vec<Frame>,
        at: usize,
    },
}

/// A writer whose bytes depend only on what is written, not on when.
///
/// # The unordered part, and why it is not fixed here
///
/// `mcap` builds its summary section from `HashMap<u16, _>` and writes the
/// repeated schema and channel records in iteration order — which Rust
/// randomises per process. Two tapes generated from the same six parameters
/// therefore differed in their last 1300 bytes: in one, `tuile.Camera` came
/// first; in the other, `foxglove.CompressedImage` did. The 121808 bytes of
/// poses before them were identical.
///
/// The cost was real. A bake names its pack after a digest that includes the
/// tape, so the same scene baked twice landed under two different names —
/// measured on 16 September 2026, when a Cloud Run bake announced
/// `05274f175b83df6a` for the trajectory a local bake had called
/// `fb45fe68fb26e559`.
///
/// The fix belongs upstream and is two lines — sort `all_schemas` and
/// `all_channels` by id before writing them (`mcap-0.25.0/src/write.rs:1298`
/// and `:1312`). Dropping the repetition instead would make the file
/// deterministic by making it poorer: a reader that seeks to the summary could
/// no longer learn the schemas without streaming the data section.
///
/// So the records stay, and the ambiguity is removed at the other end: a tape
/// declares a channel when it first writes to one (see [`Tape::push_image`]),
/// and a trajectory tape writes only camera poses. One schema and one channel
/// iterate in one order.
fn deterministic_writer<W: std::io::Write + std::io::Seek>(
    w: W,
) -> Result<mcap::Writer<W>, TapeError> {
    Ok(mcap::Writer::new(w)?)
}

impl Tape {
    /// Whether this tape is being written rather than flown.
    pub fn is_recording(&self) -> bool {
        matches!(self.0, Inner::Recording { .. })
    }

    /// Opens a path for writing, truncating anything already there.
    ///
    /// # Surviving a bad ending
    ///
    /// An MCAP file ends with a summary and a footer, and a writer killed before
    /// [`Tape::finish`] leaves a file most readers will refuse. That is the
    /// opposite of what a recording is for: the sessions worth recording are the
    /// ones that crash.
    ///
    /// So the frames go first to a journal beside the destination — ten
    /// little-endian `f64` per frame, no header, no framing — which is readable
    /// at any byte offset by construction, and is converted to MCAP on the
    /// way out. Kill the process at any moment and
    /// [`Tape::recover`] still yields every frame that reached the disk.
    pub fn recording(path: impl AsRef<Path>) -> Result<Self, TapeError> {
        let destination = path.as_ref().to_path_buf();
        let mut writer = deterministic_writer(BufWriter::new(File::create(&destination)?))?;
        let camera_schema =
            writer.add_schema(CAMERA_SCHEMA, "jsonschema", CAMERA_JSON_SCHEMA.as_bytes())?;
        let camera_channel = writer.add_channel(
            camera_schema,
            CAMERA_TOPIC,
            "json",
            &std::collections::BTreeMap::new(),
        )?;
        Ok(Self(Inner::Recording {
            journal: BufWriter::new(File::create(journal_path(&destination))?),
            destination,
            writer,
            camera_channel,
            // Déclaré au premier usage, pas d'avance : voir `push_image`.
            image_channel: None,
            pending: Vec::with_capacity(BATCH),
            open: None,
            written: 0,
        }))
    }

    /// Turns an abandoned journal into the MCAP file it was going to become.
    ///
    /// Returns how many whole frames were salvaged. A journal truncated
    /// mid-frame — the process died between two `write` calls — loses only that
    /// last partial frame.
    pub fn recover(journal: impl AsRef<Path>, into: impl AsRef<Path>) -> Result<u64, TapeError> {
        let bytes = std::fs::read(journal.as_ref())?;
        let runs: Vec<Run> = bytes
            .chunks_exact(JOURNAL_ROW)
            .map(|row| {
                let at = |n: usize| {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&row[n * 8..n * 8 + 8]);
                    f64::from_le_bytes(b)
                };
                let mut h = [0u8; 4];
                h.copy_from_slice(&row[COLUMNS.len() * 8..]);
                Run {
                    frame: Frame {
                        position: [at(0), at(1), at(2)],
                        direction: [at(3), at(4), at(5)],
                        up: [at(6), at(7), at(8)],
                        fovy: at(9),
                    },
                    hold: u32::from_le_bytes(h),
                }
            })
            .collect();
        let frames: u64 = runs.iter().map(|r| u64::from(r.hold)).sum();
        write_mcap(into.as_ref(), &runs)?;
        Ok(frames)
    }

    /// Reads a whole path into memory. A path is small — a camera per run,
    /// so an hour at sixty frames a second is under twenty megabytes — and
    /// having it all up front means a replay never stalls on IO and so never
    /// perturbs the thing it is measuring.
    pub fn replaying(path: impl AsRef<Path>) -> Result<Self, TapeError> {
        let bytes = std::fs::read(path.as_ref())?;
        let mut frames = Vec::new();
        for message in mcap::MessageStream::new(&bytes)? {
            let message = message?;
            if message.channel.topic != CAMERA_TOPIC {
                continue;
            }
            let text = std::str::from_utf8(&message.data)
                .map_err(|e| TapeError::NotAPath(format!("camera message is not utf-8: {e}")))?;
            let (frame, hold) = parse_camera(text)?;
            // `hold` is a count of frames, never a duration: nothing is
            // resampled and nothing is interpolated, so a replay is
            // frame-for-frame what was flown.
            frames.extend(std::iter::repeat_n(frame, hold.max(1) as usize));
        }
        if frames.is_empty() {
            return Err(TapeError::NotAPath(format!(
                "{} carries no {CAMERA_TOPIC} messages",
                path.as_ref().display()
            )));
        }
        Ok(Self(Inner::Replaying { frames, at: 0 }))
    }

    /// Appends one frame. Buffered; see [`Tape::finish`].
    pub fn push(&mut self, frame: Frame) {
        let Inner::Recording {
            writer,
            camera_channel,
            journal,
            pending,
            open,
            written,
            ..
        } = &mut self.0
        else {
            return;
        };
        // A camera that has not moved extends the run it is already in. Bitwise
        // equality on purpose: anything looser would coalesce two genuinely
        // different views and quietly rewrite the path.
        match open {
            Some(run) if run.frame == frame && run.hold < u32::MAX => {
                run.hold += 1;
                *written += 1;
                return;
            }
            _ => {}
        }
        if let Some(finished) = open.replace(Run { frame, hold: 1 }) {
            // The run is closed, so its length is known and it can go out. Its
            // timestamp is the frame it *started* on, which is what makes a
            // camera line up with the picture taken under it.
            let started = *written - u64::from(finished.hold);
            if let Err(e) = write_camera(writer, *camera_channel, &finished, started) {
                tracing::error!("cannot write the camera path: {e}");
            }
            pending.push(finished);
        }
        *written += 1;
        if pending.len() >= BATCH {
            if let Err(e) = write_journal(journal, pending) {
                tracing::error!("cannot write the camera journal: {e}");
            }
            pending.clear();
        }
    }

    /// Appends the picture that frame produced.
    ///
    /// `rgba` is tightly packed RGBA8, `width * height * 4` long — a readback
    /// straight from the render target. It goes into the **same** file as the
    /// camera, on the same timeline: two files would be two clocks, and nothing
    /// could then say which picture went with which pose, which is the only
    /// question a trace exists to answer.
    ///
    /// PNG rather than raw. A trace is minutes of frames and raw RGBA is a
    /// megabyte each; a globe is mostly smooth ground and compresses hard. The
    /// encode is the cost, and it is paid on whatever thread calls this — a
    /// caller recording every frame of a live session should hand it every
    /// *n*th, and one recording a headless run should hand it all of them.
    ///
    /// Silent on failure, by design: a trace that killed the session it was
    /// observing would be worse than no trace. Failures are logged.
    pub fn push_image(&mut self, rgba: &[u8], width: u32, height: u32) {
        let Inner::Recording {
            writer,
            image_channel,
            written,
            ..
        } = &mut self.0
        else {
            return;
        };
        // `written` counts frames *pushed*, and a host pushes the camera before
        // the picture it took under it — so the picture belongs to the frame
        // just closed, not the one about to open. Stamping with `written` put
        // every image one frame late: invisible standing still, and exactly
        // wrong in the fast movement a trace is opened to explain. A test holds
        // this.
        let frame_index = written.saturating_sub(1);
        // Le canal naît ici, à la première image. MCAP autorise un schéma et
        // un canal à apparaître n'importe où dans la section de données, tant
        // que c'est avant le premier message qui s'y réfère.
        let channel = match image_channel {
            Some(id) => *id,
            None => {
                let declared = writer
                    .add_schema(IMAGE_SCHEMA, "jsonschema", IMAGE_JSON_SCHEMA.as_bytes())
                    .and_then(|schema| {
                        writer.add_channel(
                            schema,
                            IMAGE_TOPIC,
                            "json",
                            &std::collections::BTreeMap::new(),
                        )
                    });
                match declared {
                    Ok(id) => *image_channel.insert(id),
                    Err(e) => {
                        tracing::error!("cannot declare the trace channel: {e}");
                        return;
                    }
                }
            }
        };
        if let Err(e) = write_image(writer, channel, rgba, width, height, frame_index) {
            tracing::error!("cannot write a trace frame: {e}");
        }
    }

    /// The next recorded frame, or `None` at the end of the path.
    pub fn next_frame(&mut self) -> Option<Frame> {
        let Inner::Replaying { frames, at } = &mut self.0 else {
            return None;
        };
        let frame = frames.get(*at).copied()?;
        *at += 1;
        Some(frame)
    }

    /// How far a replay has got, as `(flown, total)`.
    pub fn progress(&self) -> (usize, usize) {
        match &self.0 {
            Inner::Replaying { frames, at } => (*at, frames.len()),
            Inner::Recording { written, .. } => {
                let n = *written as usize;
                (n, n)
            }
        }
    }

    /// Writes what is buffered and closes the file. Returns how many frames the
    /// path holds.
    ///
    /// Consuming, because an MCAP file without its footer is one most readers
    /// refuse — leaving that to a `Drop` that cannot report failure would make a
    /// truncated path look like a short one.
    pub fn finish(self) -> Result<u64, TapeError> {
        match self.0 {
            Inner::Replaying { frames, .. } => Ok(frames.len() as u64),
            Inner::Recording {
                destination,
                mut writer,
                camera_channel,
                mut journal,
                mut pending,
                open,
                written,
                ..
            } => {
                if let Some(last) = open {
                    let started = written - u64::from(last.hold);
                    write_camera(&mut writer, camera_channel, &last, started)?;
                    pending.push(last);
                }
                if !pending.is_empty() {
                    write_journal(&mut journal, &pending)?;
                }
                journal.flush()?;
                drop(journal);
                // The summary and footer, which is what makes the file readable
                // by anything that is not a streaming reader.
                writer.finish()?;
                // Only once the file is complete on disk: a failure above leaves
                // the journal in place, which is the whole point of having one.
                let _ = std::fs::remove_file(journal_path(&destination));
                Ok(written)
            }
        }
    }
}

/// Appends frames to the crash journal and gets them to the disk.
fn write_journal(out: &mut BufWriter<File>, runs: &[Run]) -> Result<(), TapeError> {
    let mut row = [0u8; JOURNAL_ROW];
    for run in runs {
        let frame = &run.frame;
        let values = [
            frame.position[0],
            frame.position[1],
            frame.position[2],
            frame.direction[0],
            frame.direction[1],
            frame.direction[2],
            frame.up[0],
            frame.up[1],
            frame.up[2],
            frame.fovy,
        ];
        for (n, value) in values.iter().enumerate() {
            row[n * 8..n * 8 + 8].copy_from_slice(&value.to_le_bytes());
        }
        row[COLUMNS.len() * 8..].copy_from_slice(&run.hold.to_le_bytes());
        out.write_all(&row)?;
    }
    // Past the process's own buffer, so a kill -9 does not take it with it.
    out.flush()?;
    Ok(())
}

/// Reads one camera message.
///
/// Hand-parsed rather than pulled through a JSON crate: the document is written
/// by [`write_mcap`] a few lines up and has exactly five keys, so a dependency
/// here would buy nothing but a build. A malformed message is a stated error,
/// never a silently dropped frame — a replay that quietly skips part of a path
/// is worse than one that refuses to start.
fn parse_camera(text: &str) -> Result<(Frame, u32), TapeError> {
    let bad = |what: &str| TapeError::NotAPath(format!("camera message has no {what}: {text}"));
    let after = |key: &str| -> Option<&str> {
        let at = text.find(&format!("\"{key}\":"))?;
        Some(&text[at + key.len() + 3..])
    };
    let triple = |key: &str| -> Option<[f64; 3]> {
        let rest = after(key)?;
        let inner = rest.strip_prefix('[')?;
        let end = inner.find(']')?;
        let mut out = [0.0; 3];
        for (slot, part) in out.iter_mut().zip(inner[..end].split(',')) {
            *slot = part.trim().parse().ok()?;
        }
        Some(out)
    };
    let scalar = |key: &str| -> Option<f64> {
        let rest = after(key)?;
        let end = rest.find([',', '}']).unwrap_or(rest.len());
        rest[..end].trim().parse().ok()
    };
    Ok((
        Frame {
            position: triple("position").ok_or_else(|| bad("position"))?,
            direction: triple("direction").ok_or_else(|| bad("direction"))?,
            up: triple("up").ok_or_else(|| bad("up"))?,
            fovy: scalar("fovy").ok_or_else(|| bad("fovy"))?,
        },
        // Absent in a path written before run lengths existed: one frame each,
        // which is exactly what those rows were.
        scalar("hold").map_or(1, |h| h as u32),
    ))
}

/// The topic the rendered frames are published on.
const IMAGE_TOPIC: &str = "/frame";

/// Foxglove's own name for it, so any MCAP viewer shows the pictures without
/// being taught anything.
const IMAGE_SCHEMA: &str = "foxglove.CompressedImage";

const IMAGE_JSON_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "timestamp": {"type": "object", "properties": {"sec": {"type": "integer"}, "nsec": {"type": "integer"}}},
    "frame_id":  {"type": "string"},
    "data":      {"type": "string", "contentEncoding": "base64"},
    "format":    {"type": "string"}
  },
  "required": ["data", "format"]
}"#;

/// Writes one closed run of camera to the path channel.
///
/// `started` is the frame the run began on, so a camera and the picture taken
/// under it carry the same stamp. Stamping with the frame it *ended* on would
/// put every pose one run late, which is invisible in a still session and
/// exactly wrong in the fast movement a trace is opened to explain.
fn write_camera<W: std::io::Write + std::io::Seek>(
    writer: &mut mcap::Writer<W>,
    channel: u16,
    run: &Run,
    started: u64,
) -> Result<(), TapeError> {
    let f = &run.frame;
    // Rust's shortest round-trip formatting, which is exact for `f64`: the
    // values come back bit-identical, and a globe at a metre of altitude does
    // not survive anything less.
    let json = format!(
        r#"{{"position":[{},{},{}],"direction":[{},{},{}],"up":[{},{},{}],"fovy":{},"hold":{}}}"#,
        f.position[0],
        f.position[1],
        f.position[2],
        f.direction[0],
        f.direction[1],
        f.direction[2],
        f.up[0],
        f.up[1],
        f.up[2],
        f.fovy,
        run.hold,
    );
    let stamp = started * NANOS_PER_FRAME;
    writer.write_to_known_channel(
        &mcap::records::MessageHeader {
            channel_id: channel,
            sequence: started as u32,
            log_time: stamp,
            publish_time: stamp,
        },
        json.as_bytes(),
    )?;
    Ok(())
}

/// Writes one rendered frame to the image channel, PNG-encoded.
fn write_image<W: std::io::Write + std::io::Seek>(
    writer: &mut mcap::Writer<W>,
    channel: u16,
    rgba: &[u8],
    width: u32,
    height: u32,
    frame_index: u64,
) -> Result<(), TapeError> {
    let expected = width as usize * height as usize * 4;
    if rgba.len() != expected {
        return Err(TapeError::NotAPath(format!(
            "a {width}×{height} frame needs {expected} bytes, got {}",
            rgba.len()
        )));
    }
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|e| TapeError::NotAPath(format!("cannot encode a trace frame: {e}")))?;

    let stamp = frame_index * NANOS_PER_FRAME;
    let json = format!(
        r#"{{"timestamp":{{"sec":{},"nsec":{}}},"frame_id":"globe","format":"png","data":"{}"}}"#,
        stamp / 1_000_000_000,
        stamp % 1_000_000_000,
        base64(&png),
    );
    writer.write_to_known_channel(
        &mcap::records::MessageHeader {
            channel_id: channel,
            sequence: frame_index as u32,
            log_time: stamp,
            publish_time: stamp,
        },
        json.as_bytes(),
    )?;
    Ok(())
}

/// Standard base64, which is what the JSON schema says the bytes are in.
///
/// Twenty lines rather than a dependency: this crate exists to be small enough
/// that a host will take it, and an encoder with one caller and no branches is
/// not worth a version to track.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Writes the run-length path as MCAP.
///
/// MCAP rather than Parquet, and the reason is the *other* thing a session
/// records. A camera path is a column of doubles and Parquet held it well; a
/// trace is a camera path **and** the frames that came out of it, timestamped
/// together, and putting those in two files means two clocks and no way to say
/// which picture went with which pose. MCAP is a container for exactly that:
/// several channels, one timeline, and it streams — a reader can start on a
/// file whose writer died, which is the same property the journal beside this
/// one exists to give.
///
/// The camera goes out as JSON on one channel. Not for compactness — it is
/// larger than ten packed doubles — but because every tool that opens an MCAP
/// can then read it without a schema registry, and a path nobody can inspect is
/// a path nobody trusts.
fn write_mcap(path: &Path, runs: &[Run]) -> Result<(), TapeError> {
    let mut writer = deterministic_writer(BufWriter::new(File::create(path)?))?;
    let schema_id =
        writer.add_schema(CAMERA_SCHEMA, "jsonschema", CAMERA_JSON_SCHEMA.as_bytes())?;
    let channel_id = writer.add_channel(
        schema_id,
        CAMERA_TOPIC,
        "json",
        &std::collections::BTreeMap::new(),
    )?;

    // The timeline is the frame index, in nanoseconds at sixty frames a second.
    // A recording is frame-exact by construction — `hold` counts frames, never
    // durations — so this is a presentation choice for whoever opens the file,
    // and the replay recovers frames from the run lengths, not from the clock.
    let mut frame_index: u64 = 0;
    for (sequence, run) in runs.iter().enumerate() {
        let f = &run.frame;
        let json = format!(
            r#"{{"position":[{},{},{}],"direction":[{},{},{}],"up":[{},{},{}],"fovy":{},"hold":{}}}"#,
            f.position[0],
            f.position[1],
            f.position[2],
            f.direction[0],
            f.direction[1],
            f.direction[2],
            f.up[0],
            f.up[1],
            f.up[2],
            f.fovy,
            run.hold,
        );
        let stamp = frame_index * NANOS_PER_FRAME;
        writer.write_to_known_channel(
            &mcap::records::MessageHeader {
                channel_id,
                sequence: sequence as u32,
                log_time: stamp,
                publish_time: stamp,
            },
            json.as_bytes(),
        )?;
        frame_index += u64::from(run.hold.max(1));
    }
    writer.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {

    /// Deux tapes des mêmes paramètres sont le même fichier.
    ///
    /// Ce n'est pas une élégance : le digest de scène d'un pack porte sur les
    /// octets de la tape, donc une tape qui varie fait qu'une même scène cuite
    /// deux fois se range sous deux noms. Mesuré le 16 septembre 2026 — une
    /// cuisson Cloud Run a annoncé `05274f175b83df6a` pour la trajectoire
    /// qu'une cuisson locale appelait `fb45fe68fb26e559`, et les deux avaient
    /// raison.
    ///
    /// La cause était l'ordre d'itération d'un `HashMap` dans `mcap`, qui
    /// décide de l'ordre des schémas répétés dans le résumé. Elle ne mordait
    /// que parce qu'une tape déclarait deux canaux là où elle n'en écrit
    /// qu'un.
    #[test]
    fn two_tapes_of_the_same_path_are_the_same_bytes() {
        fn write(path: &std::path::Path) {
            let mut tape = Tape::recording(path).expect("recording");
            for i in 0..64 {
                let t = f64::from(i);
                tape.push(Frame {
                    position: [t, t * 2.0, t * 3.0],
                    direction: [0.0, 0.0, -1.0],
                    up: [0.0, 1.0, 0.0],
                    fovy: std::f64::consts::FRAC_PI_4,
                });
            }
            tape.finish().expect("close");
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.mcap");
        let b = dir.path().join("b.mcap");
        write(&a);
        write(&b);
        let (a, b) = (
            std::fs::read(&a).expect("read a"),
            std::fs::read(&b).expect("read b"),
        );
        assert_eq!(
            a.len(),
            b.len(),
            "deux tapes de même contenu n'ont pas la même taille"
        );
        let differing = a.iter().zip(&b).filter(|(x, y)| x != y).count();
        assert_eq!(
            differing, 0,
            "{differing} octets diffèrent entre deux tapes identiques — \
             la construction du fichier dépend d'autre chose que son contenu"
        );
    }

    /// Une tape qui n'écrit pas d'image n'en déclare pas le canal.
    #[test]
    fn a_trajectory_tape_declares_only_what_it_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.mcap");
        let mut tape = Tape::recording(&path).expect("recording");
        tape.push(Frame {
            position: [1.0, 2.0, 3.0],
            direction: [0.0, 0.0, -1.0],
            up: [0.0, 1.0, 0.0],
            fovy: 1.0,
        });
        tape.finish().expect("close");
        let bytes = std::fs::read(&path).expect("read");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains(CAMERA_SCHEMA), "le schéma caméra manque");
        assert!(
            !text.contains(IMAGE_SCHEMA),
            "le canal d'images est déclaré alors que rien ne l'utilise"
        );
    }

    use super::*;

    fn frame(n: f64) -> Frame {
        Frame {
            // Deliberately ugly numbers: an ECEF position near the surface has
            // seventeen significant digits, and losing the last of them moves
            // the camera by metres.
            position: [4517590.878123456 + n, 197461.2500000001, 4487348.5],
            // Two values one bit apart, so a round trip that quietly rounded
            // or renormalised would show up. Built rather than written out
            // because a literal that close to a known constant is a lint
            // magnet, and the point here is the bit, not the number.
            direction: [
                -std::f64::consts::FRAC_1_SQRT_2,
                0.0,
                -f64::from_bits(std::f64::consts::FRAC_1_SQRT_2.to_bits() - 1),
            ],
            up: [0.0, 1.0, 0.0],
            fovy: 0.9599310885968813,
        }
    }

    /// **The camera and the picture it produced are in one file, on one
    /// timeline.**
    ///
    /// That is the whole reason this crate left Parquet. A pose in one file and
    /// a frame in another means two clocks and nothing that can say which
    /// picture went with which pose — and "which pose was the globe in when
    /// that black square appeared" is the only question a trace is opened to
    /// answer.
    #[test]
    fn a_trace_carries_the_camera_and_the_frame_it_produced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("trace.mcap");

        // Two distinct cameras, a picture under each.
        let red = vec![255u8, 0, 0, 255];
        let blue = vec![0u8, 0, 255, 255];
        let mut tape = Tape::recording(&path).expect("open");
        tape.push(frame(0.0));
        tape.push_image(&red, 1, 1);
        tape.push(frame(1.0));
        tape.push_image(&blue, 1, 1);
        assert_eq!(tape.finish().expect("close"), 2);

        let bytes = std::fs::read(&path).expect("read");
        let mut cameras = Vec::new();
        let mut images = Vec::new();
        for message in mcap::MessageStream::new(&bytes).expect("stream") {
            let message = message.expect("message");
            match message.channel.topic.as_str() {
                CAMERA_TOPIC => cameras.push(message.log_time),
                IMAGE_TOPIC => images.push(message.log_time),
                other => panic!("unexpected topic {other}"),
            }
        }
        assert_eq!(cameras.len(), 2, "both cameras are in the file");
        assert_eq!(images.len(), 2, "and both pictures, in the same file");
        // Frame 0's camera and frame 0's picture carry the same stamp, and so
        // do frame 1's. Off-by-one here would put every pose one run late,
        // which is invisible standing still and exactly wrong in the fast
        // movement a trace is opened to explain.
        assert_eq!(cameras, images, "camera and frame share a timeline");
    }

    /// The whole point of the format: a path must come back bit-identical.
    /// Round the last digits off and the replay flies somewhere else while
    /// claiming to reproduce a session.
    #[test]
    fn a_path_round_trips_to_the_bit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("camera.mcap");
        let written: Vec<Frame> = (0..3).map(|n| frame(n as f64)).collect();

        let mut tape = Tape::recording(&path).expect("open for writing");
        for f in &written {
            tape.push(*f);
        }
        assert_eq!(tape.finish().expect("close"), 3);

        let mut tape = Tape::replaying(&path).expect("open for reading");
        let read: Vec<Frame> = std::iter::from_fn(|| tape.next_frame()).collect();
        assert_eq!(read, written, "the path came back changed");
    }

    /// More frames than one batch, so the multi-batch path is exercised — a
    /// recording that only ever wrote one batch would hide a bug that only
    /// shows up in a real session.
    #[test]
    fn a_path_longer_than_a_batch_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("long.mcap");
        let count = BATCH * 2 + 7;

        let mut tape = Tape::recording(&path).expect("open for writing");
        for n in 0..count {
            tape.push(frame(n as f64));
        }
        assert_eq!(tape.finish().expect("close"), count as u64);

        let mut tape = Tape::replaying(&path).expect("open for reading");
        assert_eq!(tape.progress(), (0, count));
        let read: Vec<Frame> = std::iter::from_fn(|| tape.next_frame()).collect();
        assert_eq!(read.len(), count);
        assert_eq!(read[count - 1], frame((count - 1) as f64));
    }

    /// The end of the path has to be visible, or a host cannot tell "flown" from
    /// "stalled" and a replay never terminates on its own.
    #[test]
    fn a_replay_reports_the_end() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("two.mcap");
        let mut tape = Tape::recording(&path).expect("open");
        tape.push(frame(0.0));
        tape.push(frame(1.0));
        tape.finish().expect("close");

        let mut tape = Tape::replaying(&path).expect("open");
        assert!(tape.next_frame().is_some());
        assert!(tape.next_frame().is_some());
        assert!(tape.next_frame().is_none(), "the end must be reported");
        assert_eq!(tape.progress(), (2, 2));
    }

    /// The case the journal exists for: the process dies without ever calling
    /// [`Tape::finish`].
    ///
    /// Simulated by dropping the tape on the floor. A footer-terminated file
    /// would lose the
    /// whole session here, not just the tail — its schema and row-group index
    /// live in a footer that is only written at close, so a file without one is
    /// not a short path, it is not a path at all.
    #[test]
    fn a_session_that_dies_without_finishing_is_still_recoverable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("crashed.mcap");
        let journal = dir.path().join(format!("crashed.mcap{JOURNAL_SUFFIX}"));

        let mut tape = Tape::recording(&path).expect("open");
        // More than a batch, so some frames have reached the disk and some are
        // still in hand — the realistic shape of a crash.
        for n in 0..BATCH + 40 {
            tape.push(frame(n as f64));
        }
        drop(tape); // the process is gone; nothing was finished

        // The file is there — it has been written into all along — but nothing
        // closed it, so it has no summary and no footer. That is precisely the
        // state the journal exists for, and asserting the file's *absence*, as
        // this test did when the container was only written at the end, would
        // now pass for the wrong reason.
        assert!(path.exists(), "the trace was being written as it went");
        assert!(journal.exists(), "and the journal is beside it");
        // **The container itself is now the better rescue.** MCAP is a
        // streaming format: a reader walks it from the front and stops where
        // the writing stopped, so every run that was flushed is readable
        // without a footer. Parquet was not — its schema and index live in a
        // footer, and a torn file was not a Parquet file at all — and that,
        // not durability in general, was why the journal was invented.
        let from_the_container = Tape::replaying(&path)
            .map(|mut t| std::iter::from_fn(|| t.next_frame()).count())
            .unwrap_or(0);
        assert!(
            from_the_container >= BATCH,
            "a torn MCAP still reads from the front: got {from_the_container} \
             frames, expected at least the {BATCH} that were flushed"
        );

        // The journal stays as the second belt: it survives a container whose
        // own last chunk is torn mid-record, which a streaming reader cannot
        // do anything with.
        let salvaged = Tape::recover(&journal, &path).expect("recover");
        assert_eq!(
            salvaged, BATCH as u64,
            "everything that reached the journal comes back"
        );

        let mut tape = Tape::replaying(&path).expect("the recovered file is a real mcap");
        let read: Vec<Frame> = std::iter::from_fn(|| tape.next_frame()).collect();
        assert_eq!(read.len(), BATCH);
        assert_eq!(read[0], frame(0.0), "and it comes back unchanged");
        assert_eq!(read[BATCH - 1], frame((BATCH - 1) as f64));
    }

    /// A journal cut mid-frame — the process died between two writes — loses
    /// that frame and nothing else.
    #[test]
    fn a_journal_truncated_mid_frame_loses_only_that_frame() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("torn.mcap");
        let journal = dir.path().join(format!("torn.mcap{JOURNAL_SUFFIX}"));

        let mut tape = Tape::recording(&path).expect("open");
        // Past a batch, so some runs have genuinely reached the disk.
        for n in 0..BATCH * 2 {
            tape.push(frame(n as f64));
        }
        drop(tape);

        let whole = Tape::recover(&journal, &path).expect("recover");
        assert!(whole > 0, "the journal reached the disk");

        // Lop off half a row and recover again.
        let bytes = std::fs::read(&journal).expect("read");
        std::fs::write(&journal, &bytes[..bytes.len() - JOURNAL_ROW / 2]).expect("truncate");
        let torn = Tape::recover(&journal, &path).expect("recover");
        assert_eq!(torn, whole - 1, "only the torn frame is lost");
    }

    /// A camera that does not move must not be written sixty times a second.
    /// Measured on the first real recording: 2599 identical rows at the head of
    /// the file, nearly half of it, from a hand off the controls.
    #[test]
    fn a_still_camera_is_stored_once_and_replays_the_same_number_of_frames() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("still.mcap");

        let mut tape = Tape::recording(&path).expect("open");
        for _ in 0..500 {
            tape.push(frame(0.0)); // never moves
        }
        tape.push(frame(1.0)); // one real move
        assert_eq!(tape.finish().expect("close"), 501, "every frame accounted");

        // Two messages on disk for 501 frames.
        let bytes = std::fs::read(&path).expect("read");
        let messages = mcap::MessageStream::new(&bytes)
            .expect("stream")
            .filter(|m| m.as_ref().is_ok_and(|m| m.channel.topic == CAMERA_TOPIC))
            .count();
        assert_eq!(
            messages, 2,
            "a still camera collapsed to one message, plus the move"
        );

        // And it still replays frame for frame.
        let mut tape = Tape::replaying(&path).expect("open");
        let read: Vec<Frame> = std::iter::from_fn(|| tape.next_frame()).collect();
        assert_eq!(read.len(), 501, "the run expanded back to what was flown");
        assert!(read[..500].iter().all(|f| *f == frame(0.0)));
        assert_eq!(read[500], frame(1.0));
    }

    /// A file that is not a camera path is refused with a named error rather
    /// than replayed as zero frames, which would look like a successful run.
    #[test]
    fn a_file_that_is_not_a_path_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nonsense.mcap");
        std::fs::write(&path, b"this is not mcap").expect("write");
        assert!(Tape::replaying(&path).is_err());
    }
}
