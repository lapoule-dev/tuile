<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-ground

The **authoritative ground**: one terrain contract that a simulation steps on
and a renderer draws, so the two never disagree about where the surface is.

- **Any frame.** The ground's source declares its [`Frame`]: an abstract flat
  world ([`PlanarFrame`], metres, optionally anchored on the globe) or a
  region of the globe ([`GeodeticFrame`], longitude and latitude in radians).
  Queries and edits are expressed in that frame; nothing assumes one or the
  other. Rays are straight in the frame's coordinates ([`Ray`]).
- **f64, deterministic.** Every number is an `f64`. Authoritative paths use
  only `+ − × ÷ sqrt` and `libm` for anything transcendental, never fused
  multiply-add or fast-math: the same edits in the same order give the same
  bits on x86-64, aarch64 and wasm32. A state digest
  ([`EditGround::digest`]) lets replicas compare.
- **Complete over its bounds.** Every column inside the bounds has its
  answer, always: no "unknown yet", and no answer depends on what is in
  memory or on load order. An implementation may still generate or load its
  tiles on demand and evict them, as long as no answer can tell.
- **Tiled and versioned.** A quadtree over the frame's bounds; an edit bumps
  the [`Version`] of every tile whose answers may have changed and reports
  them ([`TileChanged`]), so derived data (navigation meshes, clearance maps,
  rendered meshes) is rebuilt by tile. The report is complete and may be
  conservative.
- **Edited in a contiguous order.** Volume [`Brush`]es add or remove solid,
  each with a caller-given [`EditSeq`]: a fresh ground expects 1, then every
  next number. An edit from the past or beyond a missing one is refused and
  changes nothing; an edit refused for its content (out of bounds, not a
  volume) still consumes its number, the same on every replica.
- **Water as data.** Bodies with their own levels ([`WaterBody`]); per column,
  which body covers it and how deep.
- **Batched, bounded.** Every query takes a slice and answers into a slice:
  thousands of columns per call, no allocation per query, a cost per item
  bounded by the ground's size.
- **3D-ready.** A column is a list of solid spans; the reference
  [`Heightfield`] is the one-span case (bilinear, exact raycasts), and a
  volumetric ground implements the same traits.

It sits at the bottom of the graph: no other tuile crate, no GPU, no runtime,
no I/O, and it builds for `wasm32-unknown-unknown`. It is **authoritative and
complete** over its bounds, unlike `tuile_core::ground::GroundHeight`, the
camera's advisory, streaming height (`None` while unknown), and
`tuile_terrain::Heights`, the seam a heightfield is meshed through. Adapting a
ground into those, so the renderer draws exactly this surface, is a later
step. Design notes and open decisions: `docs/explorations/ground-contract.md`.

```rust
use glam::DVec3;
use tuile_ground::*;

// A 4 km abstract world, tiles versioned at level 4 (256 m).
let frame = PlanarFrame { half_extent: 2048.0, edit_level: 4, anchor: None };
// A ramp rising 1 m every 8 m eastwards, 257 samples 16 m apart.
let mut ground = Heightfield::new(frame, 257, |i, _| 2.0 * i as f64)?;

// The ground height at a column: a probe from above.
let mut out = [None];
ground.surface(&[DVec3::new(8.0, 0.0, f64::INFINITY)], &mut out)?;
assert_eq!(out[0].map(|s| s.height), Some(257.0));

// Dig a crater; the tiles whose answers moved come back with new versions.
let mut changed = Vec::new();
let crater = Edit {
    seq: EditSeq(1),
    op: EditOp::Remove,
    brush: Brush::Sphere { centre: DVec3::new(0.0, 0.0, 256.0), radius: 40.0 },
};
ground.apply(&crater, &mut changed)?;
assert!(!changed.is_empty());

// The same number twice is stale; skipping one is a gap.
assert!(matches!(ground.apply(&crater, &mut changed), Err(GroundError::Stale { .. })));
let later = Edit { seq: EditSeq(3), ..crater };
assert!(matches!(ground.apply(&later, &mut changed), Err(GroundError::Gap { .. })));
# Ok::<(), Box<dyn std::error::Error>>(())
```
