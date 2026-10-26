// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

pub mod diagnose;
pub mod film;
pub mod plots;
mod rows;
pub mod sidecar;

pub use diagnose::{flag, link, Bounds, Flagged, Why};
pub use rows::{measure, Measure, Row};

/// What [`export`] wrote.
#[derive(Debug, Clone, PartialEq)]
pub struct Exported {
    pub rows: Vec<Row>,
    /// Pictures in the film.
    pub filmed: u32,
}

/// Measures every picture of the recording at `session` and, as asked,
/// writes them as a film at `mp4` (`fps` pictures a second — 0 for the
/// rate they were recorded at, [`recorded_fps`] — `codec`, a caption burnt
/// in) and their measures beside each frame's pose, clock and sky as the
/// Parquet sidecar at `parquet`.
pub fn export(
    session: &std::path::Path,
    mp4: Option<&std::path::Path>,
    parquet: Option<&std::path::Path>,
    codec: film::Codec,
    fps: u32,
    burn_in: bool,
) -> Result<Exported, String> {
    let fps = if fps == 0 && mp4.is_some() {
        // The rate the pictures were taken at, from the wall instants of
        // the frames that have one.
        let pictured = tuile_tape::session::pictured_frames(session).map_err(|e| e.to_string())?;
        let frames = tuile_tape::session::read(session).map_err(|e| e.to_string())?;
        let rows: Vec<Row> = pictured
            .iter()
            .filter_map(|i| frames.get(*i as usize))
            .map(|f| Row {
                pictured: true,
                ..Row::of(f)
            })
            .collect();
        recorded_fps(&rows)
    } else {
        fps
    };
    let mut film = match mp4 {
        Some(path) => Some(film::Film::create(
            path,
            codec,
            fps,
            burn_in,
            Some(film_size(session)?),
        )?),
        None => None,
    };
    let rows = measure(session, |row, rgba, size| match film.as_mut() {
        Some(film) => film.push(row, rgba, size),
        None => Ok(()),
    })?;
    let filmed = match film {
        Some(film) => film.finish()?,
        None => 0,
    };
    if let Some(path) = parquet {
        sidecar::write(path, &rows)?;
    }
    Ok(Exported { rows, filmed })
}

/// The size most of the recording's pictures have: the film's. Read from
/// their headers.
pub fn film_size(session: &std::path::Path) -> Result<(u32, u32), String> {
    let mut sizes = std::collections::BTreeMap::<(u32, u32), u64>::new();
    tuile_tape::session::for_each_picture(session, |p| {
        *sizes.entry(p.size()?).or_default() += 1;
        Ok(())
    })
    .map_err(|e| e.to_string())?;
    Ok(sizes
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map_or((1280, 720), |(size, _)| size))
}

/// The pictures a second the recording was made at, read from the wall
/// instants of its pictured frames: what its film plays at, so that a
/// second of film is a second of the session.
pub fn recorded_fps(rows: &[Row]) -> u32 {
    let walls: Vec<f64> = rows
        .iter()
        .filter(|r| r.pictured && r.wall.is_finite())
        .map(|r| r.wall)
        .collect();
    match (walls.first(), walls.last()) {
        (Some(a), Some(b)) if b > a && walls.len() > 1 => {
            ((walls.len() - 1) as f64 / (b - a)).round().clamp(1.0, 240.0) as u32
        }
        _ => 30,
    }
}

/// What [`film_tape`] wrote: the frames, the clock, and how far its
/// instants are from the recorded ones.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FilmTape {
    pub frames: u64,
    pub clock: Option<tuile_tape::Clock>,
    /// The largest gap between the clock's instant for a frame and the
    /// instant the frame was recorded at, scene seconds.
    pub worst_instant: f64,
}

/// The recording at `session` as a film's tape at `out`: the same cameras,
/// frame for frame, and a clock (`/clock`) through the recorded instants —
/// so that the film path draws the poses the viewer drew, lit as it lit
/// them, and the two compare frame by frame.
///
/// A film's clock is linear: frame `n` at `start + n · step`. A held clock
/// is that exactly; one that ran while the viewer's frames came at uneven
/// intervals is fitted through its first and last instants, and
/// [`FilmTape::worst_instant`] says how far that put any frame.
pub fn film_tape(session: &std::path::Path, out: &std::path::Path) -> Result<FilmTape, String> {
    let frames = tuile_tape::session::read(session).map_err(|e| e.to_string())?;
    let mut tape = tuile_tape::Tape::recording(out).map_err(|e| e.to_string())?;
    for frame in &frames {
        tape.push(frame.camera);
    }
    let lit: Vec<(f64, f64)> = frames
        .iter()
        .filter_map(|f| f.context.map(|c| (f.index as f64, c.instant)))
        .collect();
    let clock = match (lit.first(), lit.last()) {
        (Some(&(n0, t0)), Some(&(n1, t1))) => {
            let step = if n1 > n0 { (t1 - t0) / (n1 - n0) } else { 0.0 };
            // Frame n (from 1) shows start + (n - 1) * step; index i is n - 1.
            Some(tuile_tape::Clock {
                start_utc: t0 - n0 * step,
                seconds_per_frame: step,
            })
        }
        _ => None,
    };
    let mut worst = 0.0f64;
    if let Some(clock) = clock {
        tape.set_clock(clock).map_err(|e| e.to_string())?;
        for &(i, t) in &lit {
            worst = worst.max((clock.instant(i as u32 + 1) - t).abs());
        }
    }
    let frames = tape.finish().map_err(|e| e.to_string())?;
    Ok(FilmTape {
        frames,
        clock,
        worst_instant: worst,
    })
}
