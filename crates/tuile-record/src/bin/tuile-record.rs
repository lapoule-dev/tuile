// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A recorded session, read: see the crate's documentation.
//!
//! ```text
//! tuile-record export   <session.mcap> [--mp4 F] [--parquet F] [--plots F.png]
//!                       [--codec h264|av1] [--fps N] [--burn-in]
//! tuile-record diagnose <session.parquet | session.mcap> [--luma-step X]
//!                       [--star-drop X] [--min-stars N] [--min-black N]
//!                       [--max-turn DEG] [--max-climb X] [--max-scene-step S]
//!                       [--plots F.png]
//! tuile-record goto     <session.parquet | session.mcap> <frame> [--open]
//! tuile-record film-tape <session.mcap> <film.mcap>
//! tuile-record picture  <session.mcap> <frame> <out.png>
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tuile_record::{diagnose, film, plots, sidecar, Bounds, Row};

const USAGE: &str = "\
tuile-record export   <session.mcap> [--mp4 F] [--parquet F] [--plots F.png]
                      [--codec h264|av1] [--fps N] [--burn-in]
tuile-record diagnose <session.parquet | session.mcap> [--luma-step X]
                      [--star-drop X] [--min-stars N] [--min-black N]
                      [--max-turn DEG] [--max-climb X] [--max-scene-step S]
                      [--plots F.png]
tuile-record goto     <session.parquet | session.mcap> <frame> [--open]
tuile-record film-tape <session.mcap> <film.mcap>
tuile-record picture  <session.mcap> <frame> <out.png>";

struct Args {
    positional: Vec<String>,
    flags: BTreeMap<String, Option<String>>,
}

fn args() -> Result<Args, String> {
    let mut positional = Vec::new();
    let mut flags = BTreeMap::new();
    let mut it = std::env::args().skip(1).peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--burn-in" | "--open" => {
                flags.insert(arg, None);
            }
            a if a.starts_with("--") => {
                let value = it.next().ok_or_else(|| format!("{a} needs a value"))?;
                flags.insert(arg, Some(value));
            }
            _ => positional.push(arg),
        }
    }
    Ok(Args { positional, flags })
}

impl Args {
    fn text(&self, flag: &str) -> Option<&str> {
        self.flags.get(flag).and_then(|v| v.as_deref())
    }
    fn number(&self, flag: &str, or: f64) -> Result<f64, String> {
        self.text(flag).map_or(Ok(or), |v| {
            v.parse()
                .map_err(|_| format!("{flag} wants a number, got {v:?}"))
        })
    }
    fn path(&self, flag: &str) -> Option<PathBuf> {
        self.text(flag).map(PathBuf::from)
    }
}

/// The rows of a sidecar, or of a session measured now.
fn rows_of(path: &Path) -> Result<Vec<Row>, String> {
    if path.extension().is_some_and(|e| e == "parquet") {
        sidecar::read(path)
    } else {
        tuile_record::measure(path, |_, _, _| Ok(()))
    }
}

fn write_plots(path: &Path, rows: &[Row], flagged: &[usize]) -> Result<(), String> {
    let (rgba, (w, h)) = plots::grid(rows, flagged);
    image::save_buffer(path, &rgba, w, h, image::ExtendedColorType::Rgba8)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    println!("plots: {}", path.display());
    Ok(())
}

fn bounds(args: &Args) -> Result<Bounds, String> {
    let d = Bounds::default();
    Ok(Bounds {
        luma_step: args.number("--luma-step", d.luma_step)?,
        star_drop: args.number("--star-drop", d.star_drop)?,
        min_stars: args.number("--min-stars", d.min_stars as f64)? as u64,
        min_black: args.number("--min-black", d.min_black as f64)? as u64,
        max_turn_deg: args.number("--max-turn", d.max_turn_deg)?,
        max_climb: args.number("--max-climb", d.max_climb)?,
        max_scene_step: args.number("--max-scene-step", d.max_scene_step)?,
        ..d
    })
}

fn report(rows: &[Row], bounds: &Bounds) -> Vec<usize> {
    let flagged = tuile_record::flag(rows, bounds);
    let pictured = rows.iter().filter(|r| r.pictured).count();
    println!(
        "{} frames, {pictured} pictured, {} flagged (camera still: turn <= {} deg, climb <= {:.0} %, gap <= {} frames, clock <= {} s; \
         luma step > {}, stars dropping by > {:.0} % of >= {}; >= {} black ground pixels)",
        rows.len(),
        flagged.len(),
        bounds.max_turn_deg,
        bounds.max_climb * 100.0,
        bounds.max_gap,
        bounds.max_scene_step,
        bounds.luma_step,
        bounds.star_drop * 100.0,
        bounds.min_stars,
        bounds.min_black
    );
    for f in &flagged {
        println!("{}", diagnose::line(&rows[f.row], &f.why));
    }
    flagged.iter().map(|f| f.row).collect()
}

fn run() -> Result<(), String> {
    let args = args()?;
    let (command, rest) = args
        .positional
        .split_first()
        .ok_or_else(|| USAGE.to_owned())?;
    match command.as_str() {
        "export" => {
            let session = PathBuf::from(rest.first().ok_or(USAGE)?);
            let codec = match args.text("--codec") {
                Some(c) => film::Codec::parse(c)?,
                None => film::Codec::native(),
            };
            let mp4 = args.path("--mp4");
            let parquet = args.path("--parquet");
            let exported = tuile_record::export(
                &session,
                mp4.as_deref(),
                parquet.as_deref(),
                codec,
                args.number("--fps", 0.0)? as u32,
                args.flags.contains_key("--burn-in"),
            )?;
            println!(
                "{} frames, {} pictures filmed",
                exported.rows.len(),
                exported.filmed
            );
            if let Some(p) = mp4 {
                println!("film: {}", p.display());
            }
            if let Some(p) = parquet {
                println!("sidecar: {}", p.display());
            }
            if let Some(p) = args.path("--plots") {
                let flagged: Vec<usize> = tuile_record::flag(&exported.rows, &Bounds::default())
                    .iter()
                    .map(|f| f.row)
                    .collect();
                write_plots(&p, &exported.rows, &flagged)?;
            }
        }
        "diagnose" => {
            let rows = rows_of(Path::new(rest.first().ok_or(USAGE)?))?;
            let flagged = report(&rows, &bounds(&args)?);
            if let Some(p) = args.path("--plots") {
                write_plots(&p, &rows, &flagged)?;
            }
        }
        "goto" => {
            let rows = rows_of(Path::new(rest.first().ok_or(USAGE)?))?;
            let frame: u64 = rest
                .get(1)
                .ok_or(USAGE)?
                .parse()
                .map_err(|_| "a frame is a number".to_owned())?;
            let row = rows
                .iter()
                .find(|r| r.frame == frame)
                .ok_or_else(|| format!("there is no frame {frame}"))?;
            let link = tuile_record::link(row);
            println!("{link}");
            if args.flags.contains_key("--open") {
                // The system hands the link to the viewer that registered
                // the scheme.
                let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
                std::process::Command::new(opener)
                    .arg(&link)
                    .status()
                    .map_err(|e| format!("{opener}: {e}"))?;
            }
        }
        "picture" => {
            let (session, frame, out) = match rest {
                [s, f, o, ..] => (Path::new(s), f, Path::new(o)),
                _ => return Err(USAGE.to_owned()),
            };
            let frame: u64 = frame.parse().map_err(|_| "a frame is a number".to_owned())?;
            let mut found = None;
            tuile_tape::session::for_each_picture(session, |p| {
                if p.index == frame {
                    found = Some(p.rgba()?);
                }
                Ok(())
            })
            .map_err(|e| e.to_string())?;
            let (rgba, (w, h)) =
                found.ok_or_else(|| format!("frame {frame} has no recorded picture"))?;
            image::save_buffer(out, &rgba, w, h, image::ExtendedColorType::Rgba8)
                .map_err(|e| format!("{}: {e}", out.display()))?;
            println!("{}: frame {frame}, {w}x{h}", out.display());
        }
        "film-tape" => {
            let (from, to) = match rest {
                [from, to, ..] => (Path::new(from), Path::new(to)),
                _ => return Err(USAGE.to_owned()),
            };
            let made = tuile_record::film_tape(from, to)?;
            println!(
                "{} frames; clock {}; worst instant off by {:.3} s",
                made.frames,
                made.clock.map_or("none".to_owned(), |c| c.to_arg()),
                made.worst_instant
            );
            println!(
                "the film path: tuile-film-produce {} --resolution <recorded viewport>",
                to.display()
            );
        }
        _ => return Err(USAGE.to_owned()),
    }
    Ok(())
}

fn main() {
    if let Err(why) = run() {
        eprintln!("{why}");
        std::process::exit(2);
    }
}
