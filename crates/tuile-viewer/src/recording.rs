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

/// Opens a camera path from `--record` (`record`), `TUILE_RECORD` or
/// `TUILE_REPLAY`, the first that is set.
///
/// Recording wins if both are: replaying *into* the file being written is the
/// one combination that cannot mean anything. A path that will not open is
/// reported and ignored — a viewer that refuses to start because a debugging
/// aid is missing helps nobody.
///
/// A recording is a session's (`tuile_tape::session`): beside the camera,
/// each frame's clocks, viewport, atmosphere and input.
pub(crate) fn open_tape(record: Option<&std::path::Path>) -> Option<tuile_tape::Tape> {
    let record = record
        .map(|p| p.to_string_lossy().into_owned())
        .or_else(|| std::env::var("TUILE_RECORD").ok());
    if let Some(path) = record {
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

/// How a recording keeps the pictures the viewer drew: how often, how
/// large, and in what.
///
/// The pictures are the evidence. A replay draws the same cameras again,
/// but not the session: not the tiles resident at that moment, not the
/// streaming's timing, not the exposure's history — so a recording keeps
/// what was on the screen, read back from the GPU without stalling the
/// frame (`tuile_wgpu::ReadbackRing`) and encoded on threads of its own.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PictureSettings {
    /// Pictures a second of wall time; 0 keeps every frame drawn
    /// (`--record-fps`).
    pub fps: f64,
    /// The picture's size over the frame's: 1, ½, ⅓, … — a frame is
    /// averaged in blocks of `1 / scale` pixels a side (`--record-scale`).
    pub scale: f64,
    /// JPEG, or PNG to keep every bit (`--record-codec`).
    pub codec: PictureCodec,
}

/// What a recorded picture is encoded as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PictureCodec {
    Jpeg { quality: u8 },
    Png,
}

impl Default for PictureSettings {
    /// Thirty pictures a second at half the frame's size, in JPEG: on a
    /// Retina display a 1440×900 picture of 2880×1800, some 150 kB.
    fn default() -> Self {
        Self {
            fps: 30.0,
            scale: 0.5,
            codec: PictureCodec::Jpeg { quality: 90 },
        }
    }
}

impl PictureSettings {
    /// The side of the block of frame pixels a picture's pixel averages.
    pub(crate) fn step(&self) -> u32 {
        (1.0 / self.scale).round().clamp(1.0, 16.0) as u32
    }

    /// From the text of `--record-fps`, `--record-scale` and
    /// `--record-codec`, each optional.
    pub(crate) fn parse(
        fps: Option<&str>,
        scale: Option<&str>,
        codec: Option<&str>,
    ) -> Result<Self, String> {
        let mut settings = Self::default();
        if let Some(text) = fps {
            settings.fps = text
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && (0.0..=240.0).contains(v))
                .ok_or_else(|| {
                    format!("--record-fps wants pictures a second from 0 to 240, got {text:?}")
                })?;
        }
        if let Some(text) = scale {
            settings.scale = text
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v > 0.0 && *v <= 1.0)
                .ok_or_else(|| {
                    format!("--record-scale wants a size over the frame's, 0 to 1, got {text:?}")
                })?;
        }
        if let Some(text) = codec {
            settings.codec = match text.to_ascii_lowercase().as_str() {
                "jpeg" | "jpg" => PictureCodec::Jpeg { quality: 90 },
                "png" => PictureCodec::Png,
                _ => return Err(format!("--record-codec is jpeg or png, got {text:?}")),
            };
        }
        Ok(settings)
    }
}

/// Where and how a session is recorded: `--record` and its pictures'
/// settings.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct RecordSettings {
    /// The file `--record` named, if it did: recorded from the start.
    pub path: Option<std::path::PathBuf>,
    pub pictures: PictureSettings,
}

/// Whether frames are copied off the surface: asked of the surface when it
/// is configured, so it is set before a recording's first frame.
static CAPTURING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn set_capturing(on: bool) {
    CAPTURING.store(on, std::sync::atomic::Ordering::Release);
}

pub(crate) fn capturing() -> bool {
    CAPTURING.load(std::sync::atomic::Ordering::Acquire)
}

/// Where a recording started from the key or the menu goes: the
/// directory `TUILE_RECORD_DIR` names, else `tuile-recordings` in the home
/// directory, a file named for the instant it began.
pub(crate) fn new_recording_path() -> std::path::PathBuf {
    let dir = std::env::var_os("TUILE_RECORD_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join("tuile-recordings")))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let _ = std::fs::create_dir_all(&dir);
    let stamp = tuile_atmosphere::clock::format_rfc3339(crate::app::wall_seconds()).replace(':', "-");
    dir.join(format!("tuile-session-{stamp}.mcap"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picture_settings_read_their_three_flags() {
        assert_eq!(PictureSettings::parse(None, None, None), Ok(PictureSettings::default()));
        let s = PictureSettings::parse(Some("0"), Some("1"), Some("png")).expect("reads");
        assert_eq!((s.fps, s.step(), s.codec), (0.0, 1, PictureCodec::Png));
        assert_eq!(PictureSettings::parse(None, Some("0.25"), None).map(|s| s.step()), Ok(4));
        for (fps, scale, codec) in [
            (Some("-1"), None, None),
            (Some("x"), None, None),
            (None, Some("0"), None),
            (None, Some("2"), None),
            (None, None, Some("h264")),
        ] {
            assert!(PictureSettings::parse(fps, scale, codec).is_err(), "{fps:?} {scale:?} {codec:?}");
        }
    }
}
