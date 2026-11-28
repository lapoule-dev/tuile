// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The preload reservoir, drawn: what the budget holds, and where, around and
//! under the eye. A plan view of the terrain tiles of one level centred on the
//! eye with the camera's cone and its footprint on the ground, and beside it
//! the vertical section: the tiles under the eye, level by level.
//!
//! It reads what the budget publishes ([`HeldReport::ring`], the held report
//! taken while a window shows it) and nothing else: each cell is a published
//! resource, its colour a rule over its published state and score
//! ([`score_colour`], [`state_colour`]), its tooltip the published terms of
//! the score. Nothing here decides anything, and nothing here is computed in
//! the budget's steps.

use std::collections::HashMap;

use tuile_core::budget::score::{HeldReport, RingState, RingTile};
use tuile_core::budget::{Class, ResourceId, CERTAIN};

use crate::budget_board::{
    finish, srgb, Drawing, Legend, Painter, Swatch, TextCache, DIM, EM, MARGIN, ROW, TEXT,
};
use crate::{Font, Mesh};

/// Where the eye is and what it sees, for the plan: the camera's geodetic
/// position and heading, and its footprint on the ground. All angles in
/// radians.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReservoirEye {
    pub lon: f64,
    pub lat: f64,
    pub height_m: f64,
    /// Clockwise from north.
    pub heading: f64,
    /// `(lon, lat)` points around the view's footprint on the ground, in
    /// order ([`tuile_camera::GlobeCamera::ground_footprint`]).
    pub footprint: Vec<(f64, f64)>,
    /// The tiles drawn outside the present view's frustum, by level
    /// ([`drawn_outside`]), and outside it and the host's views too.
    pub outside_present: Vec<Outside>,
    pub outside_any: Vec<Outside>,
    /// Whether the tile picked is inside the present view's frustum.
    pub selected_in_view: Option<bool>,
    /// Traversals cut short by their node bound since the session began.
    pub truncated: u64,
}

/// The plan's half-extent, in tiles of the level shown.
pub const PLAN_TILES: f64 = 8.5;
/// The plan's side, points.
const PLAN: f64 = 440.0;
/// The section's width, points.
const SECTION: f64 = 250.0;

/// The score's colour ramp, cold to hot: indigo, violet, rose, pale yellow.
const RAMP: [u32; 4] = [0x2D2A6E, 0x8E3FA0, 0xE0709A, 0xFFF2A8];
/// The deepest bucket of the ramp: scores of `2^20` µflouz per reference
/// byte and above are the hottest.
const HOT_BUCKET: f64 = 20.0;

const DRAWN: u32 = 0x009E73;
const IN_FLIGHT: u32 = 0x56B4E9;
const QUEUED: u32 = 0xBBBBBB;
const HOLE: u32 = 0xD55E00;
const EYE: u32 = 0xFFFFFF;

/// Where a score falls on the ramp, `0` (cold) to `1` (hot): the log2 of the
/// score, on the fixed scale of the histograms (`HOT_BUCKET`). Monotone.
pub fn score_t(score: u64) -> f64 {
    (f64::from(tuile_core::budget::score::score_bucket(score) as u32) / HOT_BUCKET).clamp(0.0, 1.0)
}

fn lerp3(a: u32, b: u32, t: f64) -> [f32; 4] {
    let ch = |c: u32, s: u32| f64::from((c >> s) & 0xFF);
    let m = |s: u32| ((ch(a, s) + (ch(b, s) - ch(a, s)) * t) / 255.0) as f32;
    [m(16), m(8), m(0), 1.0]
}

/// The colour of a held, undrawn tile of score `score`: cold for a low
/// score, hot for a high one, on a fixed scale.
pub fn score_colour(score: u64) -> [f32; 4] {
    let t = score_t(score) * (RAMP.len() - 1) as f64;
    let i = (t.floor() as usize).min(RAMP.len() - 2);
    lerp3(RAMP[i], RAMP[i + 1], t - i as f64)
}

/// The colour of a tile in `state` with `score`.
pub fn state_colour(state: RingState, score: u64) -> [f32; 4] {
    match state {
        RingState::Drawn => srgb(DRAWN, 1.0),
        RingState::InFlight => srgb(IN_FLIGHT, 1.0),
        RingState::Queued => srgb(QUEUED, 0.35),
        RingState::Hole => srgb(HOLE, 1.0),
        RingState::Floor | RingState::Kept | RingState::Wanted | RingState::Left => {
            score_colour(score)
        }
    }
}

/// The rectangle of terrain tile `(level, x, y)` of the geographic grid
/// (two root tiles, rows from the south): west, south, east, north, radians.
pub fn tile_rect(level: u32, x: u64, y: u64) -> (f64, f64, f64, f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    let nx = (2u64 << level) as f64;
    let ny = (1u64 << level) as f64;
    let (w, h) = (2.0 * PI / nx, PI / ny);
    let west = -PI + x as f64 * w;
    let south = -FRAC_PI_2 + y as f64 * h;
    (west, south, west + w, south + h)
}

/// The tile of `level` that holds `(lon, lat)`.
pub fn tile_at(level: u32, lon: f64, lat: f64) -> (u64, u64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    let nx = (2u64 << level) as f64;
    let ny = (1u64 << level) as f64;
    let x = ((lon + PI) / (2.0 * PI) * nx).floor().clamp(0.0, nx - 1.0);
    let y = ((lat + FRAC_PI_2) / PI * ny).floor().clamp(0.0, ny - 1.0);
    (x as u64, y as u64)
}

/// The terrain meshes of `report`'s ring, by tile `(level, x, y)`.
pub fn terrain_tiles(report: &HeldReport) -> HashMap<(u32, u64, u64), &RingTile> {
    report
        .ring
        .iter()
        .filter(|t| t.class == Class::TerrainMesh)
        .filter_map(|t| match t.resource {
            ResourceId::Mesh(id) => {
                let (l, x, y) = id.terrain_coord();
                Some(((l, x, y), t))
            }
            ResourceId::Drape { .. } => None,
        })
        .collect()
}

/// Terrain tiles at one level held and not drawn that make a level worth
/// showing: fewer is a stray, not a ring.
const RING_MIN: u32 = 3;

/// The level shown when none is asked for: the finest level of terrain tiles
/// held and not drawn (a preload, a kept tile, a left one; not the floor),
/// at least `RING_MIN` of them — the level where the ring can be seen.
/// Failing that, the level of most drawn tiles (the finest on a tie).
pub fn auto_level(report: &HeldReport, near: Option<(f64, f64)>) -> Option<u32> {
    let mut ahead: HashMap<u32, u32> = HashMap::new();
    let mut drawn: HashMap<u32, u32> = HashMap::new();
    for (&(l, x, y), t) in &terrain_tiles(report) {
        // Near the eye only, when it is known: what the plan can show.
        if let Some((lon, lat)) = near {
            let (ex, ey) = tile_at(l, lon, lat);
            if x.abs_diff(ex) as f64 > PLAN_TILES || y.abs_diff(ey) as f64 > PLAN_TILES {
                continue;
            }
        }
        match t.state {
            RingState::Wanted | RingState::Kept | RingState::Left => {
                *ahead.entry(l).or_default() += 1;
            }
            RingState::Drawn => *drawn.entry(l).or_default() += 1,
            _ => {}
        }
    }
    ahead
        .into_iter()
        .filter(|&(_, n)| n >= RING_MIN)
        .map(|(l, _)| l)
        .max()
        .or_else(|| {
            drawn
                .into_iter()
                .max_by_key(|&(l, n)| (n, l))
                .map(|(l, _)| l)
        })
}

/// How many terrain tiles marked drawn at one level lie outside the views:
/// a measure for the window's header, never a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outside {
    pub level: u32,
    /// Tiles marked drawn at this level.
    pub drawn: u32,
    /// Of them, outside every one of the views asked about.
    pub outside: u32,
}

/// The vertical slack of a tile's sphere, metres: the terrain's relief the
/// sphere does not know. The test is only ever a "surely outside".
const RELIEF_M: f64 = 60.0;

/// Whether the terrain tile `(level, x, y)` is inside one of `views`'
/// frusta, as a sphere about its ground centre (`RELIEF_M`).
pub fn tile_in_views(tile: (u32, u64, u64), views: &[tuile_core::traversal::ViewState]) -> bool {
    use tuile_core::geo::{geodetic_to_ecef, Geodetic};
    use tuile_core::math::{BoundingVolume, Sphere};
    const R: f64 = 6_371_000.0;
    let (w, s, e, n) = tile_rect(tile.0, tile.1, tile.2);
    let (lon, lat) = ((w + e) / 2.0, (s + n) / 2.0);
    let half = 0.5 * ((n - s) * R).hypot((e - w) * R * lat.cos());
    let volume = BoundingVolume::Sphere(Sphere {
        center: geodetic_to_ecef(Geodetic {
            lon,
            lat,
            height: 0.0,
        }),
        radius: half + RELIEF_M,
    });
    views.iter().any(|v| volume.intersects_frustum(v.frustum()))
}

/// The outline of a picked tile for the main view, and its arrow when the
/// tile is out of the frame: lines in the overlay, never the ground's mesh.
#[derive(Debug, Clone, Default)]
pub struct Outline {
    pub mesh: Mesh,
    pub labels: Vec<crate::Label>,
}

/// Draws the outline of terrain tile `tile` in `colour`, for a frame of
/// `size` device pixels. `project` takes `(lon, lat)` on the ground to the
/// pixel it lands on (`None` behind the eye), unclipped. A tile with no
/// edge point in the frame gets an arrow at the frame's edge toward it,
/// with `distance_m`.
pub fn outline(
    tile: (u32, u64, u64),
    colour: [f32; 4],
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
    size: (f64, f64),
    distance_m: f64,
    scale: f64,
) -> Outline {
    const STEPS: usize = 8;
    let (w, s, e, n) = tile_rect(tile.0, tile.1, tile.2);
    let corners = [(w, s), (e, s), (e, n), (w, n)];
    let mut ring: Vec<(f64, f64)> = Vec::new();
    for i in 0..4 {
        let (a, b) = (corners[i], corners[(i + 1) % 4]);
        for k in 0..STEPS {
            let t = k as f64 / STEPS as f64;
            ring.push((a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t));
        }
    }
    let px: Vec<Option<(f64, f64)>> = ring.iter().map(|&(lo, la)| project(lo, la)).collect();
    let inside = |q: (f64, f64)| q.0 >= 0.0 && q.1 >= 0.0 && q.0 <= size.0 && q.1 <= size.1;
    let mut out = Outline::default();
    let width = 2.0 * scale.max(1.0);
    let quad = |mesh: &mut Mesh, a: (f64, f64), b: (f64, f64), w: f64| {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let l = dx.hypot(dy);
        if !(1e-6..=1.0e5).contains(&l) {
            return;
        }
        let (nx, ny) = (-dy / l * w / 2.0, dx / l * w / 2.0);
        let v = |x: f64, y: f64| crate::Vertex::new(x, y, colour);
        for q in [
            (a.0 + nx, a.1 + ny),
            (b.0 + nx, b.1 + ny),
            (a.0 - nx, a.1 - ny),
            (a.0 - nx, a.1 - ny),
            (b.0 + nx, b.1 + ny),
            (b.0 - nx, b.1 - ny),
        ] {
            mesh.push(v(q.0, q.1));
        }
    };
    for i in 0..px.len() {
        if let (Some(a), Some(b)) = (px[i], px[(i + 1) % px.len()]) {
            quad(&mut out.mesh, a, b, width);
        }
    }
    if px.iter().flatten().any(|&q| inside(q)) {
        return out;
    }
    // Out of the frame: an arrow at its edge, toward the tile's centre.
    let centre = project((w + e) / 2.0, (s + n) / 2.0);
    let (cx, cy) = (size.0 / 2.0, size.1 / 2.0);
    let (mut dx, mut dy) = centre.map_or((0.0, 1.0), |c| (c.0 - cx, c.1 - cy));
    let l = dx.hypot(dy);
    if l < 1e-6 {
        (dx, dy) = (0.0, 1.0);
    } else {
        (dx, dy) = (dx / l, dy / l);
    }
    let margin = 28.0 * scale.max(1.0);
    let t = ((cx - margin) / dx.abs().max(1e-9)).min((cy - margin) / dy.abs().max(1e-9));
    let tip = (cx + dx * t, cy + dy * t);
    let k = 14.0 * scale.max(1.0);
    let back = (tip.0 - dx * k * 1.6, tip.1 - dy * k * 1.6);
    let side = (-dy * k, dx * k);
    for q in [
        tip,
        (back.0 + side.0, back.1 + side.1),
        (back.0 - side.0, back.1 - side.1),
    ] {
        out.mesh.push(crate::Vertex::new(q.0, q.1, colour));
    }
    let text = if distance_m >= 1000.0 {
        format!("{:.1} km", distance_m / 1000.0)
    } else {
        format!("{distance_m:.0} m")
    };
    let px_size = (12.0 * scale.max(1.0)) as f32;
    let mask = Font::bundled().rasterize(&text, px_size);
    let (mw, mh) = (f64::from(mask.width), f64::from(mask.height));
    out.labels.push(crate::Label {
        mask,
        position: [
            (back.0 - dx * k * 0.5 - mw / 2.0).round() as f32,
            (back.1 - dy * k * 0.5 - mh / 2.0).round() as f32,
        ],
        color: colour,
    });
    out
}

/// The terrain tiles marked drawn in `report`, by level, and how many of
/// them are outside all of `views`' frusta. A tile is a sphere about its
/// ground centre, the radius its half diagonal and `RELIEF_M`: a tile
/// counted outside is outside by more than its own size and relief.
pub fn drawn_outside(
    report: &HeldReport,
    views: &[tuile_core::traversal::ViewState],
) -> Vec<Outside> {
    use tuile_core::geo::{geodetic_to_ecef, Geodetic};
    use tuile_core::math::{BoundingVolume, Sphere};
    const R: f64 = 6_371_000.0;
    let mut out: std::collections::BTreeMap<u32, Outside> = std::collections::BTreeMap::new();
    for (&(l, x, y), t) in &terrain_tiles(report) {
        if t.state != RingState::Drawn {
            continue;
        }
        let (w, s, e, n) = tile_rect(l, x, y);
        let (lon, lat) = ((w + e) / 2.0, (s + n) / 2.0);
        let half = 0.5 * ((n - s) * R).hypot((e - w) * R * lat.cos());
        let volume = BoundingVolume::Sphere(Sphere {
            center: geodetic_to_ecef(Geodetic {
                lon,
                lat,
                height: 0.0,
            }),
            radius: half + RELIEF_M,
        });
        let seen = views.iter().any(|v| volume.intersects_frustum(v.frustum()));
        let c = out.entry(l).or_insert(Outside {
            level: l,
            ..Outside::default()
        });
        c.drawn += 1;
        c.outside += u32::from(!seen);
    }
    out.into_values().collect()
}

/// How a reason is marked on a cell. A table of configuration: the view
/// does not know which reasons the budget publishes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mark {
    /// A frame of this colour.
    Frame(u32),
    /// A dark square in the middle.
    Square,
}

/// The marks of the reasons the view has a colour for.
pub const MARKS: [(&str, Mark); 3] = [
    ("ring", Mark::Frame(0xFFFFFF)),
    ("ancestor", Mark::Frame(0xE69F00)),
    ("left over", Mark::Square),
];

/// The mark of any other reason: a thin grey frame, never nothing.
pub const NEUTRAL_MARK: Mark = Mark::Frame(0x888888);

/// The mark of a reason.
pub fn mark_of(reason: &str) -> Mark {
    MARKS
        .iter()
        .find(|(r, _)| *r == reason)
        .map_or(NEUTRAL_MARK, |(_, m)| *m)
}

/// The reason a tile held and not drawn is held, as the report publishes
/// it; `None` for a tile that is drawn or not held.
pub fn why(t: &RingTile) -> Option<&'static str> {
    match t.state {
        RingState::Floor | RingState::Kept | RingState::Wanted | RingState::Left => {
            Some(if t.reason.is_empty() {
                "unranked"
            } else {
                t.reason
            })
        }
        _ => None,
    }
}

/// The held, undrawn terrain tiles by published reason.
pub fn why_counts(report: &HeldReport) -> std::collections::BTreeMap<&'static str, usize> {
    let mut out = std::collections::BTreeMap::new();
    for t in report.ring.iter().filter(|t| t.class == Class::TerrainMesh) {
        if let Some(w) = why(t) {
            *out.entry(w).or_default() += 1;
        }
    }
    out
}

/// What the plan shows of the levels around the selected one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// The selected level; `None`: [`auto_level`].
    pub level: Option<u32>,
    /// Show the levels coarser than the selected one.
    pub below: bool,
    /// Show the levels finer than the selected one.
    pub above: bool,
    /// Only the levels within this many of the selected one; `None`: all.
    pub span: Option<u32>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            level: None,
            below: true,
            above: true,
            span: None,
        }
    }
}

impl Options {
    /// Whether level `l` is shown when `shown` is selected.
    pub fn shows(&self, shown: u32, l: u32) -> bool {
        match l.cmp(&shown) {
            std::cmp::Ordering::Equal => true,
            std::cmp::Ordering::Less => self.below && self.span.is_none_or(|s| shown - l <= s),
            std::cmp::Ordering::Greater => self.above && self.span.is_none_or(|s| l - shown <= s),
        }
    }

    /// The next span: all, within 2, within 1, back to all.
    pub fn next_span(self) -> Options {
        Options {
            span: match self.span {
                None => Some(2),
                Some(2) => Some(1),
                _ => None,
            },
            ..self
        }
    }
}

/// One cell of the plan's mosaic: a published terrain tile, where it lands.
#[derive(Debug, Clone, Copy)]
pub struct Cell<'a> {
    pub tile: (u32, u64, u64),
    /// Left, top, right, bottom, clipped to the plan, points.
    pub rect: (f64, f64, f64, f64),
    pub t: &'a RingTile,
}

/// The cells by level and state: what the plan shows, to set against what
/// the report holds.
pub fn cell_counts(cells: &[Cell<'_>]) -> std::collections::BTreeMap<(u32, &'static str), usize> {
    let mut out = std::collections::BTreeMap::new();
    for c in cells {
        *out.entry((c.tile.0, c.t.state.label())).or_default() += 1;
    }
    out
}

/// The least side of a cell worth drawing, points.
const MIN_CELL: f64 = 0.75;

/// The cells of the mosaic: every terrain tile of the levels `opts` shows
/// that the plan `lo..hi` covers, coarse levels first so that finer ones
/// lie over them. `to_px` takes `(lon, lat)` to a point of the plan.
pub fn cells<'a>(
    report: &'a HeldReport,
    shown: u32,
    opts: &Options,
    to_px: &dyn Fn(f64, f64) -> (f64, f64),
    lo: (f64, f64),
    hi: (f64, f64),
) -> Vec<Cell<'a>> {
    let mut out: Vec<Cell<'a>> = terrain_tiles(report)
        .into_iter()
        .filter(|&((l, _, _), _)| opts.shows(shown, l))
        .filter_map(|(tile, t)| {
            let (w, s, e, n) = tile_rect(tile.0, tile.1, tile.2);
            let (x0, y1) = to_px(w, s);
            let (x1, y0) = to_px(e, n);
            let rect = clamp_rect((x0, y0, x1, y1), lo, hi)?;
            (rect.2 - rect.0 >= MIN_CELL && rect.3 - rect.1 >= MIN_CELL).then_some(Cell {
                tile,
                rect,
                t,
            })
        })
        .collect();
    out.sort_by_key(|c| (c.tile.0, c.tile.1, c.tile.2));
    out
}

/// The reason of a tile in words: the category the report publishes.
pub fn reason(t: &RingTile) -> String {
    match t.state {
        RingState::Drawn => "drawn now".to_owned(),
        _ => format!("reason: {}", why(t).unwrap_or("not held")),
    }
}

/// What the tooltip of `t` says: its place, state, score and the terms.
fn legend_of(t: &RingTile, (l, x, y): (u32, u64, u64)) -> Vec<Legend> {
    let c = f64::from(CERTAIN);
    let frac = |v: u32| f64::from(v) / c;
    let w = &t.terms;
    vec![
        Legend {
            swatch: Swatch::Fill(state_colour(t.state, w.score)),
            text: format!("tile {l}/{x}/{y} · {}", t.state.label()),
        },
        Legend {
            swatch: Swatch::None,
            text: format!("score {} µflouz per reference byte", w.score),
        },
        Legend {
            swatch: Swatch::None,
            text: format!(
                "A {:.2} · D {:.2} · P {:.2} · R {:.0} ms · {} KiB",
                frac(w.a),
                frac(w.d),
                frac(w.p),
                w.r_us as f64 / 1000.0,
                w.bytes >> 10,
            ),
        },
        Legend {
            swatch: Swatch::None,
            text: format!(
                "distance {} tiles · {}",
                tuile_core::budget::score::DISTANCE_BAND_LABELS
                    [usize::from(t.band).min(tuile_core::budget::score::DISTANCE_BANDS - 1)],
                reason(t)
            ),
        },
    ]
}

/// The counts of the ring by state.
pub fn counts(report: &HeldReport) -> [(RingState, usize); 8] {
    use RingState::*;
    [Drawn, Floor, Kept, Wanted, Left, InFlight, Queued, Hole].map(|s| {
        (
            s,
            report
                .ring
                .iter()
                .filter(|t| t.class == Class::TerrainMesh && t.state == s)
                .count(),
        )
    })
}

fn seg(p: &mut Painter<'_>, a: (f64, f64), b: (f64, f64), w: f64, c: [f32; 4]) {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let n = (dx * dx + dy * dy).sqrt();
    if n < 1e-9 {
        return;
    }
    let (nx, ny) = (-dy / n * w / 2.0, dx / n * w / 2.0);
    p.tri(
        [
            (a.0 + nx, a.1 + ny),
            (b.0 + nx, b.1 + ny),
            (a.0 - nx, a.1 - ny),
        ],
        c,
    );
    p.tri(
        [
            (a.0 - nx, a.1 - ny),
            (b.0 + nx, b.1 + ny),
            (b.0 - nx, b.1 - ny),
        ],
        c,
    );
}

/// A frame of width `w` just inside `r`.
fn frame(p: &mut Painter<'_>, r: (f64, f64, f64, f64), w: f64, c: [f32; 4]) {
    p.rect(r.0, r.1, r.2 - r.0, w, c);
    p.rect(r.0, r.3 - w, r.2 - r.0, w, c);
    p.rect(r.0, r.1, w, r.3 - r.1, c);
    p.rect(r.2 - w, r.1, w, r.3 - r.1, c);
}

/// The mark of a reason on a cell, from [`MARKS`] or neutral. Never a
/// fill: the score's colour stays under it.
fn mark(p: &mut Painter<'_>, why: Option<&str>, r: (f64, f64, f64, f64)) {
    let Some(why) = why else { return };
    match mark_of(why) {
        Mark::Frame(c) => frame(p, r, 1.5, srgb(c, 1.0)),
        Mark::Square => {
            let (w, h) = (r.2 - r.0, r.3 - r.1);
            p.rect(
                r.0 + w * 0.35,
                r.1 + h * 0.35,
                w * 0.3,
                h * 0.3,
                srgb(0x101010, 0.85),
            );
        }
    }
}

fn clamp_rect(
    r: (f64, f64, f64, f64),
    lo: (f64, f64),
    hi: (f64, f64),
) -> Option<(f64, f64, f64, f64)> {
    let (x0, y0, x1, y1) = (r.0.max(lo.0), r.1.max(lo.1), r.2.min(hi.0), r.3.min(hi.1));
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1, y1))
}

/// Draws the reservoir for `report` (none yet: a line saying so), the eye
/// `eye`, at `level` (`None`: [`auto_level`]), at `scale` device pixels per
/// point.
pub fn draw(
    report: Option<&HeldReport>,
    eye: Option<&ReservoirEye>,
    opts: &Options,
    selected: Option<(u32, u64, u64)>,
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
    let text = srgb(TEXT, 1.0);
    let dim = srgb(DIM, 1.0);
    p.text(MARGIN, MARGIN, "Preload reservoir", 13.0, text);
    let Some(report) = report.filter(|r| !r.ring.is_empty()) else {
        p.text(
            MARGIN,
            MARGIN + 22.0,
            "waiting for the budget's next report (every half second)…",
            EM,
            dim,
        );
        return finish(p, scale);
    };
    let tiles = terrain_tiles(report);
    let shown = opts
        .level
        .or_else(|| auto_level(report, eye.map(|e| (e.lon, e.lat))));
    let top = MARGIN + 44.0;
    let left = MARGIN;
    let counts = counts(report);
    let n = |st: RingState| counts.iter().find(|(s, _)| *s == st).map_or(0, |c| c.1);
    let line = counts
        .iter()
        .filter(|(s, c)| *c > 0 && *s != RingState::Left)
        .map(|(s, c)| format!("{c} {}", s.label()))
        .collect::<Vec<_>>()
        .join(" · ");
    p.text(MARGIN, MARGIN + 20.0, &line, EM, dim);
    let by_class = |c: Class| report.ring.iter().filter(|t| t.class == c).count();
    let wanted = n(RingState::Wanted);
    let stale = n(RingState::Left);
    let hot = srgb(0xE69F00, 1.0);
    p.text(
        MARGIN,
        MARGIN + 36.0,
        &format!(
            "{stale} held, left: copies no longer wanted, released first when room is needed — {} the {wanted} held on purpose",
            if wanted > 0 { format!("{:.1}×", stale as f64 / wanted as f64) } else { "∞ ×".to_owned() },
        ),
        EM,
        hot,
    );
    p.text(
        MARGIN,
        MARGIN + 52.0,
        &format!(
            "{} resources listed: {} terrain meshes · {} drapes · {} 3D tiles — the counts and the plan are terrain meshes",
            report.ring.len(),
            by_class(Class::TerrainMesh),
            by_class(Class::Drape),
            by_class(Class::Tiles3dMesh),
        ),
        EM - 1.0,
        dim,
    );
    let held_why = why_counts(report)
        .iter()
        .map(|(w, c)| format!("{w} {c}"))
        .collect::<Vec<_>>()
        .join(" · ");
    p.text(
        MARGIN,
        MARGIN + 112.0,
        &format!("held, not drawn, by reason: {held_why}"),
        EM - 1.0,
        dim,
    );
    p.text(
        MARGIN,
        MARGIN + 126.0,
        "marks: white frame = ring · orange frame = ancestor · dark square = left over · grey frame = any other reason · yellow = picked",
        EM - 1.0,
        dim,
    );
    let top = top + 96.0;
    let Some(shown) = shown else {
        p.text(left, top, "no terrain tile drawn yet", EM, dim);
        return finish(p, scale);
    };
    let auto = opts.level.is_none();
    let total = |v: &[Outside]| {
        v.iter()
            .fold((0, 0), |a, o| (a.0 + o.outside, a.1 + o.drawn))
    };
    if let Some(e) = eye {
        let (o, d) = total(&e.outside_present);
        let (oa, _) = total(&e.outside_any);
        let worst: Vec<String> = e
            .outside_present
            .iter()
            .filter(|c| c.outside > 0)
            .map(|c| format!("L{} {}/{}", c.level, c.outside, c.drawn))
            .collect();
        p.text(
            MARGIN,
            MARGIN + 84.0,
            &format!(
                "drawn out of the present view: {o} of {d} ({oa} out of every view) · traversals truncated: {}",
                e.truncated
            ),
            EM - 1.0,
            srgb(0xE69F00, 1.0),
        );
        p.text(
            MARGIN,
            MARGIN + 98.0,
            &format!(
                "  by level (out/drawn): {}",
                if worst.is_empty() {
                    "none".to_owned()
                } else {
                    worst.join(", ")
                }
            ),
            EM - 1.0,
            dim,
        );
    }
    let head = format!(
        "level {shown}{} · [ ] level · A auto · B below {} · U above {} · W span {}{}",
        if auto { " (auto)" } else { "" },
        if opts.below { "on" } else { "off" },
        if opts.above { "on" } else { "off" },
        opts.span.map_or("all".to_owned(), |n| format!("±{n}")),
        eye.map_or(String::new(), |e| format!(
            " · eye {:.4}°, {:.4}° at {:.0} m",
            e.lon.to_degrees(),
            e.lat.to_degrees(),
            e.height_m
        )),
    );
    p.text(MARGIN, MARGIN + 70.0, &head, EM, text);

    // The plan: east right, north up, the eye at the centre.
    let (cx, cy) = (left + PLAN / 2.0, top + 22.0 + PLAN / 2.0);
    let (plan_top, plan_left) = (top + 22.0, left);
    let (lon0, lat0) = eye.map_or_else(
        || {
            // Without an eye: the centre of the drawn tiles at this level.
            let (mut sx, mut sy, mut n) = (0.0, 0.0, 0.0);
            for (&(l, x, y), t) in &tiles {
                if l == shown && t.state == RingState::Drawn {
                    let r = tile_rect(l, x, y);
                    sx += (r.0 + r.2) / 2.0;
                    sy += (r.1 + r.3) / 2.0;
                    n += 1.0;
                }
            }
            if n > 0.0 {
                (sx / n, sy / n)
            } else {
                (0.0, 0.0)
            }
        },
        |e| (e.lon, e.lat),
    );
    const R: f64 = 6_371_000.0;
    let kx = lat0.cos().max(0.01) * R;
    let (_, s0, _, n0) = tile_rect(shown, 0, 0);
    let tile_m = (n0 - s0) * R;
    let half_m = PLAN_TILES * tile_m;
    let k = (PLAN / 2.0) / half_m;
    let wrap = |d: f64| {
        (d + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU) - std::f64::consts::PI
    };
    let to_px = |lon: f64, lat: f64| (cx + wrap(lon - lon0) * kx * k, cy - (lat - lat0) * R * k);
    let (lo, hi) = ((plan_left, plan_top), (plan_left + PLAN, plan_top + PLAN));
    p.rect(plan_left, plan_top, PLAN, PLAN, srgb(0x1A1A1A, 1.0));
    // The grid of the selected level, dark: a cell nothing holds is black.
    let (ex, ey) = tile_at(shown, lon0, lat0);
    let reach = PLAN_TILES.ceil() as i64 + 1;
    let max_x = (2u64 << shown) as i64;
    let max_y = (1u64 << shown) as i64;
    for dy in -reach..=reach {
        for dx in -reach..=reach {
            let (gx, gy) = (ex as i64 + dx, ey as i64 + dy);
            if gx < 0 || gy < 0 || gx >= max_x || gy >= max_y {
                continue;
            }
            let (w, s, e2, n) = tile_rect(shown, gx as u64, gy as u64);
            let (x0, y1) = to_px(w, s);
            let (x1, y0) = to_px(e2, n);
            if let Some((x0, y0, x1, y1)) =
                clamp_rect((x0 + 0.5, y0 + 0.5, x1 - 0.5, y1 - 0.5), lo, hi)
            {
                p.rect(x0, y0, x1 - x0, y1 - y0, srgb(0x262626, 1.0));
            }
        }
    }
    // The mosaic: every level shown, each tile its own size, coarse first.
    let mosaic = cells(report, shown, opts, &to_px, lo, hi);
    for c in &mosaic {
        let (x0, y0, x1, y1) = c.rect;
        let (l, gx, gy) = c.tile;
        let (rx, ry, rw, rh) = (x0 + 0.3, y0 + 0.3, x1 - x0 - 0.6, y1 - y0 - 0.6);
        let own = l == shown;
        let colour = match c.t.state {
            RingState::Drawn if !own => srgb(0x007A58, 1.0),
            st => state_colour(st, c.t.terms.score),
        };
        if own || c.t.state == RingState::Drawn {
            // The level selected in full, drawn tiles of any level solid.
            p.rect(rx, ry, rw, rh, colour);
            if own {
                mark(&mut p, why(c.t), (x0, y0, x1, y1));
            }
        } else {
            // Another level held and not drawn: a faint fill and an outline,
            // under the finer tiles drawn over it, never hiding them.
            let mut faint = colour;
            faint[3] *= 0.22;
            p.rect(rx, ry, rw, rh, faint);
            frame(&mut p, (x0, y0, x1, y1), 1.0, colour);
        }
        if selected == Some(c.tile) {
            frame(&mut p, (x0, y0, x1, y1), 2.5, srgb(0xF0E442, 1.0));
        }
        p.hit_tile(
            x0,
            y0,
            x1 - x0,
            y1 - y0,
            legend_of(c.t, (l, gx, gy)),
            (l, gx, gy),
        );
    }
    // The camera: its footprint on the ground, the cone from the eye to its
    // far corners, the eye with its heading.
    if let Some(e) = eye {
        // A point of the footprint past the plan (the horizon) is held on its
        // edge, along the ray from the eye.
        let inside = |q: (f64, f64)| {
            let (dx, dy) = (q.0 - cx, q.1 - cy);
            let t = [
                if dx.abs() > 1e-9 {
                    (PLAN / 2.0 - 1.0) / dx.abs()
                } else {
                    f64::MAX
                },
                if dy.abs() > 1e-9 {
                    (PLAN / 2.0 - 1.0) / dy.abs()
                } else {
                    f64::MAX
                },
            ]
            .into_iter()
            .fold(1.0f64, f64::min);
            (cx + dx * t, cy + dy * t)
        };
        let pts: Vec<(f64, f64)> = e
            .footprint
            .iter()
            .map(|&(lo_, la)| inside(to_px(lo_, la)))
            .collect();
        if pts.len() >= 3 {
            let c = pts.iter().fold((0.0, 0.0), |a, q| {
                (a.0 + q.0 / pts.len() as f64, a.1 + q.1 / pts.len() as f64)
            });
            for i in 0..pts.len() {
                let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
                p.tri([c, a, b], srgb(EYE, 0.16));
                seg(&mut p, a, b, 1.5, srgb(EYE, 0.9));
            }
            for far in [
                pts[pts.len() / 2],
                pts[pts.len() / 2 + 1..].first().copied().unwrap_or(pts[0]),
            ] {
                seg(&mut p, (cx, cy), far, 1.0, srgb(EYE, 0.7));
            }
        }
        let (sn, cs) = e.heading.sin_cos();
        let pt = |f: f64, s: f64| (cx + sn * f + cs * s, cy - cs * f + sn * s);
        p.tri(
            [pt(11.0, 0.0), pt(-6.0, 6.0), pt(-6.0, -6.0)],
            srgb(EYE, 1.0),
        );
    }
    // The section: the chain of tiles under the eye, level by level.
    let sx = left + PLAN + 24.0;
    p.text(
        sx,
        top - 2.0,
        "The tile under the eye, at each level",
        EM,
        text,
    );
    p.text(
        sx,
        top + 10.0,
        "state · score in µflouz per reference byte",
        EM - 1.0,
        dim,
    );
    let deepest = tiles.keys().map(|k| k.0).max().unwrap_or(shown);
    let mut y = plan_top;
    for l in 0..=deepest {
        let (x, yy) = tile_at(l, lon0, lat0);
        let sel = l == shown;
        if sel {
            p.rect(sx - 4.0, y - 1.0, SECTION + 8.0, ROW, srgb(0xFFFFFF, 0.12));
        }
        p.text(sx, y, &format!("L{l}"), EM, if sel { text } else { dim });
        match tiles.get(&(l, x, yy)) {
            Some(t) => {
                p.rect(
                    sx + 34.0,
                    y + 1.0,
                    26.0,
                    ROW - 4.0,
                    state_colour(t.state, t.terms.score),
                );
                p.text(
                    sx + 68.0,
                    y,
                    &format!("{} · score {}", t.state.label(), t.terms.score),
                    EM,
                    dim,
                );
                p.hit_tile(sx, y, SECTION, ROW, legend_of(t, (l, x, yy)), (l, x, yy));
            }
            None => {
                p.rect(sx + 34.0, y + 1.0, 26.0, ROW - 4.0, srgb(0x262626, 1.0));
                p.text(sx + 68.0, y, "not held", EM, dim);
            }
        }
        y += ROW;
    }
    {
        let counts = cell_counts(&mosaic);
        let by_state: Vec<String> = RingState::ALL
            .iter()
            .filter_map(|st| {
                let n: usize = counts
                    .iter()
                    .filter(|((_, l), _)| *l == st.label())
                    .map(|(_, c)| *c)
                    .sum();
                (n > 0).then(|| format!("{n} {}", st.label()))
            })
            .collect();
        let levels: std::collections::BTreeSet<u32> = counts.keys().map(|k| k.0).collect();
        p.text(
            left,
            plan_top + PLAN + 24.0,
            &format!(
                "the plan lays out {} cells, levels {}–{}: {}",
                mosaic.len(),
                levels.first().copied().unwrap_or(shown),
                levels.last().copied().unwrap_or(shown),
                by_state.join(" · ")
            ),
            EM - 1.0,
            dim,
        );
    }
    if let Some((sl, sx_, sy_)) = selected {
        let line = match tiles.get(&(sl, sx_, sy_)) {
            Some(t) => format!(
                "selected {sl}/{sx_}/{sy_}: {} — {}{}",
                t.state.label(),
                if t.state == RingState::Drawn {
                    "outlined in the view"
                } else {
                    "not drawn: outline only in the view"
                },
                match eye.and_then(|e| e.selected_in_view) {
                    Some(true) => ", in the view",
                    Some(false) => ", out of the view (arrow at the edge)",
                    None => "",
                }
            ),
            None => format!("selected {sl}/{sx_}/{sy_}: not held"),
        };
        p.text(left, plan_top + PLAN + 6.0, &line, EM, srgb(0xF0E442, 1.0));
    }
    // The legend.
    let mut ly = plan_top + PLAN + 42.0;
    let mut lx = left;
    for (c, label) in [
        (srgb(DRAWN, 1.0), "drawn"),
        (srgb(IN_FLIGHT, 1.0), "in flight"),
        (srgb(QUEUED, 0.35), "queued"),
        (srgb(HOLE, 1.0), "missing (hole)"),
    ] {
        p.rect(lx, ly + 1.0, 12.0, ROW - 4.0, c);
        lx += 16.0 + p.text(lx + 16.0, ly, label, EM, text) + 14.0;
    }
    p.text(lx, ly, "white: eye, cone, footprint", EM, dim);
    ly += ROW + 4.0;
    p.text(left, ly, "held, not drawn — by score:", EM, text);
    let bx = left + 190.0;
    let steps = 48;
    for i in 0..steps {
        let t = f64::from(i) / f64::from(steps - 1);
        let score = if i == 0 {
            0
        } else {
            (t * HOT_BUCKET).exp2() as u64
        };
        p.rect(
            bx + f64::from(i) * 5.0,
            ly + 1.0,
            5.0,
            ROW - 4.0,
            score_colour(score),
        );
    }
    ly += ROW;
    p.text(bx, ly, "0 (cold)", EM - 1.0, dim);
    p.text(bx + 120.0 - 8.0, ly, "2^10", EM - 1.0, dim);
    p.text(bx + 240.0 - 40.0, ly, "2^20+ µflouz/B (hot)", EM - 1.0, dim);
    finish(p, scale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::budget::score::{HeldReport, RingState, RingTile, Terms};
    use tuile_core::budget::TileId;

    fn tile(l: u32, x: u64, y: u64, state: RingState, score: u64) -> RingTile {
        RingTile {
            resource: ResourceId::Mesh(TileId::from_terrain(l, x, y)),
            class: Class::TerrainMesh,
            level: l,
            band: 1,
            terms: Terms {
                score,
                ..Terms::default()
            },
            state,
            reason: "",
        }
    }

    fn luma(c: [f32; 4]) -> f32 {
        0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
    }

    /// The ramp only warms: the score's position on it never goes down as
    /// the score goes up, and its brightness follows.
    #[test]
    fn the_score_ramp_is_monotone() {
        let scores: Vec<u64> = (0..40)
            .map(|i| if i == 0 { 0 } else { 1u64 << i.min(40) })
            .collect();
        let mut last_t = -1.0;
        let mut last_l = -1.0;
        for s in scores {
            let t = score_t(s);
            let l = luma(score_colour(s));
            assert!(t >= last_t, "{s}: t {t} < {last_t}");
            assert!(l >= last_l - 1e-4, "{s}: luma {l} < {last_l}");
            (last_t, last_l) = (t, l);
        }
        assert_eq!(score_t(0), 0.0);
        assert_eq!(score_t(u64::MAX), 1.0);
        assert!(luma(score_colour(u64::MAX)) > luma(score_colour(0)) + 0.5);
    }

    /// Drawn, in flight, queued and holes keep their own colours whatever
    /// the score, and none is a point of the ramp.
    #[test]
    fn states_are_told_apart_by_colour() {
        let ramp: Vec<[f32; 4]> = [0, 1, 1 << 10, 1 << 20]
            .iter()
            .map(|&s| score_colour(s))
            .collect();
        for st in [RingState::Drawn, RingState::InFlight, RingState::Hole] {
            let c = state_colour(st, 5);
            assert_eq!(c, state_colour(st, 1 << 30), "{st:?} ignores its score");
            assert!(!ramp.contains(&c));
        }
        assert_eq!(state_colour(RingState::Wanted, 7), score_colour(7));
        assert_ne!(
            state_colour(RingState::Drawn, 0),
            state_colour(RingState::InFlight, 0)
        );
    }

    #[test]
    fn a_tile_holds_the_point_it_was_found_by() {
        for (lon, lat, l) in [
            (-0.0092f64, 0.783f64, 14u32),
            (2.0, -0.5, 3),
            (-3.1, 1.5, 20),
        ] {
            let (x, y) = tile_at(l, lon, lat);
            let (w, s, e, n) = tile_rect(l, x, y);
            assert!(w <= lon && lon <= e && s <= lat && lat <= n, "{l}/{x}/{y}");
        }
    }

    /// The auto level is the finest where tiles are held and not drawn
    /// (three at least); with none, the level of most drawn tiles.
    #[test]
    fn the_auto_level_is_the_finest_where_the_ring_is() {
        let mut r = HeldReport::default();
        for i in 0..3 {
            r.ring.push(tile(12, i, 0, RingState::Drawn, 5));
        }
        for i in 0..2 {
            r.ring.push(tile(13, i, 0, RingState::Drawn, 5));
        }
        assert_eq!(
            auto_level(&r, None),
            Some(12),
            "no ring: the most drawn level"
        );
        for i in 0..4 {
            r.ring.push(tile(13, i + 10, 0, RingState::Wanted, 50));
        }
        r.ring.push(tile(15, 0, 0, RingState::Left, 1));
        r.ring.push(tile(16, 0, 0, RingState::Floor, 1));
        for i in 0..5 {
            r.ring.push(tile(14, i, 0, RingState::Kept, 9));
        }
        assert_eq!(
            auto_level(&r, None),
            Some(14),
            "the finest level with a ring: a stray at 15 and the floor at 16 do not count"
        );
        assert_eq!(auto_level(&HeldReport::default(), None), None);
        // Near an eye far from every tile, none is shown at a level of its own.
        assert_eq!(
            auto_level(&r, Some((2.0, -1.0))),
            None,
            "nothing near the eye"
        );
        assert_eq!(terrain_tiles(&r).len(), 16);
    }

    /// A camera at 800 m looking 50° below the horizon does not see the
    /// ground under it: a tile there is outside, one ahead is inside.
    #[test]
    fn a_drawn_tile_under_the_eye_is_outside_a_tilted_view() {
        let (lon, lat) = (-0.0092f64, 0.7832f64);
        let cam = tuile_camera::GlobeCamera::from_geodetic(
            lat,
            lon,
            800.0,
            0.0,
            50f64.to_radians(),
            45f64.to_radians(),
        );
        let view = cam.view_state(glam_viewport());
        let mut r = HeldReport::default();
        let l = 18;
        let (x, y) = tile_at(l, lon, lat);
        r.ring.push(tile(l, x, y, RingState::Drawn, 1));
        // About 800 m ahead (north) at 50° pitch the ray meets the ground.
        let ahead = tile_at(l, lon, lat + 670.0 / 6_371_000.0);
        r.ring.push(tile(l, ahead.0, ahead.1, RingState::Drawn, 1));
        let o = drawn_outside(&r, &[view]);
        assert_eq!(
            o,
            vec![Outside {
                level: l,
                drawn: 2,
                outside: 1
            }]
        );
    }

    fn glam_viewport() -> glam::DVec2 {
        glam::DVec2::new(1600.0, 900.0)
    }

    /// A picked tile held and not drawn still gets its outline in the main
    /// view; out of the frame it gets an arrow and its distance.
    #[test]
    fn a_picked_tile_has_an_outline_or_an_edge_arrow() {
        let l = 12;
        let (x, y) = tile_at(l, 0.1, 0.6);
        let (w, s, e, n) = tile_rect(l, x, y);
        let (lon0, lat0) = ((w + e) / 2.0, (s + n) / 2.0);
        let k = 1.0 / (e - w); // one tile is one pixel wide per k
        let at = |dx: f64| {
            move |lon: f64, lat: f64| {
                Some((
                    200.0 + (lon - lon0) * k * 100.0 + dx,
                    150.0 - (lat - lat0) * k * 100.0,
                ))
            }
        };
        let seen = outline(
            (l, x, y),
            score_colour(5),
            &at(0.0),
            (400.0, 300.0),
            900.0,
            1.0,
        );
        assert_eq!(
            seen.mesh.len(),
            4 * 8 * 6,
            "four edges, eight segments each, two triangles"
        );
        assert!(seen.labels.is_empty());
        let off = outline(
            (l, x, y),
            score_colour(5),
            &at(5_000.0),
            (400.0, 300.0),
            2_500.0,
            1.0,
        );
        assert_eq!(off.labels.len(), 1, "the distance is written at the arrow");
        assert!(off.mesh.len() > 3, "the arrow is drawn");
        assert!(
            off.mesh.iter().rev().take(3).all(|v| v.position[0] > 200.0),
            "it points toward the tile"
        );
    }

    /// The reason of a held tile is what the report publishes; a reason the
    /// view has no colour for is marked neutrally and counted, and the plan
    /// still draws it; taking the ring's reason out of a report breaks nothing.
    #[test]
    fn a_held_tile_has_the_reason_the_report_publishes() {
        let mut t = tile(10, 1, 1, RingState::Wanted, 7);
        assert_eq!(why(&t), Some("unranked"));
        t.reason = "ancestor";
        assert_eq!(why(&t), Some("ancestor"));
        t.reason = "a category nobody told the view about";
        assert_eq!(why(&t), t.reason.into());
        assert_eq!(mark_of(t.reason), NEUTRAL_MARK);
        t.state = RingState::Drawn;
        assert_eq!(why(&t), None);

        // A report with an unknown reason, and one with the ring's reason
        // removed: both draw, and the header counts what is published.
        let mut r = HeldReport::default();
        for (i, reason) in ["ring", "mystery", "mystery", "ancestor"]
            .into_iter()
            .enumerate()
        {
            let mut t = tile(12, i as u64, 0, RingState::Wanted, 9);
            t.reason = reason;
            r.ring.push(t);
        }
        r.ring.push(tile(12, 9, 0, RingState::Drawn, 1));
        let mut cache = TextCache::default();
        let eye = None;
        let d = draw(Some(&r), eye, &Options::default(), None, &mut cache, 1.0);
        assert!(!d.hits.is_empty());
        assert_eq!(why_counts(&r).get("mystery"), Some(&2));
        r.ring.retain(|t| t.reason != "ring");
        assert_eq!(why_counts(&r).get("ring"), None);
        let d = draw(Some(&r), eye, &Options::default(), None, &mut cache, 1.0);
        assert!(!d.hits.is_empty());
    }

    /// The mosaic shows every level, each tile at its own size, and what it
    /// lays out is what the report holds, by level and state; the options
    /// take levels away and nothing else.
    #[test]
    fn the_mosaic_lays_out_what_the_report_holds() {
        let (lon, lat) = (0.0, 0.7);
        let mut r = HeldReport::default();
        // Drawn at 14 (coarse), 16 (fine, in the same place), a held ring at 16
        // and an ancestor held at 12, a hole at 16.
        let at = |l: u32, dx: i64, dy: i64| {
            let (x, y) = tile_at(l, lon, lat);
            (x as i64 + dx, y as i64 + dy)
        };
        for (l, dx, dy, st) in [
            (14, 0, 0, RingState::Drawn),
            (14, 1, 0, RingState::Drawn),
            (16, 0, 0, RingState::Drawn),
            (16, 1, 0, RingState::Drawn),
            (16, 2, 0, RingState::Wanted),
            (16, 0, 1, RingState::Hole),
            (12, 0, 0, RingState::Kept),
        ] {
            let (x, y) = at(l, dx, dy);
            r.ring.push(tile(l, x as u64, y as u64, st, 100));
        }
        // A map: 1 point a metre-ish, centred; every tile of those levels lands
        // inside the 400-point plan.
        let (w, s, e, n) = tile_rect(16, tile_at(16, lon, lat).0, tile_at(16, lon, lat).1);
        let k = 40.0 / (e - w);
        let to_px = move |lo: f64, la: f64| {
            (
                200.0 + (lo - (w + e) / 2.0) * k,
                200.0 - (la - (s + n) / 2.0) * k,
            )
        };
        let lo = (0.0, 0.0);
        let hi = (400.0, 400.0);
        let all = cells(&r, 16, &Options::default(), &to_px, lo, hi);
        let held: std::collections::BTreeMap<(u32, &str), usize> =
            r.ring.iter().fold(Default::default(), |mut m, t| {
                *m.entry((t.level, t.state.label())).or_default() += 1;
                m
            });
        // Level 12's tile covers the plan, 14's too: all are in, drawn tiles
        // of both levels among them.
        assert_eq!(cell_counts(&all), held);
        assert_eq!(all.len(), 7);
        // Coarse first: the fine tiles lie over the coarse ones.
        assert!(all.windows(2).all(|p| p[0].tile.0 <= p[1].tile.0));
        // The options take levels away, nothing more.
        let o = Options {
            above: false,
            ..Options::default()
        };
        assert!(cells(&r, 14, &o, &to_px, lo, hi)
            .iter()
            .all(|c| c.tile.0 <= 14));
        let o = Options {
            below: false,
            span: Some(1),
            ..Options::default()
        };
        assert!(cells(&r, 15, &o, &to_px, lo, hi)
            .iter()
            .all(|c| c.tile.0 == 15 || c.tile.0 == 16));
        // The drawn tiles shown are the drawn tiles held.
        let drawn = |c: &[Cell<'_>]| c.iter().filter(|c| c.t.state == RingState::Drawn).count();
        assert_eq!(
            drawn(&all),
            r.ring
                .iter()
                .filter(|t| t.state == RingState::Drawn)
                .count()
        );
    }

    /// The drawing carries one region per tile with its score in the legend,
    /// and shows the ring and the drawn tiles apart.
    #[test]
    fn the_plan_lists_each_tile_with_its_score() {
        let mut r = HeldReport::default();
        let (x, y) = tile_at(10, 0.0, 0.7);
        r.ring.push(tile(10, x, y, RingState::Drawn, 40));
        r.ring.push(tile(10, x + 1, y, RingState::Wanted, 123_456));
        r.ring.push(tile(10, x + 2, y, RingState::Queued, 9));
        let eye = ReservoirEye {
            lon: 0.0,
            lat: 0.7,
            height_m: 900.0,
            heading: 1.0,
            footprint: vec![(0.0, 0.7), (0.0001, 0.7), (0.0001, 0.7001), (0.0, 0.7001)],
            ..ReservoirEye::default()
        };
        let mut cache = TextCache::default();
        let d = draw(
            Some(&r),
            Some(&eye),
            &Options::default(),
            None,
            &mut cache,
            1.0,
        );
        assert!(d
            .hits
            .iter()
            .any(|h| h.lines.iter().any(|l| l.text.contains("score 123456"))));
        assert!(d
            .hits
            .iter()
            .any(|h| h.lines[0].text.contains("held, wanted")));
        assert!(d.hits.iter().any(|h| h.lines[0].text.contains("drawn")));
        // Waiting for a report draws a line, not a plan.
        let none = draw(None, None, &Options::default(), None, &mut cache, 1.0);
        assert!(none.hits.is_empty());
    }
}
