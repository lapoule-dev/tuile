// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! `SimWorld`: a flat square world with terrain and imagery pyramids of
//! different depths — sea in the west, a coastal plain, mountains in the
//! east — cameras seen through lenses, and a traversal that builds the
//! budget's `NeedSet` the way the real one does: SSE refinement with
//! hysteresis on what was drawn, hold-until-ready on the budget's residency,
//! ancestors preloaded, the floor always listed, a coarse ring around the
//! frustum.
//!
//! A view is a camera and a lens (spec §13): the viewport, the scale and
//! four angles, so a pitch, a portrait screen and a headset's asymmetric
//! eye select what they show. Several views — a stereo rig — give one
//! `NeedSet`, the union of the views with the largest need per tile
//! (§13.3).

use std::collections::BTreeSet;

use super::detmath;

use tuile_budget::cost::{drape_cost, Cost, TextureFormat};
use tuile_budget::{
    Bytes, Class, GeoRect, ImageryNeed, Need, NeedSet, ResourceId, TexelFootprint, TileId,
};

/// The world's side, metres.
pub const SIDE: f64 = 2_097_152.0;
/// The SSE threshold, logical pixels.
pub const TAU: f64 = 2.0;
/// The culling margin, radians, added to each of a lens's four angles (the
/// real traversal's `CULL_MARGIN` widens them by a quarter).
pub const CULL_MARGIN: f64 = 0.1;
/// The prefetch ring's extra margin, radians.
pub const RING_MARGIN: f64 = 0.5;

/// A lens (spec §13.1): the viewport in physical pixels, the scale factor
/// (physical pixels per logical point) and the field of view as the
/// tangents of its four angles, OpenXR's signs (`left` and `down`
/// negative). Tangents rather than angles keep the simulation free of
/// `libm`: they come from `detmath`'s sine and cosine.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lens {
    /// Viewport width, physical pixels.
    pub width_px: f64,
    /// Viewport height, physical pixels.
    pub height_px: f64,
    /// Physical pixels per logical point.
    pub scale: f64,
    /// Tangent of the left angle (negative).
    pub tan_left: f64,
    /// Tangent of the right angle.
    pub tan_right: f64,
    /// Tangent of the up angle.
    pub tan_up: f64,
    /// Tangent of the down angle (negative).
    pub tan_down: f64,
}

/// `tan(deg°)`, from `detmath`.
pub fn tan_deg(deg: f64) -> f64 {
    let (s, c) = detmath::sin_cos(deg * std::f64::consts::PI / 180.0);
    s / c
}

impl Lens {
    /// A symmetric lens of a fixed vertical field of view, its horizontal
    /// extent following the viewport's aspect (as `ViewState::perspective`).
    pub fn vertical(width_px: f64, height_px: f64, scale: f64, vfov_deg: f64) -> Lens {
        let up = tan_deg(vfov_deg / 2.0);
        let side = up * width_px / height_px;
        Lens {
            width_px,
            height_px,
            scale,
            tan_left: -side,
            tan_right: side,
            tan_up: up,
            tan_down: -up,
        }
    }

    /// A symmetric lens of a fixed horizontal field of view.
    pub fn horizontal(width_px: f64, height_px: f64, scale: f64, hfov_deg: f64) -> Lens {
        let side = tan_deg(hfov_deg / 2.0);
        let up = side * height_px / width_px;
        Lens {
            width_px,
            height_px,
            scale,
            tan_left: -side,
            tan_right: side,
            tan_up: up,
            tan_down: -up,
        }
    }

    /// A lens of four angles, degrees (a headset's eye).
    pub fn angles(
        width_px: f64,
        height_px: f64,
        scale: f64,
        [left, right, up, down]: [f64; 4],
    ) -> Lens {
        Lens {
            width_px,
            height_px,
            scale,
            tan_left: tan_deg(left),
            tan_right: tan_deg(right),
            tan_up: tan_deg(up),
            tan_down: tan_deg(down),
        }
    }

    /// The desktop: 1920 × 1080 at scale 1, 60° across (the simulation's
    /// focal before lenses, 1663 px).
    pub fn desktop() -> Lens {
        Lens::horizontal(1920.0, 1080.0, 1.0, 60.0)
    }

    /// A phone held upright: 1170 × 2532 physical pixels at scale 3, 60°
    /// high.
    pub fn portrait() -> Lens {
        Lens::vertical(1170.0, 2532.0, 3.0, 60.0)
    }

    /// The same phone turned: 2532 × 1170, the same vertical field of view.
    pub fn landscape() -> Lens {
        Lens::vertical(2532.0, 1170.0, 3.0, 60.0)
    }

    /// The simulation's headset's left eye (spec §13.1): 2000 × 2000 at
    /// scale 1, left −52°, right 44°, up 47°, down −53°; representative
    /// values, not a device's.
    pub fn headset_left() -> Lens {
        Lens::angles(2000.0, 2000.0, 1.0, [-52.0, 44.0, 47.0, -53.0])
    }

    /// The mirror image: the other eye.
    pub fn mirrored(self) -> Lens {
        Lens {
            tan_left: -self.tan_right,
            tan_right: -self.tan_left,
            ..self
        }
    }

    /// The same lens with the viewport's width scaled by `num / den`, its
    /// vertical field of view kept (a split screen, a window resized in
    /// width only).
    pub fn narrowed(self, num: f64, den: f64) -> Lens {
        let width_px = self.width_px * num / den;
        Lens {
            width_px,
            tan_left: self.tan_left * num / den,
            tan_right: self.tan_right * num / den,
            ..self
        }
    }

    /// The same logical lens at another scale (a window moved between
    /// displays): the physical viewport follows the scale.
    pub fn at_scale(self, scale: f64) -> Lens {
        Lens {
            width_px: self.width_px * scale / self.scale,
            height_px: self.height_px * scale / self.scale,
            scale,
            ..self
        }
    }

    /// The focal, logical pixels per unit of tangent, on the sharper axis
    /// (spec §13.2).
    pub fn focal(&self) -> f64 {
        let across = self.width_px / (self.tan_right - self.tan_left);
        let high = self.height_px / (self.tan_up - self.tan_down);
        across.max(high) / self.scale
    }
}

/// One eye: a camera and a lens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    /// Where it is and where it looks.
    pub cam: Camera,
    /// Through what.
    pub lens: Lens,
}

/// A region of the world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    /// Shallow terrain, shallow imagery.
    Sea,
    /// Shallow terrain, deep imagery: imagery must outrun terrain here.
    Plain,
    /// Deep terrain, imagery as deep.
    Mountains,
}

impl Region {
    /// The region at ground point `(x, y)`.
    pub fn at(x: f64, _y: f64) -> Region {
        if x < 0.45 * SIDE {
            Region::Sea
        } else if x < 0.7 * SIDE {
            Region::Plain
        } else {
            Region::Mountains
        }
    }

    /// The deepest terrain level.
    pub fn terrain_max(self) -> u32 {
        match self {
            Region::Sea => 7,
            Region::Plain => 11,
            Region::Mountains => 15,
        }
    }

    /// The deepest imagery level.
    pub fn imagery_max(self) -> u32 {
        match self {
            Region::Sea => 9,
            Region::Plain => 17,
            Region::Mountains => 15,
        }
    }

    /// Geometric-error multiplier.
    pub fn roughness(self) -> f64 {
        match self {
            Region::Sea => 0.5,
            Region::Plain => 1.0,
            Region::Mountains => 4.0,
        }
    }

    /// A mesh's bytes.
    pub fn mesh_bytes(self) -> Bytes {
        match self {
            Region::Sea => Bytes::kib(40),
            Region::Plain => Bytes::kib(120),
            Region::Mountains => Bytes::kib(300),
        }
    }
}

/// A camera over the flat world.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    /// Ground position, metres.
    pub x: f64,
    /// Ground position, metres.
    pub y: f64,
    /// Height, metres.
    pub h: f64,
    /// Heading, radians (0 = +x).
    pub heading: f64,
    /// Pitch below the horizon, radians.
    pub pitch: f64,
}

/// The tile `(level, x, y)` of the globe.
pub fn tile(level: u32, x: u64, y: u64) -> TileId {
    TileId::from_terrain(level, x, y)
}

/// The city: a 3D Tiles tileset over the plain, a quadtree of meshes of the
/// whole surface under a source tag of its own, drawn in place of the ground
/// it covers (`SourceOptions::covers_ground` in tuile-core). Its root covers
/// a square of [`CITY_SIDE`] from (`CITY_X0`, `CITY_Y0`).
pub const CITY: u8 = 1;
/// The city's corner, metres.
pub const CITY_X0: f64 = 0.55 * SIDE;
/// The city's corner, metres.
pub const CITY_Y0: f64 = 0.40 * SIDE;
/// The city's side, metres (32 km).
pub const CITY_SIDE: f64 = SIDE / 64.0;
/// Its deepest level (128 m tiles).
pub const CITY_DEPTH: u32 = 8;
/// Its screen-space error as a multiple of the globe's: 16 px against 2.
pub const CITY_FACTOR: f64 = 8.0;
/// A city mesh's bytes: geometry and its photographs.
pub const CITY_MESH: Bytes = Bytes::kib(400);
/// How much coarser the ground under the drawn city is refined: by the
/// city's own factor, so that it stays a fallback a few levels up.
pub const COVER_RELAX: f64 = 8.0;

/// Whether `id` is a tile of the city.
pub fn is_city(id: TileId) -> bool {
    id.tag() == CITY
}

/// The city's tile `(level, x, y)`.
pub fn city_tile(level: u32, x: u64, y: u64) -> TileId {
    tile(level, x, y).with_tag(CITY)
}

/// Its parent.
pub fn parent(id: TileId) -> Option<TileId> {
    let (l, x, y) = id.terrain_coord();
    (l > 0).then(|| tile(l - 1, x / 2, y / 2).with_tag(id.tag()))
}

/// Its children.
pub fn children(id: TileId) -> [TileId; 4] {
    let (l, x, y) = id.terrain_coord();
    let tag = id.tag();
    [
        tile(l + 1, 2 * x, 2 * y).with_tag(tag),
        tile(l + 1, 2 * x + 1, 2 * y).with_tag(tag),
        tile(l + 1, 2 * x, 2 * y + 1).with_tag(tag),
        tile(l + 1, 2 * x + 1, 2 * y + 1).with_tag(tag),
    ]
}

/// Its ground square `[x0, x1] × [y0, y1]`, metres.
pub fn square(id: TileId) -> [f64; 4] {
    let (l, x, y) = id.terrain_coord();
    let (x0, y0, side) = if is_city(id) {
        (CITY_X0, CITY_Y0, CITY_SIDE)
    } else {
        (0.0, 0.0, SIDE)
    };
    let s = side / f64::from(1u32 << l);
    [
        x0 + x as f64 * s,
        x0 + (x + 1) as f64 * s,
        y0 + y as f64 * s,
        y0 + (y + 1) as f64 * s,
    ]
}

/// Its footprint in the budget's radians (the world is the unit square).
pub fn rect(id: TileId) -> GeoRect {
    let [x0, x1, y0, y1] = square(id);
    GeoRect {
        west: x0 / SIDE,
        east: x1 / SIDE,
        north: 1.0 - y0 / SIDE,
        south: 1.0 - y1 / SIDE,
    }
}

/// Its region, by its centre.
pub fn region(id: TileId) -> Region {
    let [x0, x1, y0, y1] = square(id);
    Region::at((x0 + x1) / 2.0, (y0 + y1) / 2.0)
}

/// Whether it has children.
pub fn has_children(id: TileId) -> bool {
    let max = if is_city(id) {
        CITY_DEPTH
    } else {
        region(id).terrain_max()
    };
    id.terrain_coord().0 < max
}

/// What a resource costs: meshes by region (the city's its own), drapes by
/// mosaic size (one 256² imagery tile per level of climb, squared).
pub fn cost(r: ResourceId) -> Cost {
    match r {
        ResourceId::Mesh(t) if is_city(t) => Cost::flat(CITY_MESH),
        ResourceId::Mesh(t) => Cost::flat(region(t).mesh_bytes()),
        ResourceId::Drape { tile: t, level, .. } => {
            let up = level.saturating_sub(t.terrain_coord().0).min(4);
            let side = 256u32 << up;
            drape_cost(side, side, TextureFormat::Rgba8)
        }
    }
}

/// How a view sees one tile.
#[derive(Clone, Copy, Debug)]
pub struct Sight {
    /// 3D distance to the closest point, metres.
    pub distance: f64,
    /// Whether it is in view.
    pub visible: bool,
    /// `1 − off-axis`, `[0, 1]`.
    pub centrality: f64,
    /// Cosine of the view ray against the ground normal at the closest point.
    pub cos_obliquity: f64,
}

/// `tan(a ± m)` from `tan a` and `tan m`, widening away from the axis;
/// past a right angle, very wide.
fn widen(t: f64, tm: f64) -> f64 {
    if t >= 0.0 {
        let den = 1.0 - t * tm;
        if den <= 1e-9 {
            1e9
        } else {
            (t + tm) / den
        }
    } else {
        let den = 1.0 + t * tm;
        if den <= 1e-9 {
            -1e9
        } else {
            (t - tm) / den
        }
    }
}

/// The camera's basis: forward, right, up (z is up, `pitch` below the
/// horizon, `heading` from +x).
pub fn basis(cam: &Camera) -> ([f64; 3], [f64; 3], [f64; 3]) {
    let (hs, hc) = detmath::sin_cos(cam.heading);
    let (ps, pc) = detmath::sin_cos(cam.pitch);
    let forward = [pc * hc, pc * hs, -ps];
    let right = [hs, -hc, 0.0];
    let up = [hc * ps, hs * ps, pc];
    (forward, right, up)
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// How `view` sees `id`, its four angles widened by `margin` radians: the
/// tile's bounding sphere against the frustum's four sides and its near
/// plane, within the far distance.
pub fn sight(view: &View, id: TileId, margin: f64) -> Sight {
    let cam = &view.cam;
    let lens = &view.lens;
    let [x0, x1, y0, y1] = square(id);
    let cx = cam.x.clamp(x0, x1);
    let cy = cam.y.clamp(y0, y1);
    let (ex, ey) = (cx - cam.x, cy - cam.y);
    let dh = (ex * ex + ey * ey).sqrt();
    let distance = (dh * dh + cam.h * cam.h).sqrt();
    let (forward, right, up) = basis(cam);
    let v = [(x0 + x1) / 2.0 - cam.x, (y0 + y1) / 2.0 - cam.y, -cam.h];
    let (fx, fy, fz) = (dot(v, right), dot(v, up), dot(v, forward));
    let radius = (x1 - x0) * std::f64::consts::FRAC_1_SQRT_2;
    let (ms, mc) = detmath::sin_cos(margin);
    let tm = ms / mc;
    let (l, r) = (widen(lens.tan_left, tm), widen(lens.tan_right, tm));
    let (u, d) = (widen(lens.tan_up, tm), widen(lens.tan_down, tm));
    // Inward signed distances to the four sides through the eye.
    let side = |n: [f64; 3]| {
        let len = dot(n, n).sqrt();
        (n[0] * fx + n[1] * fy + n[2] * fz) / len >= -radius
    };
    let inside = side([1.0, 0.0, -l])
        && side([-1.0, 0.0, r])
        && side([0.0, 1.0, -d])
        && side([0.0, -1.0, u])
        && fz >= -radius;
    let far = (60.0 * cam.h + 30_000.0).min(800_000.0);
    let centre = dot(v, v).sqrt().max(1e-6);
    let cos_off = (fz / centre).clamp(-1.0, 1.0);
    Sight {
        distance,
        visible: distance <= far && inside,
        centrality: (1.0 + cos_off) / 2.0,
        cos_obliquity: (cam.h / distance).clamp(0.05, 1.0),
    }
}

/// The geometric error of `id`, metres: the city's, a sixteenth of its
/// tile's side, priced at [`CITY_FACTOR`] times the globe's threshold.
pub fn geometric_error(id: TileId) -> f64 {
    let l = id.terrain_coord().0;
    if is_city(id) {
        return CITY_SIDE / (16.0 * f64::from(1u32 << l));
    }
    SIDE / (128.0 * f64::from(1u32 << l)) * region(id).roughness()
}

/// The SSE of `id` at distance `d` through a focal of `focal` logical
/// pixels, logical pixels.
pub fn sse(id: TileId, d: f64, focal: f64) -> f64 {
    geometric_error(id) * focal / d.max(1.0)
}

/// The largest SSE any view gives `id`.
pub fn sse_max(views: &[View], id: TileId) -> f64 {
    views
        .iter()
        .map(|v| sse(id, sight(v, id, 0.0).distance, v.lens.focal()))
        .fold(0.0, f64::max)
}

/// Whether some view sees `id`, its angles widened by `margin`.
pub fn seen(views: &[View], id: TileId, margin: f64) -> bool {
    views.iter().any(|v| sight(v, id, margin).visible)
}

/// The traversal's view of the budget's residency.
pub trait Residency {
    /// Whether `t` may be treated as resident.
    fn resident(&self, t: TileId) -> bool;
}

/// The traversal: builds this step's need from every view (one eye, or a
/// stereo rig), as one union (spec §13.3) — with the city beside the globe
/// when `city`, at its own threshold, and the ground it covered last step
/// refined [`COVER_RELAX`] times coarser (the runtime's cover rule).
pub fn traverse(
    views: &[View],
    residency: &dyn Residency,
    sse_scale: f64,
    drawn_last: &BTreeSet<TileId>,
    pinned: u32,
    city: bool,
) -> NeedSet {
    let mut need = NeedSet::new();
    let tau = TAU * sse_scale;
    // The floor, always listed.
    for l in 0..=pinned {
        let n = 1u64 << l;
        for x in 0..n {
            for y in 0..n {
                let id = tile(l, x, y);
                need.ancestors.push((id, need_of(views, id, tau)));
            }
        }
    }
    let mut seen_tiles = BTreeSet::new();
    visit(
        views,
        residency,
        tau,
        drawn_last,
        tile(0, 0, 0),
        &mut need,
        &mut seen_tiles,
    );
    ring(views, tau, tile(0, 0, 0), &mut need, &seen_tiles);
    if city {
        visit(
            views,
            residency,
            tau,
            drawn_last,
            city_tile(0, 0, 0),
            &mut need,
            &mut seen_tiles,
        );
    }
    need
}

/// Whether the ground tile `id` lies under the city tiles drawn last step,
/// as the runtime tells it: its four corners and its centre each under one
/// of them (a city tile's footprint is its square here).
fn covered(drawn_last: &BTreeSet<TileId>, id: TileId) -> bool {
    let [x0, x1, y0, y1] = square(id);
    let city = city_tile(0, 0, 0);
    let mut drawn = drawn_last.range(city..).take_while(|t| is_city(**t));
    let mut points = [
        (x0, y0),
        (x1, y0),
        (x0, y1),
        (x1, y1),
        ((x0 + x1) / 2.0, (y0 + y1) / 2.0),
    ];
    let mut left = points.len();
    if drawn.clone().next().is_none() {
        return false;
    }
    for t in &mut drawn {
        let [a0, a1, b0, b1] = square(*t);
        for p in &mut points {
            if p.0.is_finite() && a0 <= p.0 && p.0 <= a1 && b0 <= p.1 && p.1 <= b1 {
                *p = (f64::NAN, f64::NAN);
                left -= 1;
            }
        }
        if left == 0 {
            return true;
        }
    }
    false
}

/// The threshold `id` is refined against: the city's factor for its
/// tiles, the cover's relaxation for ground under the city drawn last step.
fn tau_of(tau: f64, drawn_last: &BTreeSet<TileId>, id: TileId) -> f64 {
    if is_city(id) {
        tau * CITY_FACTOR
    } else if covered(drawn_last, id) {
        tau * COVER_RELAX
    } else {
        tau
    }
}

/// A tile's need over every view: the largest SSE excess and centrality,
/// the smallest distance and the finest imagery footprint any view gives
/// it (spec §13.3).
fn need_of(views: &[View], id: TileId, tau: f64) -> Need {
    let l = id.terrain_coord().0;
    let mut sse_best = 0.0f64;
    let mut centrality = 0.0f64;
    let mut distance = f64::INFINITY;
    let mut footprint: Option<TexelFootprint> = None;
    for v in views {
        let s = sight(v, id, 0.0);
        let focal = v.lens.focal();
        sse_best = sse_best.max(sse(id, s.distance, focal));
        centrality = centrality.max(s.centrality);
        distance = distance.min(s.distance);
        if let Ok(fp) = TexelFootprint::new(s.distance, s.cos_obliquity, focal, SIDE / 256.0, 1.0) {
            if footprint.is_none_or(|f| fp.ratio() > f.ratio()) {
                footprint = Some(fp);
            }
        }
    }
    let excess = detmath::log2(sse_best.max(1e-12) / tau).clamp(-8.0, 8.0);
    let class = if is_city(id) {
        Class::Tiles3dMesh
    } else {
        Class::TerrainMesh
    };
    let mut n = Need::new(l, class)
        .with_rect(rect(id))
        .with_sse_excess(excess as f32)
        .with_centrality(centrality as f32)
        .with_distance(distance as f32);
    if let Some(p) = parent(id) {
        n = n.with_parent(p);
    }
    // The city's meshes carry their own photographs.
    if let (Some(fp), false) = (footprint, is_city(id)) {
        n = n.with_imagery(ImageryNeed::base(fp, region(id).imagery_max()));
    }
    n
}

fn visit(
    views: &[View],
    res: &dyn Residency,
    tau: f64,
    drawn_last: &BTreeSet<TileId>,
    id: TileId,
    need: &mut NeedSet,
    seen_tiles: &mut BTreeSet<TileId>,
) {
    if !seen(views, id, CULL_MARGIN) {
        return;
    }
    seen_tiles.insert(id);
    // The tile's own threshold: the globe's, the city's, or the cover's.
    let tau_id = tau_of(tau, drawn_last, id);
    let kids_drawn_last = children(id).iter().any(|k| drawn_last.contains(k));
    let threshold = if kids_drawn_last {
        tau_id / 1.25
    } else {
        tau_id
    };
    let refine = has_children(id) && sse_max(views, id) > threshold;
    if !refine {
        need.drawn.push((id, need_of(views, id, tau_id)));
        return;
    }
    let visible_kids: Vec<TileId> = children(id)
        .into_iter()
        .filter(|&k| seen(views, k, CULL_MARGIN))
        .collect();
    let missing: Vec<TileId> = visible_kids
        .iter()
        .copied()
        .filter(|&k| !res.resident(k))
        .collect();
    if missing.is_empty() {
        need.ancestors.push((id, need_of(views, id, tau_id)));
        for k in visible_kids {
            visit(views, res, tau, drawn_last, k, need, seen_tiles);
        }
    } else {
        need.drawn.push((id, need_of(views, id, tau_id)));
        // Every child the hold waits on, resident or not: a resident
        // sibling evicted while the others load restarts the swap.
        for k in visible_kids {
            need.blockers
                .push((k, need_of(views, k, tau_of(tau, drawn_last, k))));
        }
    }
}

fn ring(views: &[View], tau: f64, id: TileId, need: &mut NeedSet, seen_tiles: &BTreeSet<TileId>) {
    if !seen(views, id, CULL_MARGIN + RING_MARGIN) {
        return;
    }
    let coarse = sse_max(views, id) <= 8.0 * tau || !has_children(id);
    if coarse {
        if !seen_tiles.contains(&id) {
            need.ring.push((id, need_of(views, id, tau)));
        }
        return;
    }
    for k in children(id) {
        ring(views, tau, k, need, seen_tiles);
    }
}
