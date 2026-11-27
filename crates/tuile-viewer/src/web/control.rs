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
        /// Where the worker's own numbers go — the loads, the budget, the
        /// tile requests — beside the page's frames: the page's options.
        #[serde(default)]
        telemetry: Option<super::options::TelemetryOptions>,
        /// The page's switch when the worker starts: a message sent beside
        /// the start could arrive before the worker listens again.
        #[serde(default = "on")]
        metrics_on: bool,
        /// The page's launch identifier (`service.instance.id`): the worker
        /// files its numbers under the same launch, told apart from the
        /// page's by `tuile.realm`.
        #[serde(default)]
        instance: Option<String>,
    },
    /// The metrics sent, or not: the page's switch, followed.
    Telemetry { on: bool },
    /// The sampled textures per stage the device reported: how many imagery
    /// tiles one drape may stack (`tuile_planetary::LayerBudget`). A count of
    /// texture slots, not of memory: the memory the session may use is the
    /// budget's, sized at the start from the device's and changed by
    /// `ClientMessage::Retarget` on the stream.
    ///
    /// `ceiling` is the host's most slots a draw may carry, as the page's
    /// shader was built for (`tuile_core::raster::imagery_slots_capped`).
    Slots {
        reported: u32,
        #[serde(default = "useful_slots")]
        ceiling: u32,
    },
    /// Drape the host's layer of this index.
    Imagery { layer: usize },
    /// Hold imagery so from now on: the page's device's plans after a
    /// change of choice under the session. The drapes are renewed by the
    /// [`ToWorker::Imagery`] that follows.
    Compression { imagery: super::imagery::Held },
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
    /// The quality deficit and the specification in force (its index in
    /// `SpecKind::ALL`), for the budget's line.
    #[serde(default)]
    pub under_need_tiles: [u32; 2],
    #[serde(default)]
    pub deficit_levels: [u32; 2],
    #[serde(default)]
    pub spec: u8,
    #[serde(default)]
    pub spec_overridden: bool,
    /// What the budget window reads besides (`window_numbers`), in one
    /// flat list.
    #[serde(default)]
    pub window: Vec<u64>,
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
            under_need_tiles: s.under_need_tiles,
            deficit_levels: s.deficit_levels,
            spec: tuile_core::budget::SpecKind::ALL
                .iter()
                .position(|k| *k == s.spec)
                .and_then(|i| u8::try_from(i).ok())
                .unwrap_or(0),
            spec_overridden: s.spec_overridden,
            window: {
                let mut out = Vec::new();
                window_numbers(&mut s.clone(), &mut |v| out.push(*v));
                out
            },
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
        s.under_need_tiles = self.under_need_tiles;
        s.deficit_levels = self.deficit_levels;
        s.spec = tuile_core::budget::SpecKind::ALL
            .get(usize::from(self.spec))
            .copied()
            .unwrap_or_default();
        s.spec_overridden = self.spec_overridden;
        // A list of another length is another build's: left out whole.
        let mut probe = 0usize;
        window_numbers(&mut s.clone(), &mut |_| probe += 1);
        if self.window.len() == probe {
            let mut from = self.window.iter();
            window_numbers(&mut s, &mut |v| *v = from.next().copied().unwrap_or(0));
        }
        s
    }
}

/// Every number of `s` the budget window reads that the glance's own
/// fields do not carry, each handed to `f` in one fixed order: read to send
/// them, written to take them back.
fn window_numbers(s: &mut tuile_core::budget::BudgetSignals, f: &mut dyn FnMut(&mut u64)) {
    use tuile_core::budget::{Class, LAYERS, LEVELS};
    let small = |v: &mut u32, f: &mut dyn FnMut(&mut u64)| {
        let mut x = u64::from(*v);
        f(&mut x);
        *v = u32::try_from(x).unwrap_or(u32::MAX);
    };
    for c in Class::ALL {
        for l in 0..LEVELS {
            let v = &mut s.levels;
            small(&mut v.gpu_tiles[c][l], f);
            small(&mut v.decoded_tiles[c][l], f);
            f(&mut v.decoded[c][l].0);
            small(&mut v.drawn[c][l], f);
            small(&mut v.selected[c][l], f);
            small(&mut v.under_need[c][l], f);
            for k in 0..tuile_core::budget::BLOCKED {
                small(&mut v.blocked[c][l][k], f);
                small(&mut v.blocked_oldest_ms[c][l][k], f);
            }
            for k in 0..2 {
                small(&mut v.in_flight[c][l][k], f);
                small(&mut v.queued[c][l][k], f);
            }
            for n in &mut s.bytes_by_level[c][l] {
                f(n);
            }
            for n in &mut s.level_events_total[c][l] {
                f(n);
            }
            f(&mut s.evictions_by_level_total[c][l]);
        }
        small(&mut s.levels_seen[c], f);
        small(&mut s.bytes_levels[c], f);
        f(&mut s.protected_bytes[c]);
        f(&mut s.upload_bytes_total[c]);
        f(&mut s.wasted_starts_total[c]);
        // The compression's numbers the board reads (the frames' are the
        // page's own: `BudgetSignals::take_frames_from`).
        f(&mut s.transient_bytes[c]);
        f(&mut s.transfer_bytes[c]);
        f(&mut s.reserved_bytes[c]);
        f(&mut s.encode_fallbacks_total[c]);
    }
    f(&mut s.decoded_reserved_bytes);
    let y = &mut s.layers;
    for i in 0..LAYERS {
        small(&mut y.gpu_tiles[i], f);
        f(&mut y.gpu_bytes[i].0);
        small(&mut y.drawn[i], f);
        small(&mut y.in_flight[i], f);
        for r in &mut y.requests[i] {
            small(r, f);
        }
        small(&mut y.ram_hits[i], f);
        small(&mut y.completions[i], f);
        small(&mut y.failures[i], f);
        for n in &mut s.layer_loads_total[i] {
            f(n);
        }
    }
    small(&mut s.layers_seen, f);
    f(&mut s.decoded_bytes);
    f(&mut s.decide_micros.sum);
    f(&mut s.decide_micros.count);
    f(&mut s.decide_peak_micros);
    small(&mut s.decide_work, f);
    f(&mut s.decide_work_total);
    f(&mut s.completions_total);
    // The floor's level, plus one; nought for none.
    let mut floor = s.floor_level.map_or(0, |l| u64::from(l) + 1);
    f(&mut floor);
    s.floor_level = floor.checked_sub(1).and_then(|l| u32::try_from(l).ok());
}

/// A switch that is on unless said otherwise.
fn on() -> bool {
    true
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
        // And what the budget window reads, per level and per layer.
        use tuile_core::budget::Class;
        s.levels.queued[Class::Drape][14] = [3, 2];
        s.levels_seen[Class::Drape] = 1 << 14;
        // Why its tiles under their need wait, and the oldest's age.
        s.levels.under_need[Class::Drape][14] = 6;
        s.levels.blocked[Class::Drape][14][tuile_core::budget::Blocked::Uploading.index()] = 6;
        s.levels.blocked_oldest_ms[Class::Drape][14]
            [tuile_core::budget::Blocked::Uploading.index()] = 2_400;
        s.layer_loads_total[1] = [1, 2, 3, 4, 5];
        s.layers_seen = 0b11;
        s.decide_micros.count = 9;
        s.floor_level = Some(0);
        // And the compression section's.
        s.transient_bytes[Class::Drape] = 349_524;
        s.transfer_bytes[Class::Drape] = 699_048;
        s.reserved_bytes[Class::Drape] = 442_196;
        s.decoded_reserved_bytes = 524_340;
        s.encode_fallbacks_total[Class::Drape] = 2;
        let glance = Glance::of(&s);
        assert_eq!(Glance::of(&glance.signals()), glance);
        assert_eq!(glance.signals().bytes, s.bytes);
        let back = glance.signals();
        assert_eq!(back.levels, s.levels);
        assert_eq!(back.layer_loads_total, s.layer_loads_total);
        assert_eq!((back.layers_seen, back.floor_level), (0b11, Some(0)));
        assert_eq!(back.decide_micros.count, 9);
        assert_eq!(
            (
                back.transient_bytes,
                back.transfer_bytes,
                back.reserved_bytes,
                back.decoded_reserved_bytes,
                back.encode_fallbacks_total,
            ),
            (
                s.transient_bytes,
                s.transfer_bytes,
                s.reserved_bytes,
                s.decoded_reserved_bytes,
                s.encode_fallbacks_total
            ),
            "the compression section crosses"
        );
        assert!(glance.signals().converged);
    }
}

/// The ceiling a page that does not state one gets: the default.
fn useful_slots() -> u32 {
    tuile_core::raster::USEFUL_IMAGERY_SLOTS
}
