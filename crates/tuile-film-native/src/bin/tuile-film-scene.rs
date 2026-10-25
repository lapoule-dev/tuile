// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Writes the tape of one of the discs' acceptance scenes
//! ([`tuile_atmosphere::scenes`]): the camera held at the scene's place and
//! aim, one frame an instant, the instants in the tape's clock.
//!
//! ```text
//! tuile-film-scene list
//! tuile-film-scene pic-du-midi-sunset sunset.mcap
//! ```
//!
//! The tape then goes through the film's stages like any other — baked by
//! `tuile-film-produce`, rendered by `tuile-film-render --atmosphere` — and
//! the viewer shows the same scene (`tuile-wgpu-viewer`'s `discs` example).

use std::process::ExitCode;

use tuile_atmosphere::clock::format_rfc3339;
use tuile_atmosphere::scenes::{disc_scene, disc_scenes, DiscScene};
use tuile_tape::timelapse::{CameraFrame, Motion, TimeLapse};

fn lapse(scene: &DiscScene) -> TimeLapse {
    let frames = scene.instants.max(1);
    // The seconds of sky between two frames, and the film's whole span.
    let every = if frames > 1 {
        scene.span_s / f64::from(frames - 1)
    } else {
        0.0
    };
    TimeLapse {
        lon_deg: scene.lon_deg,
        lat_deg: scene.lat_deg,
        height_m: scene.height_m,
        start_utc: scene.start_utc,
        duration_s: every * f64::from(frames),
        length_s: f64::from(frames),
        fps: 1,
        frame: CameraFrame::Earth,
        motion: Motion::Held,
        heading_deg: scene.heading_deg,
        pitch_deg: scene.pitch_deg,
        pitch_swing_deg: 0.0,
        pitch_period_s: 0.0,
        fovy_deg: scene.fovy_deg,
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [list] if list == "list" => {
            for s in disc_scenes() {
                println!(
                    "{:20} {} {} instant(s) over {:.0} s, ({:.4}, {:.4}) at {} m, heading {:.2}, pitch {:.2}, fov {}",
                    s.name,
                    format_rfc3339(s.start_utc),
                    s.instants,
                    s.span_s,
                    s.lon_deg,
                    s.lat_deg,
                    s.height_m,
                    s.heading_deg,
                    s.pitch_deg,
                    s.fovy_deg
                );
            }
            ExitCode::SUCCESS
        }
        [name, path] => {
            let Some(scene) = disc_scene(name) else {
                eprintln!("tuile-film-scene: no scene {name:?} (see `tuile-film-scene list`)");
                return ExitCode::from(2);
            };
            let lapse = lapse(&scene);
            match tuile_film_native::timelapse::write_tape(&lapse, std::path::Path::new(path)) {
                Ok(frames) => {
                    let clock = lapse.clock();
                    println!(
                        "{path}: {frames} frame(s) from {}, {:.0} s of sky a frame — clock {},{}",
                        format_rfc3339(scene.start_utc),
                        clock.seconds_per_frame,
                        clock.start_utc,
                        clock.seconds_per_frame
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("tuile-film-scene: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!("usage: tuile-film-scene list | tuile-film-scene <scene> <path.mcap>");
            ExitCode::from(2)
        }
    }
}
