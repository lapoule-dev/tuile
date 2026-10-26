// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The pictures of a recording: the frames the window drew, read back
//! without waiting for them and encoded off the render thread.
//!
//! A frame due a picture (`--record-fps`) is copied, on its own encoder,
//! into a slot of a [`tuile_wgpu::ReadbackRing`] tagged with the frame's
//! index on the tape; the slot is mapped once the GPU is done with it, a
//! later turn hands it to a worker, and the worker averages it down
//! (`--record-scale`), encodes it (`--record-codec`) and hands it back for
//! the tape, on the frame it shows. The render thread copies and hands
//! over; it never waits. When every slot is still in flight the frame goes
//! without a picture, and is counted.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::recording::{PictureCodec, PictureSettings};

/// Frames that may be in flight between the copy and the encoder.
const SLOTS: usize = 6;
/// Threads encoding.
const WORKERS: usize = 3;

struct Job {
    frame: tuile_wgpu::MappedFrame,
    step: u32,
    codec: PictureCodec,
}

struct Encoded {
    index: u64,
    format: &'static str,
    data: Vec<u8>,
    seconds: f64,
}

/// A recording's pictures, running.
pub(crate) struct Pictures {
    pub(super) ring: tuile_wgpu::ReadbackRing,
    settings: PictureSettings,
    /// The wall instant of the last picture taken.
    last: Option<f64>,
    jobs: Option<Sender<Job>>,
    done: Receiver<Encoded>,
    workers: Vec<JoinHandle<()>>,
    /// Pictures taken, and written to the tape.
    taken: u64,
    written: u64,
    bytes: u64,
    /// Seconds spent encoding, on the workers.
    encoding: f64,
    /// Seconds the render thread spent on the pictures: the copy's
    /// recording, the hand-over and the writing — what recording costs a
    /// frame.
    pub(super) on_the_frame: f64,
}

impl Pictures {
    pub(crate) fn new(settings: PictureSettings) -> Self {
        let (jobs, queue) = channel::<Job>();
        let queue = Arc::new(Mutex::new(queue));
        let (finished, done) = channel();
        let workers = (0..WORKERS)
            .map(|n| {
                let queue = Arc::clone(&queue);
                let finished = finished.clone();
                std::thread::Builder::new()
                    .name(format!("tuile-record-{n}"))
                    .spawn(move || work(&queue, &finished))
            })
            .filter_map(Result::ok)
            .collect();
        Self {
            ring: tuile_wgpu::ReadbackRing::new(SLOTS),
            settings,
            last: None,
            jobs: Some(jobs),
            done,
            workers,
            taken: 0,
            written: 0,
            bytes: 0,
            encoding: 0.0,
            on_the_frame: 0.0,
        }
    }

    /// Whether the frame drawn at `wall` is due a picture; if it is, it is
    /// counted as taken.
    pub(crate) fn due(&mut self, wall: f64) -> bool {
        let due = self.settings.fps <= 0.0
            || self
                .last
                .is_none_or(|last| wall - last >= 1.0 / self.settings.fps - 1e-4);
        if due {
            self.last = Some(wall);
            self.taken += 1;
        }
        due
    }

    /// Hands what the GPU has finished to the workers, and writes what
    /// they have finished to `tape`. With `wait`, waits for every frame in
    /// flight and every encoder: at the end of the recording.
    pub(crate) fn pump(
        &mut self,
        gpu: &tuile_wgpu::GpuContext,
        tape: &mut tuile_tape::Tape,
        wait: bool,
    ) {
        let began = web_time::Instant::now();
        if let Some(jobs) = self.jobs.as_ref() {
            for frame in self.ring.ready(gpu, wait) {
                let job = Job {
                    frame,
                    step: self.settings.step(),
                    codec: self.settings.codec,
                };
                if jobs.send(job).is_err() {
                    tracing::error!("the recording's encoders have stopped");
                }
            }
        }
        if wait {
            // No more pictures: the workers finish theirs and leave.
            self.jobs = None;
            for worker in self.workers.drain(..) {
                let _ = worker.join();
            }
        }
        for encoded in self.done.try_iter() {
            tape.push_picture(encoded.index, encoded.format, &encoded.data);
            self.written += 1;
            self.bytes += encoded.data.len() as u64;
            self.encoding += encoded.seconds;
        }
        if !wait {
            self.on_the_frame += began.elapsed().as_secs_f64();
        }
    }

    /// What the recording of pictures came to, for the log.
    pub(crate) fn report(&self, frames: u64, seconds: f64) {
        let minutes = (seconds / 60.0).max(1e-9);
        tracing::info!(
            taken = self.taken,
            written = self.written,
            dropped = self.ring.dropped(),
            megabytes = self.bytes as f64 / 1e6,
            megabytes_a_minute = self.bytes as f64 / 1e6 / minutes,
            encode_ms_a_picture = 1000.0 * self.encoding / self.written.max(1) as f64,
            frame_ms_a_frame = 1000.0 * self.on_the_frame / frames.max(1) as f64,
            "recorded pictures"
        );
    }
}

fn work(queue: &Mutex<Receiver<Job>>, finished: &Sender<Encoded>) {
    loop {
        let job = {
            let Ok(queue) = queue.lock() else {
                return;
            };
            match queue.recv() {
                Ok(job) => job,
                Err(_) => return,
            }
        };
        let began = std::time::Instant::now();
        let index = job.frame.tag;
        let (width, height) = job.frame.size;
        let step = job.step;
        let Some(rgba) = job.frame.read(step) else {
            tracing::warn!(index, "a recorded frame could not be read back");
            continue;
        };
        let (w, h) = (width / step, height / step);
        let encoded = encode(&rgba, (w, h), job.codec);
        match encoded {
            Ok((format, data)) => {
                let _ = finished.send(Encoded {
                    index,
                    format,
                    data,
                    seconds: began.elapsed().as_secs_f64(),
                });
            }
            Err(e) => tracing::warn!(index, "a recorded frame could not be encoded: {e}"),
        }
    }
}

/// `rgba` as `codec` says: its format's name and its bytes.
pub(crate) fn encode(
    rgba: &[u8],
    (w, h): (u32, u32),
    codec: PictureCodec,
) -> Result<(&'static str, Vec<u8>), String> {
    let mut out = Vec::new();
    match codec {
        PictureCodec::Png => {
            let encoder = image::codecs::png::PngEncoder::new_with_quality(
                &mut out,
                image::codecs::png::CompressionType::Fast,
                image::codecs::png::FilterType::Sub,
            );
            image::ImageEncoder::write_image(encoder, rgba, w, h, image::ExtendedColorType::Rgba8)
                .map_err(|e| e.to_string())?;
            Ok(("png", out))
        }
        PictureCodec::Jpeg { quality } => {
            // No alpha in a JPEG: the frame is opaque anyway.
            let rgb: Vec<u8> = rgba
                .chunks_exact(4)
                .flat_map(|p| [p[0], p[1], p[2]])
                .collect();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)
                .encode(&rgb, w, h, image::ExtendedColorType::Rgb8)
                .map_err(|e| e.to_string())?;
            Ok(("jpeg", out))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A smooth sky over ground, with a star: what a recorded frame is.
    fn frame((w, h): (u32, u32)) -> Vec<u8> {
        (0..w * h)
            .flat_map(|p| {
                let (x, y) = (p % w, p / w);
                if y < h / 2 {
                    let v = (20 + y * 2) as u8;
                    [v / 2, v, v.saturating_add(40), 255]
                } else {
                    [(90 + x % 7) as u8, (100 + y % 5) as u8, 80, 255]
                }
            })
            .collect()
    }

    fn decoded(data: &[u8]) -> Vec<u8> {
        image::load_from_memory(data)
            .expect("decodes")
            .into_rgba8()
            .into_raw()
    }

    /// PNG gives the frame back bit for bit.
    #[test]
    fn a_png_picture_decodes_to_the_frame() {
        let size = (96, 64);
        let rgba = frame(size);
        let (format, data) = encode(&rgba, size, PictureCodec::Png).expect("encodes");
        assert_eq!(format, "png");
        assert_eq!(decoded(&data), rgba);
    }

    /// JPEG gives it back within the codec's tolerance: on average under
    /// two levels of 255 a channel at the default quality.
    #[test]
    fn a_jpeg_picture_decodes_within_the_codecs_tolerance() {
        let size = (96, 64);
        let rgba = frame(size);
        let (format, data) =
            encode(&rgba, size, PictureCodec::Jpeg { quality: 90 }).expect("encodes");
        assert_eq!(format, "jpeg");
        let back = decoded(&data);
        assert_eq!(back.len(), rgba.len());
        let (sum, n) = back
            .iter()
            .zip(&rgba)
            .enumerate()
            .filter(|(i, _)| i % 4 != 3)
            .fold((0u64, 0u64), |(s, n), (_, (a, b))| (s + u64::from(a.abs_diff(*b)), n + 1));
        let mean = sum as f64 / n as f64;
        assert!(mean < 2.0, "mean error {mean:.2} of 255");
    }

    /// A picture is due once a period of `--record-fps`, every frame at 0.
    #[test]
    fn pictures_are_taken_at_their_rate() {
        let mut every = Pictures::new(PictureSettings {
            fps: 0.0,
            ..PictureSettings::default()
        });
        assert!((0..5).all(|n| every.due(f64::from(n) / 60.0)));
        let mut thirty = Pictures::new(PictureSettings::default());
        let taken = (0..60).filter(|n| thirty.due(f64::from(*n) / 60.0)).count();
        assert_eq!(taken, 30);
    }
}
