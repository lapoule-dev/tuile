// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A tape becomes a manifest stage.
//!
//! ```text
//! cargo run -p tuile-usd --bin tape-to-stage -- flight.mcap flight.usda \
//!     [--fps 24] [--viewport 1920x1440] [--terrain 0] [--imagery 0] [--sse 0]
//! ```

use tuile_usd::{write_manifest, ManifestConfig};

fn usage() -> ! {
    eprintln!(
        "usage: tape-to-stage <tape.mcap> <out.usda> \
         [--fps N] [--viewport WxH] [--terrain ID] [--imagery ID] [--sse PX]"
    );
    std::process::exit(2);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut positional = Vec::new();
    let mut config = ManifestConfig::default();

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| -> String {
            it.next().cloned().unwrap_or_else(|| {
                eprintln!("{name} needs a value");
                usage()
            })
        };
        match arg.as_str() {
            "--fps" => config.fps = value("--fps").parse()?,
            "--viewport" => {
                let v = value("--viewport");
                let (w, h) = v.split_once('x').ok_or("--viewport wants WxH")?;
                config.viewport_px = (w.parse()?, h.parse()?);
            }
            "--terrain" => config.terrain_asset_id = value("--terrain").parse()?,
            "--imagery" => config.imagery_asset_id = value("--imagery").parse()?,
            "--sse" => config.max_sse = value("--sse").parse()?,
            _ if arg.starts_with("--") => usage(),
            _ => positional.push(arg.clone()),
        }
    }
    let [tape_path, out_path] = positional.as_slice() else {
        usage()
    };

    let mut tape = tuile_tape::Tape::replaying(tape_path)?;
    let mut frames = Vec::new();
    while let Some(frame) = tape.next_frame() {
        frames.push(frame);
    }

    let mut out = Vec::new();
    write_manifest(&frames, &config, &mut out)?;
    std::fs::write(out_path, &out)?;
    println!(
        "{}: {} frames at {} fps, viewport {}x{}",
        out_path,
        frames.len(),
        config.fps,
        config.viewport_px.0,
        config.viewport_px.1
    );
    Ok(())
}
