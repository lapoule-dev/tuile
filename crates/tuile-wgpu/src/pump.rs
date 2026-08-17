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
    pending: VecDeque<(TileId, DecodedTileContent)>,
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
/// Shared rather than written into the host: a headless test that picks its own
/// number is testing a renderer nobody runs. Uploading is a stall — buffers are
/// created and written on the frame's own thread — so this is deliberately
/// small.
pub const UPLOADS_PER_FRAME: usize = 8;

impl ContentPump {
    pub fn new(render_origin: DVec3) -> Self {
        Self {
            render_origin,
            prepared: HashMap::new(),
            pending: VecDeque::new(),
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
            let Some((tile, content)) = self.pending.pop_front() else {
                break;
            };
            let prepared = prepare(gpu, &content, self.render_origin);
            self.gpu_bytes += prepared.gpu_bytes;
            // A re-upload replaces its predecessor: charge for the new one,
            // refund the old, or the total drifts up until it is fiction.
            if let Some(replaced) = self.prepared.insert(tile, prepared) {
                self.gpu_bytes -= replaced.gpu_bytes;
            }
            // Best effort: a closed stream just means the session is over.
            let _ = stream.send(ClientMessage::Ack { tile });
            uploaded += 1;
        }
        uploaded
    }

    /// Re-bases every prepared tile onto a new render origin — call with the
    /// camera position each frame so the f32 coordinates the GPU sees stay
    /// near zero (sub-meter precise) however far the camera is from the
    /// geocenter. Skips the work when the origin hasn't meaningfully moved.
    pub fn rebase(&mut self, queue: &wgpu::Queue, render_origin: DVec3) {
        if (render_origin - self.render_origin).length() < 1.0 {
            return;
        }
        self.render_origin = render_origin;
        for tile in self.prepared.values() {
            tile.rebase(queue, render_origin);
        }
    }

    fn on_message(&mut self, msg: ServerMessage) {
        match msg {
            ServerMessage::Select { tiles, stats } => {
                self.selection = tiles;
                self.stats = stats;
            }
            ServerMessage::Content { tile, content } => match content {
                TileContent::Decoded(decoded) => self.pending.push_back((tile, decoded)),
                TileContent::Raw { .. } => self
                    .errors
                    .push(format!("tile {tile:?}: raw content reached the renderer")),
            },
            ServerMessage::Evict { tiles } => {
                for tile in &tiles {
                    if let Some(p) = self.prepared.remove(tile) {
                        self.gpu_bytes -= p.gpu_bytes;
                    }
                }
                // Uploads are spread over frames, so a tile can still be
                // queued when its eviction arrives. Dropping it here is what
                // keeps that from leaking: uploaded after its own `Evict`, it
                // would enter `prepared` with the server no longer considering
                // it resident — so no further `Evict` would ever name it, and
                // its GPU memory would be held until the session ends.
                self.pending.retain(|(t, _)| !tiles.contains(t));
            }
            ServerMessage::Error { tile, message } => {
                self.errors.push(match tile {
                    Some(t) => format!("tile {t:?}: {message}"),
                    None => message,
                });
            }
            // A stand-in surface for a tile that has not arrived. This consumer
            // does not draw them — `Config::stand_ins` is off, so none are sent
            // — and a surface that is *not* real geometry must never be uploaded
            // as though it were: it would be acknowledged, the server would
            // stop sending the tile it stands in for, and the approximation
            // would become permanent.
            ServerMessage::Fill { .. } => {}
            // What the coarse pyramid holds, which this consumer does not gate
            // its first frame on.
            ServerMessage::Priming(p) => self.priming = Some(p),
        }
    }

    /// Selected tiles whose GPU resources are ready to draw.
    pub fn visible(&self) -> impl Iterator<Item = &PreparedTile> {
        self.selection
            .iter()
            .filter_map(|(t, _)| self.prepared.get(t))
    }

    /// Like [`Self::visible`], but for any selected tile not yet uploaded,
    /// substitutes its nearest already-prepared ancestor (via `parent_of`) —
    /// so the coarser tile a refinement replaces stays on screen until the
    /// finer one is ready, instead of flashing the background. `parent_of`
    /// returns the parent id, or `None` at a root.
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

    /// One frame of the streaming loop, exactly as an interactive host runs it.
    ///
    /// Send the camera, take this frame's share of uploads, and rebase what is
    /// resident onto the new origin — in that order, which is the order that
    /// matters. Rebasing before the uploads would leave the tiles that arrived
    /// this frame holding a model matrix for the *previous* origin, and at
    /// planetary scale that draws them somewhere else entirely.
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

    /// [`Self::visible_resolved`], and what the selection cost in sharpness.
    pub fn resolve(
        &self,
        _queue: &wgpu::Queue,
        parent_of: impl Fn(TileId) -> Option<TileId>,
    ) -> (Vec<&PreparedTile>, Resolution) {
        let (ids, counts, _) = walk(
            &self.selection,
            |id| self.prepared.contains_key(&id),
            parent_of,
        );
        let out = ids
            .into_iter()
            .filter_map(|id| self.prepared.get(&id))
            .collect();
        (out, counts)
    }

    pub fn visible_resolved(
        &self,
        parent_of: impl Fn(TileId) -> Option<TileId>,
    ) -> Vec<&PreparedTile> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for (tile, _) in &self.selection {
            let mut cur = Some(*tile);
            while let Some(id) = cur {
                if let Some(prepared) = self.prepared.get(&id) {
                    if seen.insert(id) {
                        out.push(prepared);
                    }
                    break;
                }
                cur = parent_of(id);
            }
        }
        out
    }

    /// Number of selected tiles still waiting for content or upload.
    pub fn missing(&self) -> usize {
        self.selection
            .iter()
            .filter(|(t, _)| !self.prepared.contains_key(t))
            .count()
    }

    pub fn prepared_count(&self) -> usize {
        self.prepared.len()
    }

    pub fn pending_uploads(&self) -> usize {
        self.pending.len()
    }
}

/// Which tiles to draw for a selection, and what that cost in sharpness.
///
/// Pure, and separated from the GPU so the invariant it encodes can be tested
/// without a device: for each selected tile, draw its own surface or the
/// nearest ancestor that has one, and **never refuse the climb**. Coverage is
/// the whole job, and every refusal is a hole.
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
) -> (Vec<TileId>, Resolution, Vec<TileId>) {
    let mut out = Vec::new();
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
                    out.push(id);
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
    (out, counts, unresolved)
}

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
