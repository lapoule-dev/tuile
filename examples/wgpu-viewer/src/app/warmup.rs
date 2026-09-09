// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Holding the window shut until there is a floor under the globe.
//!
//! A globe drawn before its coarse pyramid exists is a globe with nothing behind
//! its holes — the one state the whole design prevents, on screen at the moment
//! someone forms a first impression.

use super::App;

/// The state of the wait for a floor under the globe.
pub(super) struct Warmup {
    /// Whether the window is still being held shut. Frames run while it is —
    /// they are what uploads the pyramid — but nothing is shown.
    pub(super) holding: bool,
    /// The level held in memory for the session, so the wait knows what
    /// "coarse" means.
    pub(super) pinned_level: Option<u32>,
    /// When the wait began, so it can be given up on.
    pub(super) since: std::time::Instant,
    /// When it last said where it had got to. A silent minute with no window
    /// looks exactly like a hang.
    pub(super) last_report: std::time::Instant,
}

impl App {
    /// Drains the stream and uploads, with no window on screen.
    ///
    /// The pump is normally driven from `render`, which is exactly what cannot
    /// run here — so this is the same call without the frame. Its budget is
    /// larger than a rendered frame's for the same reason: nothing is competing
    /// for the GPU, and the sooner this finishes the sooner the window appears.
    pub(super) fn pump_while_hidden(&mut self) {
        const WHILE_NOBODY_IS_WATCHING: usize = 64;
        let Some(active) = self.active.as_mut() else {
            return;
        };
        active
            .pump
            .pump(&mut active.stream, &active.gpu, WHILE_NOBODY_IS_WATCHING);
        if self.still_warming() {
            return;
        }
        self.warmup.holding = false;
        self.paint_before_showing();
        if let Some(active) = self.active.as_ref() {
            active.window.set_visible(true);
            active.window.request_redraw();
        }
    }

    /// Draws one frame into the surface while the window is still hidden.
    ///
    /// Without this the window was **white and empty** for as long as its first
    /// frame took. `set_visible(true)` puts it on screen at once; the redraw
    /// that fills it only arrives on the next turn of the event loop, and the
    /// first frame after a warmup is the most expensive one the session will
    /// ever draw — every pipeline compiled on demand, the whole coarse pyramid
    /// drawn for the first time. Until it lands the window shows the platform's
    /// own blank fill, which is indistinguishable from an application that has
    /// hung, and is exactly what the hold was supposed to prevent. Holding the
    /// window shut and then showing an empty one gives away everything the hold
    /// was for.
    ///
    /// The occlusion guard is lifted for this one frame: a window that has never
    /// been shown reports itself occluded on macOS, and that is the state this
    /// is *for*. One frame is not the repeated drawing the guard exists to stop.
    fn paint_before_showing(&mut self) {
        let occluded = std::mem::replace(&mut self.occluded, false);
        self.render();
        self.occluded = occluded;
    }

    /// Whether to keep the window hidden.
    ///
    /// Ready means every primed tile has been **answered**, and every answer
    /// that was a tile is on the GPU. Both halves come from the session's own
    /// priming report rather than from a process-wide gauge: only the server
    /// knows how many tiles the tree has at those levels and how many of them
    /// the source refused, and only the consumer knows what reached the GPU.
    ///
    /// The distinction is the whole of a hang that cost an afternoon. The gate
    /// used to wait for *every* primed tile to reach the GPU, and a global grid
    /// over an ocean is not a grid the source serves: 618 of 682 tiles held, an
    /// empty queue, and a window that stayed shut until Ctrl-C. A tile the source
    /// does not have is resolved — it is never coming — so it is subtracted from
    /// what is waited for, and said out loud on the way out.
    ///
    /// Bounded by a deadline as well, because the alternative failure mode is
    /// worse than the one it prevents: a network that never answers would leave
    /// no window at all, and an application that shows nothing is
    /// indistinguishable from one that crashed.
    pub(super) fn still_warming(&mut self) -> bool {
        // Not a deadline for the pyramid — the window waits for all of it,
        // however long that takes. This is the failsafe for a network that will
        // never answer, where the alternative is an application that shows
        // nothing at all and is indistinguishable from one that crashed.
        const GIVE_UP_AFTER: std::time::Duration = std::time::Duration::from_secs(600);
        /// How often the wait says where it has got to. A silent minute with no
        /// window looks exactly like a hang.
        const REPORT_EVERY: std::time::Duration = std::time::Duration::from_secs(1);
        /// How many of the session's errors one report may quote. The reason the
        /// pyramid is short is in them, and while the window is hidden nothing
        /// else drains them — they used to pile up unread until the first frame.
        const QUOTE_ERRORS: usize = 3;

        let Some(level) = self.warmup.pinned_level else {
            return false;
        };
        let Some(active) = self.active.as_mut() else {
            return true;
        };
        let held = active.pump.prepared_through_level(level) as u32;
        let priming = active.pump.priming;
        let errors: Vec<String> = active.pump.errors.drain(..).collect();
        self.stats.errors += errors.len() as u64;
        for err in errors.iter().take(QUOTE_ERRORS) {
            tracing::warn!("server: {err}");
        }

        // Settled and complete: everything the source was ever going to give is
        // on the GPU.
        if let Some(p) = priming {
            if p.settled() && held >= p.expected() {
                if p.unavailable > 0 {
                    tracing::warn!(
                        tiles = held,
                        primed = p.total,
                        unavailable = p.unavailable,
                        seconds = self.warmup.since.elapsed().as_secs_f32(),
                        "coarse pyramid is on the GPU, less the tiles the source does \
                         not serve; the ground under those falls back to a coarser \
                         ancestor"
                    );
                } else {
                    tracing::info!(
                        tiles = held,
                        seconds = self.warmup.since.elapsed().as_secs_f32(),
                        "coarse pyramid is on the GPU"
                    );
                }
                return false;
            }
        }

        // A server that says nothing about priming is not a server that is still
        // priming. Holding the window on a report that never comes is an
        // indefinite hang with a healthy log — and the coarse tiles are already
        // on the GPU, which is the thing the hold exists to guarantee.
        if priming.is_none() && held > 0 {
            tracing::info!(
                tiles = held,
                seconds = self.warmup.since.elapsed().as_secs_f32(),
                "no priming report from this server; showing the globe on what is \
                 already resident"
            );
            return false;
        }

        let m = tuile_core::metrics::metrics();
        let p = priming.unwrap_or_default();
        if self.warmup.last_report.elapsed() >= REPORT_EVERY {
            self.warmup.last_report = std::time::Instant::now();
            tracing::info!(
                held,
                total = p.total,
                outstanding = p.outstanding,
                unavailable = p.unavailable,
                queued_uploads = active.pump.pending_uploads(),
                network_mib = m.store_bytes_fetched.get() / (1024 * 1024),
                from_cache = m.store_hits.get(),
                "holding the window until the coarse pyramid is on the GPU"
            );
        }
        if self.warmup.since.elapsed() > GIVE_UP_AFTER {
            tracing::error!(
                held,
                total = p.total,
                outstanding = p.outstanding,
                unavailable = p.unavailable,
                "the coarse pyramid never arrived; showing the globe anyway, and it \
                 will have holes in it"
            );
            return false;
        }
        true
    }
}
