// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The budget window, drawn: the numbers of the budget's tables
//! (`BudgetSignals`, the signals the budget publishes and the telemetry
//! exports) as gauges, bars and colours, read at a glance — and every
//! number still written beside its shape.
//!
//! Nothing here computes a number the signals do not carry: each bar is a
//! value of the window's tables, each colour a rule over those values
//! ([`gauge`], [`level_severity`], [`attention`]). The rules say what
//! deserves a look:
//!
//! - **alert** (vermillion, a triangle): ground missing, a class over its
//!   target or hard limit, tiles under their need with nothing in flight
//!   or queued for them — a stall — a layer's loads failing;
//! - **warn** (orange, a diamond): a class near its target, tiles under
//!   their need while their loads run, selected tiles drawn by stand-ins,
//!   memory pressure or heat;
//! - **waste** (purple, a square): many copies held at a level nothing
//!   selects and nothing draws;
//! - **ok** (bluish green, a dot).
//!
//! The palette is Okabe and Ito's, told apart by readers with any of the
//! common colour-vision deficiencies, and every colour comes with a shape
//! and a word: colour is never the only cue.
//!
//! The drawing is a [`Drawing`]: triangles and text labels in device
//! pixels, drawn by any backend's overlay — the desktop's own window, a
//! page's browser window and a headless run's texture alike.

use std::collections::{HashMap, VecDeque};

use tuile_core::budget::{BudgetSignals, Class, BLOCKED, BLOCKED_REASONS, LAYERS, LEVELS};

use crate::text::{Font, Label, Mask};
use crate::{Mesh, Vertex};

/// A panel drawn: triangles, then text over them, in device pixels; and
/// its size. What lies over the whole panel — a legend's box and its words
/// ([`tooltip`]) — is a layer of its own, [`Drawing::on_top`], drawn after
/// the panel's text: in the panel's triangles, the panel's words would be
/// written over it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Drawing {
    pub mesh: Mesh,
    pub labels: Vec<Label>,
    pub size: (u32, u32),
    /// What each bar and cell means, where it is: for a pointer over it, or
    /// a keyboard's focus on it ([`tooltip`]).
    pub hits: Vec<Hit>,
    /// Drawn last: its triangles, then its text.
    pub on_top: Layer,
}

/// Triangles, then text over them, in device pixels.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layer {
    pub mesh: Mesh,
    pub labels: Vec<Label>,
}

/// A region of the board and its legend: each colour, mark and shape
/// drawn there, what it means and its value now — one line each.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// Left, top, width, height, device pixels.
    pub rect: [f64; 4],
    pub lines: Vec<Legend>,
    /// The terrain tile `(level, x, y)` a click on it picks (the
    /// reservoir's cells); none for the board's regions.
    pub pick: Option<(u32, u64, u64)>,
}

/// One line of a legend: its swatch, as drawn on the board, and its words.
#[derive(Debug, Clone, PartialEq)]
pub struct Legend {
    pub swatch: Swatch,
    pub text: String,
}

/// What a legend line shows beside its words.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Swatch {
    /// Nothing: a line of words.
    None,
    /// A bar's segment of this colour.
    Fill([f32; 4]),
    /// A mark across a bar, of this colour.
    Tick([f32; 4]),
    /// A severity's shape.
    Marker(Severity),
}

fn line(swatch: Swatch, text: String) -> Legend {
    Legend { swatch, text }
}

/// The legend of a severity's mark: its shape and colour in words, what it
/// means.
fn marker_line(sev: Severity, what: &str) -> Legend {
    let shape = match sev {
        Severity::Ok => "green dot = ok",
        Severity::Waste => "purple square = waste",
        Severity::Warn => "orange diamond = watch",
        Severity::Alert => "red triangle = alert",
    };
    line(Swatch::Marker(sev), format!("{shape}: {what}"))
}

impl Hit {
    pub fn contains(&self, (x, y): (f64, f64)) -> bool {
        let [l, t, w, h] = self.rect;
        x >= l && x < l + w && y >= t && y < t + h
    }
}

impl From<crate::PanelLayout> for Drawing {
    fn from(p: crate::PanelLayout) -> Self {
        Self {
            mesh: p.background,
            labels: p.label.into_iter().collect(),
            size: p.size,
            hits: Vec::new(),
            on_top: Layer::default(),
        }
    }
}

/// How much a value deserves a look, least first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Ok,
    Waste,
    Warn,
    Alert,
}

/// An sRGB colour, as the overlay takes it: linear.
pub(crate) fn srgb(hex: u32, alpha: f32) -> [f32; 4] {
    let c = |shift: u32| {
        let v = f32::from(((hex >> shift) & 0xff) as u8) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    [c(16), c(8), c(0), alpha]
}

impl Severity {
    /// Its colour: Okabe and Ito's bluish green, reddish purple, orange
    /// and vermillion.
    pub fn colour(self) -> [f32; 4] {
        match self {
            Self::Ok => srgb(0x009E73, 1.0),
            Self::Waste => srgb(0xCC79A7, 1.0),
            Self::Warn => srgb(0xE69F00, 1.0),
            Self::Alert => srgb(0xD55E00, 1.0),
        }
    }

    /// Its word, for the attention list.
    pub fn word(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Waste => "waste",
            Self::Warn => "watch",
            Self::Alert => "ALERT",
        }
    }
}

/// How near its target a class may come before it is watched: nine
/// tenths.
pub const NEAR_TARGET: f64 = 0.9;

/// A class's GPU bytes against its target and hard limit, as the budget
/// counts them: its unprotected bytes over the target, or all of them over
/// the hard limit, is an alert (the budget's invariant I2); its unprotected
/// bytes at [`NEAR_TARGET`] of the target or more, a warning.
pub fn gauge(gpu: u64, protected: u64, target: u64, hard: u64) -> Severity {
    let unprotected = gpu.saturating_sub(protected);
    if unprotected > target || gpu > hard {
        Severity::Alert
    } else if target > 0 && unprotected as f64 >= NEAR_TARGET * target as f64 {
        Severity::Warn
    } else {
        Severity::Ok
    }
}

/// Copies held at a level nothing selects, from which a level is waste.
pub const WASTE_TILES: u32 = 64;

/// One level of one class, as the instant table has it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LevelCell {
    pub gpu: u32,
    pub bytes: u64,
    pub decoded: u32,
    pub drawn: u32,
    pub selected: u32,
    pub under: u32,
    /// Need, prefetch.
    pub in_flight: [u32; 2],
    pub queued: [u32; 2],
    /// Whether the budget counts a deficit of the class's kind this step
    /// (`under_need_tiles`): the per-level `under` and the stand-ins are
    /// that deficit by level, and read as nought when it is.
    pub deficit: bool,
    /// The level holds waste ([`level_waste`]).
    pub waste: bool,
    /// The tiles of `under` by why each waits
    /// (`tuile_budget::Blocked`'s index), and the oldest one's age, ms.
    pub blocked: [u32; BLOCKED],
    pub blocked_oldest_ms: [u32; BLOCKED],
}

/// The budget's deficit for `c`'s kind this step: meshes for the meshes,
/// imagery for the drapes — the header's `under need`.
pub fn class_deficit(s: &BudgetSignals, c: Class) -> u32 {
    match c {
        Class::Drape => s.under_need_tiles[1],
        _ => s.under_need_tiles[0],
    }
}

impl LevelCell {
    pub fn of(s: &BudgetSignals, c: Class, l: usize) -> Self {
        let v = &s.levels;
        let [drawable, transit] = s.bytes_by_level[c][l];
        Self {
            gpu: v.gpu_tiles[c][l],
            bytes: drawable + transit,
            decoded: v.decoded_tiles[c][l],
            drawn: v.drawn[c][l],
            selected: v.selected[c][l],
            under: if class_deficit(s, c) > 0 {
                v.under_need[c][l]
            } else {
                0
            },
            in_flight: v.in_flight[c][l],
            queued: v.queued[c][l],
            deficit: class_deficit(s, c) > 0,
            waste: level_waste(s, c, l).under != [0, 0],
            blocked: if class_deficit(s, c) > 0 {
                v.blocked[c][l]
            } else {
                [0; BLOCKED]
            },
            blocked_oldest_ms: if class_deficit(s, c) > 0 {
                v.blocked_oldest_ms[c][l]
            } else {
                [0; BLOCKED]
            },
        }
    }

    /// Selected tiles drawn by stand-ins: a deficit by level, so none when
    /// the budget counts none.
    pub fn stand_ins(&self) -> u32 {
        if self.deficit {
            self.selected.saturating_sub(self.drawn)
        } else {
            0
        }
    }

    /// Under its need with nothing in flight or queued at the level.
    pub fn stalled(&self) -> bool {
        self.under > 0 && self.loading() == 0
    }

    fn loading(&self) -> u32 {
        self.in_flight[0] + self.in_flight[1] + self.queued[0] + self.queued[1]
    }
}

/// One level of one class, judged: tiles under their need with nothing in
/// flight or queued at the level for longer than the stall's delay
/// (`long`, [`StallWatch`]) is a stall (alert); under their need while
/// loads run or before the delay, or selected tiles drawn by stand-ins, a
/// warning; bytes held for nothing ([`level_waste`]) waste.
pub fn level_severity(cell: &LevelCell, long: bool) -> Severity {
    if cell.stalled() && long {
        Severity::Alert
    } else if cell.under > 0 || cell.stand_ins() > 0 {
        Severity::Warn
    } else if cell.waste {
        Severity::Waste
    } else {
        Severity::Ok
    }
}

/// How long a stall must last before it is one: a transient — the loads
/// of a move not started yet, a tile waiting out its dwell — never paints
/// red.
pub const STALL_DELAY: f64 = 1.0;

/// Since when each class, and each level of each class, has been under its
/// need with nothing in flight or queued: a stall is an alert once it has
/// lasted [`StallWatch::delay`].
#[derive(Debug, Clone)]
pub struct StallWatch {
    delay: f64,
    now: f64,
    classes: [Option<f64>; 4],
    levels: HashMap<(usize, usize), f64>,
}

impl Default for StallWatch {
    fn default() -> Self {
        Self::with_delay(STALL_DELAY)
    }
}

impl StallWatch {
    /// A watch whose stalls are alerts after `delay` seconds.
    pub fn with_delay(delay: f64) -> Self {
        Self {
            delay: delay.max(0.0),
            now: 0.0,
            classes: [None; 4],
            levels: HashMap::new(),
        }
    }

    /// Every stall an alert at once: for pictures and tests of one state.
    pub fn at_once() -> Self {
        Self::with_delay(0.0)
    }

    /// The stall's delay, seconds.
    pub fn delay(&self) -> f64 {
        self.delay
    }

    /// Takes `s` as the state at `now` (seconds, any origin).
    pub fn note(&mut self, s: &BudgetSignals, now: f64) {
        self.now = now;
        for (c, _, _) in CLASSES {
            let k = c as usize;
            let mut class_under = 0u64;
            let mut class_loading = 0u64;
            for l in 0..LEVELS {
                let x = LevelCell::of(s, c, l);
                class_under += u64::from(x.under);
                class_loading += u64::from(x.loading());
                if x.stalled() {
                    self.levels.entry((k, l)).or_insert(now);
                } else {
                    self.levels.remove(&(k, l));
                }
            }
            if class_under > 0 && class_loading == 0 {
                self.classes[k].get_or_insert(now);
            } else {
                self.classes[k] = None;
            }
        }
    }

    fn long(&self, since: Option<f64>) -> bool {
        self.delay <= 0.0 || since.is_some_and(|t| self.now - t >= self.delay)
    }

    /// Since how many seconds level `l` of `c` has been stalled.
    pub fn level_since(&self, c: Class, l: usize) -> Option<f64> {
        self.levels.get(&(c as usize, l)).map(|t| self.now - t)
    }

    /// Whether level `l` of `c`, stalled now, has been for the delay.
    pub fn level_long(&self, c: Class, l: usize) -> bool {
        self.long(self.levels.get(&(c as usize, l)).copied())
    }

    /// Whether class `c`, stalled now, has been for the delay.
    pub fn class_long(&self, c: Class) -> bool {
        self.long(self.classes[c as usize])
    }
}

/// Something that deserves a look, in one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub severity: Severity,
    pub text: String,
    /// How large it is, to order issues of one severity.
    weight: u64,
}

/// The classes, with the names a person reads.
const CLASSES: [(Class, &str, &str); 3] = [
    (Class::TerrainMesh, "T", "terrain"),
    (Class::Drape, "D", "imagery"),
    (Class::Tiles3dMesh, "3D", "3D Tiles"),
];

fn mib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0))
}

fn floor_at(s: &BudgetSignals, l: usize) -> bool {
    s.floor_level.is_some_and(|f| l as u32 <= f)
}

/// What deserves a look in `s`, the worst first, its stalls judged by
/// `stalls`. A healthy budget has nothing here.
pub fn attention(s: &BudgetSignals, stalls: &StallWatch) -> Vec<Issue> {
    let mut out = Vec::new();
    let mut add = |severity, weight: u64, text: String| {
        out.push(Issue {
            severity,
            text,
            weight,
        });
    };
    if s.lost_tiles > 0 || s.bare_items > 0 {
        add(
            Severity::Alert,
            u64::from(s.lost_tiles + s.bare_items),
            format!(
                "ground: {} tiles lost, {} bare — ground missing",
                s.lost_tiles, s.bare_items
            ),
        );
    }
    for (c, _, name) in CLASSES {
        let cells: Vec<(usize, LevelCell)> =
            (0..LEVELS).map(|l| (l, LevelCell::of(s, c, l))).collect();
        let under: u64 = cells.iter().map(|(_, x)| u64::from(x.under)).sum();
        let loading: u64 = cells.iter().map(|(_, x)| u64::from(x.loading())).sum();
        if under > 0 && loading == 0 && stalls.class_long(c) {
            add(
                Severity::Alert,
                under,
                format!("{name}: {under} tiles under need, nothing in flight or queued — stalled"),
            );
        } else if under > 0 && loading == 0 {
            add(
                Severity::Warn,
                under,
                format!(
                    "{name}: {under} tiles under need, nothing in flight or queued yet — waiting \
                     (a stall after {:.1} s)",
                    stalls.delay()
                ),
            );
        } else if under > 0 {
            add(
                Severity::Warn,
                under,
                format!("{name}: {under} tiles under need, {loading} loads in flight or queued"),
            );
            // The levels that wait with nothing of their own loading.
            for (l, x) in &cells {
                if x.stalled() && stalls.level_long(c, *l) {
                    add(
                        Severity::Alert,
                        u64::from(x.under),
                        format!(
                            "level {l} {name}: {} under need, nothing loading at this level",
                            x.under
                        ),
                    );
                }
            }
        }
        let stand_ins: u64 = cells.iter().map(|(_, x)| u64::from(x.stand_ins())).sum();
        if stand_ins > 0 {
            add(
                Severity::Warn,
                stand_ins,
                format!("{name}: {stand_ins} selected tiles drawn by stand-ins"),
            );
        }
        for (l, x) in &cells {
            if level_severity(x, false) == Severity::Waste {
                add(
                    Severity::Waste,
                    u64::from(x.gpu - x.drawn),
                    format!(
                        "level {l} {name}: {} resident, {} drawn, none selected",
                        x.gpu, x.drawn
                    ),
                );
            }
        }
        let [drawable, transit] = s.bytes[c];
        let gpu = drawable + transit;
        let (target, hard) = (s.target_bytes[c], s.hard_limit_bytes[c]);
        match gauge(gpu, s.protected_bytes[c], target, hard) {
            Severity::Alert if gpu > hard => add(
                Severity::Alert,
                gpu,
                format!(
                    "{name}: {} MiB over its {} MiB hard limit",
                    mib(gpu),
                    mib(hard)
                ),
            ),
            Severity::Alert => add(
                Severity::Alert,
                gpu,
                format!(
                    "{name}: {} MiB unprotected, over its {} MiB target",
                    mib(gpu.saturating_sub(s.protected_bytes[c])),
                    mib(target)
                ),
            ),
            Severity::Warn => add(
                Severity::Warn,
                gpu,
                format!(
                    "{name}: {} MiB, near its {} MiB target",
                    mib(gpu),
                    mib(target)
                ),
            ),
            _ => {}
        }
    }
    let state = |n: u8| ["normal", "warning", "critical", "background"][usize::from(n.min(3))];
    match s.pressure_level {
        0 => {}
        1 => add(Severity::Warn, 1, "memory pressure: warning".to_owned()),
        n => add(Severity::Alert, 2, format!("memory pressure: {}", state(n))),
    }
    match s.thermal_level {
        0 => {}
        1 => add(Severity::Warn, 1, "thermal state 1: fair".to_owned()),
        n => add(Severity::Alert, 2, format!("thermal state {n}: hot")),
    }
    for i in (0..LAYERS).filter(|i| s.layers_seen & (1 << i) != 0) {
        let failed = s.layers.failures[i];
        if failed > 0 {
            add(
                Severity::Alert,
                u64::from(failed),
                format!("layer {i}: {failed} loads failed this step"),
            );
        }
    }
    out.sort_by(|a, b| b.severity.cmp(&a.severity).then(b.weight.cmp(&a.weight)));
    out
}

/// The decision's time over the last publications: the mean of the steps
/// they cover and the worst of them. The budget publishes every half
/// second, its peak since the last publication and its sums since the
/// start; the time a step takes is read here from the sums' differences
/// and the peaks of the same publications, so the mean and the maximum
/// always cover the same steps — the maximum is never under the mean.
#[derive(Debug, Clone, Default)]
pub struct DecideHistory {
    /// The last publication's sums: steps, µs.
    last: Option<(u64, u64)>,
    /// Per publication since: steps, µs, peak µs.
    samples: VecDeque<(u64, u64, u64)>,
}

/// The publications [`DecideHistory`] keeps: a minute.
pub const DECIDE_SAMPLES: usize = 120;

/// The decision's time over a window: mean and maximum, ms, the steps and
/// publications it covers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecideSummary {
    pub mean_ms: f64,
    pub max_ms: f64,
    pub steps: u64,
    pub publications: usize,
}

impl DecideHistory {
    /// Takes `s` if it is a publication not yet seen.
    pub fn note(&mut self, s: &BudgetSignals) {
        let (count, sum) = (s.decide_micros.count, s.decide_micros.sum);
        match self.last {
            Some((c, _)) if c == count => return,
            // A new session: start again.
            Some((c, _)) if count < c => self.samples.clear(),
            Some((c, m)) => {
                self.samples
                    .push_back((count - c, sum.saturating_sub(m), s.decide_peak_micros));
                while self.samples.len() > DECIDE_SAMPLES {
                    self.samples.pop_front();
                }
            }
            // The first: only where the sums start from — its peak covers
            // the last half second, its sums the whole session.
            None => {}
        }
        self.last = Some((count, sum));
    }

    /// The summary, once two publications have been seen.
    pub fn summary(&self) -> Option<DecideSummary> {
        let steps: u64 = self.samples.iter().map(|s| s.0).sum();
        if steps == 0 {
            return None;
        }
        let sum: u64 = self.samples.iter().map(|s| s.1).sum();
        let peak = self.samples.iter().map(|s| s.2).max().unwrap_or(0);
        Some(DecideSummary {
            mean_ms: sum as f64 / steps as f64 / 1000.0,
            max_ms: peak as f64 / 1000.0,
            steps,
            publications: self.samples.len(),
        })
    }
}

/// The texts rasterised, kept from one drawing to the next.
#[derive(Debug, Default)]
pub struct TextCache {
    masks: HashMap<(String, u32), Mask>,
}

/// What the board shows: this step, or the counters summed — since the
/// start, or since `from` (the signals then, and how many seconds ago).
pub struct BoardView<'a> {
    pub cumulative: bool,
    pub from: Option<(&'a BudgetSignals, f64)>,
    /// The imagery layers' names, by index.
    pub layers: &'a [String],
    pub decide: Option<DecideSummary>,
    /// The stalls, and for how long: what makes one an alert.
    pub stalls: &'a StallWatch,
    /// The width the board is laid out for, points: sections too wide for
    /// it are stacked, lines wrapped. None: as wide as it likes.
    pub width: Option<f64>,
    /// Only the levels with something to look at (`window_levels=issues`).
    pub issues_only: bool,
}

pub(crate) const TEXT: u32 = 0xDDDDDD;
pub(crate) const DIM: u32 = 0x8C8C8C;
pub(crate) const TRACK: u32 = 0x2A2A2A;
/// Drawn: Okabe and Ito's blue.
const DRAWN: u32 = 0x0072B2;
/// Held, not drawn.
const HELD: u32 = 0x5C5C5C;
/// Loads: need in sky blue, prefetch the same paler; queued in yellow.
const LOADS: u32 = 0x56B4E9;
const QUEUED: u32 = 0xF0E442;

/// The text's em, points.
pub(crate) const EM: f64 = 11.0;
pub(crate) const ROW: f64 = 16.0;
pub(crate) const MARGIN: f64 = 12.0;

pub(crate) struct Painter<'a> {
    pub(crate) font: &'static Font,
    pub(crate) scale: f64,
    pub(crate) mesh: Mesh,
    pub(crate) labels: Vec<Label>,
    pub(crate) cache: &'a mut TextCache,
    pub(crate) extent: (f64, f64),
    pub(crate) hits: Vec<Hit>,
}

impl Painter<'_> {
    /// `text` explains the region `x, y, w, h` (points).
    pub(crate) fn hit(&mut self, x: f64, y: f64, w: f64, h: f64, lines: Vec<Legend>) {
        let k = self.scale;
        self.hits.push(Hit {
            rect: [x * k, y * k, w * k, h * k],
            lines,
            pick: None,
        });
    }

    /// As [`Self::hit`], a click on it picks `tile`.
    pub(crate) fn hit_tile(
        &mut self,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        lines: Vec<Legend>,
        tile: (u32, u64, u64),
    ) {
        self.hit(x, y, w, h, lines);
        if let Some(last) = self.hits.last_mut() {
            last.pick = Some(tile);
        }
    }

    pub(crate) fn grow(&mut self, x: f64, y: f64) {
        self.extent.0 = self.extent.0.max(x);
        self.extent.1 = self.extent.1.max(y);
    }

    pub(crate) fn tri(&mut self, p: [(f64, f64); 3], colour: [f32; 4]) {
        for (x, y) in p {
            self.grow(x, y);
            self.mesh
                .push(Vertex::new(x * self.scale, y * self.scale, colour));
        }
    }

    pub(crate) fn rect(&mut self, x: f64, y: f64, w: f64, h: f64, colour: [f32; 4]) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let (x1, y1) = (x + w, y + h);
        self.tri([(x, y), (x1, y), (x, y1)], colour);
        self.tri([(x, y1), (x1, y), (x1, y1)], colour);
    }

    pub(crate) fn width(&self, text: &str, size: f64) -> f64 {
        f64::from(self.font.width(text, (size * self.scale) as f32)) / self.scale
    }

    /// `text` with its top-left corner at `(x, y)`; returns its width.
    pub(crate) fn text(&mut self, x: f64, y: f64, text: &str, size: f64, colour: [f32; 4]) -> f64 {
        if text.is_empty() {
            return 0.0;
        }
        let px = (size * self.scale) as f32;
        if self.cache.masks.len() > 4_096 {
            self.cache.masks.clear();
        }
        let font = self.font;
        let mask = self
            .cache
            .masks
            .entry((text.to_owned(), (px * 16.0) as u32))
            .or_insert_with(|| font.rasterize(text, px))
            .clone();
        let w = f64::from(mask.width) / self.scale;
        self.grow(x + w, y + f64::from(mask.height) / self.scale);
        self.labels.push(Label {
            mask,
            position: [
                (x * self.scale).round() as f32,
                (y * self.scale).round() as f32,
            ],
            color: colour,
        });
        w
    }

    /// `text` ending at `right`.
    pub(crate) fn text_right(
        &mut self,
        right: f64,
        y: f64,
        text: &str,
        size: f64,
        colour: [f32; 4],
    ) {
        let w = self.width(text, size);
        self.text(right - w, y, text, size, colour);
    }

    /// The severity's shape, `size` points, centred at `(x, y)`: a dot, a
    /// square, a diamond, a triangle.
    pub(crate) fn marker(&mut self, sev: Severity, x: f64, y: f64, size: f64) {
        let r = size / 2.0;
        let c = sev.colour();
        match sev {
            Severity::Ok => {
                let n = 8;
                for i in 0..n {
                    let a0 = std::f64::consts::TAU * f64::from(i) / f64::from(n);
                    let a1 = std::f64::consts::TAU * f64::from(i + 1) / f64::from(n);
                    let k = r * 0.8;
                    self.tri(
                        [
                            (x, y),
                            (x + k * a0.cos(), y + k * a0.sin()),
                            (x + k * a1.cos(), y + k * a1.sin()),
                        ],
                        c,
                    );
                }
            }
            Severity::Waste => self.rect(x - r * 0.8, y - r * 0.8, r * 1.6, r * 1.6, c),
            Severity::Warn => {
                self.tri([(x, y - r), (x + r, y), (x, y + r)], c);
                self.tri([(x, y - r), (x, y + r), (x - r, y)], c);
            }
            Severity::Alert => {
                self.tri([(x, y - r), (x + r, y + r * 0.8), (x - r, y + r * 0.8)], c)
            }
        }
    }

    /// A bar of `w × h` at `(x, y)`: its track, then `parts` stacked from
    /// the left, each its share of `max`.
    pub(crate) fn bar(
        &mut self,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        parts: &[(f64, [f32; 4])],
        max: f64,
    ) {
        self.rect(x, y, w, h, srgb(TRACK, 1.0));
        if max <= 0.0 {
            return;
        }
        let mut at = x;
        for (v, c) in parts {
            let len = (v / max * w).clamp(0.0, x + w - at);
            self.rect(at, y, len, h, *c);
            at += len;
        }
    }

    /// A badge: the severity's colour behind `text`, its marker before it.
    pub(crate) fn badge(&mut self, x: f64, y: f64, sev: Severity, text: &str) -> f64 {
        let w = self.width(text, EM) + 22.0;
        let mut bg = sev.colour();
        bg[3] = 0.28;
        self.rect(x, y - 1.0, w, ROW - 2.0, bg);
        self.marker(sev, x + 8.0, y + ROW / 2.0 - 2.0, 9.0);
        self.text(x + 16.0, y, text, EM, srgb(TEXT, 1.0));
        w
    }
}

/// A count, or a dot for nought.
fn n(v: u64) -> String {
    if v == 0 {
        "·".to_owned()
    } else {
        v.to_string()
    }
}

fn pair(a: u32, b: u32) -> String {
    if a == 0 && b == 0 {
        "·".to_owned()
    } else {
        format!("{a}/{b}")
    }
}

/// The instant chart's number columns, per class: header, width.
const INSTANT_COLUMNS: [(&str, f64); 7] = [
    ("gpu", 36.0),
    ("MiB", 44.0),
    ("dec", 36.0),
    ("drawn/sel", 66.0),
    ("under", 42.0),
    ("fly n/p", 50.0),
    ("queue n/p", 58.0),
];
const CUMULATIVE_COLUMNS: [(&str, f64); 5] = [
    ("started", 54.0),
    ("done", 54.0),
    ("wasted", 54.0),
    ("evicted", 54.0),
    ("released", 60.0),
];
const LEVEL_COLUMN: f64 = 40.0;
const RESIDENCY_BAR: f64 = 110.0;
const LOADS_BAR: f64 = 50.0;

/// The present need's waits (`tuile_budget::waits`): first loads and
/// reloads after an eviction — evicted under 5 s, under 30 s before — each
/// its rate since the last publish and its 50th and 95th percentiles since
/// the start. Nothing until a wait ended.
fn waits(p: &mut Painter<'_>, s: &BudgetSignals, y: f64) -> f64 {
    use tuile_core::budget::waits::{percentile_ms, WAIT_KINDS};
    let total: u64 = s.waits.millis.iter().flatten().sum();
    if total == 0 {
        return y;
    }
    let text = srgb(TEXT, 1.0);
    let dim = srgb(DIM, 1.0);
    let mut y = y;
    p.text(MARGIN, y, "Waits", 12.0, text);
    y += ROW;
    let secs = (s.waits_recent_micros as f64 / 1e6).max(1e-3);
    let names = ["first loads", "reloads < 5 s", "reloads < 30 s"];
    for (k, kind) in WAIT_KINDS.iter().enumerate() {
        let n: u64 = s.waits.millis[k].iter().sum();
        let rate = s.waits_recent[k] as f64 / secs;
        let q =
            |q| percentile_ms(&s.waits.millis[k], q).map_or("·".to_owned(), |v| format!("{v} ms"));
        let line = format!(
            "{}: {rate:.1}/s · p50 {} · p95 {} · {n} since the start",
            names[k],
            q(50),
            q(95)
        );
        let why = match k {
            0 => "a tile the view asks for, loaded for the first time (or evicted over 30 s ago)".to_owned(),
            1 => "a tile the view asks for again under 5 s after the budget let it go: an eviction undone at once".to_owned(),
            _ => "a tile the view asks for again 5 to 30 s after the budget let it go".to_owned(),
        };
        let mut legend = vec![
            self::line(Swatch::None, format!("waits, {kind}")),
            self::line(Swatch::None, why),
            self::line(
                Swatch::None,
                "a wait runs from the step the present need asks for a tile not on the GPU to the step it is drawable".to_owned(),
            ),
            self::line(Swatch::None, "rate: since the last publish; percentiles: since the start".to_owned()),
        ];
        if k > 0 {
            let r = s.waits.reload_reasons;
            legend.push(self::line(
                Swatch::None,
                format!(
                    "reloads by the eviction they undo: headroom {}, below the floor value {}, room made {}, recycled {}",
                    r[0], r[1], r[2], r[3]
                ),
            ));
        }
        p.hit(
            MARGIN,
            y - 1.0,
            p.width(&line, EM) + 14.0,
            ROW - 1.0,
            legend,
        );
        p.text(MARGIN + 14.0, y, &line, EM, if n == 0 { dim } else { text });
        y += ROW;
    }
    y + 8.0
}

/// Draws the board for `s` (none yet: a line saying so) at `scale` device
/// pixels per point, its top-left corner at the origin.
pub fn draw(
    s: Option<&BudgetSignals>,
    view: &BoardView<'_>,
    cache: &mut TextCache,
    scale: f64,
) -> Drawing {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let mut p = Painter {
        font: Font::bundled(),
        scale,
        mesh: Mesh::new(),
        labels: Vec::new(),
        cache,
        extent: (0.0, 0.0),
        hits: Vec::new(),
    };
    let mut y = MARGIN;
    match s {
        None => {
            p.text(MARGIN, y, "Budget: no signals yet", EM, srgb(TEXT, 1.0));
        }
        Some(s) => {
            y = header(&mut p, s, view, y);
            y = issues(&mut p, s, view, y);
            y = gauges(&mut p, s, view, y);
            y = compression(&mut p, s, view, y);
            y = levels(&mut p, s, view, y);
            y = tiers(&mut p, s, view, y);
            y = scores(&mut p, s, view, y);
            y = waits(&mut p, s, y);
            layers(&mut p, s, view, y);
        }
    }
    finish(p, scale)
}

/// The painter's work as a [`Drawing`]: its extent plus a margin, over the
/// panel's background.
pub(crate) fn finish(p: Painter<'_>, scale: f64) -> Drawing {
    let (w, h) = (p.extent.0 + MARGIN, p.extent.1 + MARGIN);
    // The background, under everything: the triangles drawn so far go
    // after it.
    let mut mesh = Mesh::new();
    let bg = crate::PANEL_BACKGROUND;
    let opaque = [bg[0], bg[1], bg[2], 1.0];
    for (x, yy) in [(0.0, 0.0), (w, 0.0), (0.0, h), (0.0, h), (w, 0.0), (w, h)] {
        mesh.push(Vertex::new(x * scale, yy * scale, opaque));
    }
    mesh.extend(p.mesh);
    Drawing {
        mesh,
        labels: p.labels,
        hits: p.hits,
        size: ((w * scale).ceil() as u32, (h * scale).ceil() as u32),
        on_top: Layer::default(),
    }
}

/// The title, the specification, pressure and heat, the decision's time.
fn header(p: &mut Painter<'_>, s: &BudgetSignals, view: &BoardView<'_>, y: f64) -> f64 {
    let text = srgb(TEXT, 1.0);
    let dim = srgb(DIM, 1.0);
    let title = match (view.cumulative, view.from) {
        (false, _) => "Budget — instant (the last step)".to_owned(),
        (true, None) => "Budget — cumulative, since the session started".to_owned(),
        (true, Some((_, secs))) => format!("Budget — cumulative, the last {secs:.0} s"),
    };
    p.text(MARGIN, y, &title, 13.0, text);
    let mut flow = Flow::new(y + 20.0, view.width);
    let spec = format!(
        "spec {}{} · SSE scale {:.2}",
        s.spec.label(),
        if s.spec_overridden {
            " (override)"
        } else {
            " (context)"
        },
        f64::from(s.sse_scale_milli) / 1000.0
    );
    let (x, y) = flow.place(p.width(&spec, EM));
    p.text(x, y, &spec, EM, text);
    let pressure = ["normal", "warning", "critical", "background"]
        .get(usize::from(s.pressure_level))
        .unwrap_or(&"?");
    let sev = |l: u8| match l {
        0 => Severity::Ok,
        1 => Severity::Warn,
        _ => Severity::Alert,
    };
    for (level, label) in [
        (s.pressure_level, format!("pressure {pressure}")),
        (s.thermal_level, format!("thermal {}", s.thermal_level)),
    ] {
        let (x, y) = flow.place(p.width(&label, EM) + 22.0);
        p.badge(x, y, sev(level), &label);
    }
    let decide = match view.decide {
        Some(d) => format!(
            "decide mean {:.2} ms, max {:.2} ms over the last {} steps ({:.0} s)",
            d.mean_ms,
            d.max_ms,
            d.steps,
            d.publications as f64 * 0.5
        ),
        None => "decide: waiting for a second publication".to_owned(),
    };
    let (x, y) = flow.place(p.width(&decide, EM));
    p.text(x, y, &decide, EM, text);
    let work = format!(
        "work {} this step, {:.0} mean · {} steps",
        s.decide_work,
        s.decide_work_total as f64 / s.decide_micros.count.max(1) as f64,
        s.decide_micros.count
    );
    let (x, y) = flow.place(p.width(&work, EM));
    p.text(x, y, &work, EM, dim);
    flow.end() + 4.0
}

/// `text` from `(x, y)`, its words onto further lines (at `x`) when the
/// view's width would cut it; returns the line under the last.
fn wrapped(
    p: &mut Painter<'_>,
    x: f64,
    y: f64,
    text: &str,
    size: f64,
    colour: [f32; 4],
    width: Option<f64>,
) -> f64 {
    let Some(right) = width.map(|w| w - MARGIN) else {
        p.text(x, y, text, size, colour);
        return y + ROW;
    };
    let mut y = y;
    let mut line = String::new();
    for word in text.split(' ') {
        let candidate = if line.is_empty() {
            word.to_owned()
        } else {
            format!("{line} {word}")
        };
        if !line.is_empty() && x + p.width(&candidate, size) > right {
            p.text(x, y, &line, size, colour);
            y += ROW;
            line = word.to_owned();
        } else {
            line = candidate;
        }
    }
    p.text(x, y, &line, size, colour);
    y + ROW
}

/// Items placed left to right, onto the next line when the view's width
/// would cut one: text is never clipped by a narrow window.
struct Flow {
    x: f64,
    y: f64,
    right: Option<f64>,
}

impl Flow {
    fn new(y: f64, width: Option<f64>) -> Self {
        Self {
            x: MARGIN,
            y,
            right: width.map(|w| w - MARGIN),
        }
    }

    /// Where an item `w` points wide goes.
    fn place(&mut self, w: f64) -> (f64, f64) {
        if self.x > MARGIN && self.right.is_some_and(|r| self.x + w > r) {
            self.x = MARGIN;
            self.y += ROW;
        }
        let at = (self.x, self.y);
        self.x += w + 12.0;
        at
    }

    /// The line under the last.
    fn end(&self) -> f64 {
        self.y + ROW
    }
}

/// The attention list: the worst issues, one line each, or one line
/// saying there are none.
fn issues(p: &mut Painter<'_>, s: &BudgetSignals, view: &BoardView<'_>, y: f64) -> f64 {
    let list = attention(s, view.stalls);
    let text = srgb(TEXT, 1.0);
    let mut y = y;
    if list.is_empty() {
        p.marker(Severity::Ok, MARGIN + 5.0, y + ROW / 2.0 - 2.0, 9.0);
        p.text(MARGIN + 14.0, y, "nothing needs attention", EM, text);
        return y + ROW + 6.0;
    }
    const SHOWN: usize = 6;
    for issue in list.iter().take(SHOWN) {
        p.hit(
            MARGIN,
            y - 1.0,
            900.0,
            ROW - 1.0,
            vec![marker_line(issue.severity, &issue.text)],
        );
        p.marker(issue.severity, MARGIN + 5.0, y + ROW / 2.0 - 2.0, 9.0);
        let w = p.text(
            MARGIN + 14.0,
            y,
            issue.severity.word(),
            EM,
            issue.severity.colour(),
        );
        y = wrapped(
            p,
            MARGIN + 20.0 + w.max(36.0),
            y,
            &issue.text,
            EM,
            text,
            view.width,
        );
    }
    if list.len() > SHOWN {
        p.text(
            MARGIN + 14.0,
            y,
            &format!("and {} more", list.len() - SHOWN),
            EM,
            srgb(DIM, 1.0),
        );
        y += ROW;
    }
    y + 6.0
}

/// The classes' GPU bytes against their targets and limits, and the
/// header's other numbers.
fn gauges(p: &mut Painter<'_>, s: &BudgetSignals, view: &BoardView<'_>, y: f64) -> f64 {
    let text = srgb(TEXT, 1.0);
    let dim = srgb(DIM, 1.0);
    let mut y = y;
    const BAR: f64 = 260.0;
    for (c, short, name) in CLASSES {
        let [drawable, transit] = s.bytes[c];
        if c == Class::Tiles3dMesh && drawable + transit == 0 && s.levels_seen[c] == 0 {
            continue;
        }
        let (target, hard, protected) = (
            s.target_bytes[c],
            s.hard_limit_bytes[c],
            s.protected_bytes[c],
        );
        let sev = gauge(drawable + transit, protected, target, hard);
        let finite_hard = (hard != u64::MAX).then_some(hard);
        let max = (finite_hard.unwrap_or(0).max(target).max(drawable + transit) as f64) * 1.05;
        let x0 = MARGIN + 74.0;
        p.hit(MARGIN, y - 1.0, 74.0 + BAR, ROW - 1.0, explain_gauge(s, c));
        p.marker(sev, MARGIN + 5.0, y + ROW / 2.0 - 2.0, 9.0);
        p.text(MARGIN + 14.0, y, &format!("{short} {name}"), EM, text);
        let mut prot = srgb(DRAWN, 1.0);
        prot[3] = 0.55;
        let mut fill = sev.colour();
        fill[3] = 0.9;
        let shown = drawable.saturating_sub(protected.min(drawable));
        p.bar(
            x0,
            y + 3.0,
            BAR,
            9.0,
            &[
                (protected.min(drawable) as f64, prot),
                (shown as f64, fill),
                (transit as f64, srgb(LOADS, 0.8)),
            ],
            max,
        );
        // The target and the hard limit, as ticks.
        // The target white, the hard limit grey: neither in a severity's
        // colour, which is the bar's alone.
        for (at, colour) in [
            (Some(target), srgb(TEXT, 1.0)),
            (finite_hard, srgb(DIM, 1.0)),
        ] {
            if let Some(v) = at.filter(|_| max > 0.0) {
                let tx = x0 + v as f64 / max * BAR;
                p.rect(tx - 1.0, y + 1.0, 2.0, 13.0, colour);
            }
        }
        let numbers = format!(
            "{} MiB (+{} transit) of {} target{}, {} protected",
            mib(drawable),
            mib(transit),
            mib(target),
            finite_hard.map_or(String::new(), |h| format!(", {} hard", mib(h))),
            mib(protected),
        );
        // Beside the bar, or under it in a narrow window.
        if fits(view, 74.0 + BAR + 10.0 + p.width(&numbers, EM)) {
            p.text(x0 + BAR + 10.0, y, &numbers, EM, text);
        } else {
            y += ROW;
            p.text(x0, y, &numbers, EM, text);
        }
        y += ROW;
    }
    // The other numbers of the header, as the table has them; ground holes
    // and floor as badges.
    let mut flow = Flow::new(y, view.width);
    let holes = if s.lost_tiles > 0 || s.bare_items > 0 {
        Severity::Alert
    } else {
        Severity::Ok
    };
    let floor = match (s.floor_in, s.floor_level) {
        (_, None) => (Severity::Ok, "floor none".to_owned()),
        (true, Some(l)) => (Severity::Ok, format!("floor in, levels ≤ {l}")),
        (false, Some(l)) => (Severity::Warn, format!("floor not in yet, levels ≤ {l}")),
    };
    for (sev, label) in [
        (
            holes,
            format!("ground holes: {} lost, {} bare", s.lost_tiles, s.bare_items),
        ),
        floor,
    ] {
        let (x, y) = flow.place(p.width(&label, EM) + 22.0);
        p.badge(x, y, sev, &label);
    }
    for item in [
        format!("decoded {} MiB", mib(s.decoded_bytes)),
        format!(
            "in flight {}",
            s.in_flight.iter().map(|(_, v)| u64::from(v)).sum::<u64>()
        ),
        format!("local reads {}", s.local_reads),
        format!(
            "under need {} mesh, {} imagery",
            s.under_need_tiles[0], s.under_need_tiles[1]
        ),
    ] {
        let (x, y) = flow.place(p.width(&item, EM));
        p.text(x, y, &item, EM, text);
    }
    y = flow.end();
    if view.cumulative {
        let from = view.from.map(|(f, _)| f);
        let since =
            |cur: u64, old: fn(&BudgetSignals) -> u64| cur.saturating_sub(from.map_or(0, old));
        let up: u64 = s.upload_bytes_total.iter().map(|(_, v)| v).sum();
        let up0: u64 = from.map_or(0, |f| f.upload_bytes_total.iter().map(|(_, v)| v).sum());
        p.text(
            MARGIN,
            y,
            &format!(
                "uploaded {} MiB · completions {} · wasted starts {} · cancels grace {}, timeout {}, preempted {}",
                mib(up.saturating_sub(up0)),
                since(s.completions_total, |f| f.completions_total),
                since(
                    s.wasted_starts_total.iter().map(|(_, v)| v).sum(),
                    |f| f.wasted_starts_total.iter().map(|(_, v)| v).sum()
                ),
                since(s.cancels_total[0], |f| f.cancels_total[0]),
                since(s.cancels_total[1], |f| f.cancels_total[1]),
                since(s.cancels_total[2], |f| f.cancels_total[2]),
            ),
            EM,
            dim,
        );
        y += ROW;
    }
    y + 8.0
}

/// What encodes on the GPU hold beside the blocks until they have run:
/// Okabe and Ito's orange.
const ENCODE: u32 = 0xE69F00;
/// Reserved off the limits, outside any resource: their reddish purple.
const RESERVED: u32 = 0xCC79A7;

/// Whether `s` has anything compressed to show: an encode, a reservation,
/// a transient peak, a fallback, an encode deferred.
pub fn compressing(s: &BudgetSignals) -> bool {
    s.encode_blocks_total > 0
        || s.upload_deferred_total[1] > 0
        || s.decoded_reserved_bytes > 0
        || Class::ALL.iter().any(|c| {
            s.reserved_bytes[*c] > 0
                || s.transient_bytes[*c] > 0
                || s.encode_fallbacks_total[*c] > 0
        })
}

/// The compression section: per class, what is held against what crosses,
/// the transient peak in flight and the encoder's reservation; the last
/// frame's encodes against the encode lane; the deferrals by lane and the
/// fallbacks to RGBA8. One dim line when nothing is compressed.
fn compression(p: &mut Painter<'_>, s: &BudgetSignals, view: &BoardView<'_>, y: f64) -> f64 {
    let text = srgb(TEXT, 1.0);
    let dim = srgb(DIM, 1.0);
    let mut y = y;
    if !compressing(s) {
        let line = "Compression — none: imagery crosses and is held as it is";
        p.hit(
            MARGIN,
            y - 1.0,
            p.width(line, EM),
            ROW - 1.0,
            explain_compression_off(),
        );
        p.text(MARGIN, y, line, EM, dim);
        return y + ROW + 8.0;
    }
    p.text(MARGIN, y, "Compression", 12.0, text);
    y += ROW;
    let from = view.from.map(|(f, _)| f);
    const BAR: f64 = 260.0;
    for (c, short, name) in CLASSES {
        let [drawable, transit] = s.bytes[c];
        let held = drawable + transit;
        let (transient, transfer, reserved) = (
            s.transient_bytes[c],
            s.transfer_bytes[c],
            s.reserved_bytes[c],
        );
        let fell = fallbacks_since(s, from, c);
        if c != Class::Drape && transient + reserved + fell == 0 {
            continue;
        }
        let x0 = MARGIN + 74.0;
        p.hit(
            MARGIN,
            y - 1.0,
            74.0 + BAR,
            ROW - 1.0,
            explain_compression_class(s, from, c),
        );
        p.text(MARGIN + 14.0, y, &format!("{short} {name}"), EM, text);
        let max = (held + reserved).max(transfer) as f64 * 1.05;
        p.bar(
            x0,
            y + 3.0,
            BAR,
            9.0,
            &[
                (held.saturating_sub(transient) as f64, srgb(DRAWN, 0.9)),
                (transient as f64, srgb(ENCODE, 0.9)),
                (reserved as f64, srgb(RESERVED, 0.9)),
            ],
            max,
        );
        if transfer > 0 && max > 0.0 {
            let tx = x0 + transfer as f64 / max * BAR;
            p.rect(tx - 1.0, y + 1.0, 2.0, 13.0, srgb(LOADS, 1.0));
        }
        let numbers = format!(
            "held {} MiB · crossing {} MiB, +{} MiB until encoded · reserved {} MiB · fallbacks {fell}",
            mib(held),
            mib(transfer),
            mib(transient),
            mib(reserved),
        );
        if fits(view, 74.0 + BAR + 10.0 + p.width(&numbers, EM)) {
            p.text(x0 + BAR + 10.0, y, &numbers, EM, text);
        } else {
            y += ROW;
            p.text(x0, y, &numbers, EM, text);
        }
        y += ROW;
    }
    // The encodes against the lane.
    let debt = s.encode_carry_blocks < 0;
    let sev = if debt { Severity::Warn } else { Severity::Ok };
    let lane = if s.encode_share_blocks == u64::MAX {
        "no encode lane".to_owned()
    } else {
        format!(
            "lane {} blocks a frame, carry {}",
            s.encode_share_blocks, s.encode_carry_blocks
        )
    };
    let encodes = format!(
        "encodes: last frame {} tiles, {} blocks, {:.2} ms GPU · {}",
        s.frame_encoded_tiles,
        s.frame_encode_blocks,
        s.frame_encode_gpu_micros as f64 / 1000.0,
        lane
    );
    p.hit(
        MARGIN,
        y - 1.0,
        14.0 + p.width(&encodes, EM),
        ROW - 1.0,
        explain_encodes(s, from),
    );
    p.marker(sev, MARGIN + 5.0, y + ROW / 2.0 - 2.0, 9.0);
    p.text(MARGIN + 14.0, y, &encodes, EM, text);
    y += ROW;
    let since = |cur: u64, old: u64| cur.saturating_sub(from.map_or(0, |_| old));
    let (b, e) = (
        since(
            s.upload_deferred_total[0],
            from.map_or(0, |f| f.upload_deferred_total[0]),
        ),
        since(
            s.upload_deferred_total[1],
            from.map_or(0, |f| f.upload_deferred_total[1]),
        ),
    );
    let deferred = format!(
        "deferred: last frame {} by bytes, {} by encodes · {} / {} in all · decoded tier reserved {} MiB",
        s.frame_deferred[0],
        s.frame_deferred[1],
        b,
        e,
        mib(s.decoded_reserved_bytes)
    );
    p.hit(
        MARGIN,
        y - 1.0,
        14.0 + p.width(&deferred, EM),
        ROW - 1.0,
        explain_deferrals(s, from),
    );
    p.text(MARGIN + 14.0, y, &deferred, EM, text);
    y += ROW;
    y + 8.0
}

/// The fallbacks to RGBA8 of class `c`, since `from` when given.
fn fallbacks_since(s: &BudgetSignals, from: Option<&BudgetSignals>, c: Class) -> u64 {
    s.encode_fallbacks_total[c].saturating_sub(from.map_or(0, |f| f.encode_fallbacks_total[c]))
}

/// The legend of the compression line when nothing is compressed.
pub fn explain_compression_off() -> Vec<Legend> {
    vec![
        line(Swatch::None, "Compression".to_owned()),
        line(
            Swatch::None,
            "none: no encode, no reservation, nothing held beside a texture".to_owned(),
        ),
    ]
}

/// The legend of a class's compression row: one line per colour and mark,
/// each with its value now.
pub fn explain_compression_class(
    s: &BudgetSignals,
    from: Option<&BudgetSignals>,
    c: Class,
) -> Vec<Legend> {
    let [drawable, transit] = s.bytes[c];
    let held = drawable + transit;
    let transient = s.transient_bytes[c];
    vec![
        line(Swatch::None, format!("{}, compressed", class_title(c))),
        line(
            Swatch::Fill(srgb(DRAWN, 0.9)),
            format!(
                "blue = held on the GPU, resident and in transit: {} MiB",
                mib(held.saturating_sub(transient))
            ),
        ),
        line(
            Swatch::Fill(srgb(ENCODE, 0.9)),
            format!(
                "orange = the transient peak in flight, the RGBA8 held beside the blocks until their encode has run: {} MiB",
                mib(transient)
            ),
        ),
        line(
            Swatch::Fill(srgb(RESERVED, 0.9)),
            format!(
                "purple = reserved off the limits, the encoder's scratch: {} MiB",
                mib(s.reserved_bytes[c])
            ),
        ),
        line(
            Swatch::Tick(srgb(LOADS, 1.0)),
            format!(
                "sky-blue mark = what the copies in transit cross in: {} MiB",
                mib(s.transfer_bytes[c])
            ),
        ),
        line(
            Swatch::None,
            format!(
                "fallbacks to RGBA8 (an encode that could not be done): {}",
                fallbacks_since(s, from, c)
            ),
        ),
    ]
}

/// The legend of the encodes line.
pub fn explain_encodes(s: &BudgetSignals, from: Option<&BudgetSignals>) -> Vec<Legend> {
    let h = &s.frame_encode_micros;
    let (sum, count) = match from {
        Some(f) => (
            h.sum.saturating_sub(f.frame_encode_micros.sum),
            h.count.saturating_sub(f.frame_encode_micros.count),
        ),
        None => (h.sum, h.count),
    };
    let tiles = s
        .encoded_tiles_total
        .saturating_sub(from.map_or(0, |f| f.encoded_tiles_total));
    let blocks = s
        .encode_blocks_total
        .saturating_sub(from.map_or(0, |f| f.encode_blocks_total));
    let debt = s.encode_carry_blocks < 0;
    vec![
        line(Swatch::None, "Encodes on the GPU".to_owned()),
        line(
            Swatch::None,
            format!(
                "last frame: {} tiles, {} blocks, {:.2} ms of GPU",
                s.frame_encoded_tiles,
                s.frame_encode_blocks,
                s.frame_encode_gpu_micros as f64 / 1000.0
            ),
        ),
        line(
            Swatch::None,
            format!(
                "in all: {tiles} tiles, {blocks} blocks; {:.2} ms a frame that encodes, over {count} frames",
                sum as f64 / count.max(1) as f64 / 1000.0
            ),
        ),
        line(
            Swatch::None,
            if s.encode_share_blocks == u64::MAX {
                "the lane: none (an unbounded frame encodes what it has)".to_owned()
            } else {
                format!(
                    "the lane: {} blocks a frame, carry {} after the last",
                    s.encode_share_blocks, s.encode_carry_blocks
                )
            },
        ),
        marker_line(
            if debt { Severity::Warn } else { Severity::Ok },
            if debt {
                "in debt: the last tile crossed the share, the next frames wait"
            } else {
                "within its share"
            },
        ),
    ]
}

/// The legend of the deferrals line.
pub fn explain_deferrals(s: &BudgetSignals, from: Option<&BudgetSignals>) -> Vec<Legend> {
    let since = |i: usize| {
        s.upload_deferred_total[i].saturating_sub(from.map_or(0, |f| f.upload_deferred_total[i]))
    };
    vec![
        line(Swatch::None, "Uploads deferred, by lane".to_owned()),
        line(
            Swatch::None,
            format!(
                "bytes: what did not fit the frame's deadline — last frame {}, {} in all",
                s.frame_deferred[0],
                since(0)
            ),
        ),
        line(
            Swatch::None,
            format!(
                "encodes: what did not fit the encode lane, whole swap groups — last frame {}, {} in all",
                s.frame_deferred[1],
                since(1)
            ),
        ),
        line(
            Swatch::None,
            format!(
                "decoded tier reserved for the decode workers' encodes: {} MiB",
                mib(s.decoded_reserved_bytes)
            ),
        ),
    ]
}

/// The chart by level: for each class, side by side, the copies held
/// (drawn, held undrawn) as a bar, the table's numbers, and the loads (in
/// flight and queued, need and prefetch) as a bar; each level's cell
/// tinted and marked by [`level_severity`]. Cumulative: the counters summed.
fn levels(p: &mut Painter<'_>, s: &BudgetSignals, view: &BoardView<'_>, y: f64) -> f64 {
    let classes = shown_classes(s);
    let block = level_block(view);
    if fits(view, LEVEL_COLUMN + classes.len() as f64 * block) {
        return levels_of(p, s, view, y, &classes);
    }
    // Too narrow for the classes side by side: one under the other.
    let mut y = y;
    for c in classes {
        y = levels_of(p, s, view, y, &[c]);
    }
    y
}

/// The classes the board shows: terrain and drapes, and 3D Tiles once it
/// has held anything.
fn shown_classes(s: &BudgetSignals) -> Vec<(Class, &'static str)> {
    CLASSES
        .iter()
        .filter(|(c, _, _)| {
            *c != Class::Tiles3dMesh
                || s.levels_seen[*c] != 0
                || s.bytes_levels[*c] != 0
                || s.disk.seen[*c] != 0
        })
        .map(|(c, short, _)| (*c, *short))
        .collect()
}

/// Whether `width` points of content fit the view's width.
fn fits(view: &BoardView<'_>, width: f64) -> bool {
    view.width.is_none_or(|w| 2.0 * MARGIN + width <= w)
}

/// A class's block of the level chart, points.
fn level_block(view: &BoardView<'_>) -> f64 {
    let numbers: f64 = if view.cumulative {
        CUMULATIVE_COLUMNS.iter().map(|c| c.1).sum()
    } else {
        INSTANT_COLUMNS.iter().map(|c| c.1).sum()
    };
    14.0 + RESIDENCY_BAR
        + 6.0
        + numbers
        + if view.cumulative {
            0.0
        } else {
            8.0 + LOADS_BAR
        }
        + 16.0
}

/// Which levels the board lists: every level from 0 to the finest any
/// class has named, or only those with something to look at (`issues`:
/// a level a rule colours, in any class shown).
pub fn level_rows(
    s: &BudgetSignals,
    view: &BoardView<'_>,
    classes: &[(Class, &str)],
) -> Vec<usize> {
    let seen = classes.iter().fold(0u32, |a, (c, _)| {
        a | s.levels_seen[*c] | s.bytes_levels[*c] | if s.disk.known { s.disk.seen[*c] } else { 0 }
    });
    if seen == 0 {
        return Vec::new();
    }
    let finest = 31 - seen.leading_zeros() as usize;
    (0..=finest.min(LEVELS - 1))
        .filter(|&l| {
            !view.issues_only
                || classes.iter().any(|(c, _)| {
                    let cell = LevelCell::of(s, *c, l);
                    level_severity(&cell, view.stalls.level_long(*c, l)) != Severity::Ok
                })
        })
        .collect()
}

fn levels_of(
    p: &mut Painter<'_>,
    s: &BudgetSignals,
    view: &BoardView<'_>,
    y: f64,
    classes: &[(Class, &'static str)],
) -> f64 {
    let text = srgb(TEXT, 1.0);
    let dim = srgb(DIM, 1.0);
    let rows = level_rows(s, view, classes);
    let seen = rows.iter().fold(0u32, |a, l| a | (1 << l));
    let numbers: f64 = if view.cumulative {
        CUMULATIVE_COLUMNS.iter().map(|c| c.1).sum()
    } else {
        INSTANT_COLUMNS.iter().map(|c| c.1).sum()
    };
    let block = 14.0
        + RESIDENCY_BAR
        + 6.0
        + numbers
        + if view.cumulative {
            0.0
        } else {
            8.0 + LOADS_BAR
        }
        + 16.0;
    // The scales: the most copies, the most loads, of a class at a level.
    let mut most = HashMap::new();
    for (c, _) in classes {
        let (mut copies, mut loads) = (1u64, 1u64);
        for l in (0..LEVELS).filter(|l| seen & (1 << l) != 0) {
            let x = LevelCell::of(s, *c, l);
            copies = copies.max(u64::from(x.gpu.max(x.selected)));
            loads = loads.max(u64::from(x.loading()));
            if view.cumulative {
                let e = s.level_events_total[*c][l];
                copies = copies.max(e[0]).max(e[1]);
            }
        }
        most.insert(*c as usize, (copies as f64, loads as f64));
    }
    // The header.
    let mut y = y;
    p.text(MARGIN, y, "level", EM, dim);
    for (i, (_, short)) in classes.iter().enumerate() {
        let x0 = MARGIN + LEVEL_COLUMN + i as f64 * block;
        p.text(
            x0 + 14.0,
            y,
            &if view.cumulative {
                format!("{short} started / done")
            } else {
                format!("{short} drawn | held")
            },
            EM,
            dim,
        );
        let mut x = x0 + 14.0 + RESIDENCY_BAR + 6.0;
        let columns: &[(&str, f64)] = if view.cumulative {
            &CUMULATIVE_COLUMNS
        } else {
            &INSTANT_COLUMNS
        };
        for (name, w) in columns {
            x += w;
            p.text_right(x, y, name, EM - 1.0, dim);
        }
        if !view.cumulative {
            p.text(x + 8.0, y, "loads", EM - 1.0, dim);
        }
    }
    y += ROW;
    if rows.is_empty() {
        p.text(
            MARGIN + LEVEL_COLUMN,
            y,
            if view.issues_only {
                "no level needs attention"
            } else {
                "no level yet"
            },
            EM,
            dim,
        );
        y += ROW;
    }
    for &l in &rows {
        let floor = floor_at(s, l);
        p.text_right(
            MARGIN + LEVEL_COLUMN - 10.0,
            y,
            &format!("{l}{}", if floor { " F" } else { "" }),
            EM,
            if floor { dim } else { text },
        );
        for (i, (c, _)) in classes.iter().enumerate() {
            let x0 = MARGIN + LEVEL_COLUMN + i as f64 * block;
            let (copies, loads) = most[&(*c as usize)];
            if view.cumulative {
                level_cumulative(p, s, view, *c, l, x0, y, copies);
            } else {
                level_instant(p, s, *c, l, x0, y, copies, loads, view.stalls);
            }
        }
        y += ROW;
    }
    y + 8.0
}

/// A level's GPU bar: of its `resident` copies, the drawn ones (blue),
/// then the ones held and not drawn (grey). The split is the bar's
/// meaning: a flagged row (waste, a warning, a stall) keeps it, its flag
/// being the marker in the gutter and the row's tint, never the bar's
/// colours.
pub fn residency_bar(x: &LevelCell, resident: u64) -> [(f64, [f32; 4]); 2] {
    let drawn = u64::from(x.drawn).min(resident);
    [
        (drawn as f64, srgb(DRAWN, 1.0)),
        ((resident - drawn) as f64, srgb(HELD, 1.0)),
    ]
}

#[allow(clippy::too_many_arguments)]
fn level_instant(
    p: &mut Painter<'_>,
    s: &BudgetSignals,
    c: Class,
    l: usize,
    x0: f64,
    y: f64,
    copies: f64,
    loads: f64,
    stalls: &StallWatch,
) {
    let text = srgb(TEXT, 1.0);
    let x = LevelCell::of(s, c, l);
    let sev = level_severity(&x, stalls.level_long(c, l));
    // What the row says, in words: the copies, then the under-need and
    // the loads over it.
    let numbers: f64 = INSTANT_COLUMNS.iter().map(|c| c.1).sum();
    let long = stalls.level_long(c, l);
    p.hit(
        x0,
        y - 1.0,
        14.0 + RESIDENCY_BAR + 6.0 + numbers,
        ROW - 1.0,
        explain_residency(s, c, l, &x, long),
    );
    let under_x =
        x0 + 14.0 + RESIDENCY_BAR + 6.0 + INSTANT_COLUMNS[..4].iter().map(|c| c.1).sum::<f64>();
    p.hit(
        under_x,
        y - 1.0,
        INSTANT_COLUMNS[4].1,
        ROW - 1.0,
        explain_under(c, l, &x, stalls),
    );
    p.hit(
        under_x + INSTANT_COLUMNS[4].1,
        y - 1.0,
        INSTANT_COLUMNS[5].1 + INSTANT_COLUMNS[6].1 + 8.0 + LOADS_BAR,
        ROW - 1.0,
        explain_loads(c, l, &x),
    );
    if sev != Severity::Ok {
        // A tint under the row, faint for waste: a flag on numbers that
        // stay readable, not a colour over them.
        let mut tint = sev.colour();
        tint[3] = if sev == Severity::Waste { 0.07 } else { 0.16 };
        p.rect(
            x0,
            y - 1.0,
            14.0 + RESIDENCY_BAR + 6.0 + 332.0 + 8.0 + LOADS_BAR,
            ROW - 1.0,
            tint,
        );
        p.marker(sev, x0 + 6.0, y + ROW / 2.0 - 2.0, 8.0);
    }
    p.bar(
        x0 + 14.0,
        y + 3.0,
        RESIDENCY_BAR,
        9.0,
        &residency_bar(&x, u64::from(x.gpu)),
        copies,
    );
    // The selected tiles not drawn: a tick where the selection ends.
    if x.selected > x.drawn {
        let tx = x0 + 14.0 + (f64::from(x.selected) / copies * RESIDENCY_BAR).min(RESIDENCY_BAR);
        p.rect(tx - 1.0, y + 1.0, 2.0, 13.0, Severity::Warn.colour());
    }
    let under_colour = match sev {
        Severity::Alert | Severity::Warn if x.under > 0 => sev.colour(),
        _ => text,
    };
    let values = [
        (n(u64::from(x.gpu)), text),
        (
            if x.bytes == 0 {
                "·".to_owned()
            } else {
                mib(x.bytes)
            },
            text,
        ),
        (n(u64::from(x.decoded)), text),
        (
            pair(x.drawn, x.selected),
            if x.drawn < x.selected {
                Severity::Warn.colour()
            } else {
                text
            },
        ),
        (n(u64::from(x.under)), under_colour),
        (pair(x.in_flight[0], x.in_flight[1]), text),
        (pair(x.queued[0], x.queued[1]), text),
    ];
    let mut cx = x0 + 14.0 + RESIDENCY_BAR + 6.0;
    for ((value, colour), (_, w)) in values.iter().zip(INSTANT_COLUMNS) {
        cx += w;
        p.text_right(cx, y, value, EM, *colour);
    }
    let mut pale = srgb(LOADS, 1.0);
    pale[3] = 0.5;
    let mut pale_q = srgb(QUEUED, 1.0);
    pale_q[3] = 0.5;
    p.bar(
        cx + 8.0,
        y + 3.0,
        LOADS_BAR,
        9.0,
        &[
            (f64::from(x.in_flight[0]), srgb(LOADS, 1.0)),
            (f64::from(x.in_flight[1]), pale),
            (f64::from(x.queued[0]), srgb(QUEUED, 1.0)),
            (f64::from(x.queued[1]), pale_q),
        ],
        loads,
    );
}

#[allow(clippy::too_many_arguments)]
fn level_cumulative(
    p: &mut Painter<'_>,
    s: &BudgetSignals,
    view: &BoardView<'_>,
    c: Class,
    l: usize,
    x0: f64,
    y: f64,
    copies: f64,
) {
    let text = srgb(TEXT, 1.0);
    let from = view.from.map(|(f, _)| f);
    let e = s.level_events_total[c][l];
    let e0 = from.map_or([0; 4], |f| f.level_events_total[c][l]);
    let ev = s.evictions_by_level_total[c][l]
        .saturating_sub(from.map_or(0, |f| f.evictions_by_level_total[c][l]));
    let d = |k: usize| e[k].saturating_sub(e0[k]);
    let mut started = srgb(LOADS, 1.0);
    started[3] = 0.5;
    p.bar(
        x0 + 14.0,
        y + 3.0,
        RESIDENCY_BAR,
        4.0,
        &[(d(0) as f64, started)],
        copies,
    );
    p.bar(
        x0 + 14.0,
        y + 8.0,
        RESIDENCY_BAR,
        4.0,
        &[(d(1) as f64, srgb(DRAWN, 1.0))],
        copies,
    );
    let mut cx = x0 + 14.0 + RESIDENCY_BAR + 6.0;
    for (v, (_, w)) in [d(0), d(1), d(2), ev, d(3)].iter().zip(CUMULATIVE_COLUMNS) {
        cx += w;
        p.text_right(cx, y, &n(*v), EM, text);
    }
}

/// Where the tiles are, level by level, for each class side by side — or
/// one under the other when the window is too narrow: on the disk (the
/// local store's ledger), in RAM (decoded copies), on the GPU (resident:
/// drawn and held) — each a bar on its own scale, its tiles and bytes
/// written beside it — and what the level holds as waste
/// ([`level_waste`]). A shell with no local store says so.
fn tiers(p: &mut Painter<'_>, s: &BudgetSignals, view: &BoardView<'_>, y: f64) -> f64 {
    let text = srgb(TEXT, 1.0);
    let dim = srgb(DIM, 1.0);
    let mut y = y;
    p.text(MARGIN, y, "Where the tiles are", EM, text);
    y += ROW;
    p.text(
        MARGIN,
        y,
        "disk: the local store · RAM: decoded · GPU: resident, drawn | held · waste: held for nothing",
        EM - 1.0,
        dim,
    );
    y += ROW;
    if !s.disk.known {
        p.text(
            MARGIN,
            y,
            "disk: not known here — no local store",
            EM - 1.0,
            dim,
        );
        y += ROW;
        p.text(
            MARGIN,
            y,
            "(a browser's HTTP cache is not visible to the page)",
            EM - 1.0,
            dim,
        );
        y += ROW;
    }
    let classes = shown_classes(s);
    if fits(view, LEVEL_COLUMN + classes.len() as f64 * TIER_BLOCK) {
        return tiers_of(p, s, view, y, &classes);
    }
    for c in classes {
        y = tiers_of(p, s, view, y, &[c]);
    }
    y
}

const TIER_BAR: f64 = 52.0;
const TIER_VALUE: f64 = 96.0;
const TIER: f64 = TIER_BAR + 6.0 + TIER_VALUE + 8.0;
const WASTE_VALUE: f64 = 92.0;
const TIER_BLOCK: f64 = 14.0 + 3.0 * TIER + WASTE_VALUE + 16.0;

fn tiers_of(
    p: &mut Painter<'_>,
    s: &BudgetSignals,
    view: &BoardView<'_>,
    y: f64,
    classes: &[(Class, &'static str)],
) -> f64 {
    let text = srgb(TEXT, 1.0);
    let dim = srgb(DIM, 1.0);
    let mut y = y;
    let rows = level_rows(s, view, classes);
    for (i, (_, short)) in classes.iter().enumerate() {
        let x0 = MARGIN + LEVEL_COLUMN + i as f64 * TIER_BLOCK + 14.0;
        for (t, name) in ["disk", "RAM", "GPU"].iter().enumerate() {
            p.text(
                x0 + t as f64 * TIER,
                y,
                &format!("{short} {name}"),
                EM - 1.0,
                dim,
            );
        }
        p.text_right(x0 + 3.0 * TIER + WASTE_VALUE, y, "waste", EM - 1.0, dim);
    }
    p.text(MARGIN, y, "level", EM, dim);
    y += ROW;
    // Each tier's scale: the most tiles it holds at a level, per class.
    let most = |c: Class, t: usize| -> f64 {
        rows.iter()
            .filter_map(|&l| tier_of(s, t, c, l))
            .map(|[n, _]| n)
            .max()
            .unwrap_or(0)
            .max(1) as f64
    };
    let scales: Vec<[f64; 3]> = classes
        .iter()
        .map(|(c, _)| [most(*c, 0), most(*c, 1), most(*c, 2)])
        .collect();
    for &l in &rows {
        let floor = floor_at(s, l);
        p.text_right(
            MARGIN + LEVEL_COLUMN - 10.0,
            y,
            &format!("{l}{}", if floor { " F" } else { "" }),
            EM,
            if floor { dim } else { text },
        );
        for (i, (c, _)) in classes.iter().enumerate() {
            let x0 = MARGIN + LEVEL_COLUMN + i as f64 * TIER_BLOCK;
            let cell = LevelCell::of(s, *c, l);
            let waste = level_waste(s, *c, l);
            if cell.waste {
                p.marker(Severity::Waste, x0 + 6.0, y + ROW / 2.0 - 2.0, 8.0);
            }
            for (t, scale) in scales[i].iter().enumerate() {
                let x = x0 + 14.0 + t as f64 * TIER;
                let Some([n, bytes]) = tier_of(s, t, *c, l) else {
                    p.bar(x, y + 3.0, TIER_BAR, 9.0, &[], 0.0);
                    p.text(x + TIER_BAR + 6.0, y, "?", EM, dim);
                    continue;
                };
                let parts: Vec<(f64, [f32; 4])> = match t {
                    0 => vec![(n as f64, srgb(DIM, 1.0))],
                    1 => vec![(n as f64, srgb(LOADS, 1.0))],
                    _ => residency_bar(&cell, n).to_vec(),
                };
                p.bar(x, y + 3.0, TIER_BAR, 9.0, &parts, *scale);
                p.hit(x, y - 1.0, TIER, ROW - 1.0, explain_tier(s, t, *c, l));
                let value = if n == 0 {
                    "·".to_owned()
                } else {
                    format!("{n} · {} MiB", mib(bytes))
                };
                p.text_right(x + TIER_BAR + 6.0 + TIER_VALUE, y, &value, EM, text);
            }
            let under = waste.under[0] + waste.under[1];
            let value = match (waste.scored, under) {
                (_, 0) if waste.scored => {
                    format!("over {} MiB", mib(waste.over[0] + waste.over[1]))
                }
                (_, 0) => "·".to_owned(),
                (true, b) => format!("{} MiB", mib(b)),
                (false, b) => format!("{} MiB*", mib(b)),
            };
            p.text_right(
                x0 + 14.0 + 3.0 * TIER + WASTE_VALUE,
                y,
                &value,
                EM,
                if under > 0 {
                    Severity::Waste.colour()
                } else {
                    dim
                },
            );
            p.hit(
                x0 + 14.0 + 3.0 * TIER,
                y - 1.0,
                WASTE_VALUE,
                ROW - 1.0,
                explain_waste(*c, l, &waste),
            );
        }
        y += ROW;
    }
    if rows.iter().any(|&l| {
        classes
            .iter()
            .any(|(c, _)| !level_waste(s, *c, l).scored && level_waste(s, *c, l).under != [0, 0])
    }) {
        p.text(
            MARGIN + LEVEL_COLUMN,
            y,
            "* by the static rule: no level selected, ≥ 64 copies held, not the floor — until the budget scores its levels",
            EM - 1.0,
            dim,
        );
        y += ROW;
    }
    y + 8.0
}

/// What a level of a class holds as waste: the bytes the budget would let
/// go of first — held **under** its exchange threshold λ — on the GPU and
/// in RAM (decoded), and the bytes held over it; `scored` when they are the
/// budget's own scores.
///
/// Until the budget publishes its scores by level and tier, the static
/// rule stands in (`scored` false): copies held at a level nothing selects
/// and nothing draws, [`WASTE_TILES`] or more, the floor's levels aside —
/// their GPU bytes, prorated, are the waste. When the scores are there,
/// this is the one place that reads them, and every view of waste follows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LevelWaste {
    /// GPU, RAM.
    pub under: [u64; 2],
    pub over: [u64; 2],
    pub scored: bool,
}

/// See [`LevelWaste`].
pub fn level_waste(s: &BudgetSignals, c: Class, l: usize) -> LevelWaste {
    // Scored: the held copies under λ are the waste — the bytes of the
    // level's held score histogram's share under λ (all classes counted
    // together in the boards, prorated to this class's bytes).
    if let (Some(lambda), Some(h)) = (s.score_lambda, s.score_held.as_ref()) {
        use tuile_core::budget::score::{score_bucket, Cell, Side, DISTANCE_BANDS};
        let mut cell = Cell::default();
        for b in 0..DISTANCE_BANDS {
            cell.add(h.board.cell(l, b, Side::Held));
        }
        if cell.count > 0 {
            let under: u64 = cell.histogram[..score_bucket(lambda)]
                .iter()
                .map(|n| u64::from(*n))
                .sum();
            let [drawable, transit] = s.bytes_by_level[c][l];
            let bytes = drawable + transit;
            let below = bytes * under / u64::from(cell.count);
            return LevelWaste {
                under: [below, 0],
                over: [bytes - below, 0],
                scored: true,
            };
        }
    }
    let v = &s.levels;
    let (gpu, drawn, selected) = (v.gpu_tiles[c][l], v.drawn[c][l], v.selected[c][l]);
    let held = gpu.saturating_sub(drawn);
    if floor_at(s, l) || selected > 0 || held < WASTE_TILES || gpu == 0 {
        return LevelWaste::default();
    }
    let [drawable, transit] = s.bytes_by_level[c][l];
    let bytes = (drawable + transit) * u64::from(held) / u64::from(gpu);
    LevelWaste {
        under: [bytes, 0],
        over: [0, 0],
        scored: false,
    }
}

/// A level's scores, as the budget publishes them: the 10th, 50th and
/// 90th percentiles of the scores of its preload candidates, of what it
/// holds and of what it evicted, and λ, the exchange threshold — what is
/// held under it is released, a candidate over it is loaded.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LevelScores {
    pub preload: Option<[f64; 3]>,
    pub held: Option<[f64; 3]>,
    pub evicted: Option<[f64; 3]>,
    pub lambda: Option<f64>,
}

/// The scores of level `l` of `c`, once the budget publishes them; none
/// before. The one place that reads them: the box plots follow.
pub fn level_scores(s: &BudgetSignals, c: Class, l: usize) -> Option<LevelScores> {
    use tuile_core::budget::score::{quantiles, Cell, Side, DISTANCE_BANDS};
    // The score boards count every class together: a level's scores are
    // the same under each class's row.
    let _ = c;
    let sum = |board: &tuile_core::budget::score::ScoreBoard, side: Side| {
        let mut cell = Cell::default();
        for b in 0..DISTANCE_BANDS {
            cell.add(board.cell(l, b, side));
        }
        cell
    };
    let f = |q: Option<[u64; 3]>| q.map(|v| v.map(|x| x as f64));
    let held = s
        .score_held
        .as_ref()
        .and_then(|h| quantiles(&sum(&h.board, Side::Held)));
    let preload = quantiles(&sum(&s.score_recent, Side::Preload));
    let evicted = quantiles(&sum(&s.score_recent, Side::Evict));
    if held.is_none() && preload.is_none() && evicted.is_none() {
        return None;
    }
    Some(LevelScores {
        preload: f(preload),
        held: f(held),
        evicted: f(evicted),
        lambda: s.score_lambda.map(|v| v as f64),
    })
}

/// The legend of a level's box plots.
pub fn explain_scores(c: Class, l: usize, x: &LevelScores) -> Vec<Legend> {
    let q = |name: &str, v: Option<[f64; 3]>| match v {
        Some([p10, p50, p90]) => format!("{name}: 10th {p10:.3}, median {p50:.3}, 90th {p90:.3}"),
        None => format!("{name}: none"),
    };
    let mut out = vec![
        line(Swatch::None, format!("{} level {l}: scores", class_title(c))),
        line(Swatch::Fill(srgb(LOADS, 1.0)), "bar = 10th to 90th percentile of the scores".to_owned()),
        line(Swatch::Tick(srgb(TEXT, 1.0)), "white tick = the median".to_owned()),
        line(
            Swatch::Tick(Severity::Alert.colour()),
            format!(
                "red mark = λ, the exchange threshold{}: held below it are released, candidates above it are loaded",
                x.lambda.map_or(String::new(), |v| format!(" ({v:.3})"))
            ),
        ),
    ];
    out.push(line(
        Swatch::Fill(srgb(LOADS, 1.0)),
        q("sky blue = preload candidates", x.preload),
    ));
    out.push(line(
        Swatch::Fill(srgb(DRAWN, 1.0)),
        q("dark blue = held", x.held),
    ));
    if x.evicted.is_some() {
        out.push(line(
            Swatch::Fill(srgb(HELD, 1.0)),
            q("grey = evicted", x.evicted),
        ));
    }
    out
}

/// The scores by level, once the budget publishes them: for each class a
/// box plot of its preload candidates and one of what it holds, side by
/// side, on one scale, λ marked. Nothing before the data is there.
fn scores(p: &mut Painter<'_>, s: &BudgetSignals, view: &BoardView<'_>, y: f64) -> f64 {
    let classes = shown_classes(s);
    let rows = level_rows(s, view, &classes);
    let any = rows.iter().any(|&l| {
        classes
            .iter()
            .any(|(c, _)| level_scores(s, *c, l).is_some())
    });
    if !any {
        return y;
    }
    let text = srgb(TEXT, 1.0);
    let dim = srgb(DIM, 1.0);
    let mut y = y;
    p.text(
        MARGIN,
        y,
        "Scores — preload candidates and held, 10th–90th percentile, median, λ",
        EM,
        text,
    );
    y += ROW;
    // One scale for every plot: the highest score or λ seen.
    let top = rows
        .iter()
        .flat_map(|&l| {
            classes
                .iter()
                .filter_map(move |(c, _)| level_scores(s, *c, l))
        })
        .flat_map(|x| [x.preload.map(|q| q[2]), x.held.map(|q| q[2]), x.lambda])
        .flatten()
        .fold(0.0f64, f64::max)
        .max(1e-9);
    const PLOT: f64 = 150.0;
    let block = 14.0 + 2.0 * (PLOT + 12.0) + 12.0;
    for &l in &rows {
        p.text_right(MARGIN + LEVEL_COLUMN - 10.0, y, &l.to_string(), EM, dim);
        for (i, (c, _)) in classes.iter().enumerate() {
            let Some(x) = level_scores(s, *c, l) else {
                continue;
            };
            let x0 = MARGIN + LEVEL_COLUMN + i as f64 * block + 14.0;
            for (k, (q, colour)) in [(x.preload, LOADS), (x.held, DRAWN)]
                .into_iter()
                .enumerate()
            {
                let px = x0 + k as f64 * (PLOT + 12.0);
                p.rect(px, y + 3.0, PLOT, 9.0, srgb(TRACK, 1.0));
                if let Some([p10, p50, p90]) = q {
                    let at = |v: f64| px + (v / top).clamp(0.0, 1.0) * PLOT;
                    p.rect(
                        at(p10),
                        y + 4.0,
                        (at(p90) - at(p10)).max(1.0),
                        7.0,
                        srgb(colour, 1.0),
                    );
                    p.rect(at(p50) - 1.0, y + 2.0, 2.0, 11.0, srgb(TEXT, 1.0));
                }
                if let Some(v) = x.lambda {
                    let lx = px + (v / top).clamp(0.0, 1.0) * PLOT;
                    p.rect(lx - 1.0, y, 2.0, 15.0, Severity::Alert.colour());
                }
            }
            p.hit(
                x0,
                y - 1.0,
                2.0 * (PLOT + 12.0),
                ROW - 1.0,
                explain_scores(*c, l, &x),
            );
        }
        y += ROW;
    }
    let _ = text;
    y + 8.0
}

/// The tiles and bytes of tier `t` (0 disk, 1 RAM, 2 GPU) of `c` at `l`:
/// see `tuile_core::budget::tiers::tier`.
fn tier_of(s: &BudgetSignals, t: usize, c: Class, l: usize) -> Option<[u64; 2]> {
    tuile_core::budget::tiers::tier(s, t, c, l)
}

/// The imagery layers: copies held and drawn as a bar, the table's numbers,
/// failures marked.
fn layers(p: &mut Painter<'_>, s: &BudgetSignals, view: &BoardView<'_>, y: f64) -> f64 {
    let text = srgb(TEXT, 1.0);
    let dim = srgb(DIM, 1.0);
    let suffix = if view.cumulative { "" } else { " (step)" };
    let columns = [
        ("gpu".to_owned(), 44.0),
        ("MiB".to_owned(), 52.0),
        ("drawn".to_owned(), 50.0),
        ("in flight".to_owned(), 62.0),
        (format!("store{suffix}"), 84.0),
        (format!("network{suffix}"), 100.0),
        (format!("RAM hits{suffix}"), 104.0),
        (format!("done{suffix}"), 84.0),
        (format!("failed{suffix}"), 86.0),
        ("on disk".to_owned(), 130.0),
    ];
    const NAME: f64 = 170.0;
    let compact = !fits(
        view,
        NAME + RESIDENCY_BAR + 6.0 + columns.iter().map(|c| c.1).sum::<f64>(),
    );
    let mut y = y;
    p.text(MARGIN, y, "layer", EM, dim);
    p.text(MARGIN + NAME, y, "drawn | held", EM, dim);
    if !compact {
        let mut x = MARGIN + NAME + RESIDENCY_BAR + 6.0;
        for (name, w) in &columns {
            x += w;
            p.text_right(x, y, name, EM - 1.0, dim);
        }
    }
    y += ROW;
    let l = &s.layers;
    let most = (0..LAYERS)
        .map(|i| f64::from(l.gpu_tiles[i]))
        .fold(1.0, f64::max);
    for i in (0..LAYERS).filter(|i| s.layers_seen & (1 << i) != 0) {
        let name = match (i, view.layers.get(i)) {
            (0, Some(n)) => format!("0 base: {n}"),
            (0, None) => "0 base".to_owned(),
            (_, Some(n)) => format!("{i} overlay: {n}"),
            (_, None) => format!("{i} overlay"),
        };
        let counts: [u64; 5] = if view.cumulative {
            let t = s.layer_loads_total[i];
            let t0 = view.from.map_or([0; 5], |(f, _)| f.layer_loads_total[i]);
            std::array::from_fn(|k| t[k].saturating_sub(t0[k]))
        } else {
            [
                l.requests[i][0],
                l.requests[i][1],
                l.ram_hits[i],
                l.completions[i],
                l.failures[i],
            ]
            .map(u64::from)
        };
        let failing = counts[4] > 0 && (!view.cumulative || l.failures[i] > 0);
        if failing {
            p.marker(Severity::Alert, MARGIN + 5.0, y + ROW / 2.0 - 2.0, 8.0);
        }
        p.text(MARGIN + 14.0, y, &name, EM, text);
        {
            let mut lines = vec![
                line(
                    Swatch::None,
                    format!("layer {name}: {} MiB on the GPU", mib(l.gpu_bytes[i].0)),
                ),
                line(
                    Swatch::Fill(srgb(DRAWN, 1.0)),
                    format!("dark blue = drapes worn: {}", l.drawn[i]),
                ),
                line(
                    Swatch::Fill(srgb(HELD, 1.0)),
                    format!(
                        "grey = held, not worn: {}",
                        l.gpu_tiles[i].saturating_sub(l.drawn[i])
                    ),
                ),
            ];
            if failing {
                lines.push(marker_line(
                    Severity::Alert,
                    &format!("{} loads failed", counts[4]),
                ));
            }
            p.hit(MARGIN, y - 1.0, NAME + RESIDENCY_BAR, ROW - 1.0, lines);
        }
        p.bar(
            MARGIN + NAME,
            y + 3.0,
            RESIDENCY_BAR,
            9.0,
            &[
                (f64::from(l.drawn[i].min(l.gpu_tiles[i])), srgb(DRAWN, 1.0)),
                (
                    f64::from(l.gpu_tiles[i].saturating_sub(l.drawn[i])),
                    srgb(HELD, 1.0),
                ),
            ],
            most,
        );
        let mut values: Vec<(String, [f32; 4])> = vec![
            (n(u64::from(l.gpu_tiles[i])), text),
            (mib(l.gpu_bytes[i].0), text),
            (n(u64::from(l.drawn[i])), text),
            (n(u64::from(l.in_flight[i])), text),
        ];
        let disk = if s.disk.known {
            let (n, b) = s.disk.layers[i]
                .iter()
                .fold((0u64, 0u64), |(n, b), [x, y]| (n + x, b + y));
            if n == 0 {
                "·".to_owned()
            } else {
                format!("{n} · {} MiB", mib(b))
            }
        } else {
            "?".to_owned()
        };
        values.extend(counts.iter().enumerate().map(|(k, v)| {
            (
                n(*v),
                if k == 4 && *v > 0 {
                    Severity::Alert.colour()
                } else {
                    text
                },
            )
        }));
        values.push((disk, if s.disk.known { text } else { dim }));
        if compact {
            // Too narrow for the columns: each number named, flowing
            // under the bar.
            let mut flow = Flow::new(y + ROW, view.width);
            for ((value, colour), (name, _)) in values.iter().zip(&columns) {
                let item = format!("{name} {value}");
                let (x, fy) = flow.place(p.width(&item, EM));
                p.text(x, fy, &item, EM, *colour);
            }
            y = flow.end();
            continue;
        }
        let mut x = MARGIN + NAME + RESIDENCY_BAR + 6.0;
        for ((value, colour), (_, w)) in values.iter().zip(&columns) {
            x += w;
            p.text_right(x, y, value, EM, *colour);
        }
        y += ROW;
    }
    y
}

/// A class's name at the head of a sentence.
fn class_title(c: Class) -> &'static str {
    match c {
        Class::TerrainMesh => "Terrain",
        Class::Drape => "Imagery",
        Class::Tiles3dMesh => "3D Tiles",
        Class::Overlay => "Overlay",
    }
}

/// A count of tiles in words: "no", "one", or the number.
fn tiles(n: u64, what: &str) -> String {
    match n {
        0 => format!("no {what}s"),
        1 => format!("one {what}"),
        n => format!("{n} {what}s"),
    }
}

/// The legend of a level's copies bar — drawn, held, the selection's end —
/// and of its mark.
pub fn explain_residency(
    s: &BudgetSignals,
    c: Class,
    l: usize,
    x: &LevelCell,
    long: bool,
) -> Vec<Legend> {
    let held = x.gpu.saturating_sub(x.drawn);
    let mut out = vec![
        line(
            Swatch::None,
            format!(
                "{} level {l}: {} MiB on the GPU",
                class_title(c),
                mib(x.bytes)
            ),
        ),
        line(
            Swatch::Fill(srgb(DRAWN, 1.0)),
            format!("dark blue = drawn: {}", x.drawn),
        ),
    ];
    out.push(line(
        Swatch::Fill(srgb(HELD, 1.0)),
        format!("grey = held on the GPU, not drawn: {held}"),
    ));
    if x.waste {
        let w = level_waste(s, c, l);
        out.push(line(
            Swatch::None,
            format!(
                "of which held for nothing: {} MiB",
                mib(w.under[0] + w.under[1])
            ),
        ));
    }
    if x.selected > x.drawn {
        out.push(line(
            Swatch::Tick(Severity::Warn.colour()),
            format!(
                "orange tick = where the selection ends: {} selected",
                x.selected
            ),
        ));
    } else {
        out.push(line(Swatch::None, format!("selected: {}", x.selected)));
    }
    out.extend(row_mark(x, long));
    out.extend(blocked_lines(x));
    out
}

/// The legend of a row's tint and mark, if it has one.
fn row_mark(x: &LevelCell, long: bool) -> Option<Legend> {
    let sev = level_severity(x, long);
    let what = match sev {
        Severity::Ok => return None,
        Severity::Alert => format!(
            "{} under need, nothing loading — a stall",
            tiles(u64::from(x.under), "tile")
        ),
        Severity::Warn if x.under > 0 && x.loading() == 0 => {
            format!(
                "{} under need, nothing loading yet",
                tiles(u64::from(x.under), "tile")
            )
        }
        Severity::Warn if x.under > 0 => format!(
            "{} under need, loads under way",
            tiles(u64::from(x.under), "tile")
        ),
        Severity::Warn => format!(
            "{} drawn by another level while theirs loads",
            tiles(u64::from(x.stand_ins()), "selected tile")
        ),
        Severity::Waste => "copies held that nothing selects or draws".to_owned(),
    };
    Some(marker_line(sev, &what))
}

/// The legend of a level's under-need cell.
pub fn explain_under(c: Class, l: usize, x: &LevelCell, stalls: &StallWatch) -> Vec<Legend> {
    let mut out = vec![line(
        Swatch::None,
        format!(
            "{} level {l}: under need: {}",
            class_title(c),
            tiles(u64::from(x.under), "tile")
        ),
    )];
    if x.under > 0 {
        if x.loading() > 0 {
            out.push(line(
                Swatch::None,
                format!("loading at this level: {}", x.loading()),
            ));
        } else {
            let since = stalls.level_since(c, l).unwrap_or(0.0);
            out.push(line(
                Swatch::None,
                format!(
                    "nothing loading for {since:.1} s (a stall after {:.1} s)",
                    stalls.delay()
                ),
            ));
        }
    }
    out.extend(row_mark(x, stalls.level_long(c, l)));
    out.extend(blocked_lines(x));
    out
}

/// Why a level's tiles under their need wait: a line per reason, the most
/// tiles first, with how long the oldest has waited. Nothing when no tile
/// is under its need.
pub fn blocked_lines(x: &LevelCell) -> Vec<Legend> {
    let mut why: Vec<(u32, usize)> = x
        .blocked
        .iter()
        .enumerate()
        .filter(|(_, n)| **n > 0)
        .map(|(k, n)| (*n, k))
        .collect();
    if why.is_empty() {
        return Vec::new();
    }
    why.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut out = vec![line(Swatch::None, "why they wait:".to_owned())];
    for (n, k) in why {
        out.push(line(
            Swatch::None,
            format!(
                "  {n} {} (oldest {:.1} s)",
                BLOCKED_REASONS[k].words(),
                f64::from(x.blocked_oldest_ms[k]) / 1_000.0
            ),
        ));
    }
    out
}

/// The legend of a level's loads bar.
pub fn explain_loads(c: Class, l: usize, x: &LevelCell) -> Vec<Legend> {
    let [fly_n, fly_p] = x.in_flight;
    let [q_n, q_p] = x.queued;
    let mut pale = srgb(LOADS, 1.0);
    pale[3] = 0.5;
    let mut pale_q = srgb(QUEUED, 1.0);
    pale_q[3] = 0.5;
    vec![
        line(Swatch::None, format!("{} level {l}: loads", class_title(c))),
        line(
            Swatch::Fill(srgb(LOADS, 1.0)),
            format!("sky blue = in flight for the present need: {fly_n}"),
        ),
        line(
            Swatch::Fill(pale),
            format!("pale blue = in flight for prefetch: {fly_p}"),
        ),
        line(
            Swatch::Fill(srgb(QUEUED, 1.0)),
            format!("yellow = queued for the present need: {q_n}"),
        ),
        line(
            Swatch::Fill(pale_q),
            format!("pale yellow = queued for prefetch: {q_p}"),
        ),
    ]
}

/// The legend of a level's waste: what it holds for nothing, and over the
/// threshold when the budget scores it.
pub fn explain_waste(c: Class, l: usize, w: &LevelWaste) -> Vec<Legend> {
    let mut out = vec![line(
        Swatch::None,
        format!("{} level {l}: waste", class_title(c)),
    )];
    out.push(line(
        Swatch::Fill(Severity::Waste.colour()),
        format!(
            "purple = held for nothing: {} MiB on the GPU, {} MiB in RAM",
            mib(w.under[0]),
            mib(w.under[1])
        ),
    ));
    if w.scored {
        out.push(line(
            Swatch::None,
            format!(
                "held over the exchange threshold: {} MiB on the GPU, {} MiB in RAM",
                mib(w.over[0]),
                mib(w.over[1])
            ),
        ));
    } else {
        out.push(line(
            Swatch::None,
            "by the static rule: nothing selected here, 64 copies or more, not the floor"
                .to_owned(),
        ));
    }
    out
}

/// The legend of one tier's bar at a level.
pub fn explain_tier(s: &BudgetSignals, t: usize, c: Class, l: usize) -> Vec<Legend> {
    let at = format!("{} level {l}", class_title(c));
    match (t, tier_of(s, t, c, l)) {
        (0, None) => vec![line(
            Swatch::None,
            format!("{at} on the disk: not known — no local store here (a browser's HTTP cache is not visible to the page)"),
        )],
        (0, Some([n, b])) => vec![
            line(Swatch::None, format!("{at} on the disk, the local store")),
            line(Swatch::Fill(srgb(DIM, 1.0)), format!("grey = tiles stored: {n} ({} MiB)", mib(b))),
        ],
        (1, Some([n, b])) => vec![
            line(Swatch::None, format!("{at} in RAM, decoded")),
            line(Swatch::Fill(srgb(LOADS, 1.0)), format!("sky blue = decoded copies: {n} ({} MiB)", mib(b))),
        ],
        (_, Some([n, b])) => {
            let drawn = u64::from(s.levels.drawn[c][l]).min(n);
            vec![
                line(Swatch::None, format!("{at} on the GPU: {n} resident ({} MiB)", mib(b))),
                line(Swatch::Fill(srgb(DRAWN, 1.0)), format!("dark blue = drawn: {drawn}")),
                line(Swatch::Fill(srgb(HELD, 1.0)), format!("grey = held, not drawn: {}", n - drawn)),
            ]
        }
        (_, None) => vec![line(Swatch::None, format!("{at}: not known"))],
    }
}

/// The legend of a class's GPU gauge.
pub fn explain_gauge(s: &BudgetSignals, c: Class) -> Vec<Legend> {
    let [drawable, transit] = s.bytes[c];
    let (target, hard, protected) = (
        s.target_bytes[c],
        s.hard_limit_bytes[c],
        s.protected_bytes[c],
    );
    let sev = gauge(drawable + transit, protected, target, hard);
    let mut prot = srgb(DRAWN, 1.0);
    prot[3] = 0.55;
    let colour = match sev {
        Severity::Alert => "red",
        Severity::Warn => "orange",
        _ => "green",
    };
    let mut out = vec![
        line(Swatch::None, format!("{} on the GPU", class_title(c))),
        line(
            Swatch::Fill(prot),
            format!(
                "pale blue = protected (the floor, what is drawn): {} MiB",
                mib(protected.min(drawable))
            ),
        ),
        line(
            Swatch::Fill(sev.colour()),
            format!(
                "{colour} = unprotected, what the budget trims: {} MiB",
                mib(drawable.saturating_sub(protected.min(drawable)))
            ),
        ),
        line(
            Swatch::Fill(srgb(LOADS, 0.8)),
            format!("sky blue = in transit to the GPU: {} MiB", mib(transit)),
        ),
        line(
            Swatch::Tick(srgb(TEXT, 1.0)),
            format!("white mark = the target: {} MiB", mib(target)),
        ),
    ];
    if hard != u64::MAX {
        out.push(line(
            Swatch::Tick(srgb(DIM, 1.0)),
            format!("grey mark = the hard limit: {} MiB", mib(hard)),
        ));
    }
    if sev != Severity::Ok {
        out.push(marker_line(
            sev,
            if sev == Severity::Alert {
                "over the target or the hard limit"
            } else {
                "near the target"
            },
        ));
    }
    out
}

/// The scrollbar of a board taller than its window: where its track and
/// its thumb are (device pixels), for a pointer to grab and drag.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scrollbar {
    /// Left, top, width, height of the track; top and height of the thumb.
    pub track: [f64; 4],
    pub thumb: (f64, f64),
    /// The content's height and the window's, device pixels.
    pub content: f64,
    pub view: f64,
}

impl Scrollbar {
    /// The offset that puts the thumb's top at `y`, the pointer having
    /// grabbed it `grab` pixels under its top.
    pub fn offset_at(&self, y: f64, grab: f64) -> f64 {
        let room = (self.track[3] - self.thumb.1).max(1.0);
        let at = (y - grab - self.track[1]).clamp(0.0, room);
        at / room * (self.content - self.view)
    }

    /// Whether `(x, y)` is on the track.
    pub fn on_track(&self, (x, y): (f64, f64)) -> bool {
        let [l, t, w, h] = self.track;
        x >= l - 4.0 && x < l + w + 4.0 && y >= t && y < t + h
    }
}

/// `d` in a window of `frame` device pixels, scrolled down `offset` pixels
/// (clamped here to what there is to scroll): moved up, with a scrollbar on
/// the right when it is taller than the window, none when it fits.
pub fn fit(
    mut d: Drawing,
    frame: (u32, u32),
    offset: &mut f64,
    scale: f64,
) -> (Drawing, Option<Scrollbar>) {
    let (content, view) = (f64::from(d.size.1), f64::from(frame.1.max(1)));
    if content <= view {
        *offset = 0.0;
        return (d, None);
    }
    let max = content - view;
    *offset = offset.clamp(0.0, max);
    let dy = offset.round() as f32;
    for v in &mut d.mesh {
        v.position[1] -= dy;
    }
    for l in &mut d.labels {
        l.position[1] -= dy;
    }
    d.labels.retain(|l| {
        let top = f64::from(l.position[1]);
        top + f64::from(l.mask.height) > 0.0 && top < view
    });
    for h in &mut d.hits {
        h.rect[1] -= f64::from(dy);
    }
    d.hits
        .retain(|h| h.rect[1] + h.rect[3] > 0.0 && h.rect[1] < view);
    let k = scale.max(1.0);
    let (w, pad) = (8.0 * k, 3.0 * k);
    let track = [f64::from(frame.0) - w - pad, pad, w, view - 2.0 * pad];
    let thumb_h = (track[3] * view / content).max(24.0 * k).min(track[3]);
    let thumb_y = track[1] + (track[3] - thumb_h) * (*offset / max);
    let mut rect = |x: f64, y: f64, w: f64, h: f64, c: [f32; 4]| {
        for (px, py) in [
            (x, y),
            (x + w, y),
            (x, y + h),
            (x, y + h),
            (x + w, y),
            (x + w, y + h),
        ] {
            d.mesh.push(Vertex::new(px, py, c));
        }
    };
    rect(track[0], track[1], track[2], track[3], srgb(TRACK, 1.0));
    rect(track[0], thumb_y, track[2], thumb_h, srgb(DIM, 1.0));
    d.size.1 = frame.1;
    (
        d,
        Some(Scrollbar {
            track,
            thumb: (thumb_y, thumb_h),
            content,
            view,
        }),
    )
}

/// The region under `at` (device pixels), if one explains itself.
pub fn hit_at(d: &Drawing, at: (f64, f64)) -> Option<&Hit> {
    // The last drawn is on top: a cell inside a row's band wins.
    d.hits.iter().rev().find(|h| h.contains(at))
}

/// `d` with `hit`'s text in a box beside it — under it, or above it near
/// the bottom — and its region outlined: what a pointer over it, or the
/// keyboard's focus on it, shows. Wrapped to a third of the window. All of
/// it in [`Drawing::on_top`]: over the board's text, not under it.
pub fn tooltip(d: &mut Drawing, hit: &Hit, frame: (u32, u32), cache: &mut TextCache, scale: f64) {
    let k = scale.max(1.0);
    let font = Font::bundled();
    let px = (EM * k) as f32;
    let lh = f64::from(font.line_height(px)).max(14.0 * k);
    let pad = 6.0 * k;
    let sw = 12.0 * k;
    let indent = |l: &Legend| {
        if l.swatch == Swatch::None {
            0.0
        } else {
            sw + 6.0 * k
        }
    };
    let w = hit
        .lines
        .iter()
        .map(|l| indent(l) + f64::from(font.width(&l.text, px)))
        .fold(0.0, f64::max)
        + 2.0 * pad;
    let h = lh * hit.lines.len() as f64 + 2.0 * pad;
    let [hx, hy, hw, hh] = hit.rect;
    let (fw, fh) = (f64::from(frame.0), f64::from(frame.1));
    let x = hx.clamp(0.0, (fw - w).max(0.0));
    let y = if hy + hh + 4.0 + h <= fh {
        hy + hh + 4.0
    } else {
        (hy - 4.0 - h).max(0.0)
    };
    let mut mesh: Mesh = Vec::new();
    let rect = |mesh: &mut Mesh, x: f64, y: f64, w: f64, h: f64, c: [f32; 4]| {
        for (px, py) in [
            (x, y),
            (x + w, y),
            (x, y + h),
            (x, y + h),
            (x + w, y),
            (x + w, y + h),
        ] {
            mesh.push(Vertex::new(px, py, c));
        }
    };
    // The region, outlined.
    let o = srgb(TEXT, 0.9);
    let t = k;
    rect(&mut mesh, hx, hy, hw, t, o);
    rect(&mut mesh, hx, hy + hh - t, hw, t, o);
    rect(&mut mesh, hx, hy, t, hh, o);
    rect(&mut mesh, hx + hw - t, hy, t, hh, o);
    // The box.
    // Opaque: nothing of the board under it may show through.
    rect(&mut mesh, x, y, w, h, srgb(0x262626, 1.0));
    rect(&mut mesh, x, y, w, t, srgb(DIM, 1.0));
    let layer = &mut d.on_top;
    layer.mesh.extend(mesh);
    for (i, l) in hit.lines.iter().enumerate() {
        let ly = y + pad + lh * i as f64;
        let cy = ly + lh / 2.0;
        // The swatch, as the board draws it.
        let mut p = Painter {
            font,
            scale: 1.0,
            mesh: Mesh::new(),
            labels: Vec::new(),
            cache,
            extent: (0.0, 0.0),
            hits: Vec::new(),
        };
        match l.swatch {
            Swatch::None => {}
            Swatch::Fill(c) => {
                p.rect(x + pad, cy - 4.5 * k, sw, 9.0 * k, srgb(TRACK, 1.0));
                p.rect(x + pad, cy - 4.5 * k, sw, 9.0 * k, c);
            }
            Swatch::Tick(c) => {
                p.rect(x + pad, cy - 4.5 * k, sw, 9.0 * k, srgb(TRACK, 1.0));
                p.rect(x + pad + sw / 2.0 - k, cy - 6.5 * k, 2.0 * k, 13.0 * k, c);
            }
            Swatch::Marker(sev) => p.marker(sev, x + pad + sw / 2.0, cy, 10.0 * k),
        }
        layer.mesh.extend(p.mesh);
        let mask = cache
            .masks
            .entry((l.text.clone(), (px * 16.0) as u32))
            .or_insert_with(|| font.rasterize(&l.text, px))
            .clone();
        let top = cy - f64::from(mask.height) / 2.0;
        layer.labels.push(Label {
            mask,
            position: [(x + pad + indent(l)).round() as f32, top.round() as f32],
            color: srgb(TEXT, 1.0),
        });
    }
}

/// What a person does in the budget window: the same on a desktop's
/// window and a page's (device pixels, in the window).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BoardInput {
    /// The wheel or the trackpad, this many pixels down.
    Wheel(f64),
    /// The pointer moved to here.
    Pointer(f64, f64),
    /// The button pressed here, and let go.
    Press(f64, f64),
    Release,
    /// The pointer left the window.
    Leave,
    Key(BoardKey),
}

/// The keys the budget window answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardKey {
    /// The focus to the next or the previous region with a legend: the
    /// legends without a mouse.
    Next,
    Previous,
    /// No focus.
    Clear,
    PageDown,
    PageUp,
    Top,
    Bottom,
    /// The tables or the board, this step or the sums, sums from now,
    /// every level or those with an issue.
    Tables,
    Cumulative,
    Reset,
    Issues,
    /// The preload reservoir or the board; its level up, down, or auto.
    Reservoir,
    LevelUp,
    LevelDown,
    LevelAuto,
}

/// The budget window's scroll, pointer and focus: where it is scrolled to,
/// whether its bar is held, which region's legend is shown.
#[derive(Debug, Clone, Default)]
pub struct BoardControl {
    /// Scrolled down, device pixels.
    pub offset: f64,
    pointer: Option<(f64, f64)>,
    focus: Option<usize>,
    /// The thumb held, this many pixels under its top.
    grab: Option<f64>,
    /// The last drawing's scrollbar, and how many regions it had.
    bar: Option<Scrollbar>,
    regions: usize,
    view: f64,
    /// The last drawing's cells a click picks, and the tile picked.
    tiles: Vec<([f64; 4], (u32, u64, u64))>,
    picked: Option<(u32, u64, u64)>,
}

impl BoardControl {
    /// Takes `ev`; returns a key that asks the window something
    /// (`Tables`, `Cumulative`, `Reset`, `Issues`) for the caller to do.
    pub fn input(&mut self, ev: BoardInput) -> Option<BoardKey> {
        match ev {
            BoardInput::Wheel(dy) => self.offset += dy,
            BoardInput::Pointer(x, y) => {
                // The pointer takes over from the keyboard's focus.
                self.focus = None;
                self.pointer = Some((x, y));
                if let (Some(grab), Some(bar)) = (self.grab, self.bar) {
                    self.offset = bar.offset_at(y, grab);
                }
            }
            BoardInput::Press(x, y) => {
                // A press where the pointer is (a window that tells the
                // button and not the place).
                let (x, y) = if x.is_nan() || y.is_nan() {
                    self.pointer?
                } else {
                    (x, y)
                };
                self.pointer = Some((x, y));
                let cell = self
                    .tiles
                    .iter()
                    .rev()
                    .find(|(r, _)| x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3])
                    .map(|(_, t)| *t);
                if let (Some(t), None) = (cell, self.bar.filter(|b| b.on_track((x, y)))) {
                    // A second click on the tile picked lets it go.
                    self.picked = if self.picked == Some(t) {
                        None
                    } else {
                        Some(t)
                    };
                }
                if let Some(bar) = self.bar.filter(|b| b.on_track((x, y))) {
                    let (top, h) = bar.thumb;
                    let grab = if (top..top + h).contains(&y) {
                        y - top
                    } else {
                        h / 2.0
                    };
                    self.grab = Some(grab);
                    self.offset = bar.offset_at(y, grab);
                }
            }
            BoardInput::Release => self.grab = None,
            BoardInput::Leave => {
                self.pointer = None;
                self.grab = None;
            }
            BoardInput::Key(k) => match k {
                BoardKey::Next | BoardKey::Previous => {
                    // Before the first drawing: the first region, or the
                    // last (made the last one by `present`).
                    let n = self.regions.max(1);
                    self.focus = Some(match (self.focus, k) {
                        (None, BoardKey::Next) => 0,
                        (None, _) if self.regions == 0 => usize::MAX,
                        (None, _) => n - 1,
                        (Some(i), BoardKey::Next) => (i + 1) % n,
                        (Some(i), _) => (i + n - 1) % n,
                    });
                }
                BoardKey::Clear => {
                    self.focus = None;
                    self.picked = None;
                }
                BoardKey::PageDown => self.offset += self.view * 0.9,
                BoardKey::PageUp => self.offset -= self.view * 0.9,
                BoardKey::Top => self.offset = 0.0,
                BoardKey::Bottom => self.offset = f64::MAX / 4.0,
                other => return Some(other),
            },
        }
        None
    }

    /// `d` as the window shows it, `frame` device pixels: scrolled, its
    /// bar when it does not fit, and the legend of the region focused — or
    /// under the pointer.
    pub fn present(
        &mut self,
        d: Drawing,
        frame: (u32, u32),
        scale: f64,
        cache: &mut TextCache,
    ) -> Drawing {
        // The focus follows the regions: a region focused near the bottom
        // is scrolled into view.
        let regions = d.hits.len();
        self.focus = match self.focus {
            _ if regions == 0 => None,
            Some(i) if i >= regions => Some(regions - 1),
            f => f,
        };
        if let Some(h) = self.focus.and_then(|i| d.hits.get(i)) {
            let (top, bottom) = (h.rect[1], h.rect[1] + h.rect[3]);
            let view = f64::from(frame.1);
            if top < self.offset {
                self.offset = top - 8.0;
            } else if bottom > self.offset + view {
                self.offset = bottom - view + 8.0;
            }
        }
        let focused = self.focus.and_then(|i| d.hits.get(i).cloned());
        let (mut d, bar) = fit(d, frame, &mut self.offset, scale);
        self.bar = bar;
        self.tiles = d
            .hits
            .iter()
            .filter_map(|h| h.pick.map(|t| (h.rect, t)))
            .collect();
        self.regions = regions;
        self.view = f64::from(frame.1);
        let shown = match focused {
            // Moved with the drawing.
            Some(mut h) => {
                h.rect[1] -= self.offset.round();
                Some(h)
            }
            None if self.grab.is_none() => self.pointer.and_then(|at| hit_at(&d, at).cloned()),
            None => None,
        };
        if let Some(h) = shown {
            tooltip(&mut d, &h, frame, cache, scale);
        }
        d
    }

    /// The tile a click picked, until a second click or a clear.
    pub fn picked(&self) -> Option<(u32, u64, u64)> {
        self.picked
    }

    /// Whether the content is taller than the window: a bar is drawn.
    pub fn scrolls(&self) -> bool {
        self.bar.is_some()
    }
}

/// Budget signals for pictures and tests: a healthy step, and a stalled
/// one.
#[doc(hidden)]
pub mod fixtures {
    use tuile_core::budget::{BudgetSignals, Class};

    const MIB: u64 = 1024 * 1024;

    fn base() -> BudgetSignals {
        let mut s = BudgetSignals::default();
        s.floor_level = Some(2);
        s.floor_in = true;
        s.sse_scale_milli = 1000;
        s.decide_micros.count = 1300;
        s.decide_micros.sum = 1300 * 900;
        s.decide_work = 90_000;
        s.decide_work_total = 1300 * 110_000;
        for (c, target, hard) in [
            (Class::TerrainMesh, 1410, 2015),
            (Class::Drape, 1763, 2519),
            (Class::Tiles3dMesh, 823, 1176),
        ] {
            s.target_bytes[c] = target * MIB;
            s.hard_limit_bytes[c] = hard * MIB;
        }
        s.layers_seen = 1;
        s
    }

    /// One level: `gpu` copies, `drawn` drawn of `selected`, at `mib`.
    fn level(
        s: &mut BudgetSignals,
        c: Class,
        l: usize,
        gpu: u32,
        drawn: u32,
        selected: u32,
        mib: u64,
    ) {
        s.levels_seen[c] |= 1 << l;
        s.bytes_levels[c] |= 1 << l;
        s.levels.gpu_tiles[c][l] = gpu;
        s.levels.decoded_tiles[c][l] = gpu;
        s.levels.decoded[c][l] = tuile_core::budget::Bytes(mib * MIB / 10);
        s.levels.drawn[c][l] = drawn;
        s.levels.selected[c][l] = selected;
        s.bytes_by_level[c][l] = [mib * MIB / 10, 0];
        s.bytes[c][0] += mib * MIB / 10;
        // The local store holds three times what the GPU does, the floor's
        // levels and all.
        s.disk.known = true;
        let mut by = tuile_core::budget::ByLevel::default();
        by[l] = [u64::from(gpu) * 3, mib * MIB / 10 * 3];
        let layer = (c == Class::Drape).then_some(0);
        s.disk.add(c, layer, &by);
        let seen = s.disk.seen;
        s.disk.mark_seen(&seen);
    }

    /// A healthy step: every selected tile drawn at its level, nothing
    /// under its need, the classes well under their targets.
    pub fn healthy() -> BudgetSignals {
        let mut s = base();
        for l in 0..=17 {
            let k = (l as u32 + 1) * 12;
            level(
                &mut s,
                Class::TerrainMesh,
                l,
                k,
                k * 3 / 4,
                k * 3 / 4,
                u64::from(k),
            );
            level(
                &mut s,
                Class::Drape,
                l,
                k,
                k * 3 / 4,
                k * 3 / 4,
                u64::from(k) * 3,
            );
        }
        s.protected_bytes[Class::TerrainMesh] = 60 * MIB;
        s.levels.in_flight[Class::Drape][17] = [3, 2];
        s.levels.queued[Class::Drape][17] = [0, 6];
        s.layers.gpu_tiles[0] = 2400;
        s.layers.gpu_bytes[0].0 = 680 * MIB;
        s.layers.drawn[0] = 1800;
        s.layers.in_flight[0] = 5;
        s.layers.requests[0] = [4, 1];
        s.layers.completions[0] = 5;
        s.layer_loads_total[0] = [900, 300, 120, 1180, 0];
        s.decoded_bytes = 1022 * MIB;
        s.in_flight[Class::Drape] = 5;
        s
    }

    /// The stall Laurent's captures show: imagery under its need at levels
    /// 15 to 17 with nothing in flight or queued, and 1522 drapes held at
    /// level 21 that nothing selects or draws.
    pub fn stalled() -> BudgetSignals {
        let mut s = base();
        for l in 0..=17 {
            let k = (l as u32 + 1) * 12;
            level(
                &mut s,
                Class::TerrainMesh,
                l,
                k,
                k * 3 / 4,
                k * 3 / 4,
                u64::from(k),
            );
            level(
                &mut s,
                Class::Drape,
                l,
                k,
                k * 3 / 4,
                k * 3 / 4,
                u64::from(k) * 3,
            );
        }
        for (l, under) in [(15, 80), (16, 70), (17, 59)] {
            s.levels.under_need[Class::Drape][l] = under;
        }
        // Why they wait: at level 16, 52 drapes handed to the GPU and not
        // acknowledged, 18 one level coarse and deferred.
        use tuile_core::budget::Blocked;
        let at16 = &mut s.levels.blocked[Class::Drape][16];
        at16[Blocked::Uploading.index()] = 52;
        at16[Blocked::Deferred.index()] = 18;
        let age16 = &mut s.levels.blocked_oldest_ms[Class::Drape][16];
        age16[Blocked::Uploading.index()] = 3_400;
        age16[Blocked::Deferred.index()] = 12_000;
        s.levels.blocked[Class::Drape][15][Blocked::Uploading.index()] = 80;
        s.levels.blocked[Class::Drape][17][Blocked::NotRequested.index()] = 59;
        s.under_need_tiles = [0, 209];
        level(&mut s, Class::Drape, 21, 1522, 0, 0, 1522 * 2);
        s.layers.gpu_tiles[0] = 3900;
        s.layers.gpu_bytes[0].0 = 1500 * MIB;
        s.layers.drawn[0] = 1800;
        s.layer_loads_total[0] = [900, 300, 120, 1180, 0];
        s.decoded_bytes = 2040 * MIB;
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The compression section, explained** (Laurent's addition to CB8):
    /// none of it without compression, one dim line; with it, per class
    /// what is held against what crosses, the transient peak and the
    /// reservation, the encodes against the lane, the deferrals by lane and
    /// the fallbacks — each region's legend with its live values.
    #[test]
    fn the_compression_section_explains_each_mark() {
        let stalls = StallWatch::at_once();
        let mut cache = TextCache::default();
        let mut s = fixtures::healthy();
        let legends = |s: &BudgetSignals, cache: &mut TextCache| -> Vec<String> {
            let d = draw(Some(s), &view(&stalls, Some(2400.0), false), cache, 1.0);
            d.hits
                .iter()
                .map(|h| {
                    h.lines
                        .iter()
                        .map(|l| l.text.clone())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .collect()
        };
        let off = legends(&s, &mut cache);
        assert!(
            off.iter().any(|l| l.starts_with("Compression\nnone")),
            "{off:?}"
        );
        assert!(!compressing(&s));
        s.transient_bytes[Class::Drape] = 3 * 349_524;
        s.transfer_bytes[Class::Drape] = 5 * 349_524;
        s.reserved_bytes[Class::Drape] = 442_196;
        s.decoded_reserved_bytes = 4 * 524_340;
        s.encode_fallbacks_total[Class::Drape] = 2;
        s.encoded_tiles_total = 40;
        s.encode_blocks_total = 40 * 5_463;
        s.frame_encoded_tiles = 1;
        s.frame_encode_blocks = 5_463;
        s.frame_encode_gpu_micros = 9_500;
        s.frame_encode_micros.record(9_500);
        s.frame_deferred = [2, 3];
        s.upload_deferred_total = [10, 30];
        s.encode_share_blocks = 2_396;
        s.encode_carry_blocks = -3_067;
        assert!(compressing(&s));
        let on = legends(&s, &mut cache);
        let find = |head: &str| {
            on.iter()
                .find(|l| l.starts_with(head))
                .unwrap_or_else(|| panic!("no legend {head}: {on:?}"))
                .clone()
        };
        let imagery = find("Imagery, compressed");
        for want in [
            "orange = the transient peak in flight",
            ": 1.0 MiB",
            "purple = reserved off the limits, the encoder's scratch: 0.4 MiB",
            "sky-blue mark = what the copies in transit cross in: 1.7 MiB",
            "fallbacks to RGBA8 (an encode that could not be done): 2",
        ] {
            assert!(imagery.contains(want), "{want} in {imagery}");
        }
        let encodes = find("Encodes on the GPU");
        for want in [
            "last frame: 1 tiles, 5463 blocks, 9.50 ms of GPU",
            "in all: 40 tiles",
            "the lane: 2396 blocks a frame, carry -3067 after the last",
            "in debt",
        ] {
            assert!(encodes.contains(want), "{want} in {encodes}");
        }
        let deferred = find("Uploads deferred, by lane");
        for want in [
            "last frame 2, 10 in all",
            "last frame 3, 30 in all",
            "2.0 MiB",
        ] {
            assert!(deferred.contains(want), "{want} in {deferred}");
        }
    }

    fn view<'a>(stalls: &'a StallWatch, width: Option<f64>, issues_only: bool) -> BoardView<'a> {
        BoardView {
            cumulative: false,
            from: None,
            layers: &[],
            decide: None,
            stalls,
            width,
            issues_only,
        }
    }

    /// The bar appears only when the board is taller than its window; the
    /// wheel scrolls it, never past either end.
    #[test]
    fn a_scrollbar_only_when_the_board_does_not_fit() {
        let s = fixtures::stalled();
        let stalls = StallWatch::at_once();
        let mut cache = TextCache::default();
        let d = draw(
            Some(&s),
            &view(&stalls, Some(1400.0), false),
            &mut cache,
            1.0,
        );
        let mut c = BoardControl::default();
        c.present(d.clone(), (1400, d.size.1 + 10), 1.0, &mut cache);
        assert!(!c.scrolls(), "it fits: no bar");
        assert_eq!(c.offset, 0.0);
        let shown = c.present(d.clone(), (1400, 300), 1.0, &mut cache);
        assert!(c.scrolls(), "taller than its window: a bar");
        assert_eq!(shown.size.1, 300);
        c.input(BoardInput::Wheel(120.0));
        c.present(d.clone(), (1400, 300), 1.0, &mut cache);
        assert_eq!(c.offset, 120.0);
        c.input(BoardInput::Wheel(1e9));
        c.present(d.clone(), (1400, 300), 1.0, &mut cache);
        assert_eq!(c.offset, f64::from(d.size.1) - 300.0, "not past the end");
        c.input(BoardInput::Key(BoardKey::Top));
        c.present(d, (1400, 300), 1.0, &mut cache);
        assert_eq!(c.offset, 0.0);
    }

    /// A narrow window stacks what does not fit side by side: taller,
    /// never wider than it can help.
    #[test]
    fn a_narrow_window_stacks_the_classes() {
        let s = fixtures::healthy();
        let stalls = StallWatch::at_once();
        let mut cache = TextCache::default();
        let wide = draw(
            Some(&s),
            &view(&stalls, Some(2400.0), false),
            &mut cache,
            1.0,
        );
        let narrow = draw(
            Some(&s),
            &view(&stalls, Some(760.0), false),
            &mut cache,
            1.0,
        );
        assert!(narrow.size.1 > wide.size.1, "stacked: taller");
        assert!(
            narrow.size.0 < wide.size.0,
            "and narrower: {} vs {}",
            narrow.size.0,
            wide.size.0
        );
        assert!(narrow.size.0 <= 760, "within the window: {}", narrow.size.0);
    }

    /// Every level from 0 to the finest, or only those with an issue.
    #[test]
    fn the_filter_shows_only_the_levels_with_an_issue() {
        let s = fixtures::stalled();
        let stalls = StallWatch::at_once();
        let classes = shown_classes(&s);
        let all = level_rows(&s, &view(&stalls, None, false), &classes);
        assert_eq!(
            all,
            (0..=21).collect::<Vec<_>>(),
            "every level, gaps included"
        );
        let issues = level_rows(&s, &view(&stalls, None, true), &classes);
        assert_eq!(issues, vec![15, 16, 17, 21]);
        let healthy = fixtures::healthy();
        assert!(level_rows(
            &healthy,
            &view(&stalls, None, true),
            &shown_classes(&healthy)
        )
        .is_empty());
    }

    /// The scores' box plots explain themselves: the bar, the median, λ.
    #[test]
    fn a_box_plot_explains_itself() {
        let x = LevelScores {
            preload: Some([0.1, 0.4, 0.9]),
            held: Some([0.2, 0.5, 0.7]),
            evicted: None,
            lambda: Some(0.3),
        };
        let lines: Vec<String> = explain_scores(Class::Drape, 15, &x)
            .into_iter()
            .map(|l| l.text)
            .collect();
        assert_eq!(lines[1], "bar = 10th to 90th percentile of the scores");
        assert_eq!(lines[2], "white tick = the median");
        assert_eq!(
            lines[3],
            "red mark = λ, the exchange threshold (0.300): held below it are released, candidates above it are loaded"
        );
        assert_eq!(
            lines[4],
            "sky blue = preload candidates: 10th 0.100, median 0.400, 90th 0.900"
        );
        // No scores published yet: no section.
        assert_eq!(level_scores(&fixtures::stalled(), Class::Drape, 15), None);
    }

    /// The legends: each colour of a bar, its meaning and its value.
    #[test]
    fn a_bar_explains_its_colours() {
        let x = LevelCell {
            gpu: 285,
            bytes: 28 * 1024 * 1024,
            drawn: 69,
            selected: 72,
            in_flight: [4, 5],
            queued: [40, 0],
            deficit: true,
            ..LevelCell::default()
        };
        let s = fixtures::healthy();
        let texts = |v: Vec<Legend>| v.into_iter().map(|l| l.text).collect::<Vec<_>>();
        assert_eq!(
            texts(explain_residency(&s, Class::Drape, 17, &x, false)),
            [
                "Imagery level 17: 28.0 MiB on the GPU",
                "dark blue = drawn: 69",
                "grey = held on the GPU, not drawn: 216",
                "orange tick = where the selection ends: 72 selected",
                "orange diamond = watch: 3 selected tiles drawn by another level while theirs loads",
            ]
        );
        assert_eq!(
            texts(explain_loads(Class::Drape, 17, &x)),
            [
                "Imagery level 17: loads",
                "sky blue = in flight for the present need: 4",
                "pale blue = in flight for prefetch: 5",
                "yellow = queued for the present need: 40",
                "pale yellow = queued for prefetch: 0",
            ]
        );
        let gauge = texts(explain_gauge(&s, Class::TerrainMesh));
        assert!(
            gauge
                .iter()
                .any(|l| l == "white mark = the target: 1410.0 MiB"),
            "{gauge:?}"
        );
        assert!(
            gauge
                .iter()
                .any(|l| l == "grey mark = the hard limit: 2015.0 MiB"),
            "{gauge:?}"
        );
    }

    /// The legend shows for the region under the pointer, and for the one
    /// the keyboard's focus is on: the same, without a mouse.
    #[test]
    fn a_legend_under_the_pointer_or_the_focus() {
        let s = fixtures::stalled();
        let stalls = StallWatch::at_once();
        let mut cache = TextCache::default();
        let d = draw(
            Some(&s),
            &view(&stalls, Some(2400.0), false),
            &mut cache,
            1.0,
        );
        let first = d.hits[0].clone();
        let texts = |d: &Drawing| d.labels.len() + d.on_top.labels.len();
        let mut c = BoardControl::default();
        let bare = c.present(d.clone(), (2400, 2000), 1.0, &mut cache);
        let [x, y, w, h] = first.rect;
        c.input(BoardInput::Pointer(x + w / 2.0, y + h / 2.0));
        assert_eq!(
            hit_at(&d, (x + w / 2.0, y + h / 2.0)).map(|h| h.lines.len()),
            Some(first.lines.len()),
            "{first:?}"
        );
        let hovered = c.present(d.clone(), (2400, 2000), 1.0, &mut cache);
        assert_eq!(
            texts(&hovered),
            texts(&bare) + first.lines.len(),
            "its legend's lines"
        );
        assert_eq!(bare.on_top, Layer::default(), "no legend, nothing on top");
        assert_eq!(
            hovered.on_top.labels.len(),
            first.lines.len(),
            "the legend's words on top, over the board's"
        );
        assert_eq!(
            hovered.labels, bare.labels,
            "the board's own words untouched"
        );
        let mut k = BoardControl::default();
        k.input(BoardInput::Key(BoardKey::Next));
        let focused = k.present(d, (2400, 2000), 1.0, &mut cache);
        assert_eq!(texts(&focused), texts(&bare) + first.lines.len());
    }

    const MIB: u64 = 1024 * 1024;

    /// The thresholds: under nine tenths of the target is fine, from it a
    /// warning, over the target (unprotected bytes) or the hard limit an
    /// alert. Protected bytes do not count against the target.
    #[test]
    fn a_class_is_coloured_by_its_target_and_limit() {
        let (t, h) = (1000 * MIB, 1500 * MIB);
        assert_eq!(gauge(800 * MIB, 0, t, h), Severity::Ok);
        assert_eq!(gauge(899 * MIB, 0, t, h), Severity::Ok);
        assert_eq!(gauge(900 * MIB, 0, t, h), Severity::Warn);
        assert_eq!(gauge(1000 * MIB, 0, t, h), Severity::Warn);
        assert_eq!(gauge(1001 * MIB, 0, t, h), Severity::Alert);
        assert_eq!(
            gauge(1200 * MIB, 400 * MIB, t, h),
            Severity::Ok,
            "protected"
        );
        assert_eq!(gauge(1501 * MIB, 1000 * MIB, t, h), Severity::Alert, "hard");
        assert_eq!(gauge(10, 0, t, u64::MAX), Severity::Ok, "no hard limit");
    }

    /// A level: a stall is under its need with nothing loading; under its
    /// need while loading, or stand-ins, a warning; a level holding waste,
    /// waste.
    #[test]
    fn a_level_is_coloured_by_its_loads_and_its_need() {
        let base = LevelCell {
            gpu: 100,
            drawn: 40,
            selected: 40,
            deficit: true,
            ..LevelCell::default()
        };
        assert_eq!(level_severity(&base, true), Severity::Ok);
        let stall = LevelCell { under: 5, ..base };
        assert_eq!(level_severity(&stall, true), Severity::Alert);
        let loading = LevelCell {
            under: 5,
            queued: [0, 1],
            ..base
        };
        assert_eq!(level_severity(&loading, true), Severity::Warn);
        let stand_ins = LevelCell { drawn: 30, ..base };
        assert_eq!(level_severity(&stand_ins, true), Severity::Warn);
        let waste = LevelCell {
            waste: true,
            ..LevelCell::default()
        };
        assert_eq!(level_severity(&waste, true), Severity::Waste);
    }

    /// A flag is a flag: a waste row's GPU bar — in the level table and
    /// in the tiers' — is the same drawn | held split, in the same
    /// colours, as the same row unflagged; only its severity (the marker,
    /// the tint) says waste.
    #[test]
    fn a_waste_row_keeps_its_gpu_split() {
        let s = fixtures::stalled();
        let captured = LevelCell::of(&s, Class::Drape, 21);
        assert!(captured.waste, "the fixture holds waste at level 21");
        let some_drawn = LevelCell {
            gpu: 200,
            drawn: 30,
            waste: true,
            ..LevelCell::default()
        };
        for flagged in [captured, some_drawn] {
            assert_eq!(level_severity(&flagged, false), Severity::Waste);
            let plain = LevelCell {
                waste: false,
                ..flagged
            };
            assert_eq!(level_severity(&plain, false), Severity::Ok);
            for resident in [u64::from(flagged.gpu), u64::from(flagged.gpu) / 2, 0] {
                assert_eq!(
                    residency_bar(&flagged, resident),
                    residency_bar(&plain, resident),
                    "{flagged:?}, {resident} resident"
                );
            }
        }
        assert_eq!(
            residency_bar(&some_drawn, 200),
            [(30.0, srgb(DRAWN, 1.0)), (170.0, srgb(HELD, 1.0))]
        );
    }

    /// Waste is what [`level_waste`] says a level holds for nothing — the
    /// static rule until the budget scores its levels: copies at a level
    /// nothing selects, never the floor's, never a few — and every view of
    /// it follows: the level's colour, the attention list.
    #[test]
    fn waste_follows_the_levels_held_for_nothing() {
        let s = fixtures::stalled();
        let w = level_waste(&s, Class::Drape, 21);
        assert!(!w.scored && w.under[0] > 0, "{w:?}");
        assert!(LevelCell::of(&s, Class::Drape, 21).waste);
        assert_eq!(
            level_waste(&s, Class::Drape, 12),
            LevelWaste::default(),
            "selected"
        );
        let mut floor = s.clone();
        floor.floor_level = Some(21);
        assert_eq!(
            level_waste(&floor, Class::Drape, 21),
            LevelWaste::default(),
            "the floor"
        );
        let mut few = s.clone();
        few.levels.gpu_tiles[Class::Drape][21] = WASTE_TILES - 1;
        assert_eq!(
            level_waste(&few, Class::Drape, 21),
            LevelWaste::default(),
            "a few"
        );
        assert!(attention(&few, &StallWatch::at_once())
            .iter()
            .all(|i| i.severity != Severity::Waste));
    }

    /// **The red triangle says why** (Laurent, 2026-11-24): hovering a
    /// level's mark lists why its tiles under their need wait, the most
    /// first, each with the oldest one's age — the residency region (the
    /// mark's) and the under-need cell alike; a level with nothing under
    /// its need says nothing of it.
    #[test]
    fn the_red_triangle_lists_why_its_tiles_wait() {
        let s = fixtures::stalled();
        let x = LevelCell::of(&s, Class::Drape, 16);
        let stalls = StallWatch::at_once();
        assert_eq!(level_severity(&x, true), Severity::Alert, "a red triangle");
        let words = |v: Vec<Legend>| v.into_iter().map(|l| l.text).collect::<Vec<_>>();
        let why = [
            "why they wait:".to_owned(),
            "  52 uploading or encoding on the GPU (oldest 3.4 s)".to_owned(),
            "  18 one level coarse, deferred until worth its cost (oldest 12.0 s)".to_owned(),
        ];
        for legend in [
            words(explain_residency(&s, Class::Drape, 16, &x, true)),
            words(explain_under(Class::Drape, 16, &x, &stalls)),
        ] {
            assert!(legend.ends_with(&why), "{legend:#?}");
        }
        let ok = LevelCell::of(&s, Class::Drape, 12);
        assert_eq!(blocked_lines(&ok), Vec::new());
    }

    /// A healthy step has nothing to look at.
    #[test]
    fn a_healthy_budget_needs_no_attention() {
        assert_eq!(
            attention(&fixtures::healthy(), &StallWatch::at_once()),
            Vec::new()
        );
    }

    /// The stall: imagery under its need with nothing in flight or queued
    /// is the first, red, item; the drapes held at level 21 that nothing
    /// draws follow as waste.
    #[test]
    fn a_stall_is_the_first_red_item() {
        let list = attention(&fixtures::stalled(), &StallWatch::at_once());
        let first = list.first().expect("an issue");
        assert_eq!(first.severity, Severity::Alert);
        assert_eq!(
            first.text,
            "imagery: 209 tiles under need, nothing in flight or queued — stalled"
        );
        assert!(
            list.iter().any(|i| i.severity == Severity::Waste
                && i.text == "level 21 imagery: 1522 resident, 0 drawn, none selected"),
            "{list:?}"
        );
        // One loading level elsewhere: no longer a stall of the class, but
        // the levels with nothing of their own loading still are.
        let mut s = fixtures::stalled();
        s.levels.in_flight[Class::Drape][14] = [2, 0];
        let list = attention(&s, &StallWatch::at_once());
        assert!(list.iter().any(|i| i.severity == Severity::Warn
            && i.text.starts_with("imagery: 209 tiles under need, 2 loads")));
        assert!(list.iter().any(|i| i.severity == Severity::Alert
            && i.text == "level 15 imagery: 80 under need, nothing loading at this level"));
    }

    /// Laurent's live state: imagery level 14 drawn 94 of 96, 2 under its
    /// need, nothing in flight or queued — read off the last walk, while the
    /// budget's deficit this step is nought. One count: no alert, no
    /// warning, nothing about level 14.
    #[test]
    fn a_level_under_need_the_budget_does_not_count_is_no_alarm() {
        let mut s = fixtures::healthy();
        let d = Class::Drape;
        s.levels_seen[d] |= 1 << 14;
        s.levels.gpu_tiles[d][14] = 120;
        s.levels.drawn[d][14] = 94;
        s.levels.selected[d][14] = 96;
        s.levels.under_need[d][14] = 2;
        s.under_need_tiles = [0, 0];
        let mut watch = StallWatch::default();
        watch.note(&s, 0.0);
        watch.note(&s, 30.0);
        let list = attention(&s, &watch);
        assert!(list.is_empty(), "{list:?}");
        let cell = LevelCell::of(&s, d, 14);
        assert_eq!(level_severity(&cell, watch.level_long(d, 14)), Severity::Ok);
        assert_eq!(cell.under, 0, "the deficit by level: none");
    }

    /// A stall is an alert only once it has lasted the delay: before, it
    /// is a warning that it waits.
    #[test]
    fn a_stall_alerts_only_after_its_delay() {
        let s = fixtures::stalled();
        let mut watch = StallWatch::with_delay(1.0);
        watch.note(&s, 10.0);
        let early = attention(&s, &watch);
        assert!(
            early.iter().all(|i| i.severity != Severity::Alert),
            "a transient painted red: {early:?}"
        );
        assert!(
            early.iter().any(|i| i.text.contains("waiting")),
            "{early:?}"
        );
        let cell = LevelCell::of(&s, Class::Drape, 15);
        assert_eq!(
            level_severity(&cell, watch.level_long(Class::Drape, 15)),
            Severity::Warn
        );
        watch.note(&s, 11.2);
        let late = attention(&s, &watch);
        assert_eq!(
            late.first().map(|i| (i.severity, i.text.as_str())),
            Some((
                Severity::Alert,
                "imagery: 209 tiles under need, nothing in flight or queued — stalled"
            ))
        );
        assert_eq!(
            level_severity(&cell, watch.level_long(Class::Drape, 15)),
            Severity::Alert
        );
        // Loads start: the stall's clock starts again.
        let mut moving = s.clone();
        moving.levels.in_flight[Class::Drape][15] = [1, 0];
        watch.note(&moving, 11.5);
        watch.note(&s, 11.6);
        assert!(attention(&s, &watch)
            .iter()
            .all(|i| !i.text.ends_with("stalled")));
    }

    /// The disk tier: the store's tiles by level beside RAM and GPU, its
    /// numbers written; with no store, said not known.
    #[test]
    fn the_tiers_are_drawn_with_their_numbers() {
        let mut s = fixtures::healthy();
        s.disk.classes[Class::Drape][12] = [345, 9 * 1024 * 1024];
        let draw_texts = |s: &BudgetSignals| {
            let mut cache = TextCache::default();
            let view = BoardView {
                cumulative: false,
                from: None,
                layers: &[],
                decide: None,
                stalls: &StallWatch::default(),
                width: None,
                issues_only: false,
            };
            draw(Some(s), &view, &mut cache, 1.0);
            cache.masks.into_keys().map(|(t, _)| t).collect::<Vec<_>>()
        };
        let texts = draw_texts(&s);
        assert!(texts.iter().any(|t| t == "345 · 9.0 MiB"), "{texts:?}");
        s.disk.known = false;
        let texts = draw_texts(&s);
        assert!(
            texts.iter().any(|t| t.starts_with("disk: not known here")),
            "{texts:?}"
        );
    }

    /// Ground missing, a class over its limit, pressure and failures are
    /// alerts.
    #[test]
    fn missing_ground_overruns_and_failures_are_alerts() {
        let mut s = fixtures::healthy();
        s.lost_tiles = 3;
        s.bytes[Class::Drape] = [2600 * MIB, 0];
        s.pressure_level = 2;
        s.layers.failures[0] = 4;
        let list = attention(&s, &StallWatch::at_once());
        for text in [
            "ground: 3 tiles lost, 0 bare — ground missing",
            "imagery: 2600.0 MiB over its 2519.0 MiB hard limit",
            "memory pressure: critical",
            "layer 0: 4 loads failed this step",
        ] {
            assert!(
                list.iter()
                    .any(|i| i.severity == Severity::Alert && i.text == text),
                "{text}: {list:?}"
            );
        }
    }

    /// The decision's time: mean and maximum over the same publications,
    /// the first only setting where the sums start — so a session whose
    /// early steps were slow does not show a mean over its maximum.
    #[test]
    fn the_decide_time_covers_the_same_steps_for_its_mean_and_its_max() {
        let mut h = DecideHistory::default();
        let mut s = BudgetSignals::default();
        // A slow start: 1000 steps of 4 ms, the peak of the last half
        // second 2 ms — the window #278 showed, max under mean.
        s.decide_micros.count = 1000;
        s.decide_micros.sum = 4_000_000;
        s.decide_peak_micros = 2_000;
        h.note(&s);
        assert_eq!(h.summary(), None, "one publication: nothing to say");
        s.decide_micros.count = 1030;
        s.decide_micros.sum = 4_000_000 + 30 * 700;
        s.decide_peak_micros = 1_500;
        h.note(&s);
        h.note(&s);
        let d = h.summary().expect("two publications");
        assert_eq!(d.steps, 30);
        assert!((d.mean_ms - 0.7).abs() < 1e-9, "{d:?}");
        assert!((d.max_ms - 1.5).abs() < 1e-9);
        assert!(d.max_ms >= d.mean_ms);
    }

    /// The board draws every number of the tables: the healthy step's
    /// level 12 terrain (156 copies, 117 of 117 drawn) is written on it.
    #[test]
    fn the_board_writes_the_tables_numbers() {
        let s = fixtures::healthy();
        let mut cache = TextCache::default();
        let view = BoardView {
            cumulative: false,
            from: None,
            layers: &["Aerial".to_owned()],
            decide: None,
            stalls: &StallWatch::default(),
            width: None,
            issues_only: false,
        };
        let d = draw(Some(&s), &view, &mut cache, 1.0);
        let texts: Vec<&str> = cache.masks.keys().map(|(t, _)| t.as_str()).collect();
        for t in [
            "156",
            "117/117",
            "0 base: Aerial",
            "nothing needs attention",
            "3/2",
        ] {
            assert!(texts.contains(&t), "{t} not drawn: {texts:?}");
        }
        assert!(d.size.0 > 900 && d.size.1 > 300, "{:?}", d.size);
        // And twice as large at twice the density.
        let d2 = draw(Some(&s), &view, &mut cache, 2.0);
        assert!(
            d2.size.0 * 10 >= d.size.0 * 19,
            "{:?} at 2x, {:?} at 1x",
            d2.size,
            d.size
        );
    }

    /// The scores the budget publishes reach the box plots and the waste:
    /// the held copies under λ are the waste, by the held histogram's share.
    #[test]
    fn level_scores_and_waste_read_the_published_scores() {
        use tuile_core::budget::score::{HeldReport, Side, Terms};
        let mut s = BudgetSignals::default();
        assert!(level_scores(&s, Class::Drape, 12).is_none());
        let mut held = HeldReport::default();
        for score in [10u64, 10, 10, 1_000] {
            held.board.put(
                12,
                1,
                Side::Held,
                &Terms {
                    score,
                    ..Terms::default()
                },
            );
        }
        s.score_held = Some(held);
        s.score_lambda = Some(100);
        s.bytes_by_level[Class::Drape][12] = [4_000, 0];
        let x = level_scores(&s, Class::Drape, 12).expect("scores");
        assert_eq!(x.lambda, Some(100.0));
        assert_eq!(x.held, Some([8.0, 8.0, 512.0]));
        let w = level_waste(&s, Class::Drape, 12);
        assert!(w.scored);
        assert_eq!(w.under, [3_000, 0], "three of four held under λ");
        assert_eq!(w.over, [1_000, 0]);
    }

    /// The waits reach the board: first loads and reloads, their rates and
    /// percentiles, and nothing at all before a wait ended.
    #[test]
    fn the_board_shows_the_waits_split_by_first_load_and_reload() {
        let mut s = fixtures::healthy();
        let mut cache = TextCache::default();
        let view = BoardView {
            cumulative: false,
            from: None,
            layers: &["Aerial".to_owned()],
            decide: None,
            stalls: &StallWatch::default(),
            width: None,
            issues_only: false,
        };
        draw(Some(&s), &view, &mut cache, 1.0);
        assert!(
            !cache.masks.keys().any(|(t, _)| t == "Waits"),
            "no wait yet, no section"
        );
        s.waits.millis[0][8] = 9; // nine first loads of 128–255 ms
        s.waits.millis[0][10] = 1;
        s.waits.millis[1][6] = 2; // two reloads of 32–63 ms, evicted < 5 s before
        s.waits_recent = [5, 2, 0];
        s.waits_recent_micros = 1_000_000;
        draw(Some(&s), &view, &mut cache, 1.0);
        let texts: Vec<&str> = cache.masks.keys().map(|(t, _)| t.as_str()).collect();
        for t in [
            "Waits",
            "first loads: 5.0/s · p50 128 ms · p95 512 ms · 10 since the start",
            "reloads < 5 s: 2.0/s · p50 32 ms · p95 32 ms · 2 since the start",
            "reloads < 30 s: 0.0/s · p50 · · p95 · · 0 since the start",
        ] {
            assert!(texts.contains(&t), "{t} not drawn: {texts:?}");
        }
    }
}
