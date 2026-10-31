<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-ground

The **authoritative ground**: one terrain contract that a simulation steps on
and a renderer draws, so the two never disagree about where the surface is.

- **Any frame.** The ground's source declares its [`Frame`]: an abstract flat
  world ([`PlanarFrame`], metres, optionally anchored on the globe) or a
  region of the globe ([`GeodeticFrame`], longitude and latitude in radians).
  Queries and edits are expressed in that frame; nothing assumes one or the
  other.
- **f64, deterministic.** Every number is an `f64`. Authoritative paths use
  only `+ − × ÷ sqrt` and `libm` for anything transcendental, never fused
  multiply-add or fast-math: the same edits in the same order give the same
  bits on x86-64, aarch64 and wasm32.
- **Tiled and versioned.** A quadtree over the frame's bounds; edits bump the
  [`Version`] of the tiles they touch and report them ([`TileChanged`]), so
  derived data (navigation meshes, clearance maps, rendered meshes) is
  rebuilt by tile.
- **Edited in a total order.** Volume [`Brush`]es add or remove solid, each
  with a caller-given [`EditSeq`]; edits out of order are refused.
- **Water as data.** Bodies with their own levels ([`WaterBody`]); per column,
  which body covers it and how deep.
- **Batched.** Every query takes a slice and answers into a slice: thousands
  of columns per call, no allocation per query.
- **3D-ready.** A column is a list of solid spans; the reference
  [`Heightfield`] is the one-span case, and a volumetric ground implements the
  same traits.

It sits at the bottom of the graph: no other tuile crate, no GPU, no runtime,
no I/O, and it builds for `wasm32-unknown-unknown`. It is **authoritative and
complete** over its bounds, unlike `tuile_core::ground::GroundHeight`, the
camera's advisory, streaming height (`None` while unknown), and
`tuile_terrain::Heights`, the seam a heightfield is meshed through. Adapting a
ground into those, so the renderer draws exactly this surface, is a later
step. Design notes and open decisions: `docs/explorations/ground-contract.md`.

```rust
use glam::{DVec2, DVec3};
use tuile_ground::*;

// A 4 km abstract world, tiles versioned at level 4 (256 m).
let frame = PlanarFrame { half_extent: 2048.0, edit_level: 4, anchor: None };
// A ramp rising 1 m every 8 m eastwards, 257 samples 16 m apart.
let mut ground = Heightfield::new(frame, 257, |i, _| 2.0 * i as f64);

let mut out = [None];
ground.surface(&[DVec3::new(8.0, 0.0, 1000.0)], &mut out).unwrap();
assert!(out[0].is_some());

// Dig a crater; the touched tiles come back with their new versions.
let mut changed = Vec::new();
let crater = Edit { seq: EditSeq(1), op: EditOp::Remove, brush: Brush::Sphere { centre: DVec3::new(0.0, 0.0, 256.0), radius: 40.0 } };
ground.apply(&crater, &mut changed).unwrap();
assert!(!changed.is_empty());
assert!(ground.apply(&crater, &mut changed).is_err(), "the same sequence number twice is refused");
# let _ = DVec2::ZERO;
```
