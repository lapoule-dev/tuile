// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the session has done, counted, for the line it prints on the way out.
//!
//! Rates matter more than levels: content that keeps *arriving* long after the
//! view settled means tiles are being evicted and reloaded, which no snapshot of
//! memory would reveal.

/// What the session has done since it started.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Stats {
    pub(crate) frames: u64,
    /// Tiles uploaded to the GPU. Compare against `distinct` below: the gap is
    /// wasted work.
    pub(crate) uploads: u64,
    /// Tiles the server told us to drop.
    pub(crate) evictions: u64,
    /// Frames that had to draw a coarser ancestor because the selected tile
    /// was not ready — visible as softness, or as a hole when even the
    /// ancestor is gone.
    pub(crate) frames_with_gaps: u64,
    /// The worst number of selected tiles drawn by *nothing* in any one frame.
    /// Zero is the only acceptable value; see [`tuile_wgpu::Resolution`].
    pub(crate) holes: u64,
    /// Errors the server reported.
    pub(crate) errors: u64,
    pub(crate) started: Option<std::time::Instant>,
}

impl std::fmt::Display for Stats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let secs = self
            .started
            .map_or(0.0, |t| t.elapsed().as_secs_f64())
            .max(1e-3);
        write!(
            f,
            "session over: {} frames in {secs:.0}s ({:.0} fps) | {} uploads, {} evictions, \
             {} frames with gaps, worst hole {} tiles, {} errors",
            self.frames,
            self.frames as f64 / secs,
            self.uploads,
            self.evictions,
            self.frames_with_gaps,
            self.holes,
            self.errors,
        )
    }
}
