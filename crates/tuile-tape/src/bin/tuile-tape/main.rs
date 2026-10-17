// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Makes tapes: one program, one subcommand per kind of tape.
//!
//! ```text
//! tuile-tape border   [path] [minutes]
//! tuile-tape merge    <out.mcap> <in.mcap> <in.mcap>...
//! tuile-tape orbit    [path] [frames] [lon] [lat] [radius-m] [altitude-m]
//! tuile-tape pyrenees [path] [minutes] [fps] [altitude_m] [offset_deg] [tilt_deg]
//! tuile-tape zoom     [path] [frames-each-way]
//! ```
//!
//! Each subcommand takes the arguments that follow its name, and nothing
//! else: there is no flag here, only positions. What a subcommand does with
//! them, and why, is told at the head of its module.

use std::process::{ExitCode, Termination};

mod border;
mod merge;
mod orbit;
mod pyrenees;
mod zoom;

const USAGE: &str = "\
usage: tuile-tape <subcommand> [arguments]

  border   [path] [minutes]
           a flight around the border of mainland France
  merge    <out.mcap> <in.mcap> <in.mcap>...
           the channels of several MCAP files, in one
  orbit    [path] [frames] [lon] [lat] [radius-m] [altitude-m]
           a circle around a point on the ground
  pyrenees [path] [minutes] [fps] [altitude_m] [offset_deg] [tilt_deg]
           a closed loop around the Pyrenees
  zoom     [path] [frames-each-way]
           a dive from geostationary orbit to the ground, and back";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((subcommand, rest)) = args.split_first() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    // A subcommand that returns an error reports it the way a `main` that
    // returns one does: `Error: …` on stderr, and a status of one.
    match subcommand.as_str() {
        "border" => border::run(rest).report(),
        "merge" => merge::run(rest),
        "orbit" => orbit::run(rest).report(),
        "pyrenees" => pyrenees::run(rest).report(),
        "zoom" => zoom::run(rest).report(),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        unknown => {
            eprintln!("tuile-tape: unknown subcommand {unknown:?}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
