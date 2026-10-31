<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# The ground contract — an authoritative terrain for simulations and renderers

Exploration note, written on 2026-10-31. **A contract and a reference
implementation exist (`crates/tuile-ground`); nothing in the renderer uses them
yet.** This note states what the contract is for, the decisions it embodies,
the alternatives left open, and what comes after.

## 1. The problem: two truths for the ground

A host that simulates on a terrain (a game server stepping vehicles, a
replay re-simulating them) and draws that terrain through tuile holds two
grounds if each side has its own: the simulation's heights and the meshes
tuile streams. They drift apart — a vehicle floats or sinks, a line of sight
the picture contradicts, a crater the simulation has and the render lacks, or
the same edits applied in a different order.

The fix is **one authoritative ground**, linked by the simulation and by the
renderer: everything else (rendered meshes, navigation meshes, clearance maps)
is *derived* from it and invalidated by its edit events.

## 2. What tuile owns, what a host owns

| tuile (generic, this crate and its successors) | the host |
|---|---|
| the frame abstraction and the tile grid | which frame its world uses |
| the ground traits: batched queries, edits, versions, change events | its terrain source (a generator, a file): an implementation of the traits |
| water as data (bodies with levels, per-column coverage) | the design of its maps (placement, fairness, resources) |
| the determinism discipline | the physics and rules of what moves on the ground |
| later: adapting a ground into the renderer, procedural sources, derived-data caching | derived data specific to it (navigation for its mobility classes) — built over the traits, by tile |

## 3. The contract (`tuile-ground`)

- **Frames are pluggable** (`Frame`). The source declares its frame; queries
  and edits are expressed in it. Two frames ship:
  - `PlanarFrame`: an abstract flat world, metres east and north, with an
    optional geodetic anchor (an equirectangular placement for a renderer:
    it drifts from the sphere by `d²/2R`, ≈ 320 m at 64 km from the anchor —
    a planar world is drawn flat, the anchor only says where);
  - `GeodeticFrame`: a region of the globe, longitude and latitude in
    radians, heights above the ellipsoid, with a spherical metric.

  A frame gives its bounds, its edit level, its metric (metres per unit at a
  column, which is all the authoritative paths need) and conversions to and
  from geodetic coordinates where it has a place on the globe. Nothing else
  in the contract assumes one frame or the other.
- **Numbers are f64**, with a determinism discipline (§ 4).
- **Tiles**: the quadtree of the frame's bounds. Edits bump a `Version` per
  tile at the frame's edit level and append `TileChanged` events. Derived data
  uses tiles of the same grid (or multiples of it) and invalidates by version.
- **Edits** are volume operations (`Brush`: sphere, capsule, box; `Add` or
  `Remove`) with a caller-given `EditSeq`. A ground applies them in strictly
  increasing order and refuses the rest, unchanged: replicas that receive the
  same edits hold the same bits however the network delivered them.
- **Queries are batched**: slices in, slices out, no allocation per query —
  `surface` (where a walker lands), `spans` (the solid intervals of a column),
  `solid` (collision, volume tests), `raycast` (first entry into solid; clear
  line of sight is `None`), `water` (body, level, depth).
- **3D-ready**: a column is a list of spans; a heightfield is the one-span
  case. The reference `Heightfield` says what it cannot hold — an overhang or a
  tunnel leaves its columns unchanged — rather than faking it.

## 4. Determinism discipline

The same state and the same query must give the same bits on x86-64, aarch64
and wasm32. Rust's `f64` gives that for `+ − × ÷ sqrt`, which IEEE 754
rounds correctly on every target. So, in authoritative paths:

- only `+ − × ÷ sqrt`, comparisons and conversions;
- transcendentals (`cos` for a geodetic metric) through `libm`, pure Rust,
  never the platform's;
- no fused multiply-add (`mul_add`) and no fast-math: they change rounding on
  some targets only;
- no iteration over hash containers in anything that produces a result
  (versions live in a `BTreeMap`);
- edits applied in the caller's total order.

The tests check replicas agree bit for bit (a state digest) over random edit
sequences; a cross-target check (the same digest on Linux, macOS and wasm) is
the next step for CI.

## 5. Decisions taken, and options left open

| Point | Taken here | Open for a decision |
|---|---|---|
| Frame | pluggable: planar (with an optional anchor) and geodetic | whether a planar world may ever be wrapped on the ellipsoid for gameplay (today: no, drawn flat at its anchor) |
| Numbers | f64 with the discipline above | a host may keep fixed point **inside** its implementation (exact edits, hashable state) and convert at the boundary; the contract stays f64 |
| Water | bodies, each with its own level; per-column coverage | rivers with a sloping surface (a level per body cannot hold one) |
| 2.5D or 3D | the contract is 3D (spans, volume edits); the reference is a heightfield | when a volumetric implementation lands (density terrain plan) |
| Tile grid | the quadtree of the frame's bounds, versioned at one level | the edit level and tile size for a given world, and whether derived data must use exactly that level |
| Raycast | a march at half a cell along the segment (reference) | an exact traversal for the volumetric implementation |

## 6. What comes after (not in this note's PR)

1. A host implements the traits over its own terrain source and checks its
   simulation reads the ground only through them.
2. tuile adapts a ground into `tuile-terrain` / `tuile-core`
   (`GroundHeight` and `Heights` are advisory and streaming; this contract is
   authoritative and complete), so the renderer draws exactly the
   authoritative surface — proven on one map: the drawn heights equal the
   simulated ones at sampled points, before and after a sequence of edits.
3. Water drawn from the same bodies.
4. A procedural source (the procedural terrain study) implementing the traits.
5. A volumetric ground (overhangs, tunnels) behind the same traits.
6. Budget accounting of edited tiles in the graphics budget.

Items 2 to 6 follow the graphics budget work.
