// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Brings the channels of several MCAP files together in one.
//!
//! ```text
//! tuile-tape merge <out.mcap> <in.mcap> <in.mcap>...
//! ```
//!
//! A camera path and what the camera follows are one plan; this writes them
//! as one file. See [`tuile_tape::merge`]. `out` may be one of the inputs:
//! everything is read before anything is written.

/// `args` are the arguments after the subcommand's name.
pub fn run(args: &[String]) -> std::process::ExitCode {
    let [out, inputs @ ..] = args else {
        eprintln!("usage: tuile-tape merge <out.mcap> <in.mcap> <in.mcap>...");
        return std::process::ExitCode::from(2);
    };
    if inputs.len() < 2 {
        eprintln!("usage: tuile-tape merge <out.mcap> <in.mcap> <in.mcap>...");
        return std::process::ExitCode::from(2);
    }
    // Written beside the destination and moved into place, so a destination
    // that is also an input is replaced whole or not at all.
    let part = format!("{out}.part");
    match tuile_tape::merge(inputs, &part).and_then(|counts| {
        std::fs::rename(&part, out)?;
        Ok(counts)
    }) {
        Ok(counts) => {
            for (topic, messages) in counts {
                println!("{topic} {messages}");
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            eprintln!("tuile-tape merge: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
