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
use std::collections::HashSet;

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
        let proj = DMat4::perspective_rh(fovy_rad, aspect, 0.05, 1.0e10);
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
    /// Soft cap on simultaneously loading descendants (reserved, M2 honours it).
    pub loading_descendant_limit: u32,
    /// Resident-content budget in bytes (CPU side). Default 512 MiB.
    ///
    /// A consumer that uploads this content pays more than this for it: mip
    /// chains, interleaved vertices and format padding all land on its side of
    /// the wire. Budget for the consumer's memory, not for this number.
    pub resident_budget_bytes: usize,
    /// Maximum simultaneous content fetches.
    pub maximum_simultaneous_fetches: usize,
    /// How many recent camera positions keep a tile safe from eviction.
    ///
    /// `1` protects only what the current view needs, which makes a turning
    /// camera evict the tiles behind it and reload them the moment it turns
    /// back — the residency has no memory of where it just was. Counting
    /// *camera positions* rather than traversals is deliberate: a traversal
    /// also runs on every load completion, so a window measured in traversals
    /// would expire in milliseconds under a burst of fetches.
    ///
    /// Larger values hold more, and are the difference between a smooth
    /// rotation and one that re-streams its own wake. Default 4.
    pub protected_view_generations: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            maximum_screen_space_error: 16.0,
            forbid_holes: true,
            loading_descendant_limit: 20,
            resident_budget_bytes: 512 * 1024 * 1024,
            maximum_simultaneous_fetches: 20,
            protected_view_generations: 4,
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
    pub visited: u32,
    pub culled: u32,
    pub selected: u32,
    pub requested: u32,
    pub max_depth: u32,
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
    out: &mut TraversalOutput,
) {
    out.selected.clear();
    out.requests.clear();
    out.stats = TraversalStats::default();
    let roots = tree.roots();
    if views.is_empty() || roots.is_empty() {
        return;
    }

    let mut requested = HashSet::new();
    for root in roots {
        visit(tree, residency, views, config, root, 0, out, &mut requested);
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

/// Recursive visit over the abstract [`TileTree`]. Returns whether this
/// subtree renders a complete picture (everything it decided to show is
/// resident) — the REPLACE hold-until-ready predicate.
#[allow(clippy::too_many_arguments)]
fn visit(
    tree: &dyn TileTree,
    residency: &ResidencyView,
    views: &[ViewState],
    config: &Config,
    id: TileId,
    depth: u32,
    out: &mut TraversalOutput,
    requested: &mut HashSet<TileId>,
) -> bool {
    let props = tree.properties(id);
    out.stats.visited += 1;
    out.stats.max_depth = out.stats.max_depth.max(depth);

    // Frustum culling: a tile invisible in every view contributes nothing
    // and never blocks an ancestor's REPLACE.
    if !views
        .iter()
        .any(|v| props.bounding_volume.intersects_frustum(v.frustum()))
    {
        out.stats.culled += 1;
        return true;
    }

    let distance = views
        .iter()
        .map(|v| props.bounding_volume.distance_to_point(v.position()))
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

    if !refines {
        if !has_content {
            // Leaf without content: nothing to show, nothing to wait for.
            return true;
        }
        if residency.is_resident(id) {
            out.selected.push((id, sse));
            return true;
        }
        request(out, requested, id, PriorityGroup::Urgent, distance);
        return false;
    }

    match props.refine {
        Refine::Add => {
            // Additive: the parent stays visible; children refine on top.
            if has_content {
                if residency.is_resident(id) {
                    out.selected.push((id, sse));
                } else {
                    request(out, requested, id, PriorityGroup::Normal, distance);
                }
            }
            for child in children {
                visit(
                    tree,
                    residency,
                    views,
                    config,
                    child,
                    depth + 1,
                    out,
                    requested,
                );
            }
            true
        }
        Refine::Replace => {
            // Tentatively select the children; roll their selections back if
            // any visible branch is not renderable yet (hold-until-ready).
            let mark = out.selected.len();
            let mut all_ready = true;
            for child in children {
                all_ready &= visit(
                    tree,
                    residency,
                    views,
                    config,
                    child,
                    depth + 1,
                    out,
                    requested,
                );
            }
            if all_ready || !config.forbid_holes {
                return all_ready;
            }
            // Hold: drop descendant selections, stand in with this tile.
            out.selected.truncate(mark);
            if has_content {
                if residency.is_resident(id) {
                    out.selected.push((id, sse));
                    return true;
                }
                request(out, requested, id, PriorityGroup::Urgent, distance);
            }
            false
        }
    }
}

fn request(
    out: &mut TraversalOutput,
    requested: &mut HashSet<TileId>,
    tile: TileId,
    group: PriorityGroup,
    priority: f64,
) {
    if requested.insert(tile) {
        out.requests.push(ContentRequest {
            tile,
            group,
            priority,
        });
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

    fn camera_at(distance: f64) -> ViewState {
        ViewState::perspective(
            dvec3(0.0, 0.0, distance),
            dvec3(0.0, 0.0, -1.0),
            dvec3(0.0, 1.0, 0.0),
            dvec2(1024.0, 768.0),
            std::f64::consts::FRAC_PI_3,
        )
    }

    fn run(ts: &Tileset, residency: &ResidencyView, views: &[ViewState]) -> TraversalOutput {
        let mut out = TraversalOutput::default();
        traverse(ts, residency, views, &Config::default(), 0, &mut out);
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
        assert!(out
            .requests
            .iter()
            .all(|r| r.group == PriorityGroup::Urgent));
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
