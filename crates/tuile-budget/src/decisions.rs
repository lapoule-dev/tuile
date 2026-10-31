// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! [`Decisions`]: what one step tells the loaders and the consumer to do.

use super::error::SessionError;
use super::imagery::UvTransform;
use super::stats::BudgetStats;
use super::tier::Source;
use super::types::{Bytes, Class, Generation, LayerId, ResourceId, TileId};

/// A load to start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Start {
    /// What to load.
    pub resource: ResourceId,
    /// The generation its result must carry.
    pub gen: Generation,
    /// Its class.
    pub class: Class,
    /// Where to read it from.
    pub source: Source,
}

/// How a drawn mesh is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// A wanted tile, drawable: drawn as itself.
    Exact,
    /// A finer tile kept on screen because the coarser tile wanted in its
    /// place is not drawable yet (zoom-out: the children stop being drawn only
    /// when the parent is drawable).
    Held,
    /// A coarser ancestor standing in, under the fallback pass: it fills only
    /// what no `Exact` or `Held` mesh covers.
    Fallback,
}

/// A drape a drawn mesh wears, for one layer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DrapeChoice {
    /// The drape: the tile's own at some level, or an ancestor's, of
    /// `layer`.
    pub resource: ResourceId,
    /// Its layer.
    pub layer: LayerId,
    /// Its imagery level.
    pub level: u32,
    /// The mapping of the mesh's UV square into it.
    pub uv: UvTransform,
}

/// One drawn mesh.
#[derive(Clone, Debug, PartialEq)]
pub struct DrawItem {
    /// The mesh drawn.
    pub mesh: ResourceId,
    /// How.
    pub role: Role,
    /// The drapes it wears: one per layer the tiles it serves ask for, in
    /// stacking order (D-A4). A mesh whose need asks for imagery is drawn
    /// only dressed, every layer it needs worn (I1, D-A1).
    pub drapes: Vec<DrapeChoice>,
    /// The wanted tiles it stands for (itself when `Exact`), sorted.
    pub serves: Vec<TileId>,
}

impl DrawItem {
    /// The drape it wears for `layer`, if any.
    pub fn drape(&self, layer: LayerId) -> Option<&DrapeChoice> {
        self.drapes.iter().find(|d| d.layer == layer)
    }
}

/// Everything drawn this step, sorted by mesh. The consumer applies it
/// before it applies the step's evictions: new before old, in one step.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DrawSet {
    /// The drawn meshes, sorted by `mesh`.
    pub items: Vec<DrawItem>,
}

impl DrawSet {
    /// The item drawing `mesh`, if any.
    pub fn get(&self, mesh: ResourceId) -> Option<&DrawItem> {
        self.items
            .binary_search_by(|i| i.mesh.cmp(&mesh))
            .ok()
            .and_then(|i| self.items.get(i))
    }

    /// Whether `tile`'s mesh is drawn, in any role.
    pub fn draws(&self, tile: TileId) -> bool {
        self.get(ResourceId::Mesh(tile)).is_some()
    }
}

/// Resources that become drawable together, so that a swap completes in one
/// step: the children of a refining parent, or a mesh and its drape.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SwapGroup {
    /// The members, sorted.
    pub members: Vec<ResourceId>,
}

/// The consumer's uploads this frame, in order, capped in bytes. The first
/// item always goes, so a single large resource cannot block the queue.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UploadPlan {
    /// The resources to upload, in order.
    pub items: Vec<ResourceId>,
    /// Their estimated bytes.
    pub bytes: Bytes,
    /// Swap groups among them, completed together when possible.
    pub groups: Vec<SwapGroup>,
}

/// One step's decisions.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct Decisions {
    /// Loads to start, in priority order.
    pub start: Vec<Start>,
    /// Loads to cancel (and their decode skipped).
    pub cancel: Vec<(ResourceId, Generation)>,
    /// Decoded resources to hand to the consumer, within the delivery credit.
    pub deliver: Vec<(ResourceId, Generation)>,
    /// GPU copies to drop, after `draw` is applied.
    pub evict: Vec<ResourceId>,
    /// Decoded CPU copies to drop.
    pub release: Vec<ResourceId>,
    /// The consumer's uploads this frame.
    pub upload_plan: UploadPlan,
    /// What to draw.
    pub draw: DrawSet,
    /// The LOD relaxation fed back to the traversal, in thousandths
    /// (1000 = none). Interactive only.
    pub sse_scale_milli: u32,
    /// Batch: every mesh and drape the frame needs is drawable, exactly.
    pub frame_ready: bool,
    /// Batch: the session must end.
    pub fatal: Option<SessionError>,
    /// The step's statistics.
    pub stats: BudgetStats,
}

impl Decisions {
    /// The relaxation as a factor on the SSE threshold.
    pub fn sse_scale(&self) -> f32 {
        self.sse_scale_milli as f32 / 1000.0
    }

    /// A digest of the decisions (statistics excluded), FNV-1a over a
    /// canonical encoding: equal inputs give equal digests on every platform.
    pub fn digest(&self) -> u64 {
        let mut h = Fnv::new();
        h.u64(self.start.len() as u64);
        for s in &self.start {
            h.resource(s.resource);
            h.u64(s.gen.0);
            h.u64(s.class.index() as u64);
            h.u64(s.source as u64);
        }
        for list in [&self.cancel, &self.deliver] {
            h.u64(list.len() as u64);
            for (r, g) in list {
                h.resource(*r);
                h.u64(g.0);
            }
        }
        for list in [&self.evict, &self.release, &self.upload_plan.items] {
            h.u64(list.len() as u64);
            for r in list {
                h.resource(*r);
            }
        }
        h.u64(self.upload_plan.bytes.0);
        h.u64(self.upload_plan.groups.len() as u64);
        for g in &self.upload_plan.groups {
            h.u64(g.members.len() as u64);
            for r in &g.members {
                h.resource(*r);
            }
        }
        h.u64(self.draw.items.len() as u64);
        for i in &self.draw.items {
            h.resource(i.mesh);
            h.u64(i.role as u64);
            h.u64(i.drapes.len() as u64);
            for d in &i.drapes {
                h.resource(d.resource);
                h.u64(u64::from(d.level));
                for f in d.uv.scale.iter().chain(&d.uv.offset) {
                    h.u64(f.to_bits());
                }
            }
            h.u64(i.serves.len() as u64);
            for t in &i.serves {
                h.u64(t.0);
            }
        }
        h.u64(u64::from(self.sse_scale_milli));
        h.u64(u64::from(self.frame_ready));
        h.u64(u64::from(self.fatal.is_some()));
        h.finish()
    }
}

/// 64-bit FNV-1a: a dozen lines, no dependency, the same on every platform.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn u64(&mut self, v: u64) {
        for b in v.to_le_bytes() {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
    fn resource(&mut self, r: ResourceId) {
        match r {
            ResourceId::Mesh(t) => {
                self.u64(0);
                self.u64(t.0);
            }
            ResourceId::Drape { tile, layer, level } => {
                self.u64(1);
                self.u64(tile.0);
                self.u64(u64::from(layer.0));
                self.u64(u64::from(level));
            }
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}
