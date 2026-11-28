// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The budget window: the graphics budget, per level and per imagery layer,
//! in a window of its own — an operating-system window on a desktop, a
//! browser window opened by the page, a texture of its own in a headless
//! run (`budget_screen`) — and in a panel over the view on a phone, which
//! has no second window. The same panel everywhere, laid out here and drawn
//! by the overlay as the credits bar is.
//!
//! It reads the budget's published signals (`tuile_core::metrics::budget`,
//! a [`BudgetSignals`]) and nothing else: the numbers the telemetry exports,
//! never a second computation of them. Two tabs: the **board**, drawn —
//! gauges, bars and colours for what deserves a look, every number written
//! beside its shape (`tuile_ui::budget_board`) — and the **tables**, the
//! same numbers as text (`window=table`; `window=graph` back). Two views
//! in each:
//!
//! - **instant**: the last step — what is held, drawn, selected, under its
//!   need, in flight and queued, by level and class, and the layers' drapes;
//! - **cumulative**: the counters summed since the session started, or since
//!   the window was told to sum from now (`window=reset`) — loads started,
//!   completed and wasted, evictions and releases by level, the layers'
//!   requests by tier, completions and failures, bytes uploaded.
//!
//! The header has the totals against the targets and limits, the pressure,
//! the LOD scale, the specification in force, the time and work a decision
//! takes, and the ground the budget could not cover (lost and bare tiles).

use tuile_core::budget::{BudgetSignals, Class, LAYERS, LEVELS};
use web_time::Instant;

use crate::steer::WindowAsk;

/// How often the panel is laid out again while shown: the numbers change
/// every step, a person reads a few times a second.
const EVERY_MS: u128 = 250;

/// The classes the table has columns for, with their short names.
const CLASSES: [(Class, &str); 3] = [
    (Class::TerrainMesh, "T"),
    (Class::Drape, "D"),
    (Class::Tiles3dMesh, "3D"),
];

/// The window's state: shown, which view, from when the cumulative view
/// sums, and the panel as last laid out.
#[derive(Default)]
pub(super) struct BudgetWindow {
    shown: bool,
    cumulative: bool,
    /// The signals the cumulative view subtracts, and when they were taken;
    /// none: since the session started.
    from: Option<(BudgetSignals, Instant)>,
    /// A reset asked for, taken on the next signals.
    reset: bool,
    /// Drawn in a window of its own rather than over the view: see
    /// [`Self::own_window`].
    own_window: bool,
    /// The tables rather than the board.
    tables: bool,
    /// The decision's time over the last publications, for the board.
    decide: tuile_ui::budget_board::DecideHistory,
    /// Since when each class and level has been stalled, for the board's
    /// alerts: one is an alert once it has lasted the delay (1 s, or
    /// `TUILE_BUDGET_STALL_MS`).
    stalls: Option<tuile_ui::budget_board::StallWatch>,
    /// The origin of the stalls' clock.
    started: Option<Instant>,
    /// The board's texts, kept from one drawing to the next.
    texts: tuile_ui::budget_board::TextCache,
    layout: Option<tuile_ui::Drawing>,
    /// Its scroll, the pointer over it and the keyboard's focus: the
    /// legend shown (`tuile_ui::budget_board::BoardControl`).
    control: tuile_ui::budget_board::BoardControl,
    /// Only the levels with an issue (`window_levels=issues`).
    issues_only: bool,
    laid: Option<(Instant, (u32, u32), u64)>,
    /// The prediction section's rows, laid out under the budget's
    /// (`predicting::prediction_rows`).
    prediction: Vec<Vec<String>>,
    /// The preload reservoir rather than the board: the tiles the budget
    /// holds around and under the eye (`tuile_ui::reservoir`).
    reservoir: bool,
    /// Its level; `None`: the level most tiles are drawn at.
    reservoir_level: Option<u32>,
    /// The camera, for the reservoir's plan.
    eye: Option<tuile_ui::reservoir::ReservoirEye>,
    /// The tile a click picked in the reservoir: outlined in the view.
    selected: Option<(u32, u64, u64)>,
}

impl BudgetWindow {
    pub(super) fn ask(&mut self, ask: WindowAsk) {
        match ask {
            WindowAsk::Show(on) => self.shown = on,
            WindowAsk::Reservoir(on) => {
                self.shown = true;
                self.reservoir = on;
                if on {
                    self.tables = false;
                }
            }
            WindowAsk::ReservoirLevel(level) => {
                self.shown = true;
                self.reservoir = true;
                self.tables = false;
                self.reservoir_level = level;
            }
            WindowAsk::Cumulative(on) => {
                self.shown = true;
                self.cumulative = on;
            }
            WindowAsk::Reset => {
                self.shown = true;
                self.cumulative = true;
                self.reset = true;
            }
            WindowAsk::Tables(on) => {
                self.shown = true;
                self.tables = on;
                if on || !self.reservoir {
                    self.reservoir = false;
                }
            }
            WindowAsk::Levels(issues) => {
                self.shown = true;
                self.issues_only = issues;
            }
        }
        // Laid out again at once.
        self.laid = None;
        if !self.shown {
            self.layout = None;
        }
        // The budget lists the reservoir's tiles only while a window shows
        // it: a session without the view pays nothing for it.
        tuile_core::metrics::want_reservoir(self.shown && self.reservoir);
    }

    /// The tile picked in the reservoir, if it is shown.
    pub(super) fn selected(&self) -> Option<(u32, u64, u64)> {
        (self.shown && self.reservoir).then_some(self.selected).flatten()
    }

    /// Whether the reservoir is what the window shows.
    pub(super) fn reservoir(&self) -> bool {
        self.reservoir
    }

    /// The camera the reservoir's plan is drawn about.
    pub(super) fn set_eye(&mut self, eye: Option<tuile_ui::reservoir::ReservoirEye>) {
        if self.shown && self.reservoir {
            self.eye = eye;
        }
    }

    pub(super) fn shown(&self) -> bool {
        self.shown
    }

    /// Drawn in a window of its own — whatever the shell's is — instead of
    /// over the view. Set once, by the shell, before anything is shown.
    pub(super) fn set_own_window(&mut self, own: bool) {
        self.own_window = own;
        self.laid = None;
    }

    /// Whether the panel wants a window of its own now: shown, on a shell
    /// that gives it one.
    pub(super) fn wants_own_window(&self) -> bool {
        self.shown && self.own_window
    }

    /// Its own window was closed by the person, or could not be opened:
    /// the window is hidden, as `window=off` hides it, and nothing else
    /// changes — the view, its session and the budget go on as they were.
    pub(super) fn closed(&mut self) {
        self.ask(WindowAsk::Show(false));
    }

    /// Laid out again on the next call, whenever that is.
    pub(super) fn again(&mut self) {
        self.laid = None;
    }

    /// The panel as laid out for its own window, if it has one and is
    /// shown.
    pub(super) fn own_layout(&self) -> Option<&tuile_ui::Drawing> {
        self.layout
            .as_ref()
            .filter(|_| self.shown && self.own_window)
    }

    pub(super) fn cumulative(&self) -> bool {
        self.cumulative
    }

    /// Whether only the levels with an issue are listed.
    pub(super) fn issues_only(&self) -> bool {
        self.issues_only
    }

    /// What a person did in its own window: scrolled, pointed, pressed a
    /// key. Says whether it asked the window something the menu shows.
    pub(super) fn input(&mut self, input: tuile_ui::budget_board::BoardInput) -> bool {
        use tuile_ui::budget_board::BoardKey as K;
        self.laid = None;
        let key = self.control.input(input);
        self.selected = self.control.picked();
        let asked = match key {
            Some(K::Tables) => WindowAsk::Tables(!self.tables),
            Some(K::Cumulative) => WindowAsk::Cumulative(!self.cumulative),
            Some(K::Reset) => WindowAsk::Reset,
            Some(K::Issues) => WindowAsk::Levels(!self.issues_only),
            Some(K::Reservoir) => WindowAsk::Reservoir(!self.reservoir),
            Some(K::LevelAuto) => WindowAsk::ReservoirLevel(None),
            Some(k @ (K::LevelUp | K::LevelDown)) if self.reservoir => {
                let at = self
                    .reservoir_level
                    .or_else(|| {
                        tuile_core::metrics::budget()
                            .and_then(|s| s.score_held)
                            .and_then(|h| tuile_ui::reservoir::auto_level(&h, self.eye.as_ref().map(|e| (e.lon, e.lat))))
                    })
                    .unwrap_or(10);
                WindowAsk::ReservoirLevel(Some(if k == K::LevelUp {
                    (at + 1).min(24)
                } else {
                    at.saturating_sub(1)
                }))
            }
            _ => return false,
        };
        self.ask(asked);
        true
    }

    /// Whether the tables are shown rather than the board.
    pub(super) fn tables(&self) -> bool {
        self.tables
    }

    /// The panel as drawn over the view, if shown there: never when it has
    /// a window of its own.
    pub(super) fn layout(&self) -> Option<&tuile_ui::Drawing> {
        self.layout
            .as_ref()
            .filter(|_| self.shown && !self.own_window)
    }

    /// The panel's own size, device pixels, for `signals` at `scale`: what
    /// a window of its own is opened at.
    pub(super) fn natural_size(
        &self,
        signals: Option<&BudgetSignals>,
        scale: f64,
        layers: &[String],
    ) -> (u32, u32) {
        let now = Instant::now();
        if !self.tables {
            let mut texts = tuile_ui::budget_board::TextCache::default();
            return self
                .board(signals, now, layers, &mut texts, scale, None)
                .size;
        }
        let rows = self.rows_for(signals, now, layers);
        tuile_ui::Panel::default()
            .layout(
                tuile_ui::Font::bundled(),
                &rows,
                (0.0, 0.0),
                (16_384, 16_384),
                scale,
            )
            .size
    }

    /// The board for `signals` at `now`, at its own size, its top-left
    /// corner at the origin.
    fn board(
        &self,
        signals: Option<&BudgetSignals>,
        now: Instant,
        layers: &[String],
        texts: &mut tuile_ui::budget_board::TextCache,
        scale: f64,
        width: Option<f64>,
    ) -> tuile_ui::Drawing {
        let view = tuile_ui::budget_board::BoardView {
            cumulative: self.cumulative,
            from: self
                .cumulative
                .then(|| {
                    self.from
                        .as_ref()
                        .map(|(f, when)| (f, now.duration_since(*when).as_secs_f64()))
                })
                .flatten(),
            layers,
            decide: self.decide.summary(),
            stalls: self.stalls.as_ref().unwrap_or(&NO_STALLS),
            width,
            issues_only: self.issues_only,
        };
        if self.reservoir {
            return tuile_ui::reservoir::draw(
                signals.and_then(|s| s.score_held.as_ref()),
                self.eye.as_ref(),
                self.reservoir_level,
                self.selected,
                texts,
                scale,
            );
        }
        tuile_ui::budget_board::draw(signals, &view, texts, scale)
    }

    /// Lays the panel out from `signals`, at most every [`EVERY_MS`], in a
    /// frame of `frame` device pixels whose safe area starts at `at`. Says
    /// whether it was laid out again.
    pub(super) fn lay_out(
        &mut self,
        signals: Option<&BudgetSignals>,
        frame: (u32, u32),
        at: (f64, f64),
        scale: f64,
        layers: &[String],
    ) -> bool {
        if !self.shown {
            return false;
        }
        let now = Instant::now();
        let step = signals.map_or(0, |s| s.decide_micros.count);
        if let Some((when, size, _)) = self.laid {
            if size == frame && now.duration_since(when).as_millis() < EVERY_MS {
                return false;
            }
        }
        if std::mem::take(&mut self.reset) {
            self.from = signals.map(|s| (s.clone(), now));
        }
        if let Some(s) = signals {
            self.decide.note(s);
            let since = *self.started.get_or_insert(now);
            self.stalls
                .get_or_insert_with(stall_watch)
                .note(s, now.duration_since(since).as_secs_f64());
        }
        // A window someone sizes: the board laid out for its width, and
        // scrolled in it. One nobody sizes (a headless run's texture) is
        // the board's own size.
        let sized = frame.0 < 16_384;
        let width = sized.then(|| f64::from(frame.0) / scale.max(1.0) - at.0 / scale.max(1.0));
        let mut texts = std::mem::take(&mut self.texts);
        let drawing: tuile_ui::Drawing = if self.tables {
            let mut rows = self.rows_for(signals, now, layers);
            // The prediction section, under the budget's tables.
            rows.extend(self.prediction.iter().cloned());
            // The table's own height: scrolled like the board.
            let frame_for = if self.own_window {
                (frame.0, 16_384)
            } else {
                frame
            };
            tuile_ui::Panel::default()
                .layout(tuile_ui::Font::bundled(), &rows, at, frame_for, scale)
                .into()
        } else {
            moved(
                self.board(signals, now, layers, &mut texts, scale, width),
                at,
            )
        };
        self.layout = Some(if self.own_window && sized {
            self.control.present(drawing, frame, scale, &mut texts)
        } else {
            drawing
        });
        self.texts = texts;
        self.laid = Some((now, frame, step));
        true
    }

    /// The prediction section, laid out with the next panel.
    pub(super) fn set_prediction(&mut self, rows: Vec<Vec<String>>) {
        self.prediction = rows;
    }

    /// The rows for `signals`, in the view asked for, at `now`.
    fn rows_for(
        &self,
        signals: Option<&BudgetSignals>,
        now: Instant,
        layers: &[String],
    ) -> Vec<Vec<String>> {
        match signals {
            None => vec![vec!["Budget: no signals yet".to_owned()]],
            Some(s) => rows(
                s,
                self.cumulative
                    .then(|| {
                        self.from
                            .as_ref()
                            .map(|(f, when)| (f, now.duration_since(*when).as_secs_f64()))
                    })
                    .flatten(),
                self.cumulative,
                layers,
            ),
        }
    }
}

/// A watch before any signals: nothing stalled yet.
static NO_STALLS: std::sync::LazyLock<tuile_ui::budget_board::StallWatch> =
    std::sync::LazyLock::new(stall_watch);

/// The stalls' watch, its delay from `TUILE_BUDGET_STALL_MS` when set (a
/// tuning, not a switch), [`tuile_ui::budget_board::STALL_DELAY`]
/// otherwise.
fn stall_watch() -> tuile_ui::budget_board::StallWatch {
    let delay = std::env::var("TUILE_BUDGET_STALL_MS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .map_or(tuile_ui::budget_board::STALL_DELAY, |ms| ms / 1000.0);
    tuile_ui::budget_board::StallWatch::with_delay(delay)
}

/// `d` with its top-left corner at `at`, device pixels.
fn moved(mut d: tuile_ui::Drawing, at: (f64, f64)) -> tuile_ui::Drawing {
    if at == (0.0, 0.0) {
        return d;
    }
    let (x, y) = (at.0.round() as f32, at.1.round() as f32);
    let top = &mut d.on_top;
    for v in d.mesh.iter_mut().chain(&mut top.mesh) {
        v.position[0] += x;
        v.position[1] += y;
    }
    for l in d.labels.iter_mut().chain(&mut top.labels) {
        l.position[0] += x;
        l.position[1] += y;
    }
    d
}

fn mib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0))
}

/// A count, or a dot for nought: a table of zeros reads as a wall.
fn n(v: u64) -> String {
    if v == 0 {
        "·".to_owned()
    } else {
        v.to_string()
    }
}

fn pair(a: u64, b: u64) -> String {
    if a == 0 && b == 0 {
        "·".to_owned()
    } else {
        format!("{a}/{b}")
    }
}

/// The panel's rows for `s`: the instant view, or the cumulative one —
/// summed since `from` (the signals then, and how many seconds ago) or
/// since the session started. `layers` names the imagery layers by their
/// index, on the device only.
pub(super) fn rows(
    s: &BudgetSignals,
    from: Option<(&BudgetSignals, f64)>,
    cumulative: bool,
    layers: &[String],
) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let line = |t: String| vec![t];
    let since = |cur: u64, old: fn(&BudgetSignals) -> u64| -> u64 {
        cur.saturating_sub(from.map_or(0, |(f, _)| old(f)))
    };
    out.push(line(match (cumulative, from) {
        (false, _) => "Budget window — instant (the last step)".to_owned(),
        (true, None) => "Budget window — cumulative, since the session started".to_owned(),
        (true, Some((_, secs))) => format!("Budget window — cumulative, the last {secs:.0} s"),
    }));
    // The decision: its time and work.
    let d = &s.decide_micros;
    let (sum, count) = match from {
        Some((f, _)) => (
            d.sum.saturating_sub(f.decide_micros.sum),
            d.count.saturating_sub(f.decide_micros.count),
        ),
        None => (d.sum, d.count),
    };
    let work = since(s.decide_work_total, |f| f.decide_work_total);
    out.push(line(format!(
        "spec {}{} · SSE scale {:.2} · pressure {} · thermal {} · decide mean {:.2} ms over {}, peak {:.2} ms over the last publish (0.5 s) · work {} this step, {:.0} mean · {} steps",
        s.spec.label(),
        if s.spec_overridden { " (override)" } else { " (context)" },
        f64::from(s.sse_scale_milli) / 1000.0,
        ["normal", "warning", "critical", "background"]
            .get(usize::from(s.pressure_level))
            .unwrap_or(&"?"),
        s.thermal_level,
        sum as f64 / count.max(1) as f64 / 1000.0,
        if from.is_some() {
            "the window"
        } else {
            "the session"
        },
        s.decide_peak_micros as f64 / 1000.0,
        s.decide_work,
        work as f64 / count.max(1) as f64,
        count,
    )));
    // The totals against their targets and limits.
    let mut totals = Vec::new();
    for (c, name) in CLASSES {
        let [drawable, transit] = s.bytes[c];
        let hard = s.hard_limit_bytes[c];
        totals.push(format!(
            "{name} {} MiB (+{} transit) of {} target{}, {} protected",
            mib(drawable),
            mib(transit),
            mib(s.target_bytes[c]),
            if hard == u64::MAX {
                String::new()
            } else {
                format!(", {} hard", mib(hard))
            },
            mib(s.protected_bytes[c]),
        ));
    }
    out.push(line(totals.join(" · ")));
    out.push(line(format!(
        "decoded {} MiB · in flight {} · local reads {} · ground holes: {} lost, {} bare · floor {} · under need {} mesh, {} imagery",
        mib(s.decoded_bytes),
        s.in_flight.iter().map(|(_, v)| u64::from(v)).sum::<u64>(),
        s.local_reads,
        s.lost_tiles,
        s.bare_items,
        match (s.floor_in, s.floor_level) {
            (_, None) => "none".to_owned(),
            (true, Some(l)) => format!("in, levels ≤ {l}"),
            (false, Some(l)) => format!("not in yet, levels ≤ {l}"),
        },
        s.under_need_tiles[0],
        s.under_need_tiles[1],
    )));
    // The executor's lanes: what its frames deferred, by bytes and by
    // encodes, and what the encodes did.
    let enc = &s.frame_encode_micros;
    let (enc_sum, enc_count) = match from {
        Some((f, _)) => (
            enc.sum.saturating_sub(f.frame_encode_micros.sum),
            enc.count.saturating_sub(f.frame_encode_micros.count),
        ),
        None => (enc.sum, enc.count),
    };
    out.push(line(format!(
        "uploads deferred {} by bytes, {} by encodes · encoded {} tiles, {} blocks · encode GPU {:.2} ms a frame that encodes",
        since(s.upload_deferred_total[0], |f| f.upload_deferred_total[0]),
        since(s.upload_deferred_total[1], |f| f.upload_deferred_total[1]),
        since(s.encoded_tiles_total, |f| f.encoded_tiles_total),
        since(s.encode_blocks_total, |f| f.encode_blocks_total),
        enc_sum as f64 / enc_count.max(1) as f64 / 1000.0,
    )));
    // What the executor holds outside any resource, off the drapes' limits:
    // the encoder's scratch while imagery is compressed, nothing once off.
    out.push(line(format!(
        "encoder scratch reserved {} MiB off the drapes",
        mib(s.reserved_bytes[Class::Drape]),
    )));
    if cumulative {
        let up: u64 = s.upload_bytes_total.iter().map(|(_, v)| v).sum();
        let up0: u64 = from.map_or(0, |(f, _)| {
            f.upload_bytes_total.iter().map(|(_, v)| v).sum()
        });
        out.push(line(format!(
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
        )));
    }
    // The classes with something to show; terrain and drapes always.
    let classes: Vec<(Class, &str)> = CLASSES
        .into_iter()
        .filter(|(c, _)| {
            *c != Class::Tiles3dMesh || s.levels_seen[*c] != 0 || s.bytes_levels[*c] != 0
        })
        .collect();
    let mut head = vec!["level".to_owned()];
    for (_, name) in &classes {
        let cols: &[&str] = if cumulative {
            &["started", "done", "wasted", "evicted", "released"]
        } else {
            &[
                "gpu",
                "MiB",
                "dec",
                "drawn/sel",
                "under",
                "fly n/p",
                "queue n/p",
            ]
        };
        head.extend(cols.iter().map(|c| format!("{name} {c}")));
    }
    out.push(Vec::new());
    out.push(head);
    let seen = classes
        .iter()
        .fold(0u32, |a, (c, _)| a | s.levels_seen[*c] | s.bytes_levels[*c]);
    let v = &s.levels;
    for l in (0..LEVELS).filter(|l| seen & (1 << l) != 0) {
        let floor = s.floor_level.is_some_and(|f| l as u32 <= f);
        let mut row = vec![format!("{l}{}", if floor { " F" } else { "" })];
        for (c, _) in &classes {
            let c = *c;
            if cumulative {
                let e = s.level_events_total[c][l];
                let e0 = from.map_or([0; 4], |(f, _)| f.level_events_total[c][l]);
                let ev = s.evictions_by_level_total[c][l]
                    .saturating_sub(from.map_or(0, |(f, _)| f.evictions_by_level_total[c][l]));
                row.extend([
                    n(e[0].saturating_sub(e0[0])),
                    n(e[1].saturating_sub(e0[1])),
                    n(e[2].saturating_sub(e0[2])),
                    n(ev),
                    n(e[3].saturating_sub(e0[3])),
                ]);
            } else {
                let [drawable, transit] = s.bytes_by_level[c][l];
                let bytes = drawable + transit;
                row.extend([
                    n(u64::from(v.gpu_tiles[c][l])),
                    if bytes == 0 {
                        "·".to_owned()
                    } else {
                        mib(bytes)
                    },
                    n(u64::from(v.decoded_tiles[c][l])),
                    pair(u64::from(v.drawn[c][l]), u64::from(v.selected[c][l])),
                    n(u64::from(v.under_need[c][l])),
                    pair(
                        u64::from(v.in_flight[c][l][0]),
                        u64::from(v.in_flight[c][l][1]),
                    ),
                    pair(u64::from(v.queued[c][l][0]), u64::from(v.queued[c][l][1])),
                ]);
            }
        }
        out.push(row);
    }
    // The imagery layers: by index, named here from the device's own list.
    out.push(Vec::new());
    let mut head = vec![
        "layer".to_owned(),
        "gpu".to_owned(),
        "MiB".to_owned(),
        "drawn".to_owned(),
        "in flight".to_owned(),
    ];
    head.extend(
        ["store", "network", "RAM hits", "done", "failed"]
            .iter()
            .map(|c| format!("{c}{}", if cumulative { "" } else { " (step)" })),
    );
    out.push(head);
    let y = &s.layers;
    for i in (0..LAYERS).filter(|i| s.layers_seen & (1 << i) != 0) {
        let name = match (i, layers.get(i)) {
            (0, Some(n)) => format!("0 base: {n}"),
            (0, None) => "0 base".to_owned(),
            (_, Some(n)) => format!("{i} overlay: {n}"),
            (_, None) => format!("{i} overlay"),
        };
        let mut row = vec![
            name,
            n(u64::from(y.gpu_tiles[i])),
            mib(y.gpu_bytes[i].0),
            n(u64::from(y.drawn[i])),
            n(u64::from(y.in_flight[i])),
        ];
        if cumulative {
            let t = s.layer_loads_total[i];
            let t0 = from.map_or([0; 5], |(f, _)| f.layer_loads_total[i]);
            row.extend((0..5).map(|k| n(t[k].saturating_sub(t0[k]))));
        } else {
            row.extend(
                [
                    y.requests[i][0],
                    y.requests[i][1],
                    y.ram_hits[i],
                    y.completions[i],
                    y.failures[i],
                ]
                .map(|v| n(u64::from(v))),
            );
        }
        out.push(row);
    }
    score_rows(s, from, cumulative, &mut out);
    // An empty row is a blank line.
    out.into_iter()
        .map(|r| if r.is_empty() { vec![String::new()] } else { r })
        .collect()
}

/// A resource as the held-tiles table names it: its tile `level/x/y` (a
/// terrain tile's address; another's id), and its kind.
fn resource_name(r: tuile_core::budget::ResourceId) -> String {
    use tuile_core::budget::ResourceId;
    let tile = |t: tuile_core::budget::TileId| {
        let (l, x, y) = t.payload().terrain_coord();
        format!("{l}/{x}/{y}")
    };
    match r {
        ResourceId::Mesh(t) => format!("{} mesh", tile(t)),
        ResourceId::Drape {
            tile: t,
            layer,
            level,
        } => {
            format!("{} drape L{} z{level}", tile(t), layer.0)
        }
    }
}

/// A probability in `CERTAIN` units, as a fraction.
fn frac(v: u64) -> String {
    format!("{:.3}", v as f64 / f64::from(tuile_core::budget::CERTAIN))
}

/// The progressive score's rows
/// (`docs/explorations/budget-progressive-coarse-preload.md` §7), the
/// numbers the `tuile_budget_score_*`, `preload_by_level_distance`,
/// `evictions_by_level_distance` and `param` series carry, in the same
/// units: the formula and its parameters; preloads and evictions by level
/// and distance band; the score's terms per level and side; the GPU-held
/// copies the release order takes next, and those it keeps longest.
fn score_rows(
    s: &BudgetSignals,
    from: Option<(&BudgetSignals, f64)>,
    cumulative: bool,
    out: &mut Vec<Vec<String>>,
) {
    use tuile_core::budget::score::{Cell, Side, DISTANCE_BANDS, DISTANCE_BAND_LABELS};
    let p = &s.score_params;
    out.push(Vec::new());
    out.push(vec![format!(
        "{} · n = {}",
        p.formula(),
        p.need_level.map_or("?".to_owned(), |n| n.to_string())
    )]);
    out.push(vec![p.restore_line()]);
    // Preloads and evictions by level × distance band.
    let mut head = vec!["level × d".to_owned()];
    for b in DISTANCE_BAND_LABELS {
        head.push(format!(
            "d{b} {}",
            if cumulative { "pre/ev" } else { "fly/ev" }
        ));
    }
    out.push(Vec::new());
    out.push(head);
    let cell = |sig: &BudgetSignals, l: usize, b: usize, side: Side| -> u64 {
        u64::from(sig.score_total.cell(l, b, side).count)
    };
    for l in 0..LEVELS {
        let mut row = vec![l.to_string()];
        let mut any = false;
        for b in 0..DISTANCE_BANDS {
            let (pre, ev) = if cumulative {
                let base = |side| from.map_or(0, |(f, _)| cell(f, l, b, side));
                (
                    cell(s, l, b, Side::Preload).saturating_sub(base(Side::Preload)),
                    cell(s, l, b, Side::Evict).saturating_sub(base(Side::Evict)),
                )
            } else {
                (
                    u64::from(s.preload_by_level_distance[l][b]),
                    u64::from(s.evictions_by_level_distance[l][b]),
                )
            };
            any |= pre != 0 || ev != 0;
            row.push(pair(pre, ev));
        }
        if any {
            out.push(row);
        }
    }
    // Releases, GPU and decoded, by reason and level: this step's, or
    // since the start in the cumulative view.
    {
        use tuile_core::budget::score::RELEASE_REASONS;
        let r = &s.released_by_level_distance;
        let totals: Vec<String> = RELEASE_REASONS
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let n = if cumulative {
                    s.released_total[i].saturating_sub(from.map_or(0, |(f, _)| f.released_total[i]))
                } else {
                    r[i].iter().flatten().map(|v| u64::from(*v)).sum()
                };
                format!("{name} {n}")
            })
            .collect();
        out.push(Vec::new());
        out.push(vec![format!(
            "released {}: {} · kept under target − {:.1} %",
            if cumulative {
                "since the start"
            } else {
                "this step"
            },
            totals.join(", "),
            f64::from(p.headroom_permille) / 10.0
        )]);
        if !cumulative {
            out.push(
                ["level", "headroom", "below floor value", "make room"]
                    .map(str::to_owned)
                    .to_vec(),
            );
            for l in 0..LEVELS {
                let row: Vec<u64> = r
                    .iter()
                    .map(|reason| reason[l].iter().map(|v| u64::from(*v)).sum())
                    .collect();
                if row.iter().any(|v| *v > 0) {
                    let mut cells = vec![format!("{l} rel")];
                    cells.extend(row.into_iter().map(n));
                    out.push(cells);
                }
            }
        }
    }
    // The terms per level and side: count, then the score's mean and range,
    // and the means of A, D, R and the bytes.
    out.push(Vec::new());
    out.push(
        [
            "level",
            "side",
            "count",
            "score mean",
            "score min",
            "score max",
            "A",
            "D",
            "R µs",
            "MiB",
        ]
        .map(str::to_owned)
        .to_vec(),
    );
    let mean = |a: u64, n: u32| if n == 0 { 0 } else { a / u64::from(n) };
    for l in 0..LEVELS {
        for side in Side::ALL {
            let mut c = Cell::default();
            match side {
                Side::Held => {
                    if let Some(h) = &s.score_held {
                        for b in 0..DISTANCE_BANDS {
                            c.add(h.board.cell(l, b, side));
                        }
                    }
                }
                _ => {
                    let board = if cumulative {
                        &s.score_total
                    } else {
                        &s.score_recent
                    };
                    for b in 0..DISTANCE_BANDS {
                        c.add(board.cell(l, b, side));
                    }
                }
            }
            if c.count == 0 {
                continue;
            }
            out.push(vec![
                l.to_string(),
                side.label().to_owned(),
                c.count.to_string(),
                mean(c.score.sum, c.count).to_string(),
                c.score.min.to_string(),
                c.score.max.to_string(),
                frac(mean(c.a.sum, c.count)),
                frac(mean(c.d.sum, c.count)),
                mean(c.r_us.sum, c.count).to_string(),
                mib(mean(c.bytes.sum, c.count)),
            ]);
        }
    }
    // The held copies: the next to go, and the last.
    if let Some(h) = &s.score_held {
        for (title, list) in [
            ("held: next released (lowest score)", &h.lowest),
            ("held: kept longest (highest score)", &h.highest),
        ] {
            out.push(Vec::new());
            out.push(vec![format!("{title}, step {}", h.step)]);
            out.push(
                [
                    "tile", "level", "d", "A", "D", "R µs", "MiB", "score", "state",
                ]
                .map(str::to_owned)
                .to_vec(),
            );
            for t in list.iter().take(12) {
                out.push(vec![
                    resource_name(t.resource),
                    t.level.to_string(),
                    DISTANCE_BAND_LABELS[usize::from(t.band).min(DISTANCE_BANDS - 1)].to_owned(),
                    frac(u64::from(t.terms.a)),
                    frac(u64::from(t.terms.d)),
                    t.terms.r_us.to_string(),
                    mib(t.terms.bytes),
                    t.terms.score.to_string(),
                    t.state.label().to_owned(),
                ]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals() -> BudgetSignals {
        let mut s = BudgetSignals::default();
        s.levels_seen[Class::TerrainMesh] = 1 << 12;
        s.levels_seen[Class::Drape] = 1 << 13;
        s.levels.gpu_tiles[Class::TerrainMesh][12] = 7;
        s.levels.drawn[Class::TerrainMesh][12] = 6;
        s.levels.selected[Class::TerrainMesh][12] = 9;
        s.levels.in_flight[Class::Drape][13] = [2, 1];
        s.bytes_by_level[Class::TerrainMesh][12] = [3 * 1024 * 1024, 0];
        s.level_events_total[Class::TerrainMesh][12] = [40, 35, 5, 2];
        s.layers_seen = 1;
        s.layers.gpu_tiles[0] = 11;
        s.layer_loads_total[0] = [3, 8, 4, 10, 1];
        s.decide_micros.sum = 3_000;
        s.decide_micros.count = 10;
        s.floor_level = Some(2);
        s
    }

    fn row<'a>(rows: &'a [Vec<String>], first: &str) -> &'a [String] {
        rows.iter()
            .find(|r| r.first().is_some_and(|c| c == first))
            .unwrap_or_else(|| panic!("no row {first}: {rows:?}"))
    }

    /// The instant view reads the step's numbers off the signals, at their
    /// level and class; the layers by index, named from the device's list.
    #[test]
    fn the_instant_view_is_the_signals_of_the_step() {
        let s = signals();
        let rows = rows(&s, None, false, &["Aerial".to_owned()]);
        let t12 = row(&rows, "12");
        assert_eq!(&t12[1..5], ["7", "3.0", "·", "6/9"]);
        let d13 = row(&rows, "13");
        // Terrain's seven columns, then the drape's: fly n/p is the sixth.
        assert_eq!(d13[1 + 7 + 5], "2/1");
        let layer = row(&rows, "0 base: Aerial");
        assert_eq!(layer[1], "11");
        assert!(rows[1][0].contains("decide mean 0.30 ms"), "{:?}", rows[1]);
    }

    /// The cumulative view is the counters since its start, or since it
    /// was reset: the signals then subtracted.
    #[test]
    fn the_cumulative_view_sums_from_its_start() {
        let s = signals();
        let rows_all = rows(&s, None, true, &[]);
        assert_eq!(&row(&rows_all, "12")[1..6], ["40", "35", "5", "·", "2"]);
        assert_eq!(&row(&rows_all, "0 base")[5..10], ["3", "8", "4", "10", "1"]);
        let mut then = s.clone();
        then.level_events_total[Class::TerrainMesh][12] = [30, 30, 5, 0];
        then.layer_loads_total[0] = [3, 2, 4, 4, 1];
        let rows_reset = rows(&s, Some((&then, 12.0)), true, &[]);
        assert!(rows_reset[0][0].contains("the last 12 s"));
        assert_eq!(&row(&rows_reset, "12")[1..6], ["10", "5", "·", "·", "2"]);
        assert_eq!(
            &row(&rows_reset, "0 base")[5..10],
            ["·", "6", "·", "6", "·"]
        );
    }

    /// The reservation in force is on the window: the encoder's scratch
    /// while imagery is compressed, nought once it is switched off — the
    /// budget's own number, so a switch is seen where its cost is.
    #[test]
    fn the_window_shows_the_reservation_in_force() {
        let mut s = signals();
        s.reserved_bytes[Class::Drape] = 3 << 20;
        let shown = |s: &BudgetSignals| {
            rows(s, None, false, &[])
                .into_iter()
                .filter_map(|r| r.into_iter().next())
                .find(|l| l.starts_with("encoder scratch"))
                .expect("the line")
        };
        assert_eq!(shown(&s), "encoder scratch reserved 3.0 MiB off the drapes");
        s.reserved_bytes[Class::Drape] = 0;
        assert_eq!(shown(&s), "encoder scratch reserved 0.0 MiB off the drapes");
    }

    /// The executor's lanes: deferrals by bytes and by encodes, apart, and
    /// what the encodes did — since the reset in the cumulative view.
    #[test]
    fn the_lanes_line_counts_bytes_and_encodes_apart() {
        let mut s = signals();
        s.upload_deferred_total = [7, 12];
        s.encoded_tiles_total = 40;
        s.encode_blocks_total = 40 * 5_463;
        s.frame_encode_micros.record(9_500);
        s.frame_encode_micros.record(10_500);
        let lanes = |rows: &[Vec<String>]| {
            rows.iter()
                .find(|r| r[0].starts_with("uploads deferred"))
                .map(|r| r[0].clone())
                .unwrap_or_default()
        };
        assert_eq!(
            lanes(&rows(&s, None, false, &[])),
            "uploads deferred 7 by bytes, 12 by encodes · encoded 40 tiles, 218520 blocks · encode GPU 10.00 ms a frame that encodes"
        );
        let mut then = s.clone();
        then.upload_deferred_total = [7, 2];
        then.encoded_tiles_total = 39;
        then.encode_blocks_total = 39 * 5_463;
        then.frame_encode_micros = Default::default();
        then.frame_encode_micros.record(9_500);
        assert_eq!(
            lanes(&rows(&s, Some((&then, 5.0)), true, &[])),
            "uploads deferred 0 by bytes, 10 by encodes · encoded 1 tiles, 5463 blocks · encode GPU 10.50 ms a frame that encodes"
        );
    }

    /// The floor's levels are marked.
    #[test]
    fn the_floor_levels_are_marked() {
        let mut s = signals();
        s.levels_seen[Class::TerrainMesh] |= 1 << 2;
        let rows = rows(&s, None, false, &[]);
        assert!(rows.iter().any(|r| r[0] == "2 F"), "{rows:?}");
    }

    /// Shown by its toggle or by asking for a view; a reset shows the
    /// cumulative view.
    #[test]
    fn the_window_follows_what_it_is_asked() {
        let mut w = BudgetWindow::default();
        assert!(!w.shown());
        w.ask(WindowAsk::Show(true));
        assert!(w.shown() && !w.cumulative());
        w.ask(WindowAsk::Reset);
        assert!(w.shown() && w.cumulative());
        w.ask(WindowAsk::Show(false));
        assert!(!w.shown() && w.layout().is_none());
    }

    /// On a shell that gives it a window of its own, the panel is laid out
    /// for that window and never drawn over the view; closing its window
    /// hides it, as `window=off` does, and asking again shows it again.
    #[test]
    fn a_panel_with_its_own_window_is_never_over_the_view() {
        let s = signals();
        let mut w = BudgetWindow::default();
        w.set_own_window(true);
        assert!(!w.wants_own_window(), "hidden: no window");
        w.ask(WindowAsk::Show(true));
        assert!(w.wants_own_window());
        assert!(w.lay_out(Some(&s), (900, 400), (0.0, 0.0), 1.0, &[]));
        assert!(w.own_layout().is_some(), "laid out for its window");
        assert!(w.layout().is_none(), "nothing over the view");
        w.closed();
        assert!(!w.shown() && !w.wants_own_window() && w.own_layout().is_none());
        w.ask(WindowAsk::Cumulative(true));
        assert!(w.wants_own_window() && w.cumulative());
    }

    /// Over the view (a phone), the panel is drawn there, and never asks
    /// for a window.
    #[test]
    fn a_panel_over_the_view_never_wants_a_window() {
        let s = signals();
        let mut w = BudgetWindow::default();
        w.ask(WindowAsk::Show(true));
        assert!(!w.wants_own_window());
        w.lay_out(Some(&s), (900, 400), (8.0, 8.0), 1.0, &[]);
        assert!(w.layout().is_some() && w.own_layout().is_none());
    }

    /// The board by default, the tables when asked, and back; asking for
    /// a tab shows the window.
    #[test]
    fn the_board_or_the_tables() {
        let s = signals();
        let mut w = BudgetWindow::default();
        w.ask(WindowAsk::Show(true));
        assert!(!w.tables());
        assert!(w.lay_out(Some(&s), (2000, 1200), (0.0, 0.0), 1.0, &[]));
        let board = w.layout().expect("drawn").clone();
        assert!(board.labels.len() > 10, "the board writes each number");
        w.ask(WindowAsk::Tables(true));
        assert!(w.tables());
        assert!(w.lay_out(Some(&s), (2000, 1200), (0.0, 0.0), 1.0, &[]));
        assert_eq!(
            w.layout().expect("drawn").labels.len(),
            1,
            "the tables: one mask"
        );
        w.ask(WindowAsk::Show(false));
        w.ask(WindowAsk::Tables(false));
        assert!(w.shown() && !w.tables());
    }

    /// A window of its own opens at the panel's size: as wide as the
    /// table, whatever the frame.
    #[test]
    fn its_own_window_opens_at_the_panels_size() {
        let s = signals();
        let w = BudgetWindow::default();
        let (width, height) = w.natural_size(Some(&s), 1.0, &[]);
        let (w2, h2) = w.natural_size(Some(&s), 2.0, &[]);
        assert!(width > 300 && height > 100, "{width}x{height}");
        assert!(w2 > width * 3 / 2 && h2 > height * 3 / 2, "denser, larger");
    }

    /// The score's rows are the signals' numbers, in the series' units:
    /// the formula with its parameters, the level × band counts, the terms
    /// per level and side, the held copies by score.
    #[test]
    fn the_score_rows_are_the_series_numbers() {
        use tuile_core::budget::score::{
            board_of, HeldReport, HeldState, HeldTile, Reason, ScoreEvent, Side, Terms,
        };
        use tuile_core::budget::{ResourceId, TileId, CERTAIN};
        let mut s = signals();
        s.score_params.a0_permille = 800;
        s.score_params.rho = 4;
        s.score_params.margin_permille = 250;
        s.score_params.floor_level = Some(3);
        s.score_params.need_level = Some(13);
        let e = ScoreEvent {
            resource: ResourceId::Mesh(TileId::from_terrain(12, 10, 20)),
            class: Class::TerrainMesh,
            level: 12,
            band: 1,
            side: Side::Evict,
            reason: Reason::Sweep,
            terms: Terms::new(
                CERTAIN * 8 / 10,
                CERTAIN / 2,
                CERTAIN * 4 / 10,
                150_000,
                256 << 10,
                256 << 10,
            ),
        };
        board_of(&[e, e], &mut s.score_total);
        board_of(&[e], &mut s.score_recent);
        s.evictions_by_level_distance[12][1] = 1;
        s.preload_by_level_distance[12][0] = 3;
        let mut held = HeldReport::default();
        held.lowest.push(HeldTile {
            resource: e.resource,
            class: e.class,
            level: 12,
            band: 1,
            terms: e.terms,
            state: HeldState::Wanted,
        });
        s.score_held = Some(held);
        let instant = rows(&s, None, false, &[]);
        assert!(
            instant.iter().any(|r| r[0].contains("a0=0.80")
                && r[0].contains("ρ=4")
                && r[0].contains("n = 13")),
            "{instant:?}"
        );
        let by_band = instant
            .iter()
            .find(|r| r.len() == 6 && r[0] == "12")
            .unwrap();
        assert_eq!(&by_band[1..3], ["3/0", "0/1"]);
        let terms = instant
            .iter()
            .find(|r| r.len() == 10 && r[0] == "12" && r[1] == "evict")
            .unwrap();
        assert_eq!(terms[2], "1");
        assert_eq!(terms[3], e.terms.score.to_string());
        assert_eq!(terms[6], "0.800");
        let tile = instant.iter().find(|r| r[0] == "12/10/20 mesh").unwrap();
        assert_eq!(tile[7], e.terms.score.to_string());
        let cumulative = rows(&s, None, true, &[]);
        let terms = cumulative
            .iter()
            .find(|r| r.len() == 10 && r[0] == "12" && r[1] == "evict")
            .unwrap();
        assert_eq!(terms[2], "2");
    }
}
