// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The async→frame bridge, and the graphics budget's executor: drains a
//! [`GeometryStream`] each frame, uploads what the budget delivered in its
//! plan's order within the frame's deadline, acknowledges each upload with
//! its actual bytes, draws the budget's draw set and frees what it evicts —
//! after the draw that stops using it.
//!
//! The pump does not know which binding feeds it — in-process, WebSocket
//! or HTTP pull — that's the point of the trait (`docs/11-crate-wgpu.md`).
//!
//! # New before old, by construction
//!
//! Nothing is drawn that the budget has not seen acknowledged: a draw set is
//! built from drawable resources only, and a step's evictions name nothing
//! its draw set uses. The pump applies a step's draw set before its
//! evictions, so between two frames the ground goes from one complete
//! picture to the next, and never through a frame with a hole.

use crate::compress::EncodePace;
use crate::context::GpuContext;
use crate::prepare::{prepare, prepare_drape, PreparedDrape, PreparedTile};
use glam::DVec3;
use std::collections::{BTreeMap, HashMap};
use std::task::{Context, Poll};
use tuile_core::budget::{
    Bytes, Class, DrawSet, EyeCoverage, FrameDeadline, FrameRecord, Generation, LayerId, PerClass,
    ResourceId, Role, UploadPlan,
};
use tuile_core::protocol::{
    Ancestry, ClientMessage, GeometryStream, Priming, ResourceContent, ServerMessage,
};
use tuile_core::runtime::SharedMeter;
use tuile_core::source::TileId;
use tuile_core::traversal::TraversalStats;
use tuile_core::traversal::ViewState;

/// What an upload does to the pump once its bytes are let in.
type Keep = Box<dyn FnOnce(&mut ContentPump)>;

/// A delivery waiting for its frame.
struct Queued {
    gen: Generation,
    class: Class,
    content: ResourceContent,
}

/// What an upload is charged, and to which class, for the refund when it
/// leaves.
#[derive(Clone, Copy)]
struct Held {
    class: Class,
    bytes: u64,
}

/// The share of a frame's period the uploads may take, thousandths.
pub const UPLOAD_SHARE_PERMILLE: u16 = 250;

/// The upload throughput assumed before any upload was measured, bytes per
/// millisecond (400 MB/s): the deadline's starting point, which the pump's
/// own measure replaces from the first upload on.
const ASSUMED_BYTES_PER_MS: f64 = 400_000.0;

pub struct ContentPump {
    /// The f64 world point all tiles and the view matrix are relative to.
    pub render_origin: DVec3,
    prepared: HashMap<TileId, PreparedTile>,
    drapes: HashMap<ResourceId, PreparedDrape>,
    /// Delivered, not yet uploaded, keyed by resource: uploaded in the
    /// plan's order, the rest after it.
    queue: BTreeMap<ResourceId, Queued>,
    /// What each upload is charged, per resource.
    held: HashMap<ResourceId, Held>,
    /// Bytes held per class, actual.
    held_bytes: PerClass<u64>,
    /// The last step's draw set, upload plan and hard limits.
    draw: DrawSet,
    plan: UploadPlan,
    hard_limit: PerClass<Bytes>,
    /// The last step's verdicts: complete (batch), converged, and the wanted
    /// ground it left black or bare.
    pub frame_ready: bool,
    pub converged: bool,
    lost: u32,
    bare: u32,
    /// The level whose meshes are drawn as a backdrop behind the draw set,
    /// each in its own drape: see [`Self::set_backdrop_level`].
    backdrop_level: Option<u32>,
    /// How many `Select` messages this pump has applied.
    selects_seen: u64,
    /// The generation this pump last sent with a `ViewerState` — monotone,
    /// one per send.
    generation_sent: u64,
    /// The generation the current draw set answers, echoed by the server.
    generation_selected: u64,
    /// What the server has said about the shape of the tree, accumulated.
    ///
    /// **The consumer cannot derive this and must not try.** A `TileId` is an
    /// opaque handle whose payload only the owning tree can read.
    ancestry: HashMap<TileId, Ancestry>,
    pub stats: TraversalStats,
    /// Sum of GPU bytes of prepared meshes (drapes not included).
    pub gpu_bytes: usize,
    /// True once the server side hung up.
    pub closed: bool,
    /// The floor, as last reported by the server. `None` until it has said
    /// anything at all — which is not the same as "nothing to wait for".
    pub priming: Option<Priming>,
    /// Non-fatal errors reported by the server (drain freely).
    pub errors: Vec<String>,
    /// How the tiles drawn are put on one line where they meet: see
    /// [`Self::set_stitching`].
    stitching: Option<u32>,
    /// Frames pumped, the tiles the last plan was for, and those of them
    /// still sliding to where it has them.
    frame: u64,
    stitched_for: Vec<TileId>,
    sliding: Vec<TileId>,
    /// The upload throughput this pump measured, bytes per millisecond: what
    /// a frame's deadline is made of.
    throughput: f64,
    /// Where each frame's record goes: the session's meter.
    meter: Option<SharedMeter>,
    /// Drapes evicted since the last resolve: their wearers let go of them
    /// there, where the device is.
    undress_wearers: Vec<ResourceId>,
    /// The GPU time a frame's imagery encodes may take: see
    /// [`Self::set_encode_pace`].
    encode_pace: EncodePace,
    /// Each resident mesh's decoded content, when kept: see
    /// [`Self::keep_contents`].
    contents: Option<HashMap<TileId, std::sync::Arc<tuile_core::DecodedTileContent>>>,
    /// The meshes the last [`Self::resolve`] drew: exact, held and fallback.
    drawn_tiles: std::collections::HashSet<TileId>,
}

/// Frames a tile takes to slide to its new edge when what is drawn beside it
/// changes: a fifth of a second at sixty frames a second.
pub const STITCH_SLIDE_FRAMES: u32 = 12;

/// How many levels the sharpest draped imagery may sit above a tile before
/// the tile counts as smeared in [`Resolution::smeared_drawn`]. Two levels is
/// a 4× stretch per axis — visibly soft; beyond it is a flat wall.
pub const SMEAR_GAP: u32 = 2;

impl ContentPump {
    pub fn new(render_origin: DVec3) -> Self {
        Self {
            render_origin,
            prepared: HashMap::new(),
            drapes: HashMap::new(),
            queue: BTreeMap::new(),
            held: HashMap::new(),
            held_bytes: PerClass::splat(0),
            draw: DrawSet::default(),
            plan: UploadPlan::default(),
            hard_limit: PerClass::splat(Bytes::MAX),
            frame_ready: false,
            converged: false,
            lost: 0,
            bare: 0,
            backdrop_level: None,
            selects_seen: 0,
            generation_sent: 0,
            generation_selected: 0,
            ancestry: HashMap::new(),
            stats: TraversalStats::default(),
            gpu_bytes: 0,
            closed: false,
            priming: None,
            errors: Vec::new(),
            stitching: Some(STITCH_SLIDE_FRAMES),
            frame: 0,
            stitched_for: Vec::new(),
            sliding: Vec::new(),
            throughput: ASSUMED_BYTES_PER_MS,
            meter: None,
            undress_wearers: Vec::new(),
            encode_pace: EncodePace::DEFAULT,
            contents: None,
            drawn_tiles: std::collections::HashSet::new(),
        }
    }

    /// Keeps each resident mesh's decoded content beside its GPU copy, for a
    /// host that reads the drawn ground on the CPU (`tuile_core::surface`):
    /// [`Self::resident_contents`]. The content is shared with the server's
    /// decoded tier, so keeping it costs memory only once the server has let
    /// go of its copy, and only until the mesh is evicted. Off by default.
    pub fn keep_contents(&mut self, keep: bool) {
        self.contents = keep.then(|| self.contents.take().unwrap_or_default());
    }

    /// Every resident mesh, its level and its decoded content, when
    /// [`Self::keep_contents`] is on.
    pub fn resident_contents(
        &self,
    ) -> impl Iterator<Item = (TileId, u32, &std::sync::Arc<tuile_core::DecodedTileContent>)> {
        self.contents
            .iter()
            .flatten()
            .filter(|(tile, _)| self.prepared.contains_key(tile))
            .map(|(tile, content)| (*tile, self.level(*tile), content))
    }

    /// The meshes the last [`Self::resolve`] drew — exact, held and fallback
    /// — not the backdrop behind them.
    pub fn drawn_tiles(&self) -> &std::collections::HashSet<TileId> {
        &self.drawn_tiles
    }

    /// How much GPU time a frame's imagery encodes may take, when the
    /// context's plan encodes on the GPU ([`crate::plan_for`], [`crate::GpuContext::with_imagery_plan`]); without an
    /// encoder it changes nothing. [`EncodePace::DEFAULT`] unless set.
    pub fn set_encode_pace(&mut self, pace: EncodePace) {
        self.encode_pace = pace;
    }

    /// Records every frame into `meter`, the session's: what the budget's
    /// signals say of the uploads, the deadline and the coverage (§16.2).
    pub fn observe(&mut self, meter: SharedMeter) {
        self.meter = Some(meter);
    }

    /// Draws every mesh at `level` the pump holds behind the draw set, each
    /// in its own drape: a complete coarse layer that covers ground the draw
    /// set does not yet reach — a camera turned faster than the server
    /// answered. `None`, the default, draws none.
    pub fn set_backdrop_level(&mut self, level: Option<u32>) {
        self.backdrop_level = level;
    }

    /// How the tiles drawn are stitched to one another
    /// (`tuile_core::stitch`): `None` for not at all — each tile's edges as
    /// its own data states them — or the number of frames a tile slides
    /// over when its neighbourhood changes. `Some(0)` is at once: what a
    /// consumer that must draw the same picture for the same selection
    /// asks for. The default is [`STITCH_SLIDE_FRAMES`].
    pub fn set_stitching(&mut self, slide_frames: Option<u32>) {
        if self.stitching.is_some() != slide_frames.is_some() {
            // Whatever was planned is for the other setting.
            self.stitched_for.clear();
        }
        self.stitching = slide_frames;
    }

    /// Puts the tiles this frame draws on one line where they meet.
    ///
    /// The rule follows what is really drawn at this instant: the tiles
    /// that hold their own ground — an ancestor standing in for a tile that
    /// has not arrived is not one of them, and an edge beside it is left as
    /// it is, over its skirt. The plan is worked out again only when that
    /// set changes; between two changes this moves on the tiles still
    /// sliding, and costs nothing once they have arrived.
    fn restitch(&mut self, gpu: &GpuContext) {
        self.frame += 1;
        let (frame, slide) = (self.frame, self.stitching);
        // The tiles that hold their own ground in the draw set: exact, and
        // the finer tiles held on a zoom-out. A fallback ancestor stands in
        // for ground that is not its own and is nobody's neighbour.
        let mut exact: Vec<TileId> = match slide {
            Some(_) => self
                .draw
                .items
                .iter()
                .filter(|i| i.role != Role::Fallback)
                .map(|i| i.mesh.tile())
                .filter(|t| self.prepared.contains_key(t))
                .collect(),
            None => Vec::new(),
        };
        exact.sort_unstable();
        if exact != self.stitched_for {
            let m = tuile_core::metrics::metrics();
            let began = web_time::Instant::now();
            let located: Vec<(TileId, (u32, u64, u64))> = exact
                .iter()
                .filter_map(|id| Some((*id, self.prepared.get(id)?.cell?)))
                .collect();
            let plans = {
                let tiles: Vec<tuile_core::stitch::Tile<'_>> = located
                    .iter()
                    .filter_map(|(id, at)| {
                        Some(tuile_core::stitch::Tile {
                            level: at.0,
                            x: at.1,
                            y: at.2,
                            source: at.0,
                            edges: &self.prepared.get(id)?.edges,
                        })
                    })
                    .collect();
                tuile_core::stitch::plan(&tiles, (2, 1), tuile_core::stitch::BAND)
            };
            let slide = slide.unwrap_or(0);
            for ((id, _), plan) in located.iter().zip(&plans) {
                // A tile that was not drawn the frame before appears where
                // it belongs; one that was slides there.
                let fresh = self.stitched_for.binary_search(id).is_err();
                let level = self.level(*id);
                let mut resized = None;
                if let Some(tile) = self.prepared.get_mut(id) {
                    let before = tile.gpu_bytes;
                    if tile.restitch(gpu, plan, frame, slide, fresh) {
                        m.restitched.inc();
                        if !fresh && slide > 0 && !self.sliding.contains(id) {
                            self.sliding.push(*id);
                        }
                    }
                    resized = Some((before, tile.gpu_bytes));
                }
                if let Some((before, after)) = resized {
                    self.recharge(level, before, after);
                }
            }
            // A tile no longer holding its own ground is nobody's neighbour:
            // it goes back to its own edges, at once.
            let nothing = tuile_core::stitch::Stitch::default();
            let mut resized = Vec::new();
            for id in &self.stitched_for {
                if exact.binary_search(id).is_err() {
                    if let Some(tile) = self.prepared.get_mut(id) {
                        let before = tile.gpu_bytes;
                        tile.restitch(gpu, &nothing, frame, 0, true);
                        resized.push((*id, before, tile.gpu_bytes));
                    }
                }
            }
            for (id, before, after) in resized {
                let level = self.level(id);
                self.recharge(level, before, after);
            }
            self.stitched_for = exact;
            m.stitch_plans.inc();
            m.stitch_seconds.record(began.elapsed());
        }
        if !self.sliding.is_empty() {
            let slide = slide.unwrap_or(0);
            let prepared = &mut self.prepared;
            self.sliding.retain(|id| {
                prepared
                    .get_mut(id)
                    .is_some_and(|tile| tile.slide(&gpu.queue, frame, slide))
            });
        }
    }

    /// Tiles still sliding to the edge their neighbourhood gave them.
    pub fn sliding(&self) -> usize {
        self.sliding.len()
    }

    /// A tile's surface grew or shrank where it is — stitched to its
    /// neighbours, or let go of them — from `before` bytes to `after`: the
    /// totals follow it. They used not to, so every stitch moved the real
    /// figure away from the counted one, and a pump that went away took back
    /// less than it had added.
    fn recharge(&mut self, level: u32, before: usize, after: usize) {
        let m = tuile_core::metrics::metrics();
        self.gpu_bytes = (self.gpu_bytes + after).saturating_sub(before);
        if after >= before {
            m.mesh_bytes_by_level.add(level, (after - before) as u64);
        } else {
            m.mesh_bytes_by_level.sub(level, (before - after) as u64);
        }
    }

    /// This frame's upload deadline on a display refreshing at `hz` (I18):
    /// the frame's period, its upload share, and the throughput this pump
    /// has measured. Only the render side knows its frame; the budget never
    /// reads this.
    pub fn deadline(&self, hz: u32) -> FrameDeadline {
        FrameDeadline::from_refresh(hz, UPLOAD_SHARE_PERMILLE, self.throughput as u64)
    }

    /// Takes every message the server has sent — draw sets, evictions,
    /// content into the upload queue — and touches nothing on the GPU: no
    /// upload, no stitch, no buffer write. An evicted surface is dropped, which
    /// releases its memory without submitting any work.
    ///
    /// For a host that may not use the GPU at all — a phone's application in
    /// the background, where the system ends anything that submits — and
    /// still wants the memory an eviction lets go of back now rather than on
    /// its next frame. Returns how many messages were taken.
    pub fn drain<S: GeometryStream>(&mut self, stream: &mut S) -> usize {
        let waker = futures_util::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut taken = 0;
        loop {
            match stream.poll_message(&mut cx) {
                Poll::Ready(Some(msg)) => {
                    self.on_message(msg);
                    taken += 1;
                }
                Poll::Ready(None) => {
                    self.closed = true;
                    break;
                }
                Poll::Pending => break,
            }
        }
        taken
    }

    /// Sends the camera, one generation on from the last: the first half of
    /// [`Self::advance`], for a host that wants the server to run a step —
    /// and answer it — without drawing anything.
    pub fn send_view<S: GeometryStream>(&mut self, stream: &mut S, views: &[ViewState]) {
        // Best effort: a closed stream means the session is over, and a frame is
        // not the place to discover it.
        self.generation_sent += 1;
        let _ = stream.send(ClientMessage::ViewerState {
            views: views.to_vec(),
            generation: self.generation_sent,
        });
    }

    /// One frame of the executor: drains every message, uploads what the
    /// deadline allows in the plan's order, acknowledges each upload with its
    /// bytes (or refuses one that would cross its class's hard limit), and
    /// stitches the tiles about to be drawn. Returns how many resources
    /// reached the GPU.
    pub fn pump<S: GeometryStream>(
        &mut self,
        stream: &mut S,
        gpu: &GpuContext,
        deadline: FrameDeadline,
    ) -> usize {
        self.drain(stream);
        let uploaded = self.upload(stream, gpu, deadline);
        // What is drawn may have changed — a draw set, an upload: the tiles
        // are stitched for this frame before anyone draws it.
        self.restitch(gpu);
        uploaded
    }

    /// One frame of the streaming loop, exactly as an interactive host runs
    /// it: send the camera, take this frame's share of the uploads, and move
    /// the render origin — in that order. Rebasing before the uploads would
    /// leave the tiles that arrived this frame holding a model matrix for the
    /// *previous* origin, and at planetary scale that draws them somewhere
    /// else entirely.
    ///
    /// Several views at once (stereo, a slice of a camera path) are one
    /// need: the traversal's union, one charge per resource (I16).
    ///
    /// Returns how many resources reached the GPU.
    pub fn advance<S: GeometryStream>(
        &mut self,
        stream: &mut S,
        gpu: &GpuContext,
        views: &[ViewState],
        render_origin: DVec3,
        deadline: FrameDeadline,
    ) -> usize {
        self.send_view(stream, views);
        let uploaded = self.pump(stream, gpu, deadline);
        self.rebase(&gpu.queue, render_origin);
        uploaded
    }

    /// The order this frame uploads in: the plan's items first, as the plan
    /// has them, then whatever else is queued — coarse before fine, by id.
    fn upload_order(&self) -> Vec<ResourceId> {
        let mut order: Vec<ResourceId> = self
            .plan
            .items
            .iter()
            .copied()
            .filter(|r| self.queue.contains_key(r))
            .collect();
        let planned: std::collections::BTreeSet<ResourceId> = order.iter().copied().collect();
        order.extend(self.queue.keys().copied().filter(|r| !planned.contains(r)));
        order
    }

    /// The uploads of one frame, within `deadline`.
    fn upload<S: GeometryStream>(
        &mut self,
        stream: &mut S,
        gpu: &GpuContext,
        deadline: FrameDeadline,
    ) -> usize {
        let order = self.upload_order();
        // What an upload moves, not what it is charged: a drape is charged
        // its share of the imagery it names, and uploads every tile of it.
        let estimate = |r: ResourceId| match self.queue.get(&r).map(|q| &q.content) {
            Some(ResourceContent::Mesh(m)) => m.cost().staging,
            Some(ResourceContent::Drape(d)) => d.cost().staging,
            None => Bytes::ZERO,
        };
        let taken: Vec<ResourceId> = deadline
            .take(&order, &self.plan.groups, Bytes::ZERO, estimate)
            .to_vec();
        let mut deferred = (order.len() - taken.len()) as u32;
        let began = web_time::Instant::now();
        let mut uploaded: Vec<(ResourceId, Bytes)> = Vec::with_capacity(taken.len());
        let mut moved = 0u64;
        // The GPU encodes have a share of their own: the blocks this frame
        // may still write, when the plan encodes on the GPU at all.
        let mut encodes_left = gpu.compressor.as_ref().and_then(|c| {
            let ns = c.lock().ok()?.ns_per_block();
            Some(self.encode_pace.blocks_per_frame(ns))
        });
        let mut encoded_any = false;
        for r in taken {
            if let (Some(left), Some(q)) = (encodes_left.as_mut(), self.queue.get(&r)) {
                let blocks = blocks_to_encode(gpu, &q.content);
                if blocks > 0 {
                    // The first encode of a frame always goes, however large:
                    // nothing waits for ever. The rest wait for a frame with
                    // room, in the plan's order, as a deadline's do.
                    if encoded_any && blocks > *left {
                        deferred += 1;
                        continue;
                    }
                    *left = left.saturating_sub(blocks);
                    encoded_any = true;
                }
            }
            let Some(q) = self.queue.remove(&r) else {
                continue;
            };
            let staging = match &q.content {
                ResourceContent::Mesh(m) => m.cost().staging.0,
                ResourceContent::Drape(d) => d.cost().staging.0,
            };
            // Recorded as what the upload moved: the frame's deadline is in
            // those bytes (I18).
            if self.upload_one(stream, gpu, r, q).is_some() {
                uploaded.push((r, Bytes(staging)));
                moved += staging;
            }
        }
        let wall = began.elapsed();
        // The throughput is of what the uploads moved.
        let bytes = moved;
        // The throughput a frame's deadline is made of, measured: a moving
        // average, so one slow upload does not halve the next frame's share.
        let ms = wall.as_secs_f64() * 1000.0;
        if bytes > 0 && ms > 0.05 {
            let measured = bytes as f64 / ms;
            self.throughput = (0.8 * self.throughput + 0.2 * measured).max(50_000.0);
        }
        let m = tuile_core::metrics::metrics();
        m.pending_uploads.set(self.queue.len() as u64);
        if let Some(meter) = &self.meter {
            if let Ok(mut meter) = meter.lock() {
                use tuile_core::budget::BudgetObserver;
                meter.frame(&FrameRecord {
                    frame: self.frame,
                    generation: self.generation_selected,
                    deadline,
                    uploaded: &uploaded,
                    deferred,
                    upload_wall: tuile_core::budget::Micros(wall.as_micros() as u64),
                    eyes: &[EyeCoverage {
                        lost: self.lost,
                        bare: self.bare,
                    }],
                });
            }
        }
        uploaded.len()
    }

    /// Uploads one delivery and acknowledges it with its actual bytes —
    /// or, when it would cross its class's hard limit, refuses it
    /// (`Discarded`). Returns the bytes acknowledged.
    fn upload_one<S: GeometryStream>(
        &mut self,
        stream: &mut S,
        gpu: &GpuContext,
        r: ResourceId,
        q: Queued,
    ) -> Option<u64> {
        let m = tuile_core::metrics::metrics();
        let started = web_time::Instant::now();
        // What this would replace leaves with it: a redelivery after a change
        // of epoch is new before old, in place.
        let replaced = self.held.get(&r).map_or(0, |h| h.bytes);
        let limit = self.hard_limit[q.class];
        let (bytes, store): (u64, Keep) = match &q.content {
            ResourceContent::Mesh(content) => {
                let tile = r.tile();
                let mut prepared = prepare(gpu, content, self.render_origin);
                // Only the ground is stitched: a 3D Tiles mesh keeps no copy.
                if !q.class.is_ground() {
                    prepared.forget_source();
                }
                let bytes = mesh_bytes(&prepared, content);
                let kept = self
                    .contents
                    .is_some()
                    .then(|| std::sync::Arc::clone(content));
                (
                    bytes,
                    Box::new(move |pump: &mut Self| {
                        let level = pump.level(tile);
                        m.meshes_by_level.inc(level);
                        m.mesh_bytes_by_level.add(level, prepared.gpu_bytes as u64);
                        pump.gpu_bytes += prepared.gpu_bytes;
                        if let (Some(contents), Some(kept)) = (pump.contents.as_mut(), kept) {
                            contents.insert(tile, kept);
                        }
                        if let Some(old) = pump.prepared.insert(tile, prepared) {
                            pump.gpu_bytes -= old.gpu_bytes;
                            m.meshes_by_level.sub(level, 1);
                            m.mesh_bytes_by_level.sub(level, old.gpu_bytes as u64);
                        }
                    }),
                )
            }
            ResourceContent::Drape(drape) => {
                let prepared = prepare_drape(gpu, drape);
                let bytes = prepared.gpu_bytes;
                (
                    bytes,
                    Box::new(move |pump: &mut Self| {
                        // Every mesh wearing the drape it replaces is dressed
                        // again at its next draw.
                        pump.drapes.insert(r, prepared);
                    }),
                )
            }
        };
        let after = self.held_bytes[q.class].saturating_sub(replaced) + bytes;
        if limit != Bytes::MAX && after > limit.0 {
            // Past the class's hard limit: the guard against the system ending
            // the process. Refused, and delivered again later.
            let _ = stream.send(ClientMessage::Discarded {
                resource: r,
                gen: q.gen,
            });
            return None;
        }
        store(self);
        self.held_bytes[q.class] = after;
        self.held.insert(
            r,
            Held {
                class: q.class,
                bytes,
            },
        );
        m.upload_seconds.record(started.elapsed());
        m.uploads.inc();
        m.prepared_tiles.set(self.prepared.len() as u64);
        if matches!(r, ResourceId::Drape { .. }) {
            if let Ok(imagery) = gpu.imagery.lock() {
                m.textures_by_level.clear();
                m.texture_bytes_by_level.clear();
                for held in imagery.live_by_level() {
                    m.textures_by_level.inc(held.0);
                    m.texture_bytes_by_level.add(held.0, held.1 as u64);
                }
                let (count, bytes) = imagery.live();
                m.imagery_textures.set(count as u64);
                let _ = bytes;
            }
        }
        // Best effort: a closed stream just means the session is over.
        let _ = stream.send(ClientMessage::Ack {
            resource: r,
            gen: q.gen,
            bytes,
        });
        Some(bytes)
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
            ServerMessage::Select {
                draw,
                plan,
                ancestry,
                stats,
                hard_limit,
                frame_ready,
                converged,
                lost,
                bare,
                generation,
            } => {
                self.ancestry.extend(ancestry);
                self.draw = draw;
                self.plan = plan;
                self.stats = stats;
                self.hard_limit = hard_limit;
                self.frame_ready = frame_ready;
                self.converged = converged;
                self.lost = lost;
                self.bare = bare;
                self.selects_seen += 1;
                self.generation_selected = generation;
            }
            ServerMessage::Content {
                resource,
                gen,
                class,
                ancestry,
                content,
            } => {
                self.ancestry.insert(resource.tile(), ancestry);
                self.queue.insert(
                    resource,
                    Queued {
                        gen,
                        class,
                        content,
                    },
                );
                tuile_core::metrics::metrics()
                    .pending_uploads
                    .set(self.queue.len() as u64);
            }
            // Delivered, not yet uploaded: dropped. A later delivery of the
            // same resource — another generation — is kept.
            ServerMessage::Cancel { resources } => {
                for (r, g) in resources {
                    if self.queue.get(&r).is_some_and(|q| q.gen == g) {
                        self.queue.remove(&r);
                    }
                }
            }
            ServerMessage::Evict { resources } => {
                let m = tuile_core::metrics::metrics();
                let mut drapes_gone = Vec::new();
                for r in &resources {
                    if let Some(h) = self.held.remove(r) {
                        self.held_bytes[h.class] = self.held_bytes[h.class].saturating_sub(h.bytes);
                    }
                    match r {
                        ResourceId::Mesh(tile) => {
                            if let Some(contents) = self.contents.as_mut() {
                                contents.remove(tile);
                            }
                            if let Some(p) = self.prepared.remove(tile) {
                                self.gpu_bytes -= p.gpu_bytes;
                                let level = self.level(*tile);
                                m.meshes_by_level.sub(level, 1);
                                m.mesh_bytes_by_level.sub(level, p.gpu_bytes as u64);
                            }
                        }
                        ResourceId::Drape { .. } => {
                            if self.drapes.remove(r).is_some() {
                                drapes_gone.push(*r);
                            }
                        }
                    }
                    // A copy still queued goes too: uploaded after its own
                    // eviction it would be held for ever, the server no
                    // longer counting it.
                    self.queue.remove(r);
                }
                // A mesh dressed with an evicted drape lets go of its
                // textures: it is dressed again when it is next drawn.
                if !drapes_gone.is_empty() {
                    self.undress_wearers.extend(drapes_gone);
                }
                m.prepared_tiles.set(self.prepared.len() as u64);
                m.pending_uploads.set(self.queue.len() as u64);
            }
            ServerMessage::Error { resource, message } => {
                self.errors.push(match resource {
                    Some(r) => format!("{r:?}: {message}"),
                    None => message,
                });
            }
            ServerMessage::Priming(priming) => self.priming = Some(priming),
        }
    }

    /// What to draw this frame: the budget's draw set, each mesh dressed
    /// with the drapes it wears, and how that went.
    ///
    /// Exact and held meshes own their ground and compete for the depth
    /// buffer; a fallback ancestor stands in for ground that is not its own
    /// and is drawn after them, testing depth and never writing it (see
    /// [`Drawn`]). Dressing binds the drapes' textures and uploads nothing;
    /// a mesh already dressed so is left as it is. Resolving is also what
    /// brings the chosen tiles onto the current render origin: deciding what
    /// to draw and making it drawable are one act.
    pub fn resolve(&mut self, gpu: &GpuContext) -> (Drawn<'_>, Resolution) {
        let mut counts = Resolution {
            lost: self.lost as usize,
            bare: self.bare as usize,
            ..Resolution::default()
        };
        let mut exact: Vec<TileId> = Vec::new();
        let mut fallback: Vec<TileId> = Vec::new();
        // The surfaces drawn in place of the ground (`Ancestry::covers`):
        // apart, as the renderer draws them before it.
        let mut covering: Vec<TileId> = Vec::new();
        let mut covering_fallback: Vec<TileId> = Vec::new();
        let mut dressed: std::collections::HashSet<TileId> = std::collections::HashSet::new();
        for item in &self.draw.items {
            let tile = item.mesh.tile();
            let covers = self.ancestry.get(&tile).is_some_and(|a| a.covers);
            let Some(mesh) = self.prepared.get_mut(&tile) else {
                // The budget draws only what was acknowledged; a mesh missing
                // here is a fault of this side, and the ground it was to cover
                // is black.
                counts.lost += item.serves.len().max(1);
                continue;
            };
            let mut wear: Vec<(ResourceId, &PreparedDrape, tuile_core::budget::UvTransform)> =
                Vec::with_capacity(item.drapes.len());
            for choice in &item.drapes {
                match self.drapes.get(&choice.resource) {
                    Some(drape) => wear.push((choice.resource, drape, choice.uv)),
                    None => counts.bare += 1,
                }
            }
            mesh.dress(gpu, &wear);
            dressed.insert(tile);
            let level = self.ancestry.get(&tile).map_or(0, |a| a.level);
            let (exact, fallback) = if covers {
                (&mut covering, &mut covering_fallback)
            } else {
                (&mut exact, &mut fallback)
            };
            match item.role {
                Role::Exact => {
                    counts.exact += item.serves.len().max(1);
                    exact.push(tile);
                }
                Role::Held => {
                    counts.coarser += item.serves.len();
                    exact.push(tile);
                }
                Role::Fallback => {
                    counts.coarser += item.serves.len();
                    for s in &item.serves {
                        let wanted = self.ancestry.get(s).map_or(0, |a| a.level);
                        let gap = wanted.saturating_sub(level);
                        if gap > counts.worst_gap {
                            counts.worst_gap = gap;
                            counts.worst_gap_drawn = level;
                            counts.worst_gap_wanted = wanted;
                        }
                    }
                    fallback.push(tile);
                }
            }
        }
        // A tile both drawn as itself and standing in for another is real
        // geometry: it is not demoted to the backdrop pass.
        fallback.retain(|t| !exact.contains(t));
        covering_fallback.retain(|t| !covering.contains(t));
        // The backdrop: every mesh of the ground at its level in its own
        // drape, behind it all — the ground's alone: a 3D Tiles mesh at that
        // depth of its own tree is no ground.
        let mut backdrop: Vec<TileId> = Vec::new();
        if let Some(level) = self.backdrop_level {
            let ground = |t: &TileId| {
                self.held
                    .get(&ResourceId::Mesh(*t))
                    .is_some_and(|h| h.class.is_ground())
            };
            let floor: Vec<TileId> = self
                .prepared
                .keys()
                .copied()
                .filter(|t| {
                    self.ancestry
                        .get(t)
                        .is_some_and(|a| a.level == level && !a.covers)
                })
                .filter(ground)
                .collect();
            for tile in floor {
                if !dressed.contains(&tile) {
                    // The finest drape of its own it holds, per layer — or
                    // none: a floor tile is drawn bare rather than left out.
                    // The budget asks a floor tile's drape at the level the
                    // view needs, so it may hold a finer one than the
                    // backdrop's level, or none yet; leaving such a tile out
                    // showed the sky through the planet on a turn the draw
                    // set had not caught up with.
                    let mut own: BTreeMap<
                        tuile_core::budget::LayerId,
                        (ResourceId, &PreparedDrape),
                    > = BTreeMap::new();
                    for (r, d) in self.drapes.iter().filter(|(r, _)| r.tile() == tile) {
                        let finer = own
                            .get(&d.layer)
                            .is_none_or(|(_, held)| d.level > held.level);
                        if finer {
                            own.insert(d.layer, (*r, d));
                        }
                    }
                    let wear: Vec<(ResourceId, &PreparedDrape, tuile_core::budget::UvTransform)> =
                        own.into_values()
                            .map(|(r, d)| (r, d, tuile_core::budget::UvTransform::IDENTITY))
                            .collect();
                    if let Some(mesh) = self.prepared.get_mut(&tile) {
                        mesh.dress(gpu, &wear);
                    }
                    dressed.insert(tile);
                }
                backdrop.push(tile);
            }
        }
        // Wearers of an evicted drape that nothing dressed again this frame
        // let go of its textures.
        if !self.undress_wearers.is_empty() {
            let gone = std::mem::take(&mut self.undress_wearers);
            for (tile, mesh) in self.prepared.iter_mut() {
                if !dressed.contains(tile) && gone.iter().any(|r| mesh.wears(*r)) {
                    mesh.undress(gpu);
                }
            }
        }
        let level_of = |t: &TileId| self.ancestry.get(t).map_or(0, |a| a.level);
        counts.smeared_drawn = exact
            .iter()
            .chain(fallback.iter())
            .filter(|id| {
                self.prepared.get(id).is_some_and(|p| {
                    p.sharpest_imagery_level
                        .is_none_or(|sharpest| level_of(id).saturating_sub(sharpest) > SMEAR_GAP)
                })
            })
            .count();
        if counts.lost > 0 {
            counts.deepest_lost = self
                .draw
                .items
                .iter()
                .filter(|i| !self.prepared.contains_key(&i.mesh.tile()))
                .map(|i| level_of(&i.mesh.tile()))
                .max()
                .unwrap_or(0);
        }
        // What this frame could draw of the draw set, where everything else
        // the engine counts is published — see `tuile_core::metrics`.
        let m = tuile_core::metrics::metrics();
        m.frame_exact.set(counts.exact as u64);
        m.frame_coarser.set(counts.coarser as u64);
        m.frame_lost.set(counts.lost as u64);
        m.frame_digest.set(
            tuile_core::determinism::digest(exact.iter().map(|t| t.0))
                ^ tuile_core::determinism::digest(fallback.iter().map(|t| t.0)).rotate_left(29),
        );
        self.drawn_tiles.clear();
        self.drawn_tiles.extend(
            exact
                .iter()
                .chain(fallback.iter())
                .chain(covering.iter())
                .chain(covering_fallback.iter())
                .filter(|t| self.prepared.contains_key(t)),
        );
        let surfaces = |ids: &[TileId]| -> Vec<&PreparedTile> {
            ids.iter().filter_map(|id| self.prepared.get(id)).collect()
        };
        let drawn = Drawn {
            exact: surfaces(&exact),
            fallback: surfaces(&fallback),
            backdrop: surfaces(&backdrop),
            covering: surfaces(&covering),
            covering_fallback: surfaces(&covering_fallback),
        };
        self.rebase_for_drawing(
            &gpu.queue,
            drawn
                .exact
                .iter()
                .chain(drawn.fallback.iter())
                .chain(drawn.backdrop.iter())
                .chain(drawn.covering.iter())
                .chain(drawn.covering_fallback.iter())
                .copied(),
        );
        (drawn, counts)
    }

    /// The draw set the pump is drawing: the last step's.
    pub fn draw_set(&self) -> &DrawSet {
        &self.draw
    }

    /// See the field: the Select counter a consumer correlates on.
    pub fn selects_seen(&self) -> u64 {
        self.selects_seen
    }

    /// The generation of the last `ViewerState` this pump sent.
    pub fn generation_sent(&self) -> u64 {
        self.generation_sent
    }

    /// The generation the current draw set answers. When this has caught up
    /// with [`Self::generation_sent`], the draw set is the last camera's —
    /// the render-purity property, by construction.
    pub fn generation_selected(&self) -> u64 {
        self.generation_selected
    }

    /// Whether the GPU holds `tile`'s mesh.
    pub fn has(&self, tile: TileId) -> bool {
        self.prepared.contains_key(&tile)
    }

    /// Whether the GPU holds `drape`.
    pub fn has_drape(&self, drape: ResourceId) -> bool {
        self.drapes.contains_key(&drape)
    }

    /// How deep a tile sits, as the *server* said — never decoded from the id.
    ///
    /// `0` for a tile nothing has ever mentioned, which is the honest answer:
    /// this side has no tree to ask. See [`Ancestry`].
    pub fn level(&self, tile: TileId) -> u32 {
        self.ancestry.get(&tile).map_or(0, |a| a.level)
    }

    pub fn prepared_count(&self) -> usize {
        self.prepared.len()
    }

    /// Drapes on the GPU.
    pub fn drape_count(&self) -> usize {
        self.drapes.len()
    }

    /// Bytes on the GPU, as acknowledged, per class.
    pub fn held_bytes(&self) -> PerClass<u64> {
        self.held_bytes
    }

    /// How many meshes at or above `level` are on the GPU with a drape of
    /// their own of `layer`: what a host gates its first frame on, since the
    /// floor is what every fallback lands on, and a floor mesh without its
    /// drape is bare.
    pub fn prepared_through_level(&self, level: u32) -> usize {
        self.prepared
            .keys()
            .filter(|t| self.level(**t) <= level)
            .filter(|t| {
                self.drapes
                    .keys()
                    .any(|r| r.tile() == **t && r.layer() == Some(LayerId(0)))
                    || !self.drapes.keys().any(|r| r.layer().is_some())
            })
            .count()
    }

    pub fn pending_uploads(&self) -> usize {
        self.queue.len()
    }
}

/// What one mesh holds once uploaded, by the measure its estimate uses:
/// its buffers and uniform, the textures it owns, and the CPU copy kept for
/// stitching (which unified memory pays for).
/// The blocks uploading `content` would have the GPU encode: its textures
/// marked for the encoder's format, and of its imagery those not already on
/// the GPU. Zero without an encoder, so a context that did not choose
/// compression paces nothing.
fn blocks_to_encode(gpu: &GpuContext, content: &ResourceContent) -> u64 {
    let Some(format) = gpu
        .compressor
        .as_ref()
        .and_then(|c| c.lock().ok().map(|c| c.format()))
    else {
        return 0;
    };
    let encodes = |t: &tuile_core::content::DecodedTexture| {
        t.resident_format().block() == Some(format) && t.rgba8().is_some()
    };
    let blocks = |t: &tuile_core::content::DecodedTexture| {
        crate::compress::tile_blocks(format, t.width, t.height)
    };
    let Ok(pool) = gpu.imagery.lock() else {
        return 0;
    };
    let imagery = |layers: &[tuile_core::raster::ImageryLayer]| -> u64 {
        layers
            .iter()
            .filter(|l| l.is_visible() && encodes(&l.texture) && !pool.holds(l.source, l.coord))
            .map(|l| blocks(&l.texture))
            .sum()
    };
    match content {
        ResourceContent::Mesh(m) => {
            m.textures
                .iter()
                .filter(|t| encodes(t))
                .map(blocks)
                .sum::<u64>()
                + imagery(&m.imagery)
        }
        ResourceContent::Drape(d) => imagery(&d.tiles),
    }
}

fn mesh_bytes(prepared: &PreparedTile, content: &tuile_core::DecodedTileContent) -> u64 {
    // The copy kept to stitch the tile, if it keeps one.
    let copy: u64 = if prepared.keeps_source() {
        content
            .meshes
            .iter()
            .map(|m| {
                m.positions.len() as u64 * tuile_core::content::VERTEX_STRIDE
                    + m.indices.len() as u64 * 4
            })
            .sum()
    } else {
        0
    };
    // The tile's uniform, at the alignment a uniform offset needs.
    let uniform = tuile_core::budget::cost::TILE_UNIFORM_BYTES
        .div_ceil(tuile_core::budget::cost::UNIFORM_ALIGNMENT)
        * tuile_core::budget::cost::UNIFORM_ALIGNMENT;
    prepared.gpu_bytes as u64 + uniform + copy
}

impl Drop for ContentPump {
    /// Takes back what this pump added to the process's numbers.
    ///
    /// The per-level series are added to on upload and subtracted from on
    /// eviction, so a pump that simply went away left its whole residency
    /// counted for ever: a page that destroys its viewer and mounts another,
    /// or a second headless run in one process, then reads the first one's
    /// meshes as its own.
    fn drop(&mut self) {
        let m = tuile_core::metrics::metrics();
        for (tile, prepared) in &self.prepared {
            let level = self.level(*tile);
            m.meshes_by_level.sub(level, 1);
            m.mesh_bytes_by_level.sub(level, prepared.gpu_bytes as u64);
        }
        // Counted from what the GPU holds, at the next upload; until then,
        // nothing this pump held is held.
        m.textures_by_level.clear();
        m.texture_bytes_by_level.clear();
        m.prepared_tiles.set(0);
        m.pending_uploads.set(0);
    }
}

/// The surfaces a frame draws, in the kinds that must not be mixed.
///
/// # Why they are separated
///
/// An ancestor drawn because some descendant has not arrived spans **all** of
/// that ancestor's descendants — including the siblings that did arrive. Drawn
/// as ordinary geometry it therefore competes with them for the depth buffer,
/// and over real relief a coarse tessellation crosses a fine one repeatedly: the
/// winner changes from pixel to pixel along the crossing curve.
///
/// So a fallback is drawn after the surfaces that own their ground, testing
/// depth and never writing it, and the backdrop before everything, taking no
/// part in depth: it colours every pixel nothing else covers — no coverage is
/// refused — but it can never win a pixel from a surface that owns it.
pub struct Drawn<'a> {
    /// Tiles drawn as themselves, and finer tiles held on a zoom-out: each
    /// holding its own ground.
    pub exact: Vec<&'a PreparedTile>,
    /// Ancestors standing in for ground that is not their own.
    pub fallback: Vec<&'a PreparedTile>,
    /// A complete coarse level of the ground behind it all (see
    /// [`ContentPump::set_backdrop_level`]).
    pub backdrop: Vec<&'a PreparedTile>,
    /// The surfaces drawn in place of the ground (3D Tiles of a mesh of the
    /// whole surface, [`Ancestry::covers`](tuile_core::protocol::Ancestry)):
    /// drawn as themselves, and the ancestors standing in for theirs — what
    /// `TileRenderer::paint_replacing` draws before the ground, which keeps
    /// out of the pixels they take.
    pub covering: Vec<&'a PreparedTile>,
    pub covering_fallback: Vec<&'a PreparedTile>,
}

/// How the draw set resolved onto what the GPU actually holds.
///
/// `lost` and `bare` are the defects: ground the view wants with nothing on
/// its ancestor chain to cover it — the clear colour, black — and ground
/// drawn without the drapes it needs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    /// Wanted tiles drawn as themselves.
    pub exact: usize,
    /// Drawn surfaces whose sharpest draped imagery sits more than
    /// `SMEAR_GAP` levels above the tile — one texel stretched over the
    /// ground: the flat-colour smear, counted at the only honest place, the
    /// draw list itself. Tiles with no imagery at all count too.
    pub smeared_drawn: usize,
    /// Wanted tiles drawn by a coarser ancestor, or by finer tiles held on a
    /// zoom-out, while their own streams in.
    pub coarser: usize,
    /// Wanted tiles drawn by nothing.
    pub lost: usize,
    /// Drawn items missing a drape they need.
    pub bare: usize,
    /// The deepest level that was lost, which says whether the hole is a
    /// detail tile at the horizon or a whole coarse region.
    pub deepest_lost: u32,
    /// The largest number of levels any fallback had to climb.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use tuile_core::budget::{DrapeChoice, DrawItem, SwapGroup, UvTransform};
    use tuile_core::protocol::StreamError;

    /// A stream that hands out a script of server messages, and keeps what
    /// was sent to it.
    #[derive(Default)]
    struct Scripted {
        inbox: VecDeque<ServerMessage>,
        sent: Mutex<Vec<ClientMessage>>,
    }

    impl GeometryStream for Scripted {
        fn send(&self, msg: ClientMessage) -> Result<(), StreamError> {
            self.sent.lock().expect("sent").push(msg);
            Ok(())
        }

        fn poll_message(&mut self, _cx: &mut Context<'_>) -> Poll<Option<ServerMessage>> {
            match self.inbox.pop_front() {
                Some(msg) => Poll::Ready(Some(msg)),
                None => Poll::Pending,
            }
        }
    }

    fn mesh(tile: TileId, gen: u64) -> ServerMessage {
        ServerMessage::Content {
            resource: ResourceId::Mesh(tile),
            gen: Generation(gen),
            class: Class::TerrainMesh,
            ancestry: Ancestry {
                level: 0,
                parent: None,
                covers: false,
            },
            content: ResourceContent::Mesh(Arc::new(tuile_core::DecodedTileContent {
                meshes: Vec::new(),
                textures: Vec::new(),
                imagery: Vec::new(),
                withheld_drape: None,
                copyright: None,
                local_origin_ecef: DVec3::ZERO,
                transform_local: glam::Mat4::IDENTITY,
            })),
        }
    }

    fn select(draw: DrawSet, plan: UploadPlan) -> ServerMessage {
        ServerMessage::Select {
            draw,
            plan,
            ancestry: Vec::new(),
            stats: TraversalStats::default(),
            hard_limit: PerClass::splat(Bytes::MAX),
            frame_ready: false,
            converged: false,
            lost: 0,
            bare: 0,
            generation: 1,
        }
    }

    /// Draining takes every message and uploads nothing: content waits in
    /// the queue for a frame, and nothing is acknowledged — the server must
    /// not believe a resource is on the GPU when no frame has run.
    #[test]
    fn draining_takes_messages_and_does_no_gpu_work() {
        let mut stream = Scripted::default();
        stream.inbox.push_back(mesh(TileId(1), 1));
        stream.inbox.push_back(mesh(TileId(2), 2));
        let mut pump = ContentPump::new(DVec3::ZERO);
        assert_eq!(pump.drain(&mut stream), 2);
        assert_eq!(pump.pending_uploads(), 2);
        assert_eq!(pump.prepared_count(), 0, "a drain uploaded");
        assert!(
            stream.sent.lock().expect("sent").is_empty(),
            "a drain acked"
        );
        assert_eq!(pump.drain(&mut stream), 0);
    }

    /// Sending the view is one generation on each time, as a frame's is.
    #[test]
    fn a_view_sent_without_a_frame_is_a_new_generation() {
        let mut stream = Scripted::default();
        let mut pump = ContentPump::new(DVec3::ZERO);
        pump.send_view(&mut stream, &[]);
        pump.send_view(&mut stream, &[]);
        let generations: Vec<u64> = stream
            .sent
            .lock()
            .expect("sent")
            .iter()
            .filter_map(|m| match m {
                ClientMessage::ViewerState { generation, .. } => Some(*generation),
                _ => None,
            })
            .collect();
        assert_eq!(generations, [1, 2]);
        assert_eq!(pump.generation_sent(), 2);
    }

    /// **Uploads follow the plan's order, then the rest.** The plan names the
    /// fine tile before the coarse one; the queue's own order (by id) is the
    /// other way round, and loses.
    #[test]
    fn the_queue_uploads_in_the_plans_order() {
        let mut stream = Scripted::default();
        let (coarse, fine, other) = (TileId(1), TileId(9), TileId(5));
        stream.inbox.push_back(mesh(coarse, 1));
        stream.inbox.push_back(mesh(fine, 2));
        stream.inbox.push_back(mesh(other, 3));
        stream.inbox.push_back(select(
            DrawSet::default(),
            UploadPlan {
                items: vec![ResourceId::Mesh(fine), ResourceId::Mesh(coarse)],
                bytes: Bytes::ZERO,
                groups: vec![SwapGroup::default()],
            },
        ));
        let mut pump = ContentPump::new(DVec3::ZERO);
        pump.drain(&mut stream);
        assert_eq!(
            pump.upload_order(),
            vec![
                ResourceId::Mesh(fine),
                ResourceId::Mesh(coarse),
                ResourceId::Mesh(other)
            ]
        );
    }

    /// **A cancel drops only its own generation**, and an eviction takes a
    /// queued copy too: uploaded after its own eviction it would be held for
    /// ever, the server no longer counting it.
    #[test]
    fn a_cancel_drops_its_generation_and_an_evict_its_queued_copy() {
        let mut stream = Scripted::default();
        stream.inbox.push_back(mesh(TileId(1), 1));
        stream.inbox.push_back(mesh(TileId(2), 2));
        stream.inbox.push_back(ServerMessage::Cancel {
            resources: vec![(ResourceId::Mesh(TileId(1)), Generation(7))],
        });
        let mut pump = ContentPump::new(DVec3::ZERO);
        pump.drain(&mut stream);
        assert_eq!(
            pump.pending_uploads(),
            2,
            "another generation's cancel took it"
        );
        stream.inbox.push_back(ServerMessage::Cancel {
            resources: vec![(ResourceId::Mesh(TileId(1)), Generation(1))],
        });
        stream.inbox.push_back(ServerMessage::Evict {
            resources: vec![ResourceId::Mesh(TileId(2))],
        });
        pump.drain(&mut stream);
        assert_eq!(pump.pending_uploads(), 0);
    }

    /// **A layer worn through an ancestor's transform lands where the
    /// ancestor's pixels are.** A child's south-east quarter, worn from its
    /// parent's drape: its uv origin is the parent's middle, and a layer
    /// covering only the parent's north-west quarter does not reach it.
    #[test]
    fn a_drape_worn_from_an_ancestor_lands_on_its_pixels() {
        let quarter = UvTransform {
            scale: [0.5, 0.5],
            offset: [0.5, 0.5],
        };
        let whole = tuile_core::raster::Placement {
            coverage: [0.0, 0.0, 1.0, 1.0],
            translation: [0.0, 0.0],
            scale: [1.0, 1.0],
        };
        let through = whole.through(quarter).expect("it covers the child");
        // The child's uv 0 is the parent's 0.5, its uv 1 the parent's 1.
        assert_eq!(through.translation, [0.5, 0.5]);
        assert_eq!(through.scale, [0.5, 0.5]);
        let north_west = tuile_core::raster::Placement {
            coverage: [0.0, 0.0, 0.5, 0.5],
            ..whole
        };
        assert!(
            north_west
                .through(quarter)
                .is_none_or(|p| p.coverage[2] <= 0.0 || p.coverage[3] <= 0.0),
            "a layer over another quarter reaches the child"
        );
        let _ = (
            DrapeChoice {
                resource: ResourceId::drape(TileId(1), LayerId(0), 3),
                layer: LayerId(0),
                level: 3,
                uv: quarter,
            },
            DrawItem {
                mesh: ResourceId::Mesh(TileId(2)),
                role: Role::Exact,
                drapes: Default::default(),
                serves: [TileId(2)].into_iter().collect(),
            },
        );
    }
}
