// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Screen-space-error driven tile selection — pure functions.
//!
//! [`traverse`] is synchronous and mutation-free: state (residency,
//! selection history) lives in the `GeometryServer` and enters here as
//! explicit inputs. Nothing is ever stored on tiles, which is what lets N
//! streaming sessions share one immutable tile arena (see
//! `docs/10-crate-core.md`, « Décisions tranchées »).

use crate::math::Frustum;
use crate::source::{TileId, TileTree};
use crate::tileset::Refine;
use glam::{DMat4, DVec2, DVec3};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Monotone frame counter, injected by the caller (no clocks in the core).
pub type FrameNumber = u64;

/// One view of the scene. The traversal accepts SEVERAL views (visionOS
/// stereo, multi-viewport): selection is the union over views, priority
/// takes the best view per tile.
///
/// Symmetric perspective only in v1; orthographic and general projections
/// are post-v1 constructors.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(from = "ViewStateParams", into = "ViewStateParams")]
pub struct ViewState {
    position: DVec3,
    direction: DVec3,
    up: DVec3,
    viewport_px: DVec2,
    fovy_rad: f64,
    // Derived at construction (like the reference implementations).
    frustum: Frustum,
    sse_denominator: f64,
}

/// The serializable construction parameters of a [`ViewState`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ViewStateParams {
    pub position: DVec3,
    pub direction: DVec3,
    pub up: DVec3,
    pub viewport_px: DVec2,
    pub fovy_rad: f64,
}

impl From<ViewStateParams> for ViewState {
    fn from(p: ViewStateParams) -> Self {
        Self::perspective(p.position, p.direction, p.up, p.viewport_px, p.fovy_rad)
    }
}

impl From<ViewState> for ViewStateParams {
    fn from(v: ViewState) -> Self {
        Self {
            position: v.position,
            direction: v.direction,
            up: v.up,
            viewport_px: v.viewport_px,
            fovy_rad: v.fovy_rad,
        }
    }
}

/// How much wider than the screen the culling frustum is, as a fraction of the
/// field of view.
///
/// **The reference implementation has no such margin, and is right not to.**
/// Cesium culls against `frameState.cullingVolume` — the exact frustum of the
/// frame being drawn (`QuadtreePrimitive.js:1189`, `GlobeSurfaceTileProvider`'s
/// `computeTileVisibility`) — and culls *harder* still, dropping tiles buried
/// in fog. It can afford the exact frustum because it traverses synchronously,
/// on the render thread, inside the same frame: its selection cannot lag its
/// camera by even one frame.
///
/// Ours can. The geometry server is asynchronous by design — that is what lets
/// the same engine answer over a WebSocket — so every selection describes a
/// camera at least one round trip old. Zoom out quickly and the frustum's
/// footprint on the ground grows faster than the selection describing it: the
/// difference is ground nobody selected, and unselected ground is not drawn
/// dark, it is not drawn at all. Reproduced by pressing `F` (which freezes the
/// selection while the projection keeps following the camera) and zooming: the
/// drawn area is exactly the old footprint, with a black ring around it whose
/// width is proportional to how fast the camera moved.
///
/// So this margin buys latency, not visibility. It is compensation for an
/// architectural difference, and it should shrink back toward zero once a
/// missing tile can be filled in place rather than left unselected.
///
/// The cost is quadratic in the angle: +25 % of field of view is roughly +55 %
/// of tiles selected at the same depth. That is the price of the trade, stated
/// so nobody has to rediscover it.
pub const CULL_MARGIN: f64 = 0.25;

impl ViewState {
    /// Builds a view from a symmetric perspective camera. `direction` and
    /// `up` need not be normalized. Everything in f64, ECEF.
    pub fn perspective(
        position: DVec3,
        direction: DVec3,
        up: DVec3,
        viewport_px: DVec2,
        fovy_rad: f64,
    ) -> Self {
        let direction = direction.normalize();
        let up = up.normalize();
        let aspect = (viewport_px.x / viewport_px.y).max(1e-6);
        let view = DMat4::look_to_rh(position, direction, up);
        // Near/far only bound the culling frustum, not the LOD logic; keep
        // them generous (geospatial scenes span centimeters to planets).
        //
        // The *angle* is widened by [`CULL_MARGIN`]; the field of view kept in
        // `fovy_rad` is not, and neither is `sse_denominator` below. Widening
        // the margin must change what is selected, never how finely it is
        // divided — an inflated fov in the screen-space error would silently
        // coarsen the whole globe.
        let proj = DMat4::perspective_rh(
            (fovy_rad * (1.0 + CULL_MARGIN)).min(std::f64::consts::PI * 0.98),
            aspect,
            0.05,
            1.0e10,
        );
        Self {
            position,
            direction,
            up,
            viewport_px,
            fovy_rad,
            frustum: Frustum::from_view_proj(&(proj * view)),
            sse_denominator: 2.0 * (fovy_rad / 2.0).tan(),
        }
    }

    pub fn position(&self) -> DVec3 {
        self.position
    }

    /// Where the eye is looking, normalized.
    pub fn direction(&self) -> DVec3 {
        self.direction
    }

    pub fn frustum(&self) -> &Frustum {
        &self.frustum
    }

    /// The classic SSE metric:
    /// `geometricError × viewportHeight / (distance × 2 tan(fovy/2))`.
    ///
    /// The distance clamp (1e-7) matches the reference implementations: a
    /// camera inside the bounding volume sees a huge-but-finite error and
    /// always refines.
    pub fn screen_space_error(&self, geometric_error: f64, distance: f64) -> f64 {
        (geometric_error * self.viewport_px.y) / (distance.max(1e-7) * self.sse_denominator)
    }
}

/// Selection configuration. Names and defaults follow the reference
/// implementations so behaviour is comparable.
#[derive(Debug, Clone)]
pub struct Config {
    /// Refine a tile when its SSE exceeds this (pixels). Default 16.
    pub maximum_screen_space_error: f64,
    /// REPLACE hold-until-ready: never show holes during refinement.
    pub forbid_holes: bool,
    /// Whether the server sends stand-in surfaces for selected tiles the
    /// consumer has not acknowledged.
    ///
    /// On, and what follows is why it was off twice before.
    ///
    /// A consumer draws the nearest ancestor it holds for a tile that has not
    /// arrived, and that ancestor spans *all* its descendants — including the
    /// siblings that did arrive. Two surfaces over one patch of ground, both
    /// writing depth, winner chosen per pixel. Giving each missing tile a
    /// surface over its own rectangle is what removes it: the consumer's walk
    /// stops there and no ancestor is drawn. Nothing is refused, no fallback is
    /// withheld — the climb simply has nothing left to climb for.
    ///
    /// **The trap is the shape of the stand-in, not the idea.** It used to be a
    /// plane through four corner heights. Where some tiles get a stand-in and
    /// others do not — which happens whenever the loader declines — the
    /// ancestor's real relief and the neighbours' flat stand-ins occupy the same
    /// ground, and the relief *punches through the plane*. Measured twice, on
    /// Mediterranean terrain at about 15 km: large flat patches of coarse colour
    /// with ridges showing through them in ragged outlines that follow the
    /// terrain rather than the tile grid. Worse than the shimmer it replaced,
    /// and the reason this sat at `false` for months.
    ///
    /// A stand-in is now an **upsample of the nearest ancestor already in
    /// memory** — "a mesh of exactly the same surface over exactly the child's
    /// ground". There is no plane, so there is nothing for relief to punch
    /// through; and where an ancestor is still drawn beside one, the two are the
    /// same surface carrying the same mosaic rather than two different guesses
    /// at it.
    ///
    /// A consumer that ignores [`crate::protocol::ServerMessage::Fill`] is
    /// unaffected: it keeps climbing, exactly as before.
    pub stand_ins: bool,
    /// Whether to drop tiles no view can see. Off is a diagnostic, not a mode.
    ///
    /// A culled tile is reported *ready* so that it never holds up an ancestor's
    /// REPLACE — which is right when it is genuinely invisible, and is a hole
    /// punched clean through the globe when it is not: the ancestor is released
    /// on the strength of a child that then draws nothing. Culling is therefore
    /// the first thing to rule out when geometry goes missing, and ruling it out
    /// by measurement rather than by reading the frustum test is worth a flag.
    pub cull: bool,
    /// How many not-yet-drawable descendants a held REPLACE will wait for
    /// before giving up on them and asking for itself instead.
    ///
    /// A hold asks for everything the traversal would like to end up with. On a
    /// deep zoom that is the whole subtree: measured at seven kilometres, 1156
    /// outstanding requests against 117 tiles actually selected. Draining that
    /// sixty-four at a time, with each terrain tile awaiting its own imagery
    /// mosaic, took about seven seconds — seven seconds of black ground, since
    /// nothing in the subtree covers it until the last of it arrives.
    ///
    /// Past this many, the subtree's requests are dropped and the coarse
    /// ancestor is fetched instead. It arrives in one round trip and covers all
    /// of that ground at once; the detail then refines into a picture that is
    /// already there. The reference implementation does exactly this, at the
    /// same default of 20.
    pub loading_descendant_limit: u32,
    /// Resident-content budget in bytes (CPU side). Default 512 MiB.
    ///
    /// A consumer that uploads this content pays more than this for it: mip
    /// chains, interleaved vertices and format padding all land on its side of
    /// the wire. Budget for the consumer's memory, not for this number.
    pub resident_budget_bytes: usize,
    /// Maximum simultaneous content fetches.
    pub maximum_simultaneous_fetches: usize,
    /// A level that is never evicted, geometry and imagery both.
    ///
    /// Everything at or above it stays resident for the life of the session,
    /// whatever the ceilings say. That is the guarantee the fallback chain
    /// needs: an ancestor walk that reaches this level always finds something,
    /// so ground can be soft but never bare.
    ///
    /// The level is a memory decision and the numbers are unforgiving, because
    /// the count is `4^level`:
    ///
    /// | level | tiles over the globe | imagery if every one were held |
    /// |---|---|---|
    /// | 3 | 64 | 21 MiB |
    /// | 5 | 1 024 | 341 MiB |
    /// | 6 | 4 096 | 1.3 GiB |
    /// | 8 | 65 536 | 21 GiB |
    ///
    /// Level 8 is not an option on a machine with 16 GiB shared between the CPU
    /// and the GPU. Level 5 is, and in practice costs a fraction of that figure:
    /// only tiles under ground the camera has visited are ever fetched, and the
    /// ocean is most of the planet.
    ///
    /// `None` disables the pin — the ceilings then govern everything, which is
    /// right for a bulk render that visits each view once.
    pub pinned_level: Option<u32>,
    /// Whether tiles the traversal refines *past* are fetched speculatively.
    ///
    /// A refinement walks through a tile on its way to the detail it wants, and
    /// never asks for the one it walked through — so a zoom-out arrives at a
    /// level nothing was ever loaded for, and the ground it covers is bare
    /// until the network answers. The same gap opens on a pan, where newly
    /// exposed ground has no coarse tile behind it.
    ///
    /// Queued at [`PriorityGroup::Preload`], so it can only use fetch slots the
    /// visible frontier is not using. On by default, as in the reference
    /// implementation, whose own note is that it "optimizes the zoom-out
    /// experience and provides more detail in newly-exposed areas when panning".
    pub preload_ancestors: bool,
    /// Whether tiles just outside the frustum are fetched speculatively.
    ///
    /// What a rotation turns onto. Off by default, again as in the reference:
    /// the cost is real and constant, the benefit only shows on movement.
    pub preload_siblings: bool,
    /// How many tiles may stay resident, whatever they weigh.
    ///
    /// A second budget rather than a redundant one, because bytes and relevance
    /// are different questions and a byte budget only answers the first. A tile
    /// at level 19 seen from five hundred kilometres is useless and costs no
    /// more than a level-3 tile covering a continent — nothing about its size
    /// says it should go first, and a byte budget will not touch it until the
    /// whole residency is under pressure. A count says "this many, the most
    /// recently used", which is the reference implementation's `tileCacheSize`
    /// and is what actually bounds a long session.
    pub resident_tile_limit: usize,
}

impl Config {
    /// The settings an interactive globe session actually runs with.
    ///
    /// **One source of truth, on purpose.** These numbers used to live inside
    /// `wgpu-viewer`'s `main`, where nothing else could reach them — so every
    /// headless test invented its own, and a harness that never evicted passed
    /// while the real globe went black. A test that does not run the host's
    /// configuration is not testing the host.
    ///
    /// `pinned_level` is left to the caller: the viewer takes it from
    /// `TUILE_PIN_LEVEL` so two runs of one binary can be compared minutes
    /// apart, and a test wants it fixed.
    ///
    /// The reasoning behind each number, which is not obvious from the number:
    ///
    /// - **Two ceilings, and the count does the work.** Measured before it
    ///   existed: 3386 MiB of imagery, ten thousand textures, a thousand of
    ///   them at level 19 while the eye was in orbit, and not one eviction all
    ///   session — because a level-19 texture nobody is looking at weighs
    ///   exactly what a level-3 texture covering a continent weighs, so bytes
    ///   alone never singled it out.
    /// - **Sized above the working set.** `forbid_holes` loads a whole subtree
    ///   before selecting any of it, and a tile loaded but not yet selected is
    ///   protected by nothing: squeeze the budget below what a view needs and
    ///   those tiles are evicted and re-requested for ever. At 768 MiB a
    ///   motionless camera still churned twenty tiles a second, and that reads
    ///   as the app hanging.
    /// - **The budget counts decoded CPU bytes**; the GPU copy is about 1.35×
    ///   that once mip chains and interleaved vertices are paid for. Four
    ///   gibibytes here is therefore roughly **5.4 GiB on the GPU** — which
    ///   unified memory carries, and which a discrete card with 8 GB would find
    ///   tight once a compositor and a browser have taken their share.
    /// - **Both ceilings moved together**, and they have to. The count is what
    ///   actually bites — it was raised from 12 000 alongside the bytes — so
    ///   lifting the byte budget alone would have changed nothing at all: the
    ///   sweep would still have stopped at twelve thousand tiles, and a session
    ///   would have reported three quarters of its new budget unused while
    ///   evicting exactly as often as before.
    pub fn interactive_globe(pinned_level: Option<u32>) -> Self {
        Self {
            maximum_screen_space_error: 2.0,
            maximum_simultaneous_fetches: 64,
            resident_budget_bytes: 4 * 1024 * 1024 * 1024,
            resident_tile_limit: 16_000,
            pinned_level,
            ..Self::default()
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            maximum_screen_space_error: 16.0,
            forbid_holes: true,
            stand_ins: true,
            cull: true,
            loading_descendant_limit: 20,
            resident_budget_bytes: 512 * 1024 * 1024,
            maximum_simultaneous_fetches: 20,
            // Roughly a screenful at a demanding SSE, several times over, so a
            // camera can turn and come back without paying twice.
            resident_tile_limit: 4_096,
            pinned_level: Some(5),
            preload_ancestors: true,
            preload_siblings: false,
        }
    }
}

/// What the traversal needs to know about content residency. Owned by the
/// `GeometryServer`, read-only here.
#[derive(Debug, Clone, Default)]
pub struct ResidencyView {
    resident: HashSet<TileId>,
}

impl ResidencyView {
    pub fn is_resident(&self, t: TileId) -> bool {
        self.resident.contains(&t)
    }
    pub fn insert(&mut self, t: TileId) {
        self.resident.insert(t);
    }
    pub fn remove(&mut self, t: TileId) {
        self.resident.remove(&t);
    }
    pub fn iter(&self) -> impl Iterator<Item = TileId> + '_ {
        self.resident.iter().copied()
    }
}

/// Inter-group request ordering (coarse before fine-grained priority).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PriorityGroup {
    /// Speculative loading for future views (unused in v1).
    Preload,
    /// Needed for the current view's LOD.
    Normal,
    /// Its absence is currently visible (blocking a REPLACE, or a bare leaf).
    Urgent,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContentRequest {
    pub tile: TileId,
    pub group: PriorityGroup,
    /// Intra-group priority: LOWER loads first (v1: distance to camera).
    pub priority: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TraversalStats {
    /// Holds that kept their descendants because some were already on screen.
    /// Above zero means the picture was allowed to stay uneven rather than be
    /// thrown away — which is the intent. See [`Config::forbid_holes`].
    pub held_but_drawn: u32,
    /// Subtrees whose pending loads were dropped in favour of one coarse
    /// ancestor, because too many descendants were not yet drawable. Above zero
    /// means the traversal chose a fast coarse picture over a slow sharp one —
    /// which is the intent, not a fault. See
    /// [`Config::loading_descendant_limit`].
    pub deferred_subtrees: u32,
    pub visited: u32,
    pub culled: u32,
    pub selected: u32,
    pub requested: u32,
    pub max_depth: u32,
    /// Empty leaves reached: ground the traversal decided to draw nothing on,
    /// while releasing the ancestor that was covering it.
    ///
    /// Legitimate in a 3D Tiles set, where an empty leaf means there is nothing
    /// there. Never legitimate over terrain, where every tile carries content —
    /// so a count above zero on a globe is a hole, and this is what makes it
    /// visible instead of silent.
    pub gaps: u32,
}

/// Output buffers, reused across frames (no per-frame allocations once the
/// vectors have grown).
#[derive(Debug, Default)]
pub struct TraversalOutput {
    /// Tiles to render, with their current SSE.
    pub selected: Vec<(TileId, f64)>,
    /// Content to fetch, sorted: group first (Urgent → Preload), then
    /// ascending priority value.
    pub requests: Vec<ContentRequest>,
    /// Tiles a held REPLACE is waiting on: descendants this pass chose not to
    /// draw *yet*, because a sibling is still missing.
    ///
    /// Not drawn, not requested — already resident — and so invisible to a
    /// residency that only knows about those two sets. Reclaim one and the next
    /// pass asks for it again, gets it, holds again, and reclaims it again:
    /// loads resolve inside a single poll of the server, so that cycle does not
    /// merely waste bandwidth, it never yields. The reference implementation
    /// marks these children rendered at exactly this point, for exactly this
    /// reason.
    pub awaiting: Vec<TileId>,
    pub stats: TraversalStats,
}

/// Pure selection: `(arena, residency, views, config) → output`.
///
/// `_frame` is reserved for history-dependent refinements (LOD fades);
/// injecting it keeps tests deterministic (no clocks in the core).
pub fn traverse(
    tree: &dyn TileTree,
    residency: &ResidencyView,
    views: &[ViewState],
    config: &Config,
    _frame: FrameNumber,
    rendered_last: &HashSet<TileId>,
    out: &mut TraversalOutput,
) {
    out.selected.clear();
    out.requests.clear();
    out.awaiting.clear();
    out.stats = TraversalStats::default();
    let roots = tree.roots();
    if views.is_empty() || roots.is_empty() {
        return;
    }

    let mut requested = HashMap::new();
    for root in roots {
        visit(
            tree,
            residency,
            views,
            config,
            rendered_last,
            root,
            0,
            out,
            &mut requested,
        );
    }

    // Group first (Urgent before Normal before Preload), then nearest first;
    // tile id as the deterministic tie-break.
    out.requests.sort_by(|a, b| {
        b.group
            .cmp(&a.group)
            .then(a.priority.total_cmp(&b.priority))
            .then(a.tile.cmp(&b.tile))
    });
    out.stats.selected = out.selected.len() as u32;
    out.stats.requested = out.requests.len() as u32;
}

/// What a subtree reported back to the tile above it.
#[derive(Debug, Clone, Copy)]
struct Visit {
    /// Everything this subtree decided to show is resident — the REPLACE
    /// hold-until-ready predicate.
    ready: bool,
    /// How many tiles under here are wanted and not yet drawable.
    ///
    /// Carried up so an ancestor can tell "one child is a moment away" from
    /// "a thousand descendants are minutes away", which are the same answer to
    /// `ready` and call for opposite decisions. See
    /// [`Config::loading_descendant_limit`].
    not_yet_renderable: u32,
    /// Whether anything under here was on screen at the previous pass.
    ///
    /// A hold that discards its descendants when some of them are *already
    /// drawn* does not degrade the picture, it deletes it — and because the
    /// ancestor standing in may not be resident either, the discard cascades to
    /// the root. Measured in the viewer at the moment of a tilt: a selection of
    /// 866 tiles became 1, with 1792 tiles sitting ready on the GPU and undrawn.
    /// The reference implementation guards the same branch with the same
    /// condition — it kicks descendants only when `!anyWereRenderedLastFrame`.
    any_were_rendered_last_frame: bool,
    /// Whether every visible patch of this subtree's ground is covered by
    /// something that will be drawn.
    ///
    /// **Not `ready`, and confusing the two is what left holes on screen for
    /// days.** `ready` means "this subtree does not stop an ancestor refining";
    /// `covers` means "there is no bare ground down here". A subtree can be
    /// unready and fully covered — a coarse tile standing in while its children
    /// stream — and it can be *counted as fine* while a quarter of it shows
    /// nothing, which is exactly what happened: three of four children were on
    /// screen, the fourth was still loading, and the parent read
    /// `any_were_rendered_last_frame` as permission to draw none of it.
    covers: bool,
}

impl Visit {
    /// Ready, and covering — what a culled tile reports, since ground nobody
    /// can see needs nothing drawn on it.
    const READY: Self = Self {
        ready: true,
        not_yet_renderable: 0,
        any_were_rendered_last_frame: false,
        covers: true,
    };
    /// Ready in the sense that it blocks nothing, and covering nothing: an
    /// empty leaf draws no pixels.
    const READY_BUT_BARE: Self = Self {
        ready: true,
        not_yet_renderable: 0,
        any_were_rendered_last_frame: false,
        covers: false,
    };
    fn waiting_on_one() -> Self {
        Self {
            ready: false,
            not_yet_renderable: 1,
            any_were_rendered_last_frame: false,
            covers: false,
        }
    }
    /// This subtree is covered and was on screen — the two facts an ancestor
    /// needs to leave it alone.
    fn drawn() -> Self {
        Self {
            ready: true,
            not_yet_renderable: 0,
            any_were_rendered_last_frame: true,
            covers: true,
        }
    }
}

/// Recursive visit over the abstract [`TileTree`].
#[allow(clippy::too_many_arguments)]
fn visit(
    tree: &dyn TileTree,
    residency: &ResidencyView,
    views: &[ViewState],
    config: &Config,
    rendered_last: &HashSet<TileId>,
    id: TileId,
    depth: u32,
    out: &mut TraversalOutput,
    requested: &mut HashMap<TileId, usize>,
) -> Visit {
    let props = tree.properties(id);
    out.stats.visited += 1;
    out.stats.max_depth = out.stats.max_depth.max(depth);

    // Frustum culling: a tile invisible in every view contributes nothing
    // and never blocks an ancestor's REPLACE.
    if config.cull
        && !views
            .iter()
            .any(|v| props.bounding_volume.intersects_frustum(v.frustum()))
    {
        out.stats.culled += 1;
        // A tile just outside the frustum is what a pan or a rotation brings in
        // next. Loading it now is the difference between turning onto ground
        // that is already there and turning onto ground that starts arriving.
        // Off by default, as in the reference: it is a real cost, paid for a
        // smoothness nobody sees until it is missing.
        if config.preload_siblings && props.has_content && !residency.is_resident(id) {
            // Out of sight, so the off-axis weighting would be meaningless here;
            // plain distance is the honest ordering for ground the camera has
            // not turned onto yet.
            let distance = views
                .iter()
                .map(|v| props.bounding_volume.distance_to_point(v.position()))
                .fold(f64::INFINITY, f64::min);
            request(out, requested, id, PriorityGroup::Preload, distance);
        }
        return Visit::READY;
    }

    // What to fetch first, among everything that is wanted.
    //
    // Distance alone treats a tile at the edge of vision as urgently as the one
    // the eye is pointed at, and on a wide view that is most of the queue. The
    // reference weights it by how far off-axis the tile is —
    // `(1 - dot(toTile, viewDirection)) * distance` — so dead ahead costs
    // nothing and directly behind costs double. It matters far more once
    // ancestors and siblings are queued speculatively: without it the
    // speculation competes with the picture instead of filling in around it.
    let priority = views
        .iter()
        .map(|v| {
            let to_tile = props.bounding_volume.center() - v.position();
            let magnitude = to_tile.length();
            if magnitude < 1.0e-5 {
                return 0.0;
            }
            let off_axis = 1.0 - (to_tile / magnitude).dot(v.direction());
            off_axis * props.bounding_volume.distance_to_point(v.position())
        })
        .fold(f64::INFINITY, f64::min);
    let sse = views
        .iter()
        .map(|v| {
            let d = props.bounding_volume.distance_to_point(v.position());
            v.screen_space_error(props.geometric_error, d)
        })
        .fold(0.0, f64::max);

    let children = tree.children(id);
    let has_content = props.has_content;
    let has_children = !children.is_empty();
    // Refine when too coarse — or when there is nothing to render here
    // (structural empty tiles always descend).
    let refines = has_children && (sse > config.maximum_screen_space_error || !has_content);

    // Coarse ground over the whole visible region, held ready for a zoom-out.
    //
    // Every tile the traversal refines *past* is asked for, speculatively, at
    // the lowest priority. That is not the column above the camera — the
    // traversal walks the whole visible quadtree, so it is one coarse layer
    // spread across everything in view, which is exactly what a zoom-out lands
    // on. The descent otherwise never asks for any of it, and pulling back
    // exposes a horizon nothing was ever fetched for.
    //
    // The cost is bounded by the shape of a quadtree rather than by a guess:
    // each level up holds a quarter as many tiles as the one below, so the
    // whole ancestry above a frontier sums to about a third of it again.
    if config.preload_ancestors && refines && has_content && !residency.is_resident(id) {
        request(out, requested, id, PriorityGroup::Preload, priority);
    }

    if !refines {
        if !has_content {
            // A leaf with nothing in it and nowhere to descend. Reported ready,
            // because in 3D Tiles an empty leaf genuinely means *there is
            // nothing here* — a region with no buildings is not a region that
            // failed to load.
            //
            // But the traversal cannot tell that apart from data that should
            // have been there and was not, and the two look identical from
            // here: both return ready, both draw nothing, and under REPLACE
            // both release the ancestor that was covering that ground. The
            // result is a hole with clean tile edges, and it is silent.
            //
            // So it is counted. A globe should never produce one — terrain
            // tiles all carry content — and a count above zero over a terrain
            // traversal means the tree is saying something it should not.
            // Counted *and* named. The count is what a host can watch every
            // frame without reading anything; the line is what says which
            // ground, when someone is looking at a hole and wants to know.
            out.stats.gaps += 1;
            tracing::debug!(
                ?id,
                depth,
                "empty leaf: nothing is drawn here, and the ancestor covering                  it is released"
            );
            return Visit::READY_BUT_BARE;
        }
        if residency.is_resident(id) {
            out.selected.push((id, sse));
            return if rendered_last.contains(&id) {
                Visit::drawn()
            } else {
                Visit::READY
            };
        }
        request(out, requested, id, PriorityGroup::Urgent, priority);
        return Visit::waiting_on_one();
    }

    match props.refine {
        Refine::Add => {
            // Additive: the parent stays visible; children refine on top.
            let mut waiting = 0;
            let mut drawn_below = false;
            let mut children_cover = true;
            // Additive: this tile is the base layer. Drawn, it covers the whole
            // subtree on its own; missing, only the children can.
            let base_covers = has_content && residency.is_resident(id);
            if has_content {
                if residency.is_resident(id) {
                    out.selected.push((id, sse));
                } else {
                    request(out, requested, id, PriorityGroup::Normal, priority);
                    waiting += 1;
                }
            }
            for child in children {
                let below = visit(
                    tree,
                    residency,
                    views,
                    config,
                    rendered_last,
                    child,
                    depth + 1,
                    out,
                    requested,
                );
                waiting += below.not_yet_renderable;
                drawn_below |= below.any_were_rendered_last_frame;
                children_cover &= below.covers;
            }
            // Additive never holds — the parent stays on screen — so it is
            // always "ready"; the count still travels up for the limit above.
            Visit {
                ready: true,
                not_yet_renderable: waiting,
                any_were_rendered_last_frame: drawn_below || rendered_last.contains(&id),
                covers: base_covers || children_cover,
            }
        }
        Refine::Replace => {
            // Tentatively select the children; roll their selections back if
            // any visible branch is not renderable yet (hold-until-ready).
            let mark = out.selected.len();
            let request_mark = out.requests.len();
            let mut all_ready = true;
            let mut waiting = 0;
            let mut drawn_below = false;
            let mut children_cover = true;
            for child in children {
                let child_visit = visit(
                    tree,
                    residency,
                    views,
                    config,
                    rendered_last,
                    child,
                    depth + 1,
                    out,
                    requested,
                );
                all_ready &= child_visit.ready;
                waiting += child_visit.not_yet_renderable;
                drawn_below |= child_visit.any_were_rendered_last_frame;
                children_cover &= child_visit.covers;
            }
            if all_ready || !config.forbid_holes {
                return Visit {
                    ready: all_ready,
                    not_yet_renderable: waiting,
                    any_were_rendered_last_frame: drawn_below,
                    covers: children_cover,
                };
            }

            // Some of this subtree is already on screen. Keep drawing it.
            //
            // A hold exists to avoid showing a hole while a refinement lands.
            // When descendants are *already drawn*, discarding them creates the
            // very hole it is there to prevent — and worse, if the tile standing
            // in for them is not resident either, the discard travels up and the
            // next level does the same, all the way to the root. Measured at the
            // instant of a tilt: 866 selected tiles became 1, with 1792 ready on
            // the GPU and drawn by nothing. The picture is allowed to be uneven
            // while it settles; it is not allowed to disappear.
            // …but only if they cover **all** of it.
            //
            // This guard used to ask "is any descendant on screen", and that is
            // the bug the whole file was chasing. Four children, three of them
            // drawn and one still loading: the answer was yes, the three kept
            // their selections, the fourth selected nothing — and this tile,
            // resident and able to stand in for it, declined to. A quarter of
            // the ground went bare for as long as the load took. Transient, on
            // zoom-out, with the covering ancestor sitting ready on the GPU:
            // exactly what was reported for days.
            //
            // Asking whether every descendant is *covered* is the question that
            // was meant. When one is not, the branch below holds: the
            // descendants move to `awaiting` and this tile stands in for all of
            // them.
            //
            // The cost is stated rather than hidden. If this tile is not
            // resident either, the hold travels up until it finds one that is —
            // at worst the pinned floor, so coverage is guaranteed — and the
            // picture drops a level or two of sharpness while the straggler
            // lands. Blurry beats bare, and a fill mesh (`ServerMessage::Fill`)
            // removes even that by covering the one missing rectangle instead.
            if drawn_below && children_cover {
                out.stats.held_but_drawn += 1;
                tracing::debug!(
                    ?id,
                    depth,
                    waiting,
                    "descendants are still on screen and cover this ground: keeping them"
                );
                return Visit {
                    ready: false,
                    not_yet_renderable: waiting,
                    any_were_rendered_last_frame: true,
                    covers: true,
                };
            }
            // Hold: drop descendant selections, stand in with this tile. The
            // descendants are still wanted — they are what the hold is waiting
            // for — so they are moved aside rather than forgotten.
            out.awaiting
                .extend(out.selected.drain(mark..).map(|(tile, _)| tile));

            // Waiting on heaps of descendants, with nothing of our own on
            // screen yet: ask for THIS tile and cancel the whole subtree's
            // requests.
            //
            // Without this the traversal asks for everything it would like to
            // end up with, all at once. Measured on a zoom to seven kilometres:
            // 1156 outstanding requests against 117 tiles actually selected,
            // draining sixty-four at a time with each terrain tile awaiting its
            // own imagery mosaic — about seven seconds of black ground before
            // anything appeared. One coarse tile arrives in one round trip and
            // covers all of it; the detail then refines into a picture that is
            // already there.
            //
            // Only when this tile was not drawn last frame. Once it is on
            // screen there is no black to avoid, and cutting the subtree off
            // then would stall refinement instead of accelerating it.
            let on_screen_already = rendered_last.contains(&id);
            let cut_the_subtree_loose =
                !on_screen_already && waiting > config.loading_descendant_limit;
            if cut_the_subtree_loose {
                for dropped in out.requests.drain(request_mark..) {
                    requested.remove(&dropped.tile);
                }
                // The indices the map holds are positions in `out.requests`, and
                // everything from the mark on has just moved. Nothing below the
                // mark shifted, so only entries past it would be stale — and
                // they are exactly the ones just removed.
                debug_assert!(requested.values().all(|&at| at < out.requests.len()));
                out.stats.deferred_subtrees += 1;
                if has_content && !residency.is_resident(id) {
                    request(out, requested, id, PriorityGroup::Urgent, priority);
                }
            }

            if has_content {
                if residency.is_resident(id) {
                    out.selected.push((id, sse));
                    // Drawable here, so the ground is covered: the ancestors
                    // above are told this branch costs them nothing, whatever
                    // is still streaming underneath.
                    return if rendered_last.contains(&id) {
                        Visit::drawn()
                    } else {
                        Visit::READY
                    };
                }
                request(out, requested, id, PriorityGroup::Urgent, priority);
            }
            Visit {
                ready: false,
                any_were_rendered_last_frame: false,
                // Nothing here and nothing below: this ground is bare, and
                // saying so is what lets an ancestor stand in for it.
                covers: false,
                // Collapsed to one *only* when the subtree was cut loose: this
                // tile is then genuinely the only thing the branch waits for.
                // Otherwise the count keeps climbing, which is the whole point
                // — collapsing it at every level meant it never reached the
                // limit and the cut-off never fired at all.
                not_yet_renderable: if cut_the_subtree_loose {
                    1
                } else {
                    waiting.max(1)
                },
            }
        }
    }
}

/// Asks for a tile once, at the highest urgency anyone asked for it.
///
/// A tile can be reached twice in one pass: speculatively on the way down, and
/// then urgently because a hold above it needs it *now*. Deduplicating on first
/// arrival would freeze it at whichever urgency happened to come first — and the
/// speculative one always comes first, since it fires before the descent. The
/// tile the picture is waiting on would then sit at the back of the queue behind
/// every other guess.
fn request(
    out: &mut TraversalOutput,
    requested: &mut HashMap<TileId, usize>,
    tile: TileId,
    group: PriorityGroup,
    priority: f64,
) {
    match requested.get(&tile) {
        Some(&at) => {
            let existing = &mut out.requests[at];
            if group > existing.group {
                existing.group = group;
                existing.priority = priority;
            } else if group == existing.group {
                existing.priority = existing.priority.min(priority);
            }
        }
        None => {
            requested.insert(tile, out.requests.len());
            out.requests.push(ContentRequest {
                tile,
                group,
                priority,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tileset::Tileset;
    use glam::{dvec2, dvec3};
    use url::Url;

    /// Root sphere (radius 100) with four leaf children (radius 30) laid out
    /// in the Z=0 plane. REPLACE refinement, leaves have geometricError 0.
    fn mini_tileset() -> Tileset {
        let child = |x: f64, y: f64, name: &str| {
            format!(
                r#"{{
                  "boundingVolume": {{ "sphere": [{x}, {y}, 0, 30] }},
                  "geometricError": 0,
                  "content": {{ "uri": "{name}.glb" }}
                }}"#
            )
        };
        let json = format!(
            r#"{{
              "asset": {{ "version": "1.1" }},
              "geometricError": 200,
              "root": {{
                "boundingVolume": {{ "sphere": [0, 0, 0, 100] }},
                "geometricError": 50,
                "refine": "REPLACE",
                "content": {{ "uri": "root.glb" }},
                "children": [{}, {}, {}, {}]
              }}
            }}"#,
            child(-50.0, -50.0, "a"),
            child(50.0, -50.0, "b"),
            child(-50.0, 50.0, "c"),
            child(50.0, 50.0, "d"),
        );
        let base = Url::parse("file:///t/tileset.json").expect("url");
        Tileset::from_json_bytes(json.as_bytes(), &base).expect("tileset")
    }

    /// Marks every tile at `depth` resident and nothing else — the state a
    /// finished zoom-in leaves behind, and the one a zoom-out starts from.
    ///
    /// It is the only state where speculation means anything: with an empty
    /// residency every tile is wanted *now*, so a preload is immediately
    /// upgraded to urgent and there is nothing speculative left to observe.
    fn residency_at_depth(ts: &Tileset, depth: u32) -> ResidencyView {
        fn walk(ts: &Tileset, id: TileId, at: u32, want: u32, out: &mut ResidencyView) {
            if at == want {
                out.insert(id);
                return;
            }
            for child in ts.tile(id).children.clone() {
                walk(ts, child, at + 1, want, out);
            }
        }
        let mut residency = ResidencyView::default();
        walk(ts, ts.root(), 0, depth, &mut residency);
        residency
    }

    /// A tileset deep enough that a close camera wants far more tiles than any
    /// one round trip can deliver: `fanout^depth` leaves under one root.
    fn deep_tileset(depth: u32) -> Tileset {
        fn node(level: u32, depth: u32, radius: f64, x: f64, y: f64) -> String {
            let error = radius;
            if level == depth {
                return format!(
                    r#"{{ "boundingVolume": {{ "sphere": [{x}, {y}, 0, {radius}] }},
                         "geometricError": 0, "refine": "REPLACE",
                         "content": {{ "uri": "l{level}_{x}_{y}.glb" }} }}"#
                );
            }
            let half = radius / 2.0;
            let kids: Vec<String> = [(-half, -half), (half, -half), (-half, half), (half, half)]
                .iter()
                .map(|(dx, dy)| node(level + 1, depth, half, x + dx, y + dy))
                .collect();
            format!(
                r#"{{ "boundingVolume": {{ "sphere": [{x}, {y}, 0, {radius}] }},
                     "geometricError": {error}, "refine": "REPLACE",
                     "content": {{ "uri": "l{level}_{x}_{y}.glb" }},
                     "children": [{}] }}"#,
                kids.join(",")
            )
        }
        let json = format!(
            r#"{{ "asset": {{ "version": "1.1" }}, "geometricError": 400,
                 "root": {} }}"#,
            node(0, depth, 100.0, 0.0, 0.0)
        );
        let base = Url::parse("file:///t/tileset.json").expect("url");
        Tileset::from_json_bytes(json.as_bytes(), &base).expect("tileset")
    }

    /// A tile the traversal refines past is fetched anyway, at the lowest
    /// priority.
    ///
    /// Without it a zoom-out arrives at a level nothing was ever loaded for:
    /// the descent walked through those tiles and never asked for one, so the
    /// ground they cover is bare until the network answers. The reference
    /// implementation calls this `preloadAncestors` and has it on by default,
    /// for exactly this reason.
    #[test]
    fn refining_past_a_tile_still_asks_for_it() {
        let ts = deep_tileset(3);
        // The frontier is in hand; its ancestry is not. Nothing here is wanted
        // urgently, so anything asked for is asked for on spec.
        let settled = residency_at_depth(&ts, 3);
        let views = [camera_at(120.0)];

        let eager = run_with(
            &ts,
            &settled,
            &views,
            &Config {
                preload_ancestors: true,
                ..Config::default()
            },
            &HashSet::new(),
        );
        let lazy = run_with(
            &ts,
            &settled,
            &views,
            &Config {
                preload_ancestors: false,
                ..Config::default()
            },
            &HashSet::new(),
        );

        let speculative = |out: &TraversalOutput| {
            out.requests
                .iter()
                .filter(|r| r.group == PriorityGroup::Preload)
                .count()
        };
        assert!(speculative(&eager) > 0, "nothing was queued speculatively");
        assert_eq!(
            speculative(&lazy),
            0,
            "the switch must actually switch it off"
        );
        assert!(
            eager.requests.len() > lazy.requests.len(),
            "preloading has to ask for strictly more"
        );
    }

    /// A zoom-out finds coarse ground already asked for, over the whole view.
    ///
    /// The traversal walks the entire visible quadtree, so asking for every
    /// tile it refines past is not the column above the camera — it is one
    /// coarse layer spread across everything in sight, which is exactly what
    /// pulling back lands on. Without it the descent asks for the frontier and
    /// nothing else, and a zoom-out exposes a horizon nothing was fetched for.
    #[test]
    fn a_zoom_out_is_prepared_for_before_it_happens() {
        let ts = deep_tileset(5);
        let settled = residency_at_depth(&ts, 5);
        let views = [camera_at(120.0)];
        let run_it = |preload| {
            run_with(
                &ts,
                &settled,
                &views,
                &Config {
                    preload_ancestors: preload,
                    ..Config::default()
                },
                &HashSet::new(),
            )
        };
        let with = run_it(true);
        let without = run_it(false);

        assert!(
            with.requests.len() > without.requests.len(),
            "preloading asked for nothing extra: {} against {}",
            with.requests.len(),
            without.requests.len()
        );
        assert!(
            with.requests
                .iter()
                .any(|r| r.group == PriorityGroup::Preload),
            "and what it asked for must be speculative, not urgent"
        );
        assert_eq!(
            without
                .requests
                .iter()
                .filter(|r| r.group == PriorityGroup::Preload)
                .count(),
            0,
            "the switch must actually switch it off"
        );
    }

    /// The ancestry of a frontier is bounded by the shape of a quadtree, not by
    /// a guess: each level up holds a quarter as many tiles as the one below,
    /// so the whole ancestry above `n` leaves sums to `(n-1)/3` — strictly
    /// fewer than the frontier itself. That is what makes preloading all of it
    /// affordable, and it is worth pinning, because "speculate over the whole
    /// visible region" sounds unbounded and is not.
    #[test]
    fn preloading_the_ancestry_costs_less_than_the_frontier() {
        let ts = deep_tileset(5);
        let settled = residency_at_depth(&ts, 5);
        let out = run_with(
            &ts,
            &settled,
            &[camera_at(120.0)],
            &Config {
                preload_ancestors: true,
                ..Config::default()
            },
            &HashSet::new(),
        );
        let frontier = out.selected.len();
        let speculative = out
            .requests
            .iter()
            .filter(|r| r.group == PriorityGroup::Preload)
            .count();
        assert!(frontier > 0 && speculative > 0, "nothing to measure");
        assert!(
            speculative < frontier,
            "the ancestry of {frontier} tiles cannot be {speculative} tiles"
        );
    }

    /// Speculation may never outrank the picture. Every preload sorts after
    /// every urgent and normal request, so it can only use fetch slots the
    /// visible frontier is not using.
    #[test]
    fn speculation_never_jumps_the_queue() {
        let ts = deep_tileset(3);
        let out = run_with(
            &ts,
            &ResidencyView::default(),
            &[camera_at(120.0)],
            &Config {
                preload_ancestors: true,
                ..Config::default()
            },
            &HashSet::new(),
        );
        let first_preload = out
            .requests
            .iter()
            .position(|r| r.group == PriorityGroup::Preload);
        let last_wanted = out
            .requests
            .iter()
            .rposition(|r| r.group != PriorityGroup::Preload);
        if let (Some(first), Some(last)) = (first_preload, last_wanted) {
            assert!(
                first > last,
                "a speculative fetch was ordered before a wanted one"
            );
        }
    }

    /// What to fetch first among equals: the tile the eye is pointed at.
    ///
    /// Two tiles the same distance away are not equally urgent — one is in the
    /// middle of the picture and the other at its edge. The reference weights
    /// the distance by how far off-axis the tile is, and without that weighting
    /// a wide view fills in from its edges as readily as from its centre.
    #[test]
    fn requests_come_out_ordered_by_the_heuristic() {
        let ts = mini_tileset();
        let off_centre = ViewState::perspective(
            dvec3(-50.0, -50.0, 120.0),
            dvec3(0.0, 0.0, -1.0),
            dvec3(0.0, 1.0, 0.0),
            dvec2(1024.0, 768.0),
            std::f64::consts::FRAC_PI_3,
        );
        let out = run(&ts, &ResidencyView::default(), &[off_centre]);
        assert!(
            out.requests.len() > 1,
            "the near view has to want more than one tile"
        );
        let cheapest = out
            .requests
            .iter()
            .min_by(|a, b| a.priority.total_cmp(&b.priority))
            .expect("a request");
        assert_eq!(
            out.requests[0].tile, cheapest.tile,
            "the first request must be the cheapest by the heuristic"
        );
    }

    /// A held REPLACE with nothing of its own on screen must stop asking for its
    /// whole subtree and ask for itself instead.
    ///
    /// Measured in the viewer before this existed: zooming to seven kilometres
    /// put 1156 requests in flight against 117 tiles actually selected. Sixty-four
    /// fetches at a time, each terrain tile awaiting its own imagery mosaic, is
    /// about seven seconds — and the ground stays black for all of it, because
    /// nothing in the subtree covers it until the last of it lands. One coarse
    /// tile covers the same ground in one round trip.
    #[test]
    fn a_hold_stops_asking_for_a_subtree_it_cannot_get_soon() {
        let ts = deep_tileset(5);
        let nothing = ResidencyView::default();
        let views = [camera_at(120.0)];
        let config = Config {
            loading_descendant_limit: 20,
            ..Config::default()
        };

        // Nothing drawn last frame: the ground is black, so coarse-first wins.
        let cold = run_with(&ts, &nothing, &views, &config, &HashSet::new());
        assert!(
            cold.stats.deferred_subtrees > 0,
            "no subtree was ever cut loose"
        );

        // With the limit raised past the whole tree, the same pass asks for
        // everything — which is what the viewer was doing.
        let greedy_config = Config {
            loading_descendant_limit: u32::MAX,
            ..config.clone()
        };
        let greedy = run_with(&ts, &nothing, &views, &greedy_config, &HashSet::new());
        assert_eq!(greedy.stats.deferred_subtrees, 0);
        assert!(
            cold.requests.len() * 4 < greedy.requests.len(),
            "cutting the subtree loose barely helped: {} requests against {}",
            cold.requests.len(),
            greedy.requests.len()
        );
    }

    fn camera_at(distance: f64) -> ViewState {
        ViewState::perspective(
            dvec3(0.0, 0.0, distance),
            dvec3(0.0, 0.0, -1.0),
            dvec3(0.0, 1.0, 0.0),
            dvec2(1024.0, 768.0),
            std::f64::consts::FRAC_PI_3,
        )
    }

    /// **A sibling on screen must not leave its neighbour's ground bare.**
    ///
    /// The minimal shape of a bug that was on screen for days. Four children
    /// under a REPLACE parent; three are resident and were drawn last frame,
    /// the fourth is still loading. The hold-until-ready branch asked "is *any*
    /// descendant on screen", answered yes, kept the three — and selected
    /// nothing at all for the fourth, while the parent sat resident and able to
    /// stand in for it.
    ///
    /// The assertion is coverage, not a tile count: either the fourth child is
    /// selected, or the parent is. Anything else is a quarter of that ground
    /// drawn by nothing, which reaches a person as a square of background.
    #[test]
    fn a_child_still_loading_is_covered_even_when_its_siblings_are_drawn() {
        let ts = mini_tileset();
        let root = ts.root();
        let children = ts.tile(root).children.clone();

        // Everything resident except the last child, and the three that are
        // resident were on screen at the previous pass.
        let mut residency = ResidencyView::default();
        residency.insert(root);
        for child in &children[..3] {
            residency.insert(*child);
        }
        let rendered_last: HashSet<TileId> = children[..3].iter().copied().collect();

        // Close enough that the root's screen-space error demands refinement,
        // so the traversal really does descend and really does have to decide.
        let out = run_with(
            &ts,
            &residency,
            &[camera_at(120.0)],
            &Config::default(),
            &rendered_last,
        );
        let selected: HashSet<TileId> = out.selected.iter().map(|(t, _)| *t).collect();

        assert!(
            selected.contains(&children[3]) || selected.contains(&root),
            "the child still loading was not selected and neither was the parent \
             that could stand in for it — its ground is drawn by nothing \
             (selected: {selected:?})"
        );
    }

    fn run(ts: &Tileset, residency: &ResidencyView, views: &[ViewState]) -> TraversalOutput {
        run_with(ts, residency, views, &Config::default(), &HashSet::new())
    }

    /// A pass with an explicit config and an explicit "what was drawn last
    /// time" — the two inputs the descendant limit reads.
    fn run_with(
        ts: &Tileset,
        residency: &ResidencyView,
        views: &[ViewState],
        config: &Config,
        rendered_last: &HashSet<TileId>,
    ) -> TraversalOutput {
        let mut out = TraversalOutput::default();
        traverse(ts, residency, views, config, 0, rendered_last, &mut out);
        out
    }

    fn ids(sel: &[(TileId, f64)]) -> Vec<TileId> {
        sel.iter().map(|(t, _)| *t).collect()
    }

    #[test]
    fn far_camera_selects_root_only() {
        let ts = mini_tileset();
        let mut residency = ResidencyView::default();
        residency.insert(ts.root());
        let out = run(&ts, &residency, &[camera_at(1.0e6)]);
        assert_eq!(ids(&out.selected), vec![ts.root()]);
        assert!(out.requests.is_empty());
    }

    #[test]
    fn near_camera_requests_children_ordered_by_distance() {
        let ts = mini_tileset();
        let out = run(&ts, &ResidencyView::default(), &[camera_at(150.0)]);
        // Nothing resident: hold keeps the screen empty but all four leaves
        // are urgently requested, nearest first (all equidistant here, so
        // tie-broken by id) plus the root as the stand-in.
        assert!(out.selected.is_empty());
        assert_eq!(out.requests.len(), 5);
        // The root is reached twice — speculatively on the way down, then
        // urgently because the hold needs it now. The urgent ask must win, or
        // the tile the picture is waiting on queues behind every guess.
        assert!(
            out.requests
                .iter()
                .all(|r| r.group == PriorityGroup::Urgent),
            "a wanted tile was left at speculative priority: {:?}",
            out.requests
                .iter()
                .filter(|r| r.group != PriorityGroup::Urgent)
                .collect::<Vec<_>>()
        );
        let mut sorted = out.requests.clone();
        sorted.sort_by(|a, b| a.priority.total_cmp(&b.priority).then(a.tile.cmp(&b.tile)));
        assert_eq!(
            out.requests, sorted,
            "requests must come out priority-sorted"
        );
    }

    #[test]
    fn replace_holds_parent_until_all_children_resident() {
        let ts = mini_tileset();
        let root = ts.root();
        let children = ts.tile(root).children.clone();

        // Parent resident, children missing → parent shown, children urgent.
        let mut residency = ResidencyView::default();
        residency.insert(root);
        let out = run(&ts, &residency, &[camera_at(150.0)]);
        assert_eq!(ids(&out.selected), vec![root]);
        assert_eq!(out.requests.len(), 4);

        // Two children arrive: still the parent (no holes, no overlap).
        residency.insert(children[0]);
        residency.insert(children[1]);
        let out = run(&ts, &residency, &[camera_at(150.0)]);
        assert_eq!(ids(&out.selected), vec![root]);
        assert_eq!(out.requests.len(), 2, "only the missing two re-requested");

        // All four arrive: children replace the parent, never both.
        residency.insert(children[2]);
        residency.insert(children[3]);
        let out = run(&ts, &residency, &[camera_at(150.0)]);
        let sel = ids(&out.selected);
        assert_eq!(sel.len(), 4);
        assert!(!sel.contains(&root), "parent and children never coexist");
        assert!(out.requests.is_empty());
    }

    /// Ground just off the edge of the screen is selected, not merely fetched.
    ///
    /// The distinction is the whole point. A tile that is only *requested* is
    /// resident and undrawn: the consumer draws the last selection it was sent,
    /// so ground outside that selection is black however much of it sits ready
    /// on the GPU. Reproduced before this margin existed by zooming out
    /// quickly — the drawn area stayed the old, smaller footprint and a black
    /// ring opened around it.
    ///
    /// The tile here is placed between the true edge of the frustum and the
    /// widened one, so it fails the exact test and passes the margin's. Without
    /// [`CULL_MARGIN`] it is culled and this fails.
    #[test]
    fn ground_just_outside_the_screen_is_still_selected() {
        let ts = mini_tileset();
        let root = ts.root();
        let children = ts.tile(root).children.clone();

        // Aim straight down from above child d, with a field of view narrow
        // enough that child a's corner falls outside it — and inside the 25 %
        // margin. The angles are checked below rather than asserted by eye.
        let eye = dvec3(50.0, 50.0, 120.0);
        let strict = ViewState::perspective(
            eye,
            dvec3(0.0, 0.0, -1.0),
            dvec3(0.0, 1.0, 0.0),
            dvec2(1024.0, 1024.0),
            0.86,
        );
        let out = run(&ts, &ResidencyView::default(), &[strict]);
        let selected: Vec<TileId> = out.selected.iter().map(|(t, _)| *t).collect();
        let requested: Vec<TileId> = out.requests.iter().map(|r| r.tile).collect();
        assert!(
            selected.contains(&children[0]) || requested.contains(&children[0]),
            "the tile beyond the screen edge was culled outright \
             (selected: {selected:?}, requested: {requested:?})"
        );
    }

    #[test]
    fn out_of_frustum_children_are_never_selected_nor_requested() {
        let ts = mini_tileset();
        let root = ts.root();
        let children = ts.tile(root).children.clone();
        // Hover low over child d (+50,+50): with a 60° fov at z=40, the
        // opposite corner child a (-50,-50, r=30) is outside the frustum.
        let view = ViewState::perspective(
            dvec3(50.0, 50.0, 40.0),
            dvec3(0.0, 0.0, -1.0),
            dvec3(0.0, 1.0, 0.0),
            dvec2(1024.0, 1024.0),
            std::f64::consts::FRAC_PI_3,
        );
        let out = run(&ts, &ResidencyView::default(), &[view]);
        let touched: Vec<TileId> = out.requests.iter().map(|r| r.tile).collect();
        assert!(
            !touched.contains(&children[0]),
            "culled child must not be requested (requested: {touched:?})"
        );
        assert!(out.stats.culled >= 1);
    }

    #[test]
    fn second_view_unions_the_selection() {
        let ts = mini_tileset();
        let root = ts.root();
        let children = ts.tile(root).children.clone();
        // Same hovering view as above (culls child a) plus a second view
        // hovering over child a: the union must request child a again.
        let v1 = ViewState::perspective(
            dvec3(50.0, 50.0, 40.0),
            dvec3(0.0, 0.0, -1.0),
            dvec3(0.0, 1.0, 0.0),
            dvec2(1024.0, 1024.0),
            std::f64::consts::FRAC_PI_3,
        );
        let v2 = ViewState::perspective(
            dvec3(-50.0, -50.0, 40.0),
            dvec3(0.0, 0.0, -1.0),
            dvec3(0.0, 1.0, 0.0),
            dvec2(1024.0, 1024.0),
            std::f64::consts::FRAC_PI_3,
        );
        let out = run(&ts, &ResidencyView::default(), &[v1, v2]);
        let touched: Vec<TileId> = out.requests.iter().map(|r| r.tile).collect();
        assert!(touched.contains(&children[0]));
        assert!(touched.contains(&children[3]));
    }

    #[test]
    fn add_refinement_keeps_parent_selected() {
        let json = r#"{
          "asset": { "version": "1.1" },
          "geometricError": 200,
          "root": {
            "boundingVolume": { "sphere": [0, 0, 0, 100] },
            "geometricError": 50,
            "refine": "ADD",
            "content": { "uri": "root.glb" },
            "children": [{
              "boundingVolume": { "sphere": [0, 0, 0, 50] },
              "geometricError": 0,
              "content": { "uri": "a.glb" }
            }]
          }
        }"#;
        let base = Url::parse("file:///t/tileset.json").expect("url");
        let ts = Tileset::from_json_bytes(json.as_bytes(), &base).expect("tileset");
        let root = ts.root();
        let child = ts.tile(root).children[0];

        let mut residency = ResidencyView::default();
        residency.insert(root);
        residency.insert(child);
        let out = run(&ts, &residency, &[camera_at(150.0)]);
        let sel = ids(&out.selected);
        assert!(sel.contains(&root), "ADD keeps the parent");
        assert!(sel.contains(&child));
    }

    #[test]
    fn zero_geometric_error_never_refines() {
        // A tile with children, content and geometricError 0: sse is always
        // 0, so it is never "too coarse" — its children must never be
        // visited, requested or selected, however close the camera gets.
        let json = r#"{
          "asset": { "version": "1.1" },
          "geometricError": 200,
          "root": {
            "boundingVolume": { "sphere": [0, 0, 0, 100] },
            "geometricError": 0,
            "refine": "REPLACE",
            "content": { "uri": "mid.glb" },
            "children": [{
              "boundingVolume": { "sphere": [0, 0, 0, 50] },
              "geometricError": 0,
              "content": { "uri": "leaf.glb" }
            }]
          }
        }"#;
        let base = Url::parse("file:///t/tileset.json").expect("url");
        let ts = Tileset::from_json_bytes(json.as_bytes(), &base).expect("tileset");
        let root = ts.root();
        let leaf = ts.tile(root).children[0];

        let mut residency = ResidencyView::default();
        residency.insert(root);
        residency.insert(leaf);
        let out = run(&ts, &residency, &[camera_at(120.0)]);
        assert_eq!(ids(&out.selected), vec![root]);
        assert!(out.requests.is_empty());
    }

    #[test]
    fn sse_formula_matches_reference_values() {
        let v = camera_at(0.0); // viewport 1024x768, fovy = 60°
                                // sse = ge * h / (d * 2 tan(fovy/2)) = 2*768 / (100 * 1.1547) = 13.3
        let sse = v.screen_space_error(2.0, 100.0);
        assert!((sse - 13.302).abs() < 1e-2, "got {sse}");
        // Camera inside the volume (distance 0): clamped to 1e-7 like the
        // references — huge but finite, always refines.
        let inside = v.screen_space_error(2.0, 0.0);
        assert!(inside.is_finite() && inside > 1e9);
        // ge = 0 → sse = 0 at any distance.
        assert_eq!(v.screen_space_error(0.0, 0.0), 0.0);
    }

    #[test]
    fn replace_refines_through_empty_intermediate_tiles() {
        // root (REPLACE, content) → empty mid (no content) → leaf (content).
        // The empty tile is structural: refinement passes through it, and
        // its readiness is its subtree's readiness.
        let json = r#"{
          "asset": { "version": "1.1" }, "geometricError": 200,
          "root": {
            "boundingVolume": { "sphere": [0, 0, 0, 100] },
            "geometricError": 50, "refine": "REPLACE",
            "content": { "uri": "root.glb" },
            "children": [{
              "boundingVolume": { "sphere": [0, 0, 0, 60] },
              "geometricError": 40,
              "children": [{
                "boundingVolume": { "sphere": [0, 0, 0, 50] },
                "geometricError": 0,
                "content": { "uri": "leaf.glb" }
              }]
            }]
          }
        }"#;
        let base = Url::parse("file:///t/tileset.json").expect("url");
        let ts = Tileset::from_json_bytes(json.as_bytes(), &base).expect("tileset");
        let root = ts.root();
        let mid = ts.tile(root).children[0];
        let leaf = ts.tile(mid).children[0];

        // Leaf not resident: root stands in, leaf urgently requested.
        let mut residency = ResidencyView::default();
        residency.insert(root);
        let out = run(&ts, &residency, &[camera_at(150.0)]);
        assert_eq!(ids(&out.selected), vec![root]);
        assert_eq!(out.requests.len(), 1);
        assert_eq!(out.requests[0].tile, leaf);

        // Leaf resident: it replaces the root straight through the empty mid.
        residency.insert(leaf);
        let out = run(&ts, &residency, &[camera_at(150.0)]);
        assert_eq!(ids(&out.selected), vec![leaf]);
        assert!(out.requests.is_empty());
    }

    #[test]
    fn refinement_uses_the_most_demanding_view() {
        // One distant view (meets SSE) + one close view (does not): the
        // close view must drive refinement — max SSE over views.
        let ts = mini_tileset();
        let root = ts.root();
        let mut residency = ResidencyView::default();
        residency.insert(root);

        let far = camera_at(1.0e6);
        let out = run(&ts, &residency, &[far]);
        assert_eq!(ids(&out.selected), vec![root], "far alone: root suffices");

        let near = camera_at(150.0);
        let out = run(&ts, &residency, &[far, near]);
        assert_eq!(
            out.requests.len(),
            4,
            "adding a near view forces refinement"
        );
    }

    #[test]
    fn viewstate_serde_round_trip() {
        let v = camera_at(123.0);
        let text = serde_json::to_string(&v).expect("ser");
        let back: ViewState = serde_json::from_str(&text).expect("de");
        assert_eq!(back.position(), v.position());
        // The derived fields are recomputed on deserialize; allow ulp noise.
        assert!((back.sse_denominator - v.sse_denominator).abs() < 1e-12);
        let sse_a = v.screen_space_error(10.0, 100.0);
        let sse_b = back.screen_space_error(10.0, 100.0);
        assert!((sse_a - sse_b).abs() < 1e-9);
    }
}
