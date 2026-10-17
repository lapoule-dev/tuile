// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Writes the tape of a time-lapse of the sky ([`tuile_tape::timelapse`]),
//! and beside it what a renderer needs to light it: the clock.
//!
//! ```text
//! tuile-tape timelapse sky.mcap place=0.14,42.94 height=4000 \
//!     start=2026-10-19T00:00Z days=14 length=120 fps=60 \
//!     spin=10 pitch=8 swing=12,40 fov=70
//! ```
//!
//! Prints, and writes to `<tape>.clock`, the `--clock` a render of it is
//! given: the first frame's instant and the seconds of sky a frame.

use tuile_tape::timelapse::TimeLapse;
use tuile_tape::Tape;

/// `args` are the arguments after the subcommand's name.
pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let (path, words) = args
        .split_first()
        .ok_or("timelapse <path.mcap> [key=value ...]")?;
    let lapse = TimeLapse::parse(words)?;
    let mut tape = Tape::recording(path)?;
    for camera in lapse.cameras() {
        tape.push(camera);
    }
    let written = tape.finish()?;
    let clock = lapse.clock().to_arg();
    std::fs::write(format!("{path}.clock"), format!("{clock}\n"))?;
    println!(
        "{path}: {written} frames, {:.0} s of sky a frame, from ({:.4}, {:.4}) at {} m",
        lapse.clock().seconds_per_frame,
        lapse.lon_deg,
        lapse.lat_deg,
        lapse.height_m
    );
    println!("--clock {clock}");
    Ok(())
}
