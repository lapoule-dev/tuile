# 3D Tiles in the graphics budget: today, and one ledger for the whole scene

Exploration note, written on 2026-10-31 from a read-only pass over `main` (70150d8d)
and the branch of PR #173 (`feat/viewer-ion-photorealistic`, b87a9605). Nothing was
built or run. **Nothing here is implemented.** Line numbers are those of these two
revisions.

## 1. How the budget sees 3D Tiles

**Classes.** `TerrainMesh`, `Tiles3dMesh`, `Drape`, `Overlay`
(`crates/tuile-budget/src/types.rs:21-53`), held as a fixed `[T; 4]`. A mesh's class
comes from `TileTree::mesh_class`: `TerrainMesh` by default
(`tuile-core/src/source.rs:167-169`), `Tiles3dMesh` for `Tileset` and `TilesetTree`
(`tileset.rs:256-259`, `tiles3d.rs:60-63`); `CompositeTileTree` forwards it by tag.

**The interactive split** (`config.rs:179-241`), as shares of the device budget G:

| Part | Hard limit | Target / overflow |
|---|---|---|
| Ground pool (TerrainMesh + Drape) | 0.82 G | 70 % / 15 % |
| Tiles3dMesh (outside the pool) | 0.15 G | 70 % / 15 % |
| Overlay | 0.03 G | 70 % / 15 % |

Decoded tier 0.25 G for all classes; `pinned_level` 2; 8 MiB uploaded per frame.

**A 3D Tiles content's cost** (`content.rs:164-175`): `mesh_cost(vertices, 32,
indices, cpu_copy = true)` — vertex and index buffers, the per-tile uniform, **and a
CPU copy that only terrain stitching needs** — plus every glb texture's
`DecodedTexture::cost()`, all in `Tiles3dMesh` under one `ResourceId::Mesh`. Textures
are RGBA8 with a full mip chain (`Texels::cost` → `drape_cost`); nothing marks them
for GPU compression. External-tileset tiles cost one empty uniform.

**Identity.** `TileId` bits 60–63 are a source tag. A globe tile and a tileset tile
both carry tag 0, so in one ledger they would collide unless re-tagged, as
`CompositeTileTree` already does (`source.rs:344-378`).

**Refinement.** REPLACE holds the parent drawn and lists the children it waits on as
`blockers` (`traversal.rs:1330-1381`); the budget protects them as one swap group.
ADD draws parent and children; a missing child falls back to the nearest drawable
ancestor. Grafts are invisible to `decide()`.

**Floor.** `is_floor` is class-blind (`engine.rs:337-339`): a tileset's tiles at depth
≤ `pinned_level` are Floor and never evicted.

## 2. PR #173: two budgets for one GPU

PR #173 runs one geometry server, one pump and one budget **cloned from the globe's**
per tileset (`surface.rs:102-111`): `Tiles3dMesh` given the terrain mesh's limits,
everything else copied, the ground pool included. Each pump takes the frame's whole
upload share. The globe still selects, loads and draws everything under the tileset;
the stencil only rejects its pixels.

| Profile | G | Scene GPU with one tileset | Decoded copies |
|---|---|---|---|
| Desktop, ≥ 16 GiB machine | 4 GiB | **5.25 GiB** | 2 GiB |
| Desktop page (`Profile::browser`) | 1536 MiB | **≈ 2015 MiB** | 768 MiB in one wasm heap |
| Handheld page, 8 GiB | 384 MiB | 504 MiB | 192 MiB |

Each further tileset adds 0.492 G on the GPU and 0.25 G decoded. The globe's own
0.15 G for 3D Tiles stays unused.

**Signals.** Pressure, thermal state and the load gate reach the globe's server only:
a tileset never shrinks on a memory warning and keeps fetching while the page is
hidden. Global gauges and the published budget signals are written by every server
(last writer wins); a server going away zeroes the globe's gauges. The two `decide`s
run on two threads: each is deterministic, the scene is not.

## 3. One ledger: options

**(a) One server, several sources — recommended.** `CompositeTileTree` and
`CompositeLoader` exist (`source.rs:284-456`) but no production code uses them. What
it takes:

1. *Composite.* Forward `epoch` and `warm_up`; an occluder per source (today it must
   be unanimous, so a tileset would switch off horizon culling for the globe); an SSE
   scale per source (16 px for a tileset, 1–2 px for the globe), reaching `need_of`
   too; sources added and removed at runtime (the hardest part).
2. *Runtime.* Floor walk and priming per source; no `steepest_ground` and no CPU copy
   for 3D Tiles content; metrics per class.
3. *Budget.* The floor and the P1.1 "lost tiles cap the draw" rule made ground-only,
   with terrain-only inputs taking identical paths (proven by the unchanged
   `tests/reference/recorded.txt`). Whether `Tiles3dMesh` joins the pool is a second,
   deliberate decision: changing the shares re-blesses the recordings.
4. *Pump.* One pump, `resolve()` split by class; the backdrop terrain-only.
5. *Viewer and web.* One server and one stream; tilesets toggled as sources.

**(b) Several servers, one arbiter — not recommended.** `decide` reads the whole need:
partial needs from two servers would evict each other's tiles. An arbiter merging
shards is (a) plus cross-thread timing in the result. **(b′)**, separate budgets
resized by a host arbiter, is the two-budget design the architecture's simulation
rejected (D-A0, `graphics-budget-architecture.md` §3.3).

**Determinism.** Keep four classes: a fifth changes every recorded digest chain.
`budget_sim` only builds terrain needs, so changes written to leave terrain-only
paths identical should keep the recordings valid — to be proven by running the
lockstep, not assumed.

## 4. The ground under a tileset

No document states the rule "ground under a covered pixel is demoted to fallback
priority", and the CPU has no coverage information: a `region` volume is converted to
an OBB and its rectangle dropped (`tileset.rs:497-498`); only the GPU stencil knows,
per pixel. What the rule needs: an inner lon/lat footprint per drawable tileset tile,
a host flag "this tileset replaces the ground", and a deterministic cover test in
`need()` over what was drawn last step. Demote **in the traversal first** (covered
ground refines at a relaxed SSE, so the coarse ground stays as the fallback): the
budget is untouched and the digests stay valid. A budget-side cover relation only if
measurement shows that is not enough. Risks: blurrier ground through holes in the
tileset (water), a frame of coarse ground when a cover tile goes, stencil and
tile-level cover disagreeing at edges. Never a black frame either way.

## 5. Gaps between the docs and the code

- The spec gives 3D Tiles 15 % of the device; no server ever fills both classes.
- D-A0 says one arbiter over one GPU pool; PR #173 runs one ledger per tileset.
- `docs/10-crate-core.md` says compositions share one engine; no production composite.
- `pinned_level` is documented as the terrain and imagery floor; it pins any class.
- `graphics-budget-roadmap.md` has no 3D Tiles item.

## 6. Order

1. A stopgap inside PR #173 only if it is to be used before the real work: globe and
   tilesets sized to fit G together, no pinned floor for a tileset, signals and the
   gate sent to every server, the upload share shared across pumps, gauges labelled.
2. The composite (no digest impact).
3. Runtime per source.
4. Budget: floor and P1.1 ground-only, recordings unchanged; then the pool decision.
5. One pump.
6. Viewer and web on one server.
7. The cover rule, traversal-side.

Placed in the budget plan as its own step, after the steps in progress and before the
compression budget fixes the ledger's shape (Laurent's call, 2026-10-31: 3D Tiles are
parked until the budget is closed; this is how they come back into it).
