// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Opening, salvaging and closing the camera recordings.
//!
//! Two different jobs share this file because they share a file format. A
//! **tape** reproduces a session — the same gestures, flown again exactly — and
//! a **trace** explains one, because "there was a black square" is not something
//! a camera path can record and needs the picture beside it.

/// Where the trace goes, from `TUILE_TRACE`.
pub(crate) fn trace_path() -> Option<String> {
    std::env::var("TUILE_TRACE").ok().filter(|p| !p.is_empty())
}

/// Opens the trace, which records the camera and the frame together.
pub(crate) fn open_trace() -> Option<tuile_tape::Tape> {
    let path = trace_path()?;
    match tuile_tape::Tape::recording(&path) {
        Ok(tape) => {
            tracing::info!(path, "tracing the camera and every frame it draws");
            Some(tape)
        }
        Err(e) => {
            tracing::error!(path, "cannot open the trace: {e}");
            None
        }
    }
}

/// Opens a camera path from `TUILE_RECORD` or `TUILE_REPLAY`, whichever is set.
///
/// Recording wins if both are: replaying *into* the file being written is the
/// one combination that cannot mean anything. A path that will not open is
/// reported and ignored — a viewer that refuses to start because a debugging
/// aid is missing helps nobody.
pub(crate) fn open_tape() -> Option<tuile_tape::Tape> {
    if let Ok(path) = std::env::var("TUILE_RECORD") {
        salvage_any_abandoned_journal(&path);
        return match tuile_tape::Tape::recording(&path) {
            Ok(tape) => {
                tracing::info!(path, "recording the camera path");
                Some(tape)
            }
            Err(e) => {
                tracing::error!(path, "cannot record the camera path: {e}");
                None
            }
        };
    }
    if let Ok(path) = std::env::var("TUILE_REPLAY") {
        return match tuile_tape::Tape::replaying(&path) {
            Ok(tape) => {
                let (_, frames) = tape.progress();
                tracing::info!(path, frames, "replaying a camera path");
                Some(tape)
            }
            Err(e) => {
                tracing::error!(path, "cannot replay the camera path: {e}");
                None
            }
        };
    }
    None
}

/// Turns a journal left behind by a session that died into the MCAP file it
/// was going to become, before this one overwrites it.
///
/// Recovering on the way *in* rather than offering a command to do it means a
/// crashed flight is never silently thrown away by the next run — which is the
/// only moment anyone would notice it had been.
pub(crate) fn salvage_any_abandoned_journal(path: &str) {
    let journal = format!("{path}{}", tuile_tape::JOURNAL_SUFFIX);
    if !std::path::Path::new(&journal).exists() {
        return;
    }
    let rescued = format!("{path}.recovered");
    match tuile_tape::Tape::recover(&journal, &rescued) {
        Ok(0) => {
            let _ = std::fs::remove_file(&journal);
        }
        Ok(frames) => {
            let _ = std::fs::remove_file(&journal);
            tracing::warn!(
                frames,
                path = rescued,
                "a previous session ended without closing its recording; \
                 what reached the disk was salvaged here"
            );
        }
        Err(e) => tracing::error!(journal, "cannot salvage the previous recording: {e}"),
    }
}
