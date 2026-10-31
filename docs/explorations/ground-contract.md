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
    radians, heights above the ellipsoid, with a spherical metric. Its
    latitudes lie within ±90° (poles included) and its longitudes within ±2π,
    spanning at most 2π (checked when a ground is built). Longitudes are
    **periodic**: a brush whole turns away is the same brush (to the rounding
    of `lon ± k·2π`), a brush across the antimeridian edits both sides, and a
    region narrower than a turn clips a brush to its edges. A region whose
    `max.x − min.x` is exactly 2π is the whole globe: its first and last
    sample meridians are one place (a region a rounding short of 2π has two,
    a hair apart). A row of samples at ±90° is one place too.

  A frame also measures brushes: the offset of a column from another in
  metres, in the tangent plane at the first (`tangent_offset`), and the
  rectangle of columns within reach of a brush (`reach`). In a geodetic frame
  the offset is the orthographic projection on the sphere's tangent plane
  (a distance `R·sin θ` for an arc `R·θ`, short by `θ²/6` of it, 4·10⁻⁵ at
  100 km; heights stay along each column, the curvature `d²/2R` under a
  brush is not folded in), and the reach is a spherical cap: latitudes
  widened by `asin(r/R)`, longitudes by `asin(sin θ / cos φ)` at the latitude
  `φ` closest to a pole, and every longitude when the cap contains a pole. So
  a ball at a pole carves a disc over every longitude, and a ball near it
  reaches the far side. A capsule's axis is the chord between its ends, so it
  follows their great circle: its reach is the great-circle arc the short way
  (latitudes up to the arc's vertex closest to a pole, by Clairaut;
  longitudes the short way, across the antimeridian if shorter), widened by
  the cap of `r / cos(θ/2)`. No column outside a brush's footprint is reached
  (a property test checks every sample). A position beyond ±90° of latitude names no place: a
  brush there is refused (`InvalidBrush`).

  A frame gives its bounds, its edit level, its metric (metres per unit at a
  column, which is all the authoritative paths need) and conversions to and
  from geodetic coordinates where it has a place on the globe. Nothing else
  in the contract assumes one frame or the other.
- **Numbers are f64**, with a determinism discipline (§ 4).
- **Complete over its bounds, whatever is resident.** Every column inside the
  bounds has its answer, always: an answer never depends on what is in memory
  or on load order, and there is no "unknown yet" (unlike
  `tuile_core::ground::GroundHeight`, advisory and streaming). An
  implementation **may** load or generate its tiles on demand and evict them
  invisibly: a procedural source computes a tile on first query, caches it,
  evicts it and recomputes it bit for bit; a disk-backed source pages tiles;
  edits are a per-tile, versioned, ordered layer re-applied when a tile comes
  back. It will matter: a 128 km world at 8 m spacing is about 2.7·10⁸
  samples, some 2 GB of `f64`. A memory budget for such a source is an
  implementation setting, not part of the contract. Rendering keeps tuile's
  own streaming, levels of detail and graphics budget; the later adapter
  (§ 6) makes it draw the same surface. Reads are infallible: a source that
  cannot load or regenerate a tile has no answer, must not give a wrong one,
  and fails fatally — outside the contract, as running out of memory is. Its
  digest covers the non-resident state too: it may be computed from the
  source's identity (what regenerates the tiles bit for bit) and its edit
  layer, without loading anything.
- **Tiles**: the quadtree of the frame's bounds, levels 0 to 30. Tile `x`
  covers `[edge(x), edge(x+1))` with `edge(x) = min + (max − min)·(x/2^l)`,
  computed one way everywhere and the last edge being `max` itself, so
  `tile_of` and `tile_bounds` agree to the bit on any bounds (a geodetic
  region's are not powers of two) whose edges are strictly increasing — tiles
  wider than the rounding of the bounds; a heightfield checks it for its cells
  and its edit-level tiles when built (`DegenerateCells`). Edits bump a
  `Version` per tile at the frame's edit level and append `TileChanged`
  events. **The report is complete and may be conservative**: a point query
  on a column of an unreported tile answers bit for bit as before, a ray
  changes only if it crosses a reported tile, and an edit that changes
  nothing reports nothing. In the reference heightfield a changed sample
  changes every cell it is a corner of, so the report covers the changed
  samples' rectangle widened by one cell.
- **Edits** are volume operations (`Brush`: sphere, capsule at any slope,
  box; `Add` or `Remove`) with a caller-given `EditSeq`, applied in a
  **contiguous** order. A fresh ground expects 1, then every next number:
  - the expected number is **consumed** — the edit is applied, or refused for
    its content (`OutOfBounds`; `InvalidBrush`: a non-finite coordinate, a
    radius not positive, a box whose min is above its max, a latitude beyond
    ±90°). A refusal for content is the same on
    every replica, so it consumes the number and changes nothing else;
  - an edit from the past (`Stale`) or past a missing one (`Gap { expected,
    got }`) is refused and changes nothing, not even the expected number:
    the caller delivers the missing edits first.

  So a lost or reordered edit stops a replica at a prefix of the script
  instead of letting it diverge. A brush is measured in each column's
  tangent plane (above). A box is taken as given and does not wrap. A
  heightfield resolves edits at its sample spacing. A brush's fields are
  public: it is validated when the edit is applied.
- **Queries are batched**: slices in, slices out, no allocation per query —
  `surface` (where a walker lands; a probe at `+∞` gives the column's top,
  the idiom a height adapter uses), `spans` (the solid intervals of a
  column), `solid` (collision, volume tests), `raycast` (first entry into
  solid; clear line of sight is `None`; a ray starting in solid hits at 0, and
  one entering the bounds below the surface hits on the entry face (max edges
  included, for a ray moving inward only), exactly on it; water does not
  block; rays do not wrap), `water` (body, level, depth). Solid is closed: a
  point exactly on a surface is solid and stands on it.
- **Costs are bounded** per query by the ground's size, never by the query's
  values. The reference: `surface`, `solid`, `water`, `spans` O(1); `raycast`
  O(cells crossed), at most `2(n−1)+1` cells, each an exact quadratic;
  `apply` O(samples under the footprint + tiles reported), both at most `n²`
  (an edit level finer than the cells is refused at build).
- **Rays are straight in the frame's coordinates.** In a geodetic frame a ray
  at constant height follows the curved surface; a true chord between the
  same ends sags below it by about `d²/8R` at its middle (`d` its length,
  `R` the earth's radius): ≈ 2 m at 10 km, ≈ 49 m at 50 km. A long geodetic
  line of sight is optimistic by that much.
- **Water**: bodies, each with its own level; a body is named by its index
  (`WaterBodyId`) in the list, the one place it is defined. Coverage belongs
  to the source, not to edits: digging below a body's level beside it does
  not flood the hole.
- **One place, one value**: the samples of one place (a pole row, the seam of
  a whole-globe frame) must agree in the source (checked at build), and an
  edit decides each place once, at its first sample, and copies the bits, so
  they stay equal to the bit. A cell's slope is measured with the metric at
  its centre, so normals near a pole stay upright.
- **3D-ready**: a column is a list of spans; a heightfield is the one-span
  case. The reference `Heightfield` says what it cannot hold — an overhang or a
  tunnel leaves its columns unchanged — rather than faking it. Its raycast is
  exact up to rounding: a 2D walk over the cells the segment's projection
  crosses, and in each cell the entry root of the segment against the
  bilinear patch, chosen by the sign of the quadratic term and computed around
  the segment's point at the cell's entry (a long segment does not
  extrapolate the patch across the map).
- **Digest**: `EditGround::digest` covers everything that decides answers and
  order (the frame's bounds, edit level and metric, the samples, the water
  coverage and bodies, the versions, the last consumed edit; not a planar
  frame's anchor, which only places the world for a renderer). Replicas of
  one implementation compare it; digests of different implementations are not
  comparable.

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
- edits applied in the caller's contiguous order;
- an implementation that pages or regenerates tiles recomputes them bit for
  bit: residency never shows in an answer.

The properties are written once, generic over any `EditGround` built by a
factory — a **conformance suite** a new implementation runs as is: replicas
agree bit for bit over random edit scripts; any delivery order leaves a
replica at a prefix of the script; a replica buffering early edits reaches
the script's end; change reports are complete (no point answer moves outside
the reported tiles, no ray whose rectangle misses them changes); versions move
by one on reported tiles only; raycasts agree with the surface, rays starting
and ending outside the bounds included. They run on the heightfield in a
planar frame, a geodetic region, a polar cap over every longitude, a polar
window with edges and an equatorial band over every longitude (edits biased
toward tile edges, the pole and the antimeridian), and on an **evicting test
double** — a ground that rebuilds itself from its source and edit log every
third read — which must also answer bit for bit like the heightfield it pages.

**Golden digests** pin a fixed script's state and a fixed set of answers
(surfaces, normals, solidity, water, raycasts) in a planar frame, a geodetic
region and around a pole. They were recorded on macOS (aarch64) with rustc
1.99.0 (last with the great-circle capsule footprint, one-place pole rows
and seams, and normals at the cell's centre); the CI `test` jobs run them on Linux (x86-64) and macOS (aarch64), so
a drift on one target fails there. Checking them on wasm32 is the next step
for CI.

## 5. Decisions taken, and options left open

| Point | Taken here | Open for a decision |
|---|---|---|
| Frame | pluggable: planar (with an optional anchor) and geodetic (poles included; wraps over the whole globe) | whether a planar world may ever be wrapped on the ellipsoid for gameplay (today: no, drawn flat at its anchor) |
| Numbers | f64 with the discipline above | a host may keep fixed point **inside** its implementation (exact edits, hashable state) and convert at the boundary; the contract stays f64 |
| Edit order | contiguous from 1; content refusals consume their number | question 1 below |
| Water | bodies, each with its own level, named by index; per-column coverage, not changed by edits | rivers with a sloping surface (a level per body cannot hold one); flooding on edits (question 6) |
| 2.5D or 3D | the contract is 3D (spans, volume edits); the reference is a heightfield | when a volumetric implementation lands (see `procedural-unbounded-terrain.md`) |
| Tile grid | the quadtree of the frame's bounds, levels ≤ 30, versioned at one level; reports complete, may be conservative | the edit level and tile size for a given world, whether derived data must use exactly that level; alignment with tuile's tiling (question 4) |
| Raycast | exact for the reference heightfield, straight in frame coordinates | question 2 and 3 below |
| Metric | spherical, radius = WGS84 semi-major axis | question 5 below |
| Digest | on `EditGround`, comparable within one implementation | question 7 below |
| Residency | complete over the bounds; on-demand tiles allowed behind the trait, invisible to answers | confirm (below) |

Questions for Laurent:

1. Edit seq: contiguous with caller buffering, or monotonic with gaps? Do
   refused-but-deterministic edits consume their number? (most important)
   — *implemented: contiguous, and they consume it.*
2. Raycast exact now or approximate with documented error? Does water block
   LOS? Should Hit carry a normal or the span? — *implemented: exact for the
   heightfield; water does not block; `Hit` carries the point and fraction
   only.*
3. Geodetic rays: frame-straight or true chords? — *implemented:
   frame-straight, with the `d²/8R` error documented.*
4. Tile grid vs tuile's tiling: TileKey (u8 level, u32 x/y from the frame's
   min corner) never shares edges with tuile_terrain's global TMS (2×1 roots)
   or TileId's terrain layout (27-bit x/y, 4-bit source tag,
   crates/tuile-budget/src/tile_id.rs) → the adapter would invalidate by
   rectangle overlap. Should a GeodeticFrame be definable as a terrain
   TileCoord (bounds = tile_rect(c); ground tile (l,x,y) = global (c.level+l,
   (c.x<<l)+x, (c.y<<l)+y)), at the cost of arbitrary regions? — *default if
   unanswered: the frame's own grid; the adapter invalidates by rectangle
   overlap.*
5. Spherical metric: brush radii up to ~0.7 % off vs the renderer's ellipsoid
   (~700 m on a 100 km brush) — OK, or ellipsoid radii of curvature (libm +
   arithmetic)? — *default if unanswered: spherical.*
6. Water and edits: does digging below a body's level beside it flood; is
   coverage part of edits? — *today: no flooding, coverage is the source's.*
7. Replica digest: part of EditGround or per implementation? — *implemented
   on `EditGround`.*
8. Residency: on-demand tiles behind the trait, invisible to answers —
   confirm. — *default if unanswered: allowed, invisible to answers; a load
   failure is fatal.*
9. A client-side partial replica of the ground (tiles around the camera,
   synchronised by tile version and the server's edits) for prediction:
   later design. — *default if unanswered: none in this contract; clients
   draw through tuile's streaming and read the server's answers.*

## 6. What comes after (not in this note's PR)

1. A host implements the traits over its own terrain source and checks its
   simulation reads the ground only through them.
2. tuile adapts a ground into `tuile-terrain` / `tuile-core`
   (`GroundHeight` and `Heights` are advisory and streaming; this contract is
   authoritative and complete), so the renderer draws exactly the
   authoritative surface — proven on one map: the drawn heights equal the
   simulated ones at sampled points, before and after a sequence of edits.
   `tuile_ground::Geodetic` mirrors `tuile_core::geo::Geodetic` (the ground
   sits below the core and cannot use it): the adapter converts with a
   `From`.
3. Water drawn from the same bodies.
4. A procedural source (`procedural-unbounded-terrain.md`) implementing the
   traits, generating tiles on demand.
5. A volumetric ground (overhangs, tunnels) behind the same traits.
6. Budget accounting of edited tiles in the graphics budget.
7. The golden digests checked on wasm32 in CI.

Items 2 to 6 follow the graphics budget work.
