// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Writes the tape of a time-lapse of the sky
//! ([`tuile_tape::timelapse::TimeLapse`]), its frames' instants in it.
//!
//! ```text
//! tuile-film-timelapse sky.mcap place=0.14,42.94 height=4000 \
//!     start=2026-10-19T00:00Z days=14 length=120 fps=60 \
//!     track=sun-moon pitch=-4 fov=70
//! ```
//!
//! The tape then goes through the film's stages like any other: baked by
//! `tuile-film-produce`, which reads the instants from it.

use std::process::ExitCode;

use tuile_tape::timelapse::TimeLapse;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((path, words)) = args.split_first() else {
        eprintln!("usage: tuile-film-timelapse <path.mcap> [key=value ...]");
        return ExitCode::from(2);
    };
    let lapse = match TimeLapse::parse(words) {
        Ok(lapse) => lapse,
        Err(e) => {
            eprintln!("tuile-film-timelapse: {e}");
            return ExitCode::from(2);
        }
    };
    match tuile_film_native::timelapse::write_tape(&lapse, std::path::Path::new(path)) {
        Ok(frames) => {
            println!(
                "{path}: {frames} frames, {:.0} s of sky a frame, from ({:.4}, {:.4}) at {} m",
                lapse.clock().seconds_per_frame,
                lapse.lon_deg,
                lapse.lat_deg,
                lapse.height_m
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("tuile-film-timelapse: {e}");
            ExitCode::FAILURE
        }
    }
}
