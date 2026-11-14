// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A render's progress, told as it goes ([`tuile_farm::progress`]): each
//! frame drawn by the observer the render is handed ([`Drawn`]), each
//! frame the encoder takes by a sink around the film's ([`Taken`]), and the
//! encoder's last frames written out — the sink closed — as the step
//! [`Step::Encode`].
//!
//! What is counted is what has happened, never a clock: a card that stops
//! drawing, or an encoder that stops taking frames (the render then waits
//! for its room), leaves the counts where they are.

pub use tuile_farm::progress::{Progress, Report, Step};

use crate::observe::{FrameOut, Observer};
use crate::sink::{Eyes, Sink};
use crate::Error;

/// Tells `progress` of every frame the render draws.
pub struct Drawn(pub Progress);

impl Observer for Drawn {
    fn frame(&mut self, _frame: &FrameOut<'_>) {
        self.0.frame();
    }
}

/// A film's sink, each frame it takes told to `progress`, and its close —
/// the encoder's last frames written out — told as [`Step::Encode`].
pub struct Taken<S> {
    pub sink: S,
    pub progress: Progress,
}

impl<S: Sink + ?Sized> Sink for Taken<Box<S>> {
    fn open(&mut self, width: u32, height: u32, fps: u32) -> Result<(), Error> {
        self.sink.open(width, height, fps)
    }
    fn wants_i420(&self) -> bool {
        self.sink.wants_i420()
    }
    fn wants_rgba(&self) -> bool {
        self.sink.wants_rgba()
    }
    fn picture(&mut self, index: u32, frame: u32, rgba: &[u8], i420: &[u8]) -> Result<(), Error> {
        self.sink.picture(index, frame, rgba, i420)?;
        self.progress.encoded(1);
        Ok(())
    }
    fn open_stereo(&mut self, picture: (u32, u32), eye: (u32, u32), fps: u32) -> Result<(), Error> {
        self.sink.open_stereo(picture, eye, fps)
    }
    fn stereo(
        &mut self,
        index: u32,
        frame: u32,
        rgba: &[u8],
        i420: &[u8],
        eyes: &Eyes<'_>,
    ) -> Result<(), Error> {
        self.sink.stereo(index, frame, rgba, i420, eyes)?;
        self.progress.encoded(1);
        Ok(())
    }
    fn close(&mut self) -> Result<(), Error> {
        self.progress.step(Step::Encode);
        self.sink.close()
    }
    fn spent(&self) -> Vec<(String, f64)> {
        self.sink.spent()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink that keeps what it is told, and refuses a picture after
    /// `refuse_after`.
    #[derive(Default)]
    struct Kept {
        pictures: u32,
        closed: bool,
        refuse_after: Option<u32>,
    }

    impl Sink for Kept {
        fn wants_rgba(&self) -> bool {
            false
        }
        fn picture(&mut self, _: u32, _: u32, _: &[u8], _: &[u8]) -> Result<(), Error> {
            if self.refuse_after == Some(self.pictures) {
                return Err("the encoder stopped".into());
            }
            self.pictures += 1;
            Ok(())
        }
        fn close(&mut self) -> Result<(), Error> {
            self.closed = true;
            Ok(())
        }
        fn spent(&self) -> Vec<(String, f64)> {
            vec![("encode".into(), 1.5)]
        }
    }

    #[test]
    fn every_frame_drawn_is_told() {
        let progress = Progress::new();
        let mut drawn = Drawn(progress.clone());
        let camera = tuile_film::FrameCamera {
            eye: glam::DVec3::ZERO,
            view: glam::Mat4::IDENTITY,
            projection: glam::Mat4::IDENTITY,
            near: 1.0,
        };
        for index in 0..4 {
            drawn.frame(&FrameOut {
                frame: index + 1,
                index,
                width: 2,
                height: 2,
                rgba: None,
                tiles: 0,
                entered: 0,
                layers: &[],
                selection: &[],
                camera: &camera,
                timings: Default::default(),
                linear: None,
                sky: None,
            });
        }
        assert_eq!(progress.report().frames, 4);
    }

    /// Every frame the encoder takes is told, and its close is the encode
    /// step; a frame it refuses is not.
    #[test]
    fn the_frames_the_encoder_takes_are_told_and_its_close_is_the_encode() {
        let progress = Progress::new();
        progress.step(Step::Render);
        let mut taken = Taken {
            sink: Box::new(Kept {
                refuse_after: Some(3),
                ..Kept::default()
            }) as Box<dyn Sink>,
            progress: progress.clone(),
        };
        assert!(!taken.wants_rgba(), "what the sink wants, as it says");
        for i in 0..3 {
            taken.picture(i, i + 1, &[], &[]).expect("taken");
        }
        assert_eq!(progress.report().encoded, 3);
        taken.picture(3, 4, &[], &[]).expect_err("refused");
        assert_eq!(progress.report().encoded, 3, "a refused frame is not told");
        assert_eq!(progress.report().step, Step::Render);
        taken.close().expect("closed");
        assert_eq!(progress.report().step, Step::Encode);
        assert_eq!(taken.spent(), vec![("encode".to_string(), 1.5)]);
    }
}
