// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the page and its worker say to each other beside the stream.
//!
//! The streaming protocol crosses as bytes (`tuile_web::post`) — the camera,
//! the acknowledgements with their bytes, the memory and thermal signals,
//! everything the budget hears. What is left is what a desktop shares
//! through memory and a page cannot: the start-up, the slot count the loader
//! reads, a change of layer, the loads held while the tab is hidden, and the
//! budget's signals for the page's overlay. Each is a small object, posted
//! as a structured clone.

use serde::{Deserialize, Serialize};

use super::options::{ProfileChoice, SourcesSpec};

/// From the page to its worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum ToWorker {
    /// Bring the session up: the sources, which terrain and layers, the
    /// numbers to run with. No credential: the catalog is a proxy that holds
    /// its own.
    #[serde(rename_all = "camelCase")]
    Start {
        sources: SourcesSpec,
        opening: usize,
        profile: ProfileChoice,
        device_memory: Option<f64>,
        /// The most decode workers to start: see `tuile_web::schedule::pool_size`.
        #[serde(default)]
        decode_workers: Option<u32>,
        /// How the page's device holds imagery: the plans the loader decodes
        /// by. RGBA8 unless the host asked for compression and the device
        /// can hold it: see [`super::imagery`].
        #[serde(default)]
        imagery: super::imagery::Held,
    },
    /// The sampled textures per stage the device reported: how many imagery
    /// tiles one drape may stack (`tuile_planetary::LayerBudget`). A count of
    /// texture slots, not of memory: the memory the session may use is the
    /// budget's, sized at the start from the device's and changed by
    /// `ClientMessage::Retarget` on the stream.
    Slots { reported: u32 },
    /// Drape the host's layer of this index.
    Imagery { layer: usize },
    /// Hold the loads not yet started, or let them go.
    Pause { paused: bool },
    /// Open the host's 3D Tiles tileset of this asset number: a source of the
    /// session's scene, its tiles on the session's one stream.
    OpenTileset { asset: i64 },
    /// Take it out of the scene.
    CloseTileset { asset: i64 },
}

/// From the worker to its page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum ToPage {
    /// The geometry server is running; the stream is open.
    Started,
    /// It will not: why, for the person.
    Failed { reason: String },
    /// A change of layer has resolved: the generation now draped, or why not.
    Switched {
        layer: usize,
        outcome: Result<u64, String>,
    },
    /// The budget's signals as the worker's session last published them:
    /// what the page's overlay reads, as a desktop's reads its own.
    Signals { glance: Box<Glance> },
    /// A tileset is in the scene, with what must be credited while it is
    /// drawn.
    TilesetOpened {
        asset: i64,
        /// The tag its tiles carry in the scene.
        tag: u8,
        credits: Vec<CreditWire>,
    },
    /// It could not be opened: why, for the person.
    TilesetFailed { asset: i64, reason: String },
}

/// A credit as it crosses: its words and its link. Its logo is not drawn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CreditWire {
    pub text: String,
    #[serde(default)]
    pub link: Option<String>,
}

impl CreditWire {
    pub(crate) fn of(credit: &tuile_planetary::Credit) -> Self {
        Self {
            text: credit.text.clone(),
            link: credit.link.as_ref().map(|l| l.to_string()),
        }
    }

    pub(crate) fn credit(&self) -> tuile_planetary::Credit {
        tuile_planetary::Credit {
            text: self.text.clone(),
            logo: None,
            link: self.link.as_deref().and_then(|l| url::Url::parse(l).ok()),
        }
    }
}

/// The budget's signals the page shows, as they cross to it: what the
/// status line and the overlay read of `tuile_core::budget::BudgetSignals`,
/// which the page publishes as its own.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Glance {
    pub bytes: [[u64; 2]; 4],
    pub target_bytes: [u64; 4],
    pub hard_limit_bytes: [u64; 4],
    pub in_flight: [u32; 4],
    pub local_reads: u32,
    pub evictions_total: [[u64; 4]; 4],
    pub uploads_total: [u64; 4],
    pub cancels_total: [u64; 3],
    pub failures_total: [[u64; 2]; 4],
    pub drawn_tiles: [u32; 3],
    pub lost_tiles: u32,
    pub bare_items: u32,
    pub sse_scale_milli: u32,
    pub pressure_level: u8,
    pub thermal_level: u8,
    pub floor_in: bool,
    pub converged: bool,
}

impl Glance {
    /// What the page shows of `signals`.
    pub(crate) fn of(s: &tuile_core::budget::BudgetSignals) -> Self {
        Self {
            bytes: s.bytes.0,
            target_bytes: s.target_bytes.0,
            hard_limit_bytes: s.hard_limit_bytes.0,
            in_flight: s.in_flight.0,
            local_reads: s.local_reads,
            evictions_total: s.evictions_total.0,
            uploads_total: s.uploads_total.0,
            cancels_total: s.cancels_total,
            failures_total: s.failures_total.0,
            drawn_tiles: s.drawn_tiles,
            lost_tiles: s.lost_tiles,
            bare_items: s.bare_items,
            sse_scale_milli: s.sse_scale_milli,
            pressure_level: s.pressure_level,
            thermal_level: s.thermal_level,
            floor_in: s.floor_in,
            converged: s.converged,
        }
    }

    /// The signals again, the fields the page shows filled in.
    pub(crate) fn signals(&self) -> tuile_core::budget::BudgetSignals {
        use tuile_core::budget::PerClass;
        let mut s = tuile_core::budget::BudgetSignals::default();
        s.bytes = PerClass(self.bytes);
        s.target_bytes = PerClass(self.target_bytes);
        s.hard_limit_bytes = PerClass(self.hard_limit_bytes);
        s.in_flight = PerClass(self.in_flight);
        s.local_reads = self.local_reads;
        s.evictions_total = PerClass(self.evictions_total);
        s.uploads_total = PerClass(self.uploads_total);
        s.cancels_total = self.cancels_total;
        s.failures_total = PerClass(self.failures_total);
        s.drawn_tiles = self.drawn_tiles;
        s.lost_tiles = self.lost_tiles;
        s.bare_items = self.bare_items;
        s.sse_scale_milli = self.sse_scale_milli;
        s.pressure_level = self.pressure_level;
        s.thermal_level = self.thermal_level;
        s.floor_in = self.floor_in;
        s.converged = self.converged;
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the page shows survives the crossing: read off the signals,
    /// written back, read again — the same numbers.
    #[test]
    fn a_glance_reads_back_the_signals_it_was_taken_from() {
        let mut s = tuile_core::budget::BudgetSignals::default();
        s.bytes.0[2] = [7 << 20, 1 << 20];
        s.in_flight.0[0] = 12;
        s.local_reads = 5;
        s.evictions_total.0[2][1] = 3;
        s.sse_scale_milli = 1250;
        s.converged = true;
        let glance = Glance::of(&s);
        assert_eq!(Glance::of(&glance.signals()), glance);
        assert_eq!(glance.signals().bytes, s.bytes);
        assert!(glance.signals().converged);
    }
}
