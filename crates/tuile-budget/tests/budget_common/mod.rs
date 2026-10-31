// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Shared fixtures of the budget's behaviour tests: a quadtree of terrain
//! tiles, needs built from it, and a [`Rig`] that plays loader and consumer
//! by hand, one step at a time, on a stepped clock.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use tuile_budget::cost::{drape_cost, Cost, TextureFormat};
use tuile_budget::{
    Budget, BudgetConfig, Bytes, Class, Decisions, FrameInput, GeoRect, ImageryNeed, LayerId,
    LayerReach, Micros, Need, NeedDelta, NeedSet, Presence, PresenceOracle, ResourceId,
    TexelFootprint, TileId,
};

/// The estimated bytes of every test mesh.
pub const MESH: Bytes = Bytes(100 * 1024);

/// The estimated resident bytes of every test drape.
pub fn drape() -> Bytes {
    drape_cost(256, 256, TextureFormat::Rgba8).gpu
}

/// What a resource costs in these tests: meshes stage what they hold.
pub fn est(r: ResourceId) -> Cost {
    match r {
        ResourceId::Mesh(_) => Cost::flat(MESH),
        ResourceId::Drape { .. } => drape_cost(256, 256, TextureFormat::Rgba8),
    }
}

/// The terrain tile `(level, x, y)`.
pub fn t(level: u32, x: u64, y: u64) -> TileId {
    TileId::from_terrain(level, x, y)
}

/// Its parent.
pub fn parent(id: TileId) -> Option<TileId> {
    let (l, x, y) = id.terrain_coord();
    (l > 0).then(|| t(l - 1, x / 2, y / 2))
}

/// Its four children.
pub fn children(id: TileId) -> [TileId; 4] {
    let (l, x, y) = id.terrain_coord();
    [
        t(l + 1, 2 * x, 2 * y),
        t(l + 1, 2 * x + 1, 2 * y),
        t(l + 1, 2 * x, 2 * y + 1),
        t(l + 1, 2 * x + 1, 2 * y + 1),
    ]
}

/// Its footprint: the world is the unit square of radians, `y` southward.
pub fn rect(id: TileId) -> GeoRect {
    let (l, x, y) = id.terrain_coord();
    let size = 1.0 / f64::from(1u32 << l);
    GeoRect {
        west: x as f64 * size,
        east: (x + 1) as f64 * size,
        north: 1.0 - y as f64 * size,
        south: 1.0 - (y + 1) as f64 * size,
    }
}

/// A footprint whose ideal imagery level is exactly `level`: the ratio is
/// 0.9 × 2^level (focal 10⁶ px, 1 m texels, 1 px target).
pub fn footprint(level: u32) -> TexelFootprint {
    let d = 1.0e6 / (0.9 * f64::from(1u32 << level));
    TexelFootprint::new(d, 1.0, 1.0e6, 1.0, 1.0).expect("a valid footprint")
}

/// A terrain need for `id`, with its parent and footprint, no imagery.
pub fn need(id: TileId) -> Need {
    let (l, ..) = id.terrain_coord();
    let mut n = Need::new(l, Class::TerrainMesh)
        .with_rect(rect(id))
        .with_sse_excess(1.0)
        .with_distance(1_000.0);
    if let Some(p) = parent(id) {
        n = n.with_parent(p);
    }
    n
}

/// The same need, wanting imagery at `level` (source maximum 22).
pub fn draped(id: TileId, level: u32) -> Need {
    need(id).with_imagery(ImageryNeed::base(footprint(level), 22))
}

/// The base imagery layer's drape of `id` at `level`.
pub fn dr(id: TileId, level: u32) -> ResourceId {
    ResourceId::drape(id, LayerId(0), level)
}

/// The drape a draw item wears for the base layer, if any.
pub fn base_drape(item: &tuile_budget::DrawItem) -> Option<tuile_budget::DrapeChoice> {
    item.drape(LayerId(0)).copied()
}

/// The drape of `id` for `layer` at `level`.
pub fn dl(id: TileId, layer: u16, level: u32) -> ResourceId {
    ResourceId::drape(id, LayerId(layer), level)
}

/// A need for `id` wearing `layers` (each reaching level 22), all at the
/// imagery `level`.
pub fn layered(id: TileId, level: u32, layers: &[u16]) -> Need {
    need(id).with_imagery(ImageryNeed {
        footprint: footprint(level),
        layers: layers
            .iter()
            .map(|&l| LayerReach {
                layer: LayerId(l),
                max_level: 22,
            })
            .collect(),
    })
}

/// A need with `tiles` drawn, each wearing `layers` at imagery `level`.
pub fn drawn_layered(tiles: &[TileId], level: u32, layers: &[u16]) -> NeedSet {
    NeedSet {
        drawn: tiles
            .iter()
            .map(|&id| (id, layered(id, level, layers)))
            .collect(),
        ..NeedSet::default()
    }
}

/// A need with `tiles` drawn, no imagery.
pub fn drawn(tiles: &[TileId]) -> NeedSet {
    NeedSet {
        drawn: tiles.iter().map(|&id| (id, need(id))).collect(),
        ..NeedSet::default()
    }
}

/// A need with `tiles` drawn, each wanting imagery at `level`.
pub fn drawn_draped(tiles: &[TileId], level: u32) -> NeedSet {
    NeedSet {
        drawn: tiles.iter().map(|&id| (id, draped(id, level))).collect(),
        ..NeedSet::default()
    }
}

/// A local store whose index answers from a set.
#[derive(Clone, Debug, Default)]
pub struct Store(pub BTreeSet<ResourceId>);

impl PresenceOracle for Store {
    fn holds(&self, r: ResourceId) -> Presence {
        if self.0.contains(&r) {
            Presence::Stored
        } else {
            Presence::Absent
        }
    }
}

/// A budget with a stepped clock, playing loader and consumer by hand.
pub struct Rig {
    /// The budget under test.
    pub b: Budget,
    /// The clock: the time of the next step.
    pub now: Micros,
    /// The next frame number.
    pub frame: u64,
    /// The step length.
    pub dt: Micros,
    /// The local store.
    pub store: Store,
    /// Generations of the last delivery of each resource.
    pub delivered: BTreeMap<ResourceId, tuile_budget::Generation>,
    /// Loads started and not yet completed by the rig.
    pub pending: BTreeMap<ResourceId, tuile_budget::Generation>,
    /// The need of the last step, `None` before the first.
    pub need_last: Option<NeedSet>,
}

impl Rig {
    /// A rig on `cfg`, stepping at 60 frames per second.
    pub fn new(cfg: BudgetConfig) -> Self {
        Rig {
            b: Budget::new(cfg).expect("a valid configuration"),
            now: Micros::ZERO,
            frame: 0,
            dt: Micros(16_667),
            store: Store::default(),
            delivered: BTreeMap::new(),
            pending: BTreeMap::new(),
            need_last: None,
        }
    }

    /// The interactive configuration used by default: 512 MiB, no floor.
    pub fn interactive_config() -> BudgetConfig {
        let mut c = BudgetConfig::interactive(Bytes::mib(512));
        c.pinned_level = None;
        c
    }

    /// A rig on [`Rig::interactive_config`].
    pub fn interactive() -> Self {
        Rig::new(Rig::interactive_config())
    }

    /// A rig on [`BudgetConfig::batch`], no floor.
    pub fn batch() -> Self {
        let mut c = BudgetConfig::batch();
        c.pinned_level = None;
        Rig::new(c)
    }

    /// One step: decide at `now`, then advance the clock. The first step
    /// reads the whole need; every later one what changed since the step
    /// before, as the geometry server does.
    pub fn step(&mut self, need: &NeedSet) -> Decisions {
        let mut delta = NeedDelta::new();
        let input = match &self.need_last {
            Some(last) => {
                delta.diff(last, need);
                FrameInput::from_delta(self.frame, self.now, &delta)
            }
            None => FrameInput::new(self.frame, self.now, need),
        }
        .with_presence(&self.store);
        let d = self.b.decide(input);
        self.need_last = Some(need.clone());
        for (r, g) in &d.deliver {
            self.delivered.insert(*r, *g);
        }
        for (r, g) in &d.cancel {
            if self.pending.get(r) == Some(g) {
                self.pending.remove(r);
            }
        }
        for s in &d.start {
            self.pending.insert(s.resource, s.gen);
        }
        self.now = self.now.plus(self.dt);
        self.frame += 1;
        d
    }

    /// Every load started by `d` completes, at its estimate.
    pub fn load(&mut self, d: &Decisions) {
        for s in &d.start {
            if self.pending.get(&s.resource) == Some(&s.gen) {
                self.pending.remove(&s.resource);
                self.b.on_loaded(s.resource, s.gen, est(s.resource));
            }
        }
    }

    /// The loads still pending whose resource passes `keep` complete, at
    /// their estimates; the others stay in flight.
    pub fn load_where(&mut self, keep: impl Fn(ResourceId) -> bool) {
        let done: Vec<_> = self
            .pending
            .iter()
            .filter(|(r, _)| keep(**r))
            .map(|(r, g)| (*r, *g))
            .collect();
        for (r, g) in done {
            self.pending.remove(&r);
            self.b.on_loaded(r, g, est(r));
        }
    }

    /// The consumer uploads `d`'s plan, at the estimates.
    pub fn upload(&mut self, d: &Decisions) {
        for r in &d.upload_plan.items {
            if let Some(g) = self.delivered.get(r) {
                self.b.on_drawable(*r, *g, est(*r).gpu);
            }
        }
    }

    /// Loads and uploads everything `d` asks for.
    pub fn complete(&mut self, d: &Decisions) {
        self.load(d);
        self.upload(d);
    }

    /// Completes every load still pending, then steps and completes until a
    /// step asks for nothing, at most `max` steps; returns the last decisions.
    pub fn settle(&mut self, need: &NeedSet, max: usize) -> Decisions {
        for (r, g) in std::mem::take(&mut self.pending) {
            self.b.on_loaded(r, g, est(r));
        }
        let mut last = self.step(need);
        for _ in 0..max {
            let idle = last.start.is_empty()
                && last.deliver.is_empty()
                && last.upload_plan.items.is_empty();
            if idle {
                break;
            }
            self.complete(&last);
            last = self.step(need);
        }
        last
    }

    /// Advances the clock by `d` with steps on `need`, completing nothing.
    pub fn idle_for(&mut self, need: &NeedSet, d: Micros) -> Vec<Decisions> {
        let end = self.now.plus(d);
        let mut out = Vec::new();
        while self.now < end {
            out.push(self.step(need));
        }
        out
    }
}
