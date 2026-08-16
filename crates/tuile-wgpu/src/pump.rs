// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The async→frame bridge: drains a [`GeometryStream`] each frame, spreads
//! GPU uploads over frames (no hitches), acknowledges materialized tiles
//! and frees evicted ones.
//!
//! The pump does not know which binding feeds it — in-process, WebSocket
//! or HTTP pull — that's the point of the trait (`docs/11-crate-wgpu.md`).

use crate::context::GpuContext;
use crate::prepare::{prepare, PreparedTile};
use glam::DVec3;
use std::collections::{HashMap, HashSet, VecDeque};
use std::task::{Context, Poll};
use tuile_core::content::{DecodedTileContent, TileContent};
use tuile_core::protocol::{ClientMessage, GeometryStream, Priming, ServerMessage};
use tuile_core::source::TileId;
use tuile_core::traversal::TraversalStats;
use tuile_core::traversal::ViewState;

pub struct ContentPump {
    /// The f64 world point all tiles and the view matrix are relative to.
    pub render_origin: DVec3,
    prepared: HashMap<TileId, PreparedTile>,
    /// Decoded tiles awaiting a frame's upload budget. The flag says whether
    /// the entry is a stand-in — see [`ServerMessage::Fill`].
    pending: VecDeque<(TileId, DecodedTileContent, bool)>,
    /// Which prepared tiles are stand-ins rather than real geometry.
    fills: HashSet<TileId>,
    /// Current selection (tile + SSE), as sent by the geometry server.
    pub selection: Vec<(TileId, f64)>,
    pub stats: TraversalStats,
    /// Sum of GPU bytes of prepared tiles.
    pub gpu_bytes: usize,
    /// True once the server side hung up.
    pub closed: bool,
    /// The coarse pyramid, as last reported by the server. `None` until it has
    /// said anything at all — which is not the same as "nothing to wait for".
    pub priming: Option<Priming>,
    /// Non-fatal errors reported by the server (drain freely).
    pub errors: Vec<String>,
}

/// Tiles uploaded per frame by an interactive session.
///
/// Shared rather than written into the host, for the reason the whole
/// `Config::interactive_globe` exists: a headless test that picks its own
/// number is testing a renderer nobody runs. Uploading is a stall — buffers are
/// created and written on the frame's own thread — so this is deliberately
/// small, and it is why a stand-in must jump the queue rather than wait behind
/// real content.
pub const UPLOADS_PER_FRAME: usize = 8;

impl ContentPump {
    pub fn new(render_origin: DVec3) -> Self {
        Self {
            render_origin,
            prepared: HashMap::new(),
            pending: VecDeque::new(),
            fills: HashSet::new(),
            selection: Vec::new(),
            stats: TraversalStats::default(),
            gpu_bytes: 0,
            closed: false,
            priming: None,
            errors: Vec::new(),
        }
    }

    /// Call once per frame: drains every queued server message, then
    /// performs at most `max_uploads` GPU uploads. Returns the number of
    /// tiles uploaded this call.
    pub fn pump<S: GeometryStream>(
        &mut self,
        stream: &mut S,
        gpu: &GpuContext,
        max_uploads: usize,
    ) -> usize {
        let waker = futures_util::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        loop {
            match stream.poll_message(&mut cx) {
                Poll::Ready(Some(msg)) => self.on_message(msg),
                Poll::Ready(None) => {
                    self.closed = true;
                    break;
                }
                Poll::Pending => break,
            }
        }

        let mut uploaded = 0;
        while uploaded < max_uploads {
            let Some((tile, content, is_fill)) = self.pending.pop_front() else {
                break;
            };
            // The real tile may have landed while this stand-in waited its turn
            // in the queue. Uploading it now would overwrite geometry with an
            // approximation, and the frame after would look *worse* than the one
            // before — the one failure mode a stand-in must never have.
            if is_fill && self.prepared.contains_key(&tile) && !self.fills.contains(&tile) {
                continue;
            }
            let m = tuile_core::metrics::metrics();
            let started = std::time::Instant::now();
            let prepared = prepare(gpu, &content, self.render_origin);
            m.upload_seconds.record(started.elapsed());
            m.uploads.inc();
            let level = tile.terrain_coord().0;
            m.meshes_by_level.inc(level);
            m.mesh_bytes_by_level.add(level, prepared.gpu_bytes as u64);
            // Imagery is shared, so it is counted where it is actually held —
            // once per texture, at the *imagery* level, which is not this tile's.
            m.textures_by_level.clear();
            m.texture_bytes_by_level.clear();
            for held in gpu
                .imagery
                .lock()
                .expect("imagery textures")
                .live_by_level()
            {
                m.textures_by_level.inc(held.0);
                m.texture_bytes_by_level.add(held.0, held.1 as u64);
            }
            self.gpu_bytes += prepared.gpu_bytes;
            // A re-upload replaces its predecessor: charge for the new one,
            // refund the old, or the total drifts up until it is fiction.
            if let Some(replaced) = self.prepared.insert(tile, prepared) {
                self.gpu_bytes -= replaced.gpu_bytes;
                m.meshes_by_level.sub(level, 1);
                m.mesh_bytes_by_level.sub(level, replaced.gpu_bytes as u64);
            }
            m.prepared_tiles.set(self.prepared.len() as u64);
            m.pending_uploads.set(self.pending.len() as u64);
            // Best effort: a closed stream just means the session is over.
            if is_fill {
                self.fills.insert(tile);
            } else {
                self.fills.remove(&tile);
            }
            // Only real geometry is acknowledged. Acking a stand-in would tell
            // the server the consumer holds the tile, and it would stop sending
            // the very thing everyone is waiting for.
            if !is_fill {
                // Best effort: a closed stream just means the session is over.
                let _ = stream.send(ClientMessage::Ack { tile });
            }
            uploaded += 1;
        }
        uploaded
    }

    /// Re-bases every prepared tile onto a new render origin — call with the
    /// camera position each frame so the f32 coordinates the GPU sees stay
    /// near zero (sub-meter precise) however far the camera is from the
    /// geocenter. Skips the work when the origin hasn't meaningfully moved.
    /// One frame of the streaming loop, exactly as an interactive host runs it.
    ///
    /// Send the camera, take whatever the server has produced up to this
    /// frame's upload budget, and rebase what is resident onto the new origin —
    /// in that order, which is the order that matters. Rebasing before the
    /// uploads would leave the tiles that arrived this frame holding a model
    /// matrix for the *previous* origin, and at planetary scale that draws them
    /// somewhere else entirely.
    ///
    /// Extracted from `wgpu-viewer` so a headless test can run the host's own
    /// loop rather than an approximation of it. Everything after this — the
    /// resolve and the draw — is immutable and belongs to whoever owns a
    /// surface.
    ///
    /// Returns how many tiles reached the GPU.
    pub fn advance<S: GeometryStream>(
        &mut self,
        stream: &mut S,
        gpu: &crate::context::GpuContext,
        view: ViewState,
        render_origin: DVec3,
    ) -> usize {
        // Best effort: a closed stream means the session is over, and a frame is
        // not the place to discover it.
        let _ = stream.send(ClientMessage::ViewerState { views: vec![view] });
        let uploaded = self.pump(stream, gpu, UPLOADS_PER_FRAME);
        self.rebase(&gpu.queue, render_origin);
        uploaded
    }

    /// Moves the render origin. Tiles follow when they are drawn, not now.
    ///
    /// This used to rewrite the model uniform of **every resident tile** on any
    /// frame where the eye had moved a metre — a session holding 8 500 tiles to
    /// draw 400 paid eight thousand `write_buffer` calls per moving frame. A
    /// profile named the cost precisely: each one allocates a fresh Metal
    /// staging buffer through `StagingBuffer::new`, each allocation is a kernel
    /// round trip, and freeing them again at submit cost as much again. Together
    /// they were 60 % of the real work in a frame, and all of it only while the
    /// camera moved — which is exactly when it was felt.
    ///
    /// So the origin is recorded and nothing is written. A tile brings itself up
    /// to date in [`Self::rebase_for_drawing`], which is called with the handful
    /// that will actually be drawn.
    pub fn rebase(&mut self, _queue: &wgpu::Queue, render_origin: DVec3) {
        self.render_origin = render_origin;
    }

    /// Brings the tiles about to be drawn onto the current render origin.
    ///
    /// Called with the output of [`Self::resolve`], so the work is proportional
    /// to what is on screen rather than to what the session has accumulated.
    /// Each tile skips itself if it is already close enough, so a still camera
    /// writes nothing at all.
    pub fn rebase_for_drawing<'a>(
        &self,
        queue: &wgpu::Queue,
        tiles: impl IntoIterator<Item = &'a PreparedTile>,
    ) {
        for tile in tiles {
            tile.rebase(queue, self.render_origin);
        }
    }

    fn on_message(&mut self, msg: ServerMessage) {
        match msg {
            ServerMessage::Select { tiles, stats } => {
                self.selection = tiles;
                self.stats = stats;
            }
            ServerMessage::Content { tile, content } => match content {
                TileContent::Decoded(decoded) => self.pending.push_back((tile, decoded, false)),
                TileContent::Raw { .. } => self
                    .errors
                    .push(format!("tile {tile:?}: raw content reached the renderer")),
            },
            // A stand-in, refused whenever the real thing is already here or on
            // its way. Both are filed under the same tile, so accepting a late
            // one would replace real geometry with an approximation — the exact
            // failure the separate variant exists to make impossible to miss.
            ServerMessage::Fill { tile, content } => {
                let already = self.prepared.contains_key(&tile)
                    && !self.fills.contains(&tile)
                    || self.pending.iter().any(|(t, _, fill)| *t == tile && !*fill);
                if !already {
                    // Ahead of real content, and deliberately. A stand-in is
                    // twenty-five vertices and it is what keeps the ground
                    // covered; a real tile is orders of magnitude larger and
                    // its absence is invisible while a stand-in holds its
                    // place. Queued behind, they would arrive frames late and
                    // the hole they exist to fill would be on screen the whole
                    // time.
                    self.pending.push_front((tile, content, true));
                }
            }
            ServerMessage::Evict { tiles } => {
                let m = tuile_core::metrics::metrics();
                for tile in &tiles {
                    if let Some(p) = self.prepared.remove(tile) {
                        self.fills.remove(tile);
                        self.gpu_bytes -= p.gpu_bytes;
                        let level = tile.terrain_coord().0;
                        m.meshes_by_level.sub(level, 1);
                        m.mesh_bytes_by_level.sub(level, p.gpu_bytes as u64);
                    }
                }
                m.prepared_tiles.set(self.prepared.len() as u64);
                // Uploads are spread over frames, so a tile can still be
                // queued when its eviction arrives. Dropping it here is what
                // keeps that from leaking: uploaded after its own `Evict`, it
                // would enter `prepared` with the server no longer considering
                // it resident — so no further `Evict` would ever name it, and
                // its GPU memory would be held until the session ends.
                self.pending.retain(|(t, _, _)| !tiles.contains(t));
            }
            ServerMessage::Error { tile, message } => {
                self.errors.push(match tile {
                    Some(t) => format!("tile {t:?}: {message}"),
                    None => message,
                });
            }
            ServerMessage::Priming(priming) => self.priming = Some(priming),
        }
    }

    /// Selected tiles whose GPU resources are ready to draw.
    pub fn visible(&self) -> impl Iterator<Item = &PreparedTile> {
        self.selection
            .iter()
            .filter_map(|(t, _)| self.prepared.get(t))
    }


    /// What the selection resolved to, and what it failed to resolve to.
    ///
    /// Three outcomes per selected tile, and only the third is a bug:
    ///
    /// | outcome | on screen |
    /// |---|---|
    /// | the tile itself is prepared | the intended detail |
    /// | an ancestor is prepared | coarser ground — degraded, not broken |
    /// | the walk reached a root with nothing | **nothing at all: the black square** |
    ///
    /// The three used to be one number. `missing` counts the first against the
    /// other two, which says how *sharp* the picture is and nothing about
    /// whether it is there; and it is computed against the last selection
    /// received, so a stale one reports `missing 0` while describing ground
    /// nobody is looking at. Black ground was therefore invisible in the logs
    /// for two days — the count that mattered was never taken.
    /// Takes the queue because resolving is also what brings the chosen tiles
    /// onto the current render origin.
    ///
    /// Those were two calls for one afternoon, and the second one was forgotten
    /// exactly where it mattered: a harness that resolved and drew without
    /// rebasing put **60 % of a frame bare** during a zoom, because every tile
    /// was still positioned against an origin the eye had left. Nothing in the
    /// type system objected. Deciding what to draw and making it drawable are
    /// one act, so they are one call, and the mistake is no longer available.
    pub fn resolve(
        &self,
        queue: &wgpu::Queue,
        parent_of: impl Fn(TileId) -> Option<TileId>,
    ) -> (Drawn<'_>, Resolution) {
        let (drawn, counts, _) = self.resolve_reporting(queue, parent_of);
        (drawn, counts)
    }

    /// [`Self::resolve`], and the tiles it could not draw exactly.
    ///
    /// Those are the fill candidates, and naming them is the point: a tile drawn
    /// by its ancestor is not a hole — it is *worse than a hole in one specific
    /// way*, because the ancestor also covers the siblings that did arrive, so
    /// two surfaces end up over the same ground and the depth test picks a
    /// winner per pixel. Giving each of these its own stand-in over its own
    /// rectangle is what removes both the black and the shimmer.
    pub fn resolve_reporting(
        &self,
        queue: &wgpu::Queue,
        parent_of: impl Fn(TileId) -> Option<TileId>,
    ) -> (Drawn<'_>, Resolution, Vec<TileId>) {
        let (exact, fallback, counts, unresolved) = walk(
            &self.selection,
            |id| self.prepared.contains_key(&id),
            parent_of,
        );
        let surfaces = |ids: Vec<TileId>| -> Vec<&PreparedTile> {
            ids.into_iter()
                .filter_map(|id| self.prepared.get(&id))
                .collect()
        };
        let drawn = Drawn {
            exact: surfaces(exact),
            fallback: surfaces(fallback),
        };
        self.rebase_for_drawing(
            queue,
            drawn.exact.iter().chain(drawn.fallback.iter()).copied(),
        );
        (drawn, counts, unresolved)
    }

    /// Number of selected tiles still waiting for content or upload.
    ///
    /// Says how sharp the picture is, not whether it is there — see
    /// [`ContentPump::resolve`].
    pub fn missing(&self) -> usize {
        self.selection
            .iter()
            .filter(|(t, _)| !self.prepared.contains_key(t))
            .count()
    }

    /// Whether the GPU holds a surface for `tile` — **its own**, real or
    /// stand-in.
    ///
    /// The question `resolve` asks of every selected tile, and the one that
    /// decides whether an ancestor is drawn over it. Exposed because it is the
    /// only honest way to observe the stand-in path from outside: a stand-in
    /// and the real tile are deliberately indistinguishable here, and a test
    /// that could tell them apart would be testing something the renderer does
    /// not know.
    pub fn has(&self, tile: TileId) -> bool {
        self.prepared.contains_key(&tile)
    }

    pub fn prepared_count(&self) -> usize {
        self.prepared.len()
    }

    /// How many tiles at or above `level` are on the GPU.
    ///
    /// What a host gates its first frame on: the coarse pyramid is the floor
    /// every fallback lands on, and showing a globe before it is there is
    /// showing the one state the whole design exists to avoid.
    /// Every prepared tile at exactly `level`, in no particular order.
    ///
    /// For drawing a complete coarse layer **behind** the selection. The tiles
    /// are already here — pinned, uploaded, and protected from eviction — and
    /// the only thing missing was an instruction to draw them: `resolve` walks
    /// up from *selected* tiles, so ground the traversal never selected asks
    /// nothing of the coarse tile sitting ready beside it.
    ///
    /// One level, not the pyramid: a single complete level covers the globe on
    /// its own, and level 3 is 128 tiles — a background pass that costs a
    /// hundred draw calls rather than three thousand.
    pub fn at_level(&self, level: u32) -> impl Iterator<Item = &PreparedTile> {
        self.prepared
            .iter()
            .filter(move |(id, _)| id.terrain_coord().0 == level)
            .map(|(_, tile)| tile)
    }

    pub fn prepared_through_level(&self, level: u32) -> usize {
        self.prepared
            .keys()
            .filter(|t| t.terrain_coord().0 <= level)
            .count()
    }

    pub fn pending_uploads(&self) -> usize {
        self.pending.len()
    }
}

/// The surfaces a frame draws, in the two kinds that must not be mixed.
///
/// # Why they are separated
///
/// An ancestor drawn because some descendant has not arrived spans **all** of
/// that ancestor's descendants — including the siblings that did arrive. Drawn
/// as ordinary geometry it therefore competes with them for the depth buffer,
/// and over real relief a coarse tessellation crosses a fine one repeatedly: the
/// winner changes from pixel to pixel along the crossing curve. On screen that
/// is sharp imagery and blurry imagery interleaved in ragged outlines that
/// follow the terrain rather than the tile grid — measured at 100 % of the
/// pixels over a tile that had its own surface, in
/// `tests/no_two_surfaces.rs`.
///
/// So a fallback is not geometry, it is **backdrop**: it is drawn without
/// touching depth, before everything, exactly like the whole-planet shell. It
/// still colours every pixel nothing else covers — no coverage is refused, which
/// is what the two earlier attempts at this got wrong — but it can never win a
/// pixel from a surface that owns that ground.
///
/// What it costs, stated: a fallback does not depth-test against *itself*
/// either, so relief inside one cannot occlude relief behind it. At a grazing
/// angle a far ridge of a stand-in ancestor can paint over a near one, for the
/// few frames the real tile takes to arrive. That is a wrong-but-plausible
/// picture of ground that is coarse anyway, against a per-pixel flicker over
/// ground that is already correct.
pub struct Drawn<'a> {
    /// Tiles the traversal chose, holding their own ground.
    pub exact: Vec<&'a PreparedTile>,
    /// Ancestors standing in for ground that is not their own.
    pub fallback: Vec<&'a PreparedTile>,
}

/// How a selection resolved onto what the GPU actually holds.
///
/// See [`ContentPump::resolve`]. `lost` is the only one of the three that is a
/// defect: it is ground the traversal asked for, with not one tile on its
/// ancestor chain resident to stand in — which on screen is the clear colour,
/// and the clear colour here is black.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    /// Drawn at the level the traversal chose.
    pub exact: usize,
    /// Drawn by a coarser ancestor while the chosen tile streams in.
    pub coarser: usize,
    /// Drawn by nothing.
    pub lost: usize,
    /// The deepest level that was lost, which says whether the hole is a
    /// detail tile at the horizon or a whole coarse region.
    pub deepest_lost: u32,
    /// The largest number of levels any fallback had to climb.
    ///
    /// A stand-in one or two levels up is a softer patch of ground. One eight
    /// levels up is a mesh spanning a whole region, interpolated straight
    /// through the relief — and a camera near the surface is then *underneath*
    /// it, seeing only its back faces, which the pipeline discards. That draws
    /// nothing while counting as a success, which is why the gap is measured
    /// and not just the fact of falling back.
    pub worst_gap: u32,
    /// The level actually drawn, and the level asked for, at that worst gap.
    pub worst_gap_drawn: u32,
    pub worst_gap_wanted: u32,
}

impl Resolution {
    /// Whether any ground was drawn by nothing.
    pub fn has_holes(&self) -> bool {
        self.lost > 0
    }
}


/// Which tiles to draw for a selection, and what that cost in sharpness.
///
/// Pure, and separated from the GPU so the invariant it encodes can be tested
/// without a device, an adapter and a texture format.
///
/// # The rule, and the two ways it has been broken
///
/// For each selected tile: draw its own surface, or the nearest ancestor that
/// has one. **The climb is never refused.** Coverage is the whole job, and
/// every refusal is a hole — black ground, which the project forbids outright
/// (see `CLAUDE.md`).
///
/// Two attempts to be cleverer both had to be undone, and they are recorded
/// here so they are not attempted a third time:
///
/// 1. *Stop climbing once the server sends stand-ins.* An ancestor spans all of
///    its descendants, so drawing one for a tile that lacks a surface also
///    covers siblings that have one — two surfaces over one patch of ground,
///    which shimmers. Removing the climb removed the shimmer and replaced it
///    with large black rectangles wherever a stand-in had not yet arrived.
/// 2. *Refuse only the ancestors that would overlap something already drawn.*
///    Surgical in principle, holes in practice: refusing is still refusing, and
///    the ground it declines to cover is black until something else arrives.
///
/// The overlap is real and it is the lesser evil. Removing it means giving every
/// selected tile a surface of its own **before** the ancestor stops being drawn
/// — new active, then old inactive, never a gap — not withholding the fallback
/// and hoping.
fn walk(
    selection: &[(TileId, f64)],
    has: impl Fn(TileId) -> bool,
    parent_of: impl Fn(TileId) -> Option<TileId>,
) -> (Vec<TileId>, Vec<TileId>, Resolution, Vec<TileId>) {
    let mut out = Vec::new();
    let mut fallback = Vec::new();
    let mut seen = HashSet::new();
    let mut counts = Resolution::default();
    let mut unresolved = Vec::new();
    for (tile, _) in selection {
        let mut cur = Some(*tile);
        let mut exact = true;
        loop {
            let Some(id) = cur else {
                // Off the top of the tree with nothing anywhere on the way: this
                // ground is drawn by nothing, and that is the one number worth
                // shouting about.
                counts.lost += 1;
                counts.deepest_lost = counts.deepest_lost.max(tile.terrain_coord().0);
                unresolved.push(*tile);
                break;
            };
            if has(id) {
                if seen.insert(id) {
                    if exact {
                        out.push(id);
                    } else {
                        fallback.push(id);
                    }
                }
                if exact {
                    counts.exact += 1;
                } else {
                    counts.coarser += 1;
                    unresolved.push(*tile);
                    let gap = tile.terrain_coord().0.saturating_sub(id.terrain_coord().0);
                    if gap > counts.worst_gap {
                        counts.worst_gap = gap;
                        counts.worst_gap_drawn = id.terrain_coord().0;
                        counts.worst_gap_wanted = tile.terrain_coord().0;
                    }
                }
                break;
            }
            exact = false;
            cur = parent_of(id);
        }
    }
    // A tile can be both: chosen by the traversal *and* the nearest resident
    // ancestor of some other tile. It is then real geometry and must not be
    // demoted — the fallback list is only for surfaces standing in for ground
    // that is not their own.
    let exact: HashSet<TileId> = out.iter().copied().collect();
    fallback.retain(|id| !exact.contains(id));
    (out, fallback, counts, unresolved)
}

#[cfg(test)]
mod coverage_tests {
    use super::*;

    fn parent_of(id: TileId) -> Option<TileId> {
        let (z, x, y) = id.terrain_coord();
        (z > 0).then(|| TileId::from_terrain(z - 1, x / 2, y / 2))
    }

    /// How many of `drawn` sit over ground another of them already covers.
    ///
    /// An ancestor spans every one of its descendants, so an ancestor and a
    /// descendant both being drawn *into the same depth buffer* is two surfaces
    /// over one patch of ground: both write depth, and over real relief a coarse
    /// tessellation crosses a fine one repeatedly, so the winner changes from
    /// pixel to pixel along the crossing curve.
    ///
    /// Counting it is the point. "Two surfaces over the same ground" was
    /// described in a comment for months and never measured, which is why it
    /// survived three wrong explanations before
    /// `tests/no_two_surfaces.rs` put a number on it.
    fn overlapping_surfaces(drawn: &[TileId]) -> usize {
        let is_ancestor_of = |a: TileId, b: TileId| {
            let (az, ax, ay) = a.terrain_coord();
            let (bz, bx, by) = b.terrain_coord();
            let up = match bz.checked_sub(az) {
                Some(0) | None => return false,
                Some(up) => up,
            };
            bx >> up == ax && by >> up == ay
        };
        drawn
            .iter()
            .flat_map(|a| drawn.iter().map(move |b| (*a, *b)))
            .filter(|(a, b)| is_ancestor_of(*a, *b))
            .count()
    }

    /// **Coverage is never traded away.** A selected tile with no surface of its
    /// own is still covered by its nearest resident ancestor.
    ///
    /// Two attempts to remove the overlap by *withholding* that ancestor — stop
    /// climbing entirely, then refuse only the ancestors that overlap — each
    /// replaced the shimmer with large black rectangles at both zoom-in and
    /// zoom-out. Black ground is forbidden (`CLAUDE.md`); shimmer is not.
    #[test]
    fn an_ancestor_still_covers_a_tile_that_has_not_arrived() {
        let here = TileId::from_terrain(3, 4, 4);
        let neighbour = TileId::from_terrain(3, 5, 4);
        let grandparent = TileId::from_terrain(1, 1, 1);
        let selection = vec![(here, 0.0), (neighbour, 0.0)];
        let has = |id: TileId| id == here || id == grandparent;

        let (exact, fallback, counts, unresolved) = walk(&selection, has, parent_of);
        assert_eq!(
            exact,
            vec![here],
            "the tile that arrived is the only one holding its own ground"
        );
        assert_eq!(
            fallback,
            vec![grandparent],
            "the fallback was withheld, which is a hole — black ground is never \
             the lesser evil"
        );
        assert_eq!(counts.lost, 0, "nothing may be left uncovered");
        assert_eq!(counts.exact, 1);
        assert_eq!(counts.coarser, 1);
        assert_eq!(
            unresolved,
            vec![neighbour],
            "a tile drawn only by an ancestor must still be reported as wanting \
             its own surface"
        );
    }

    /// **Nothing that competes for the depth buffer overlaps anything else.**
    ///
    /// This is the invariant, and it holds for *every* selection rather than for
    /// the lucky ones. The two kinds are separated at the source: tiles holding
    /// their own ground go to `exact` and compete normally; ancestors standing in
    /// for ground that is not theirs go to `fallback` and are drawn as backdrop,
    /// without touching depth. So an ancestor can never win a pixel from a
    /// surface that owns it, whatever the relief does.
    ///
    /// The fixture is the one that used to z-fight: a tile, a neighbour that has
    /// not arrived, and the grandparent covering both.
    #[test]
    fn the_depth_buffer_never_sees_two_surfaces_on_one_patch() {
        let here = TileId::from_terrain(3, 4, 4);
        let neighbour = TileId::from_terrain(3, 5, 4);
        let grandparent = TileId::from_terrain(1, 1, 1);
        let selection = vec![(here, 0.0), (neighbour, 0.0)];

        let (exact, fallback, _, _) =
            walk(&selection, |id| id == here || id == grandparent, parent_of);
        assert_eq!(
            overlapping_surfaces(&exact),
            0,
            "two competing surfaces over one patch of ground — exact: {exact:?}"
        );
        assert!(
            !exact.contains(&grandparent),
            "the ancestor is standing in for ground that is not its own and must \
             not compete for it — exact: {exact:?}, fallback: {fallback:?}"
        );
    }

    /// **A tile that is both chosen and an ancestor stays real geometry.**
    ///
    /// The demotion is per *role*, not per tile: a coarse tile the traversal
    /// selected in its own right owns its ground, and it may simultaneously be
    /// the nearest resident ancestor of some deeper tile that has not arrived.
    /// Demoting it then would take a chosen surface out of the depth buffer and
    /// let anything drawn later paint over it.
    #[test]
    fn a_selected_tile_is_not_demoted_because_something_else_leans_on_it() {
        let coarse = TileId::from_terrain(1, 1, 1);
        let deep = TileId::from_terrain(3, 4, 4);
        let selection = vec![(coarse, 0.0), (deep, 0.0)];

        let (exact, fallback, counts, _) = walk(&selection, |id| id == coarse, parent_of);
        assert_eq!(exact, vec![coarse], "the selected coarse tile owns its ground");
        assert!(
            fallback.is_empty(),
            "the same surface must not be drawn twice — fallback: {fallback:?}"
        );
        assert_eq!(counts.lost, 0);
        assert_eq!(counts.coarser, 1, "the deep tile is still standing in");
    }

    /// **A stand-in moves a tile out of the fallback set entirely.**
    ///
    /// A stand-in *is* that tile's surface, so the walk stops there and no
    /// ancestor is named at all. Nothing is refused — which is the difference
    /// between this and the two attempts that withheld the fallback.
    #[test]
    fn a_stand_in_leaves_nothing_to_fall_back_to() {
        let here = TileId::from_terrain(3, 4, 4);
        let neighbour = TileId::from_terrain(3, 5, 4);
        let grandparent = TileId::from_terrain(1, 1, 1);
        let selection = vec![(here, 0.0), (neighbour, 0.0)];
        // To the consumer a stand-in is indistinguishable from the real tile,
        // which is the design: `resolve` asks "do I have a surface for this?".
        let has = |id: TileId| id == here || id == neighbour || id == grandparent;

        let (exact, fallback, counts, _) = walk(&selection, has, parent_of);
        assert_eq!(exact.len(), 2, "one surface per selected tile — {exact:?}");
        assert!(fallback.is_empty(), "nothing left to stand in — {fallback:?}");
        assert_eq!(counts.coarser, 0);
        assert_eq!(counts.lost, 0);
    }

    /// Nothing anywhere on the chain is the one case that is genuinely bare, and
    /// it must be counted rather than hidden — it is the number that says the
    /// picture is broken.
    #[test]
    fn a_chain_with_nothing_on_it_is_counted_as_lost() {
        let orphan = TileId::from_terrain(3, 4, 4);
        let (exact, fallback, counts, unresolved) =
            walk(&[(orphan, 0.0)], |_| false, parent_of);
        assert!(exact.is_empty() && fallback.is_empty());
        assert_eq!(counts.lost, 1);
        assert_eq!(counts.deepest_lost, 3);
        assert_eq!(unresolved, vec![orphan]);
    }
}
