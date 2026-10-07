// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A continuous field of gains over a film's tiles, carried by their
//! corners.
//!
//! The other way to bring a film's tiles together gives every tile one gain
//! ([`crate::TileGains`]), and such gains can only change along a closed
//! line of tile edges. Measured on a film across mountains, the seams
//! between captures are **open lines**: to close them, a gain a block has to
//! step where the tiles meet without a step — which is what must never be
//! done. So there it mends almost nothing.
//!
//! Here the gain is a **field**: a value at each corner of the tiles' grid,
//! and inside a tile the four of its corners blended across it. Two
//! neighbours read the same two corners along the edge they share, so
//! **along that edge they are given exactly the same gain** — whatever the
//! field does. A junction that was right stays right, not because it was
//! found to be right, but because of what a field is.
//!
//! A field that is continuous everywhere cannot remove a sharp seam: it can
//! only even out the tone on either side. So where a seam is plain — the
//! tiles themselves step across an edge, and the reference shows nothing
//! there to explain it, all along a run of edges — the field **may jump**:
//! the two tiles are given corners of their own along that edge. The jump
//! is free along the seam and closes to nothing at its ends, where the
//! corner is one again for every tile around it: an open line dies out
//! instead of having to be closed.
//!
//! What a tile is measured by is not this module's business: a
//! [`Measure`] hands it a list of numbers a tile, of which it knows only
//! that the first three are the tile's tone — what a seam is read from —
//! and that each has a bound. The field is the smooth one whose mean over
//! each tile brings every one of that tile's numbers to the film's own —
//! least squares, re-weighed so that an odd tile does not drag it. Moments
//! or transfer curves, it is the same fit.

use std::collections::BTreeMap;

use crate::measure::{Blended, Limits, Local, Measure};
use crate::tiles::{find, light_of, luma, shown, weighted_median, Observed, TileAt, FLOOR};

/// What the field may not go past, and how it is told things.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FieldBounds {
    /// The finest level at which the imagery is one homogeneous picture.
    pub reference_level: u8,
    /// What a correction may not go past, whatever is measured.
    pub limits: Limits,
    /// How much the field is held smooth across a tile, against one tile's
    /// measure fully believed. Larger: the field follows only what is
    /// larger than a few tiles.
    pub smooth: f32,
    /// A seam begins where both the tiles and their offsets step by more
    /// than this across an edge, the same way…
    pub seam_stops: f32,
    /// …and goes on along edges that touch it and step by more than this.
    pub seam_linked_stops: f32,
    /// A seam of fewer edges than this is not one.
    pub least_seam: usize,
}

impl Default for FieldBounds {
    fn default() -> Self {
        Self {
            reference_level: 12,
            limits: Limits::default(),
            smooth: 2.0,
            seam_stops: 0.6,
            seam_linked_stops: 0.3,
            least_seam: 3,
        }
    }
}

/// What a field was made of and what it leaves, steps in stops of light as
/// the median and the 95th centile.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CornerReport {
    pub tiles: usize,
    pub measured: usize,
    pub edges: usize,
    /// Edges the field may jump across, and the seams they make.
    pub seam_edges: usize,
    pub seams: usize,
    /// The largest difference between what two neighbours are given along
    /// an edge that is not a seam, over every number of the measure.
    /// Nought, by construction.
    pub widest_break: f32,
    /// Seam edges where the two tiles step by under a tenth of a stop and
    /// the field jumps by more than a twentieth: steps made where there was
    /// none.
    pub steps_made: usize,
    /// The step at the seam edges — the tiles' facing cells, less what the
    /// reference shows there — with nothing done and with the field.
    pub seam_before: (f32, f32),
    pub seam_after: (f32, f32),
    /// How far tiles are from the film's own tone against the reference.
    pub apart_before: (f32, f32),
    pub apart_after: (f32, f32),
    /// Tiles whose four corners are given nothing at all.
    pub untouched: usize,
    /// Corners whose tone is held at a bound.
    pub held: usize,
}

/// The corners of a tile, in the order they are kept: top-left, top-right,
/// bottom-left, bottom-right.
const CORNERS: usize = 4;

/// A field over a film's tiles: for each tile, at each of its corners, the
/// correction of each number of the measure it was fitted by, and the
/// light a contrast turns about there (its stops under white). A tile that
/// is not here has nothing done to it.
///
/// All of it is carried by the corners and blended across a tile, so all
/// of it is the same on either side of an edge two tiles share.
#[derive(Debug, Clone, PartialEq)]
pub struct CornerField {
    pub measure: Measure,
    pub given: BTreeMap<TileAt, [Vec<f32>; CORNERS]>,
    pub pivot: BTreeMap<TileAt, [f32; CORNERS]>,
}

impl Default for CornerField {
    fn default() -> Self {
        Self {
            measure: Measure::Moments,
            given: BTreeMap::new(),
            pivot: BTreeMap::new(),
        }
    }
}

/// Everything a fit of the field worked out, as CSV tables.
#[derive(Debug, Clone, Default)]
pub struct CornerTrace {
    head: String,
    tiles: Vec<String>,
    edges: Vec<String>,
}

impl CornerTrace {
    /// The tables: a file name and its content.
    pub fn tables(&self) -> [(&'static str, String); 2] {
        let table = |head: &str, rows: &[String]| {
            let mut text = format!("{head}\n");
            for row in rows {
                text += row;
                text.push('\n');
            }
            text
        };
        [
            ("tiles.csv", table(&self.head, &self.tiles)),
            (
                "edges.csv",
                table(
                    "edge,a,b,upright,direct,d_tone_light,d_offset_light,evidence,seam,seam_id,jump_light,break,step,coherence,step_tiles",
                    &self.edges,
                ),
            ),
        ]
    }
}

impl CornerField {
    /// A tile's four corners, each in the form it is blended in: what a
    /// renderer is handed. `None` for a tile the field does nothing to.
    pub fn corners(&self, tile: TileAt) -> Option<[Blended; CORNERS]> {
        let given = self.given.get(&tile)?;
        let pivot = self.pivot.get(&tile).copied().unwrap_or([-2.5; CORNERS]);
        Some(std::array::from_fn(|corner| {
            self.measure.blended(&given[corner], pivot[corner])
        }))
    }

    /// What is done to a tile at a place in it, `u` across and `v` down,
    /// each 0 to 1: its corners blended, and what that does to a texel.
    pub fn at(&self, tile: TileAt, u: f32, v: f32) -> Local {
        match self.corners(tile) {
            Some(corners) => Blended::mix(&corners, u, v).local(),
            None => Local::IDENTITY,
        }
    }

    /// Finds the field for a film, the tiles measured by their moments:
    /// see the module.
    pub fn solve(observed: &Observed, bounds: &FieldBounds) -> (Self, CornerReport) {
        Self::solve_with(observed, Measure::Moments, bounds, None)
    }

    /// Finds the field for a film, the tiles measured as `measure` says.
    pub fn solve_by(
        observed: &Observed,
        measure: Measure,
        bounds: &FieldBounds,
    ) -> (Self, CornerReport) {
        Self::solve_with(observed, measure, bounds, None)
    }

    /// [`Self::solve_by`], and everything it worked out on the way.
    pub fn solve_traced(
        observed: &Observed,
        measure: Measure,
        bounds: &FieldBounds,
    ) -> (Self, CornerReport, CornerTrace) {
        let mut trace = CornerTrace::default();
        let (field, report) = Self::solve_with(observed, measure, bounds, Some(&mut trace));
        (field, report, trace)
    }

    fn solve_with(
        observed: &Observed,
        measure: Measure,
        bounds: &FieldBounds,
        trace: Option<&mut CornerTrace>,
    ) -> (Self, CornerReport) {
        let reference = bounds.reference_level;
        let found = measure.len();
        let nothing = Self {
            measure,
            ..Self::default()
        };
        let at: Vec<TileAt> = observed
            .tiles
            .keys()
            .filter(|a| a.0 > reference)
            .copied()
            .collect();
        let n = at.len();
        let index: BTreeMap<TileAt, usize> = at.iter().enumerate().map(|(i, a)| (*a, i)).collect();
        let usage: Vec<f32> = at
            .iter()
            .map(|a| observed.tiles[a].usage.max(0.0))
            .collect();
        // What each tile measures against the reference: all this fit
        // knows of it. The first three numbers are its tone.
        let measures: Vec<Option<Vec<f32>>> = at
            .iter()
            .map(|a| measure.of(observed, *a, reference))
            .collect();
        let tone: Vec<f32> = at
            .iter()
            .map(|a| (luma(observed.tiles[a].mean()) + FLOOR).log2())
            .collect();
        let mut report = CornerReport {
            tiles: n,
            measured: measures.iter().flatten().count(),
            ..CornerReport::default()
        };
        if report.measured == 0 {
            return (nothing, report);
        }

        // The film's own, number by number: the middle of its tiles'
        // measures, each counted for how much of the film it is. The field
        // brings tiles to it, so what is most of the film is asked for
        // nothing.
        let own: Vec<f32> = (0..found)
            .map(|c| {
                let mut values: Vec<(f32, f32)> = (0..n)
                    .filter_map(|i| measures[i].as_ref().map(|m| (m[c], usage[i].max(1e-3))))
                    .filter(|(v, _)| v.is_finite())
                    .collect();
                if values.is_empty() {
                    0.0
                } else {
                    weighted_median(&mut values)
                }
            })
            .collect();
        // What each tile wants of the field: to be brought to the film's
        // own. A number that could not be taken wants nothing.
        let wanted: Vec<Option<Vec<f32>>> = measures
            .iter()
            .map(|m| {
                m.as_ref().map(|m| {
                    (0..found)
                        .map(|c| if m[c].is_finite() { own[c] - m[c] } else { 0.0 })
                        .collect()
                })
            })
            .collect();
        let tone_of = |values: &[f32]| light_of(Measure::tone(values));

        // Edges: a tile and the one to its right, a tile and the one below.
        // For each, what the tiles themselves show across it — whole tile
        // against whole tile, and their facing outer cells — and what is
        // left of it once what the reference shows is taken away.
        struct Edge {
            a: usize,
            b: usize,
            upright: bool,
            direct: f32,
            d_tone: f32,
            d_offset: Option<f32>,
            /// How the two meet at their edge.
            met: crate::tiles::Junction,
        }
        let mut edges: Vec<Edge> = Vec::new();
        for (i, (level, x, y)) in at.iter().enumerate() {
            for (next, upright) in [((*level, x + 1, *y), true), ((*level, *x, y + 1), false)] {
                let Some(j) = index.get(&next).copied() else {
                    continue;
                };
                let (mine, theirs) = (&observed.tiles[&at[i]], &observed.tiles[&at[j]]);
                let g = crate::tiles::GRID;
                let mut along: Vec<f32> = (0..g)
                    .map(|k| {
                        let (p, q) = if upright {
                            (mine.cells[k * g + g - 1], theirs.cells[k * g])
                        } else {
                            (mine.cells[(g - 1) * g + k], theirs.cells[k])
                        };
                        ((luma(p) + FLOOR) / (luma(q) + FLOOR)).log2()
                    })
                    .collect();
                along.sort_by(f32::total_cmp);
                edges.push(Edge {
                    a: i,
                    b: j,
                    upright,
                    direct: along[along.len() / 2],
                    d_tone: tone[i] - tone[j],
                    d_offset: match (&measures[i], &measures[j]) {
                        (Some(p), Some(q)) => Some(tone_of(p) - tone_of(q)),
                        _ => None,
                    },
                    // Against the reference where the tiles were set
                    // against one — what a line is fitted on; else from
                    // the two tiles alone.
                    met: (measure == Measure::Linear)
                        .then(|| observed.junction_against(at[i], at[j], upright))
                        .flatten()
                        .or_else(|| observed.junction(at[i], at[j], upright))
                        .unwrap_or(crate::tiles::Junction {
                            step: 0.0,
                            coherence: 0.0,
                            tiles: 0.0,
                        }),
                });
            }
        }
        report.edges = edges.len();

        // Where the field may jump: where two tiles do not meet *at their
        // edge*, all along it ([`crate::Junction`]). Read against the
        // reference under them when there is one — the step that is in the
        // film and not in the ground; else off the two tiles alone, the
        // ground's own slope on either side taken away.
        let evidence: Vec<f32> = edges.iter().map(|e| e.met.apart()).collect();
        // The two ends of an edge, as corners of the grid of its level.
        let ends = |e: &Edge| {
            let (level, x, y) = at[e.a];
            if e.upright {
                [(level, x + 1, y), (level, x + 1, y + 1)]
            } else {
                [(level, x, y + 1), (level, x + 1, y + 1)]
            }
        };
        let mut meeting: BTreeMap<(u8, u32, u32), Vec<usize>> = BTreeMap::new();
        for (k, e) in edges.iter().enumerate() {
            for end in ends(e) {
                meeting.entry(end).or_default().push(k);
            }
        }
        // Strong edges begin a seam; it goes on along the edges that touch
        // it and are strong enough to follow.
        let mut seam: Vec<bool> = evidence.iter().map(|e| *e > bounds.seam_stops).collect();
        let mut front: Vec<usize> = (0..edges.len()).filter(|k| seam[*k]).collect();
        while let Some(k) = front.pop() {
            for end in ends(&edges[k]) {
                for j in &meeting[&end] {
                    if !seam[*j] && evidence[*j] > bounds.seam_linked_stops {
                        seam[*j] = true;
                        front.push(*j);
                    }
                }
            }
        }
        // Seams: seam edges that touch one another. One of too few edges
        // is not a seam.
        let mut of_seam: Vec<usize> = (0..edges.len()).collect();
        for touching in meeting.values() {
            let cut: Vec<usize> = touching.iter().copied().filter(|k| seam[*k]).collect();
            for pair in cut.windows(2) {
                let (ra, rb) = (find(&mut of_seam, pair[0]), find(&mut of_seam, pair[1]));
                if ra != rb {
                    of_seam[ra] = rb;
                }
            }
        }
        let mut length: BTreeMap<usize, usize> = BTreeMap::new();
        for k in (0..edges.len()).filter(|k| seam[*k]) {
            *length.entry(find(&mut of_seam, k)).or_default() += 1;
        }
        for k in 0..edges.len() {
            if seam[k] && length[&find(&mut of_seam, k)] < bounds.least_seam {
                seam[k] = false;
            }
        }
        report.seam_edges = seam.iter().filter(|s| **s).count();
        report.seams = length.values().filter(|l| **l >= bounds.least_seam).count();

        // The unknowns. Every tile has four corners of its own, and across
        // every edge that is not a seam the two tiles' corners along it are
        // made one: that is all the continuity there is, and it is exact.
        let slot = |tile: usize, corner: usize| tile * CORNERS + corner;
        let mut class: Vec<usize> = (0..n * CORNERS).collect();
        let mut one = |a: usize, b: usize| {
            let (ra, rb) = (find(&mut class, a), find(&mut class, b));
            if ra != rb {
                class[ra] = rb;
            }
        };
        for (k, e) in edges.iter().enumerate() {
            if seam[k] {
                continue;
            }
            if e.upright {
                one(slot(e.a, 1), slot(e.b, 0));
                one(slot(e.a, 3), slot(e.b, 2));
            } else {
                one(slot(e.a, 2), slot(e.b, 0));
                one(slot(e.a, 3), slot(e.b, 1));
            }
        }
        let class: Vec<usize> = (0..n * CORNERS).map(|s| find(&mut class, s)).collect();

        // What each unknown touches: the tiles it is a corner of.
        let mut of_class: BTreeMap<usize, Vec<(usize, usize)>> = BTreeMap::new();
        for tile in 0..n {
            for corner in 0..CORNERS {
                of_class
                    .entry(class[slot(tile, corner)])
                    .or_default()
                    .push((tile, corner));
            }
        }
        // The sides of a tile: pairs of its corners the field is held
        // smooth between.
        const SIDES: [(usize, usize); 4] = [(0, 1), (2, 3), (0, 2), (1, 3)];

        // The field: the one whose mean over each tile is what the tile
        // wants, and that changes as little as it can across a tile. Least
        // squares, an unknown at a time, the tiles re-weighed each round so
        // that one whose measure stands far from the field — snow, cloud —
        // is believed less. A weak pull to nothing keeps what nothing
        // measures where it is.
        const PULL: f32 = 0.01;
        const FAR: f32 = 0.5;
        let mut value: BTreeMap<usize, Vec<f32>> =
            of_class.keys().map(|c| (*c, vec![0.0; found])).collect();
        let mut believed = vec![1.0f32; n];
        let mean_of = |value: &BTreeMap<usize, Vec<f32>>, tile: usize| {
            let mut sum = vec![0.0f32; found];
            for corner in 0..CORNERS {
                let v = &value[&class[slot(tile, corner)]];
                for c in 0..found {
                    sum[c] += v[c] / CORNERS as f32;
                }
            }
            sum
        };
        for round in 0..30 {
            for _ in 0..20 {
                for (this, corners) in &of_class {
                    let mut sum = vec![0.0f32; found];
                    let mut total = PULL;
                    for (tile, corner) in corners {
                        // How many of the tile's corners this unknown is.
                        let mine = (0..CORNERS)
                            .filter(|c| class[slot(*tile, *c)] == *this)
                            .count() as f32;
                        if let Some(w) = &wanted[*tile] {
                            // The tile's mean without this unknown's part.
                            let mut others = vec![0.0f32; found];
                            for c in 0..CORNERS {
                                let other = class[slot(*tile, c)];
                                if other != *this {
                                    for k in 0..found {
                                        others[k] += value[&other][k] / CORNERS as f32;
                                    }
                                }
                            }
                            // Counted once a tile, not once a corner of it.
                            let weight = believed[*tile] * usage[*tile].max(0.25) / mine;
                            let share = mine / CORNERS as f32;
                            for k in 0..found {
                                sum[k] += weight * share * (w[k] - others[k]);
                            }
                            total += weight * share * share;
                        }
                        for (p, q) in SIDES {
                            let other = if p == *corner {
                                q
                            } else if q == *corner {
                                p
                            } else {
                                continue;
                            };
                            let other = class[slot(*tile, other)];
                            if other == *this {
                                continue;
                            }
                            for k in 0..found {
                                sum[k] += bounds.smooth * value[&other][k];
                            }
                            total += bounds.smooth;
                        }
                    }
                    let mut next: Vec<f32> = sum.iter().map(|s| s / total).collect();
                    measure.within(&mut next, &bounds.limits);
                    value.insert(*this, next);
                }
            }
            if round < 29 {
                for tile in 0..n {
                    if let Some(w) = &wanted[tile] {
                        let got = mean_of(&value, tile);
                        // Believed by its tone: that is what stands far when
                        // the ground is not the ground the reference saw.
                        let off = (0..3).map(|c| (got[c] - w[c]).abs()).fold(0.0, f32::max);
                        believed[tile] = 1.0 / (1.0 + (off / FAR) * (off / FAR));
                    }
                }
            }
        }
        for v in value.values_mut() {
            measure.keep(v);
        }
        report.held = value
            .values()
            .filter(|found| {
                let g = Measure::tone(found);
                let light = light_of(g);
                light.abs() >= bounds.limits.light_stops - 1.0 / 64.0
                    || g.iter()
                        .any(|c| (c - light).abs() >= bounds.limits.tint_stops - 1.0 / 64.0)
            })
            .count();

        let given_of = |tile: usize| -> [Vec<f32>; CORNERS] {
            std::array::from_fn(|corner| value[&class[slot(tile, corner)]].clone())
        };
        // The pivot of a contrast at a corner: the tone, once the gain is
        // on, of the tiles that meet there — so that contrast turns each
        // about its own light, and is one value on either side of an edge.
        let mut pivot: BTreeMap<usize, f32> = BTreeMap::new();
        for (this, corners) in &of_class {
            let around: f32 = corners
                .iter()
                .map(|(tile, _)| tone[*tile] + tone_of(&mean_of(&value, *tile)))
                .sum::<f32>()
                / corners.len() as f32;
            pivot.insert(*this, (around * 64.0).round() / 64.0);
        }
        // What two neighbours are given along the edge they share: the
        // jump of tone from one to the other, as the mean of the two
        // corners on it, and the most the two sides differ by at either
        // corner, over every number.
        let along = |e: &Edge| {
            let (ca, cb) = (given_of(e.a), given_of(e.b));
            let (mine, theirs) = if e.upright {
                ([&ca[1], &ca[3]], [&cb[0], &cb[2]])
            } else {
                ([&ca[2], &ca[3]], [&cb[0], &cb[1]])
            };
            let jump =
                (tone_of(mine[0]) + tone_of(mine[1]) - tone_of(theirs[0]) - tone_of(theirs[1]))
                    / 2.0;
            let widest = (0..2)
                .flat_map(|i| (0..found).map(move |c| (i, c)))
                .map(|(i, c)| (mine[i][c] - theirs[i][c]).abs())
                .fold(0.0, f32::max);
            (jump, widest)
        };
        let (mut seam_before, mut seam_after) = (Vec::new(), Vec::new());
        let mut jumps = Vec::with_capacity(edges.len());
        for (k, e) in edges.iter().enumerate() {
            let (jump, widest) = along(e);
            jumps.push((jump, widest));
            if seam[k] {
                seam_before.push(e.met.step.abs());
                seam_after.push((e.met.step + jump).abs());
                report.steps_made += usize::from(e.met.step.abs() < 0.1 && jump.abs() > 0.05);
            } else {
                report.widest_break = report.widest_break.max(widest);
            }
        }
        report.seam_before = shown(&mut seam_before);
        report.seam_after = shown(&mut seam_after);
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for tile in 0..n {
            if let Some(w) = &wanted[tile] {
                let got = mean_of(&value, tile);
                before.push(tone_of(w).abs());
                after.push(light_of([0, 1, 2].map(|c| got[c] - w[c])).abs());
            }
        }
        report.apart_before = shown(&mut before);
        report.apart_after = shown(&mut after);
        let untouched = |tile: usize| given_of(tile).iter().all(|c| c.iter().all(|v| *v == 0.0));
        report.untouched = (0..n).filter(|tile| untouched(*tile)).count();

        if let Some(trace) = trace {
            // A row a tile: what it measured (`m_`), the film's own
            // (`own_`) and what it is given, as the mean over the tile
            // (`given_`), for every number of the measure by its name.
            let names = measure.names();
            let mut head = String::from("tile,level,x,y,usage,measured,tone_light,believed");
            for prefix in ["m_", "own_", "given_"] {
                for name in names {
                    head += &format!(",{prefix}{name}");
                }
            }
            head += ",gain_tl,gain_tr,gain_bl,gain_br";
            trace.head = head;
            for (i, (level, x, y)) in at.iter().enumerate() {
                let mut row = format!(
                    "{i},{level},{x},{y},{:.1},{},{:.4},{:.3}",
                    usage[i],
                    u8::from(measures[i].is_some()),
                    tone[i],
                    believed[i]
                );
                let number = |v: f32| {
                    if v.is_finite() {
                        format!(",{v:.5}")
                    } else {
                        String::from(",")
                    }
                };
                for c in 0..found {
                    row += &number(measures[i].as_ref().map_or(f32::NAN, |m| m[c]));
                }
                for value in &own {
                    row += &number(*value);
                }
                for value in mean_of(&value, i) {
                    row += &number(value);
                }
                for corner in given_of(i) {
                    row += &number(tone_of(&corner));
                }
                trace.tiles.push(row);
            }
            for (k, e) in edges.iter().enumerate() {
                trace.edges.push(format!(
                    "{k},{},{},{},{:.4},{:.4},{},{:.4},{},{},{:.4},{:.4},{:.4},{:.3},{:.4}",
                    e.a,
                    e.b,
                    u8::from(e.upright),
                    e.direct,
                    e.d_tone,
                    e.d_offset.map_or(String::new(), |v| format!("{v:.4}")),
                    evidence[k],
                    u8::from(seam[k]),
                    if seam[k] {
                        find(&mut of_seam, k).to_string()
                    } else {
                        String::new()
                    },
                    jumps[k].0,
                    jumps[k].1,
                    e.met.step,
                    e.met.coherence,
                    e.met.tiles,
                ));
            }
        }

        let touched: Vec<usize> = (0..n).filter(|tile| !untouched(*tile)).collect();
        let given = touched
            .iter()
            .map(|tile| (at[*tile], given_of(*tile)))
            .collect();
        let pivot = touched
            .iter()
            .map(|tile| {
                let corners: [f32; CORNERS] =
                    std::array::from_fn(|corner| pivot[&class[slot(*tile, corner)]]);
                (at[*tile], corners)
            })
            .collect();
        (
            Self {
                measure,
                given,
                pivot,
            },
            report,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiles::{TileSeen, GRID};

    const REFERENCE: u8 = 12;

    fn flat(tone: f32, usage: f32) -> TileSeen {
        TileSeen {
            cells: [[tone; 3]; GRID * GRID],
            edges: [[[tone; 3]; GRID]; 4],
            usage,
            tones: None,
            paired: None,
        }
    }

    /// A film of `wide` × `high` tiles of level 13 over even ground, each
    /// `of` stops lighter than the ground is, and the reference under it as
    /// the ground is.
    fn film(wide: u32, high: u32, of: impl Fn(u32, u32) -> f32) -> Observed {
        let mut observed = Observed::default();
        for y in 0..high {
            for x in 0..wide {
                observed
                    .tiles
                    .insert((13, 100 + x, 200 + y), flat(0.1 * of(x, y).exp2(), 1.0));
                observed
                    .tiles
                    .insert((REFERENCE, (100 + x) / 2, (200 + y) / 2), flat(0.1, 0.0));
            }
        }
        observed
    }

    fn solved(observed: &Observed) -> (CornerField, CornerReport) {
        CornerField::solve(observed, &FieldBounds::default())
    }

    /// The light of the gain at a corner of a tile of the film, in stops.
    fn corner(field: &CornerField, x: u32, y: u32, which: usize) -> f32 {
        field
            .given
            .get(&(13, 100 + x, 200 + y))
            .map_or(0.0, |c| light_of(Measure::tone(&c[which])))
    }

    /// The most two neighbours differ by along an edge they share, over
    /// every edge of the film but those `but` says to leave out.
    fn widest_break(
        field: &CornerField,
        wide: u32,
        high: u32,
        but: impl Fn(u32, u32, bool) -> bool,
    ) -> f32 {
        let mut widest = 0.0f32;
        for y in 0..high {
            for x in 0..wide {
                if x + 1 < wide && !but(x, y, true) {
                    widest = widest
                        .max((corner(field, x, y, 1) - corner(field, x + 1, y, 0)).abs())
                        .max((corner(field, x, y, 3) - corner(field, x + 1, y, 2)).abs());
                }
                if y + 1 < high && !but(x, y, false) {
                    widest = widest
                        .max((corner(field, x, y, 2) - corner(field, x, y + 1, 0)).abs())
                        .max((corner(field, x, y, 3) - corner(field, x, y + 1, 1)).abs());
                }
            }
        }
        widest
    }

    #[test]
    fn two_neighbours_are_given_the_same_gain_along_their_edge_whatever_the_field() {
        // Ground whose offsets wander by up to a stop from tile to tile,
        // with no seam anywhere to be found: the field has work to do
        // everywhere, and may jump nowhere.
        let wander = |x: u32, y: u32| ((x * 7 + y * 13) % 5) as f32 * 0.25 - 0.5;
        let observed = film(10, 8, wander);
        let bounds = FieldBounds {
            seam_stops: 9.0,
            ..FieldBounds::default()
        };
        let (field, report) = CornerField::solve(&observed, &bounds);
        assert_eq!(report.seam_edges, 0);
        assert!(!field.given.is_empty(), "the field did nothing");
        // Counted by the fit, and counted again here from the field alone.
        assert_eq!(report.widest_break, 0.0);
        assert_eq!(widest_break(&field, 10, 8, |_, _, _| false), 0.0);
        // And so in the picture: either side of an edge, the same gain.
        for v in [0.0, 0.3, 1.0] {
            assert_eq!(
                field.at((13, 103, 204), 1.0, v),
                field.at((13, 104, 204), 0.0, v),
                "gain, black point, contrast, pivot and saturation, all of it"
            );
        }
        assert_eq!(report.steps_made, 0);
    }

    #[test]
    fn a_seam_across_the_film_is_jumped_and_each_side_left_flat() {
        // The left third a stop darker, top to bottom.
        let observed = film(12, 6, |x, _| if x < 4 { -1.0 } else { 0.0 });
        let (field, report) = solved(&observed);
        assert_eq!((report.seams, report.seam_edges), (1, 6), "{report:?}");
        // The larger side is the film's own and is given nothing; the
        // other is brought up a stop, flat right to the seam.
        for y in 0..6 {
            for x in 0..4 {
                for which in 0..4 {
                    assert!((corner(&field, x, y, which) - 1.0).abs() < 0.05, "{x} {y}");
                }
            }
            for x in 4..12 {
                assert_eq!(corner(&field, x, y, 0), 0.0);
            }
        }
        assert_eq!(report.untouched, 48);
        assert!(
            report.seam_before.0 > 0.95 && report.seam_after.1 < 0.05,
            "{report:?}"
        );
        // Nothing breaks but at the seam.
        assert_eq!(report.widest_break, 0.0);
        assert_eq!(
            widest_break(&field, 12, 6, |x, _, upright| upright && x == 3),
            0.0
        );
        assert_eq!(report.steps_made, 0);
    }

    #[test]
    fn a_seam_that_ends_dies_out_without_making_a_step() {
        // A capture a stop darker in the top-left of the film, plainly cut
        // on its right for three rows; below that it fades, a quarter of a
        // stop a row, and there is no line to follow any more.
        let observed = film(12, 8, |x, y| {
            if x >= 4 {
                0.0
            } else {
                -(1.0 - 0.25 * y.saturating_sub(2) as f32).max(0.0)
            }
        });
        let (field, report) = solved(&observed);
        // The plain part of the seam is found, and it is an open line: it
        // does not reach the bottom of the film.
        assert_eq!(report.seams, 1, "{report:?}");
        // Four rows step by more than a seam begins at; the fifth, half a
        // stop, is followed because it touches them; the sixth is not.
        assert_eq!(report.seam_edges, 5, "{report:?}");
        // Along it the field jumps and the step goes…
        assert!(
            report.seam_after.0 < 0.5 * report.seam_before.0,
            "{report:?}"
        );
        let jump = corner(&field, 3, 0, 1) - corner(&field, 4, 0, 0);
        assert!(jump > 0.6, "{jump}");
        // …and everywhere else, past its end too, neighbours are given the
        // same gain along their edge: the line is not closed by force.
        assert_eq!(report.widest_break, 0.0);
        let on_seam = |x: u32, y: u32, upright: bool| upright && x == 3 && y < 5;
        assert_eq!(widest_break(&field, 12, 8, on_seam), 0.0);
        assert_eq!(report.steps_made, 0);
        // At the bottom of the film, where the two sides are one tone, the
        // field is one value across what was the seam's line.
        assert_eq!(corner(&field, 3, 7, 3), corner(&field, 4, 7, 2));
    }

    /// A tile of ground with relief and colour: light that swings by
    /// `swing` stops from cell to cell, colours `colour` away from grey,
    /// under a veil of `veil`.
    fn ground(tone: f32, swing: f32, colour: f32, veil: f32) -> TileSeen {
        let mut seen = flat(tone, 1.0);
        for (k, c) in seen.cells.iter_mut().enumerate() {
            // Whole 2 × 2 blocks swing together, so that a level up shows
            // the same swing.
            let up = ((k % GRID) / 2 + (k / GRID) / 2) % 2 == 0;
            let y = tone * (if up { swing } else { -swing }).exp2();
            *c = [
                y * (1.0 + colour) + veil,
                y + veil,
                y * (1.0 - colour) + veil,
            ];
        }
        seen
    }

    #[test]
    fn the_field_turns_contrast_saturation_and_black_point_too() {
        // One tone throughout — a gain has nothing to do. But the left
        // third is another capture: flatter, duller, under a veil.
        let mut observed = Observed::default();
        for y in 0..6u32 {
            for x in 0..12u32 {
                let tile = if x < 4 {
                    ground(0.1, 0.25, 0.1, 0.004)
                } else {
                    ground(0.1, 0.5, 0.2, 0.0)
                };
                observed.tiles.insert((13, 100 + x, 200 + y), tile);
                let mut under = ground(0.1, 0.5, 0.2, 0.0);
                under.usage = 0.0;
                observed
                    .tiles
                    .insert((REFERENCE, (100 + x) / 2, (200 + y) / 2), under);
            }
        }
        let apart = observed.apart((13, 100, 200), REFERENCE).expect("measured");
        assert!(
            apart.contrast < -0.5 && apart.saturation < -0.5 && apart.black > 0.002,
            "{apart:?}"
        );
        let same = observed.apart((13, 108, 200), REFERENCE).expect("measured");
        assert!(
            same.contrast.abs() < 0.01 && same.saturation.abs() < 0.01 && same.black.abs() < 1e-4
        );

        let bounds = FieldBounds::default();
        let (field, report) = CornerField::solve(&observed, &bounds);
        // Well inside the flat capture the field raises contrast and
        // saturation as far as it may, and takes the veil away; well inside
        // the other it does nothing.
        let dull = field.at((13, 101, 203), 0.5, 0.5);
        assert!(
            dull.contrast > 1.2 && dull.contrast <= bounds.limits.shape_stops.exp2() + 0.01,
            "{dull:?}"
        );
        assert!(
            dull.saturation > 1.2 && dull.saturation <= bounds.limits.shape_stops.exp2() + 0.01
        );
        assert!(
            dull.black[1] > 0.002 && dull.black[1] <= bounds.limits.black + 1e-4,
            "{dull:?}"
        );
        // The two captures are of one tone: there is no seam to jump, and
        // the field goes from one to the other smoothly — nothing left of
        // it a few tiles into the other capture, and no step anywhere.
        assert_eq!(report.seam_edges, 0, "{report:?}");
        let far = field.at((13, 110, 203), 0.5, 0.5);
        assert!(far.contrast < 1.03 && far.saturation < 1.03, "{far:?}");
        assert_eq!(report.steps_made, 0);
        // And all of it is one value on either side of every edge.
        for (x, v) in [(100, 0.2), (103, 0.5), (106, 0.9)] {
            assert_eq!(
                field.at((13, x, 202), 1.0, v),
                field.at((13, x + 1, 202), 0.0, v)
            );
            assert_eq!(
                field.at((13, x, 202), v, 1.0),
                field.at((13, x, 203), v, 0.0)
            );
        }
        assert_eq!(report.widest_break, 0.0);
        // Graded, a cell of the dull capture is more contrasted and more
        // coloured than it was.
        let was = observed.tiles[&(13, 101, 203)].cells[0];
        let is = dull.apply(was);
        let colour = |c: [f32; 3]| (c[0] - c[2]) / (c[0] + c[2]);
        assert!(colour(is) > colour(was) * 1.15, "{was:?} {is:?}");
    }

    #[test]
    fn the_field_is_the_same_fit_whatever_the_tiles_are_measured_by() {
        // The same film, measured two ways: the same seams, the same
        // breaks — none — and the same tone at every corner.
        let observed = film(12, 8, |x, y| {
            if x >= 4 {
                0.0
            } else {
                -(1.0 - 0.25 * y.saturating_sub(2) as f32).max(0.0)
            }
        });
        let bounds = FieldBounds::default();
        let (by_moments, report) = CornerField::solve_by(&observed, Measure::Moments, &bounds);
        let (by_curves, of_curves) = CornerField::solve_by(&observed, Measure::Curves, &bounds);
        assert_eq!(of_curves, report);
        assert_eq!(by_curves.measure, Measure::Curves);
        assert!(!by_moments.given.is_empty());
        for (at, corners) in &by_moments.given {
            let others = &by_curves.given[at];
            for (mine, theirs) in corners.iter().zip(others) {
                assert_eq!(Measure::tone(mine), Measure::tone(theirs));
                assert_eq!(theirs.len(), Measure::Curves.len());
            }
        }
    }

    #[test]
    fn no_corner_goes_past_its_bounds() {
        let observed = film(12, 6, |x, _| if x < 4 { -3.0 } else { 0.0 });
        let bounds = FieldBounds::default();
        let (field, report) = solved(&observed);
        for corners in field.given.values() {
            for c in corners {
                assert!(
                    light_of(Measure::tone(c)).abs() <= bounds.limits.light_stops + 1.0 / 32.0,
                    "{c:?}"
                );
            }
        }
        assert!(report.held > 0, "{report:?}");
        assert!((corner(&field, 0, 0, 0) - bounds.limits.light_stops).abs() < 0.05);
    }

    #[test]
    fn a_field_that_is_traced_is_the_same_field_and_says_what_it_did() {
        let observed = film(12, 6, |x, _| if x < 4 { -1.0 } else { 0.0 });
        let bounds = FieldBounds::default();
        let (field, report) = CornerField::solve(&observed, &bounds);
        let (traced, traced_report, trace) =
            CornerField::solve_traced(&observed, Measure::Moments, &bounds);
        assert_eq!((&traced, &traced_report), (&field, &report));
        let tables = trace.tables();
        let rows = |name: &str| -> Vec<Vec<String>> {
            let (_, text) = tables.iter().find(|(n, _)| *n == name).expect("a table");
            let mut lines = text.lines();
            let columns = lines.next().expect("a head").split(',').count();
            lines
                .map(|l| l.split(',').map(str::to_string).collect::<Vec<_>>())
                .inspect(|row| assert_eq!(row.len(), columns, "{name}: {row:?}"))
                .collect()
        };
        assert_eq!(rows("tiles.csv").len(), report.tiles);
        let edges = rows("edges.csv");
        assert_eq!(edges.len(), report.edges);
        assert_eq!(
            edges.iter().filter(|e| e[8] == "1").count(),
            report.seam_edges
        );
    }

    #[test]
    fn a_field_fitted_on_lines_jumps_where_the_film_steps_and_the_reference_does_not() {
        // Eight tiles by four over one ground; the four on the right are a
        // stop darker in the film and not in the reference. What is kept of
        // the tiles' own cells shows nothing — only the places set against
        // the reference do.
        let mut observed = Observed::default();
        for y in 0..4u32 {
            for x in 0..8u32 {
                let by = if x < 4 { 1.0f32 } else { 0.5 };
                let places: [[f32; 3]; crate::PAIRS * crate::PAIRS] = std::array::from_fn(|k| {
                    let (gx, gy) = (
                        (x as usize * crate::PAIRS + k % crate::PAIRS) as f32,
                        (y as usize * crate::PAIRS + k / crate::PAIRS) as f32,
                    );
                    let light = 0.08 + 0.03 * ((gx * 0.7).sin() + (gy * 0.5).cos());
                    [light, light * 1.1, light * 0.8]
                });
                let mut seen = flat(-3.0, 1.0);
                seen.paired = Some(Box::new(crate::Paired {
                    tile: places.map(|c| c.map(|v| v * by)),
                    reference: places,
                }));
                observed.tiles.insert((13, 100 + x, 200 + y), seen);
            }
        }
        let bounds = FieldBounds {
            reference_level: 0,
            ..FieldBounds::default()
        };
        let (field, report) = CornerField::solve_by(&observed, Measure::Linear, &bounds);
        assert_eq!(report.measured, 32);
        assert_eq!(report.seam_edges, 4, "{report:?}");
        assert_eq!(report.steps_made, 0);
        // On either side of the seam the two are given a stop apart, right
        // at the edge; away from it each side is flat.
        let gain = |x: u32, u: f32| field.at((13, 100 + x, 201), u, 0.5).gain[1].log2();
        assert!(
            (gain(4, 0.01) - gain(3, 0.99) - 1.0).abs() < 0.1,
            "{} {}",
            gain(3, 0.99),
            gain(4, 0.01)
        );
        assert!((gain(0, 0.5) - gain(3, 0.5)).abs() < 0.1);
        assert!(report.seam_after.0 < 0.1, "{report:?}");
    }
}
