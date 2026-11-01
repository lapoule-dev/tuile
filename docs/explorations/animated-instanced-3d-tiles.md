# Animated 3D Tiles with GPU instancing, for games

Exploration note, written on 2026-10-31. **Nothing here is implemented.** It studies
what it would take for tuile to stream and draw *instanced* and *animated* content
inside OGC 3D Tiles — forests, wind turbines, parked and moving vehicles, idle
crowds, animated characters — over the streamed globe, for a game host first
(`tuile-bevy`), without breaking any other mode (viewer, web, film, server).

It covers what the standards offer and what the reference clients do (§1), what
tuile does today (§2), a design (§3), the graphics budget (§4), and a phased plan
placed after the current budget work (§5), with its risks (§6).

Related notes: [`graphics-budget-to-be-and-spec.md`](graphics-budget-to-be-and-spec.md)
(the budget's invariants), [`graphics-budget-roadmap.md`](graphics-budget-roadmap.md)
(the A → B → C order this plan comes after),
[`web-native-convergence.md`](web-native-convergence.md) (the WebGL2 floor).

---

## 0. On one page

- **The standard path is glTF + `EXT_mesh_gpu_instancing`.** 3D Tiles 1.1
  deprecates i3dm and moves instancing into glTF content: per-node `TRANSLATION`
  / `ROTATION` / `SCALE` accessors, feature ids per instance through
  `EXT_instance_features`. i3dm still exists in the wild and maps onto the same
  internal form.
- **3D Tiles says nothing about animation.** Content is glTF, and glTF has
  animations, skins and morph targets; the 3D Tiles spec neither allows nor
  forbids them, and defines no clock. Time-dynamic tiles are a *secondary* scope
  item of the proposed 3D Tiles 2.0 work (2026). Animation inside a tile is
  therefore **a client's choice**, and a client must own the time.
- **The JS reference client plays every glTF animation of a tile, on repeat,
  from the scene clock, in lockstep for all instances** (one set of joint
  matrices per node). The C++ engine converts i3dm into glTF +
  `EXT_mesh_gpu_instancing` and leaves drawing (and animation) to its host
  engines.
- **tuile today draws one copy of an instanced mesh and no animation**: the core
  decodes glTF without extensions, bakes every position to the tile frame, and
  refuses i3dm with a typed error.
- **Design in one sentence.** The core decodes both i3dm and
  `EXT_mesh_gpu_instancing` into one form — *a mesh in its own local frame × N
  instance affines, f32, relative to the tile's rebasing origin, composed in f64*
  — plus, when animated, a *baked pose table* sampled from the glTF clips at a
  rate the consumer asks for. `tuile-wgpu` draws it with one instanced draw per
  mesh (an instance-step vertex buffer: valid on WebGL2), skins in the vertex
  shader from a pose texture indexed by `(scene time + per-instance phase)`, and
  sways vegetation with a procedural wind term. **Time is an input the host
  passes per frame, never read by the core**; in a film it is `frame / fps`,
  exactly.
- **Ambient vs gameplay.** Animated content *in tiles* is ambient: it plays by
  itself, it is budgeted, every client draws it the same way. Gameplay actors
  (a car the player drives, an NPC with AI) are host entities. `tuile-bevy` gets
  a *promotion* API that hands chosen instances to Bevy as entities (with Bevy's
  own skinning and animation graph) and hides them in the tile — spawned first,
  hidden second, never a frame with neither.
- **Budget.** Bytes are cheap (a 5,000-tree tile is ~0.4 MB of mesh and
  instances), **raster work is not** (5,000 × 4,000 triangles = 20 M). The cost
  needs a second axis — triangles drawn — or forests will pass the byte budget
  and blow the frame. Animation never enters `decide`: residency is a function
  of the view, not of time; the bounding volume is the animation's envelope,
  computed once at decode.
- **Order.** After the budget roadmap (compression budget, A2, A4, B, C).
  Phase 1 (instanced decoding) is S–M; the whole track is ~XL, in ten phases,
  each landing on its own.

---

## 1. What the standards offer

### 1.1 i3dm — the legacy Instanced 3D Model (3D Tiles 1.0)

Spec: [Instanced 3D Model](https://github.com/CesiumGS/3d-tiles/blob/main/specification/TileFormats/Instanced3DModel/README.adoc)
(and the OGC text, [22-025r4](https://docs.ogc.org/cs/22-025r4/22-025r4.html),
which carries the same chapter). *"Instanced 3D Model was deprecated in 3D
Tiles 1.1."*

- **Layout.** A 32-byte header (`magic = "i3dm"`, `version = 1`, `byteLength`,
  feature table JSON/binary lengths, batch table JSON/binary lengths,
  `gltfFormat`), then the feature table, the batch table, and the glTF.
- **`gltfFormat`.** `1`: an embedded glb. `0`: a UTF-8 **URI** to a glTF,
  relative to the i3dm. The URI form lets a thousand tiles share one tree model
  — a sharing the budget must see (§4.3) — and it means decoding needs a second
  fetch (§3.2.4).
- **Per-instance semantics (feature table):**

  | Semantic | Type | Meaning |
  |---|---|---|
  | `POSITION` | float32[3] | instance position (required unless quantized) |
  | `POSITION_QUANTIZED` | uint16[3] | quantized position in the volume below |
  | `NORMAL_UP`, `NORMAL_RIGHT` | float32[3] each | orthonormal up/right; both or neither |
  | `NORMAL_UP_OCT32P`, `NORMAL_RIGHT_OCT32P` | uint16[2] each | oct-encoded up/right |
  | `SCALE` | float32 | uniform scale |
  | `SCALE_NON_UNIFORM` | float32[3] | per-axis scale |
  | `BATCH_ID` | uint8/uint16 (default)/uint32 | link to the batch table; instance index if absent |

- **Global semantics:** `INSTANCES_LENGTH` (required), `RTC_CENTER`,
  `QUANTIZED_VOLUME_OFFSET` / `QUANTIZED_VOLUME_SCALE` (required with quantized
  positions), `EAST_NORTH_UP` (with no per-instance orientation, orient each
  instance in the WGS84 east/north/up frame at its position; otherwise no
  rotation).
- **Composition.** The instance transform is applied in the *tile* frame, after
  the glTF's own transforms (including the glTF's Y-up → Z-up): the model is a
  rigid object placed in the world. The JS client records this explicitly
  (`transformInWorldSpace = true` for i3dm, `false` for
  `EXT_mesh_gpu_instancing`, `references/cesium/packages/engine/Source/Scene/ModelComponents.js:697`).
  This order difference is the one subtlety in unifying the two formats (§3.2.3).

### 1.2 glTF + `EXT_mesh_gpu_instancing` (3D Tiles 1.1)

3D Tiles 1.1 makes glTF *"the primary tile format"* and points instancing at
[`EXT_mesh_gpu_instancing`](https://github.com/KhronosGroup/glTF/blob/main/extensions/2.0/Vendor/EXT_mesh_gpu_instancing/README.md)
and per-instance features at `EXT_instance_features`
([3D Tiles glTF content](https://github.com/CesiumGS/3d-tiles/blob/main/specification/TileFormats/glTF/README.adoc)).

- **Ratified** by Khronos; applies to a **node** with a mesh.
- Attributes, all accessors of one node with **the same count**:
  - `TRANSLATION` — VEC3 float;
  - `ROTATION` — VEC4 quaternion, float or normalized byte/short;
  - `SCALE` — VEC3 float;
  - custom attributes prefixed with `_` (`_ID`, `_COLOR`, `_FEATURE_ID_n`…),
    application-defined. **This is the hook for per-instance animation phase,
    clip choice or wind stiffness** (§3.5.4).
- **Composition:** `InstanceXform = T · R · S`;
  `WorldPosition = NodeWorldXform · InstanceXform · VertexPosition` — the
  instance transform is in the node's *object* space, under the node.
- *"When instancing is used on the node, the non-instanced version of the mesh
  should not be rendered"*, and there is no fallback: files list it in
  `extensionsRequired`. A client that ignores it — tuile today — draws one copy
  at the node's origin, which is wrong, not degraded.
- The README names *"animation of individual instances"* and per-instance
  materials as limitations: the extension has no notion of per-instance pose.
  It is silent about skins.
- `EXT_instance_features` attaches feature id sets to instances (a feature
  count, an optional null id, an attribute index into the `_FEATURE_ID_n`
  instance attributes, an optional property table of `EXT_structural_metadata`).
  It is the instance-level twin of `EXT_mesh_features`
  (see the C++ engine's generated type,
  [ExtensionExtInstanceFeatures](https://cesium.com/learn/cesium-native/ref-doc/structCesiumGltf_1_1ExtensionExtInstanceFeatures.html)).
  For a game it is the key to *which* instance is which: the promotion API (§3.6)
  and picking select instances by feature id and metadata.

### 1.3 glTF animation, skins, morph targets

From the [glTF 2.0 specification](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html)
(§3.7.3 skins, §3.7.2.2 morph targets, §3.11 animations):

- **Animations** are sets of **channels** (a target node + path:
  `translation`, `rotation`, `scale`, `weights`) driven by **samplers** (input:
  keyframe times in seconds, float, strictly increasing; output: values;
  interpolation `STEP`, `LINEAR` — slerp for rotations — or `CUBICSPLINE` with
  in/out tangents). glTF defines no clock, no playback, no looping: *when* an
  animation plays is the application's business.
- **Skins**: a list of joint nodes, `inverseBindMatrices`, vertex attributes
  `JOINTS_n` (u8/u16 ×4) and `WEIGHTS_n` (float or normalized ×4).
  `jointMatrix_j = globalTransform(joint_j) · inverseBind_j`; the skinned mesh
  node's own transform is ignored (only the joints place the mesh).
- **Morph targets**: per-primitive displacement sets for `POSITION`, `NORMAL`,
  `TANGENT`; `final = base + Σ_k w_k · Δ_k`, weights from the mesh default or a
  `weights` channel.
- **Gray area — skin × instancing.** `EXT_mesh_gpu_instancing` composes
  instances under the node world transform, while a skinned node's transform is
  ignored. The JS client resolves it by expressing joint matrices relative to the
  node (`computedJointMatrix = nodeWorldTransform⁻¹ · skinJointMatrix`,
  `references/cesium/packages/engine/Source/Scene/Model/ModelRuntimeNode.js:607`),
  so that skinning happens in node space and the instance and node transforms
  follow. All instances then share **one** pose. This is the behaviour to match
  by default (CLAUDE.md: the reference implementations are authoritative in gray
  areas), with per-instance phase as tuile's own addition (§3.5.4), off unless the
  content or the host asks.

### 1.4 `KHR_animation_pointer`

[`KHR_animation_pointer`](https://github.com/KhronosGroup/glTF/blob/main/extensions/2.0/Khronos/KHR_animation_pointer/README.md)
(ratified) lets a channel target *"any mutable property in a glTF asset"* through
a JSON pointer: material factors (`/materials/0/pbrMetallicRoughness/baseColorFactor`),
camera parameters, node TRS, `extras`. Integer and boolean properties must use
`STEP`. It does not mention instancing.

For games this is the blinking beacon on a wind turbine, the emissive pulse of a
sign, a UV scroll (with `KHR_texture_transform`). It is a later, small subset
(§5, phase 9): material scalars and colours only.

### 1.5 3D Tiles and time

- The 1.1 spec ([core](https://github.com/CesiumGS/3d-tiles/blob/main/specification/README.adoc))
  has no time model. Tile content is a resource like any other; what is
  time-related is metadata (a tile's "last updated" timestamp is a metadata
  example) and `asset.tilesetVersion` for cache busting.
- A per-tile `expire` (re-request after a duration or date) existed in the 1.0
  drafts and the JS client still parses it
  (`references/cesium/packages/engine/Source/Scene/Cesium3DTile.js:379`); it is
  not in the 1.1 specification. It refreshes content; it does not animate it.
- External drivers (a time-dynamic document format moving models along sampled
  paths) live *outside* 3D Tiles, in the client's entity layer — the same split
  as §3.6's ambient vs gameplay.
- The proposed **3D Tiles 2.0** community standard work item (OGC public comment,
  June–July 2026) lists *"Time-dynamic 3D Tiles"* as secondary scope, beside
  voxels, vector tiles and Gaussian splats, and rebases on glTF 2.1
  ([OGC request](https://www.ogc.org/requests/ogc-seeks-public-comment-on-proposed-3d-tiles-2-0-community-standard-work-item/)).
  **Consequence:** whatever tuile builds for time must be a thin input (a scene
  time) that a future standard can drive, not a format of tuile's own.

### 1.6 What the reference clients do

**The JS web client** (`references/cesium/packages/engine/Source/Scene/`):

- Instancing: `EXT_mesh_gpu_instancing` and `EXT_instance_features` are in the
  supported list (`Model/ModelUtility.js:346`); i3dm is loaded by
  `Model/I3dmLoader.js` into the same model components as glTF instancing, then
  drawn by `Model/InstancingPipelineStage.js` with per-instance vertex attributes
  (and a separate 2D/Columbus attribute set).
- Precision: an i3dm without `RTC_CENTER` (or with a zero one) has its positions
  re-centred on their bounding sphere's centre before upload, *"to avoid
  jittering"* (`Model/I3dmLoader.js:424`). Same idea as tuile's rule 7.
- Animation in tiles: `Model/Model3DTileContent.js:316` — once a tile's model is
  ready, **all its animations are added with `REPEAT`**. Time is the scene
  clock (`frameState.time`, `Model/ModelAnimationCollection.js:421`), sampled
  on the CPU per model; skinning in the vertex shader with a uniform array of
  joint matrices (`Model/SkinningPipelineStage.js`), morph targets in
  `Model/MorphTargetsPipelineStage.js`. One pose per node, shared by all its
  instances.
- Classification tilesets refuse skins, morph targets, animations and
  instancing (`Cesium3DTileset.js:1823`).

**The C++ engine** (`references/cesium-native/`):

- Converts i3dm to glTF with `EXT_mesh_gpu_instancing`
  (`Cesium3DTilesContent/src/I3dmToGltfConverter.cpp`): re-centres instance
  positions on their mean, adding a new RTC centre (`:78`), decomposes instance
  matrices into TRS and logs a warning with identity when decomposition fails,
  and handles the rare glTF that is already instanced (`:853`). Batch tables
  become `EXT_structural_metadata` + `EXT_instance_features`.
- Keeps animation, skin and instancing accessors alive through its glTF
  utilities (`CesiumGltfContent/src/GltfUtilities.cpp:877`) but plays nothing:
  rendering and animation belong to the host engine integration, which is not
  in the clone.

**Takeaway.** Both converge on *"i3dm is glTF instancing in another envelope"*.
Neither offers per-instance animation variation, a deterministic clock, or
animation-aware LOD/bounds. Those are open ground.

---

## 2. What tuile does today

Confirmed in the code on `main` (763b901f):

- `crates/tuile-core/src/content.rs`:
  - `sniff` recognises `i3dm`/`pnts`/`cmpt` and returns
    `ContentError::Unsupported` (`:50`);
  - `decode_glb` loads with `gltf::Gltf::from_slice_without_validation` (gltf
    1.4.1), walks the default scene and **bakes every vertex into the tile's
    rebased frame** in f64 before casting to f32 (`walk_node`, `:483`). There
    is no mesh-local form, so an instance has nowhere to go;
  - no extension is read; `EXT_mesh_gpu_instancing` nodes come out as one copy
    at the node's own transform; skins, animations and morph targets are
    ignored; `MaterialDesc` has no alpha mode (foliage needs `MASK`) and no
    double-sided flag.
- `gltf` 1.4.1 has typed readers for skins (`read_joints`, `read_weights`,
  `read_inverse_bind_matrices`), morph targets (`read_morph_targets`) and
  animation samplers (`read_inputs`, `read_outputs`), but **no typed
  `EXT_mesh_gpu_instancing`**: node extensions are reachable as raw JSON through
  `Node::extension_value` behind the crate's `extensions` feature, which the
  workspace does not enable (`Cargo.toml:103`). Accessors named there are then
  read with the crate's accessor machinery.
- `crates/tuile-wgpu`: one interleaved vertex layout (position, normal, uv;
  32 bytes), a per-tile uniform (model matrix + stitching strips), and
  `draw_indexed(0..n, 0, 0..1)` per mesh (`renderer.rs:647`). The shader is
  ground-centric (stitching, imagery passes, aerial perspective).
- The budget (`crates/tuile-core/src/budget/`) charges a `Tiles3dMesh` by
  `DecodedTileContent::cost` → `mesh_cost(vertices, 32, indices, cpu_copy=true)`
  plus owned textures with their mips. It counts **bytes** only.
- `crates/tuile-bevy` draws the globe through `tuile_wgpu::HostPass` on Bevy's
  device; host entities are placed by `GeoAnchor` (f64 ECEF) against a
  `FloatingOrigin`. Its sources are terrain and imagery (`sources.rs`): **no 3D
  Tiles content source yet** — a prerequisite for any of this in a game.

---

## 3. Design

### 3.1 Principles carried over

1. `tuile-core` stays render-agnostic and wasm-clean: instance decoding,
   animation sampling and pose baking are pure functions on `glam` (f64 for
   composition), no GPU, no clock, no thread.
2. Rule 7: f64 until the rebase; per-instance translations are rebased to the
   tile's origin and cast; mesh-local coordinates (metres around a tree's root)
   are f32 from the start.
3. Time is an **input**. The core never reads a clock; the host passes a scene
   time each frame — like the budget's `FrameInput::now`, *"never read inside"*.
4. One draw path shared by every client (viewer, web, film, Bevy). A feature
   built for the game lands in the shared layers (`docs/00-vision.md`).
5. Never a gap: an instanced tile replaced by its refinement follows the same
   create → activate → deactivate order as ground; a promoted instance is shown
   by the host before it is hidden in the tile.

### 3.2 Decoding instanced content

#### 3.2.1 The decoded form

`DecodedTileContent` gains one field; nothing existing changes meaning:

```rust
pub struct DecodedTileContent {
    pub meshes: Vec<DecodedMesh>,          // as today: baked to the tile frame
    pub instanced: Vec<InstancedMesh>,     // new: one mesh × N placements
    /* textures, imagery, withheld_drape, local_origin_ecef, transform_local … */
}

pub struct InstancedMesh {
    /// Positions in the mesh's own frame (node-local, metres), f32.
    pub mesh: DecodedMesh,
    /// Per instance: the full placement mesh → tile render frame, as a 3×4
    /// affine (rotation·scale columns + translation relative to
    /// `local_origin_ecef`), composed in f64 and cast once.
    pub instances: Vec<InstanceAffine>,    // 48 bytes each
    /// Per instance, feature id set 0 (`EXT_instance_features` or i3dm
    /// `BATCH_ID`), when present.
    pub feature_ids: Option<Vec<u32>>,
    /// Per instance, the application attributes the renderer understands
    /// (`_PHASE`, `_CLIP`, `_WIND`); absent = defaults.
    pub params: Option<Vec<InstanceParams>>,
    /// Skin and clips, when animated (§3.5).
    pub rig: Option<Arc<Rig>>,
    /// Envelope of every instance in every pose, tile frame (§4.4).
    pub bounds: Aabb,
}
```

Why a **pre-composed affine** rather than raw TRS:

- i3dm and `EXT_mesh_gpu_instancing` compose in different orders (§1.1, §1.2);
  composing everything in f64 at decode — tile world × RTC × Y-up→Z-up × node
  chain × instance (glTF), or tile world × RTC × instance × Y-up→Z-up × node chain
  (i3dm) — gives one form and removes the order from the renderer;
- non-uniform node scale, ENU frames and the axis swap fold in for free;
- 48 bytes (three `vec4`) is 8 more than TRS (40) and saves the quaternion
  expansion per vertex; three instance attributes fit WebGL2's 16-attribute limit
  with room for skinning;
- the translation column is relative to the tile origin, so a tile's instances
  are rebased by the per-tile uniform the renderer already updates
  (`PreparedTile::rebase`) — no per-instance work when the render origin moves.

Precision check: a tile is at most a few kilometres across; an f32 translation
relative to its centre is good to a millimetre at 10 km
(2⁻²³ × 10⁴ m ≈ 1.2 mm), the same budget the baked meshes already accept.

#### 3.2.2 `EXT_mesh_gpu_instancing`

- Enable the `gltf` `extensions` feature; in `walk_node`, when a node carries
  `EXT_mesh_gpu_instancing`, read its accessors by index through the document
  (with the same `buffers` closure as the primitive reader); handle normalized
  byte/short quaternions; check equal counts (else `ContentError::Malformed`,
  never a panic).
- Emit an `InstancedMesh` per primitive with positions **not** baked
  (node-local), instead of a baked `DecodedMesh`.
- Read `EXT_instance_features` only for feature id set 0 in v1.
- An `extensionsRequired` list naming an extension tuile does not implement
  becomes a typed `Unsupported` error rather than a wrong picture. Today a
  meshopt-compressed tile already fails at accessor reading; making the rule
  explicit is cheap and should land with phase 1.

#### 3.2.3 i3dm

A new container parser beside `tuile-b3dm` (same crate or `tuile-i3dm`; the
names are format names, not brands): header, feature table (JSON + binary, the
same machinery as b3dm's), batch table kept as bytes for metadata, glTF field.

Per instance, in f64:

- position: `POSITION`, or `QUANTIZED_VOLUME_OFFSET + POSITION_QUANTIZED / 65535 ×
  QUANTIZED_VOLUME_SCALE`; plus `RTC_CENTER`;
- orientation: `NORMAL_UP`/`NORMAL_RIGHT` (or their oct-decoded forms) as the
  basis `[right, up, right × up]`; else ENU at the instance's *world* position
  when `EAST_NORTH_UP`; else identity. ENU needs the instance in ECEF, so the
  tile transform is applied first — this is where f64 matters most;
- scale: `SCALE` or `SCALE_NON_UNIFORM`.

Then the affine `tile_world × RTC × instance × Y-up→Z-up × node_chain`, rebased
and cast. The same `InstancedMesh` comes out as for the glTF path, so one test
compares them (the pattern of the existing `b3dm_matches_equivalent_glb`).

`cmpt` (composite) must come with it: real forests ship as `cmpt` holding several
i3dm (one per species) and b3dm. Composite is a concatenation of inner tiles
with a 16-byte header — S, and it unblocks most published instanced data.

#### 3.2.4 External glTF (`gltfFormat = 0`)

`decode` is I/O-free and must stay so. The loader (`tiles3d.rs`) does the
second fetch:

1. `i3dm::parse` reports `Model::External(uri)`;
2. the loader resolves the URI against the content URI, fetches it through the
   `TileFetcher` (so the store and HTTP cache apply), and **interns it** by
   resolved URI;
3. `decode_i3dm(table, model_bytes, hints)`.

Interning matters beyond bandwidth: the decoded model and its GPU buffers are
shared by every tile that names it (§4.3). Phase 4, not phase 1: embedded i3dm
first.

#### 3.2.5 The wire

`DecodedTileContent` never crosses a wire. `Raw` stays glb: a glb with
`EXT_mesh_gpu_instancing` passes through `tuile-server` untouched. For an i3dm
source, the static-HTTP binding forwards the i3dm as is; the WebSocket binding
(glb on the wire) needs an i3dm → glb conversion — a writer, deferred until a
consumer asks for it (USD's `PointInstancer` is the natural target for
`tuile-usd`, also deferred).

### 3.3 The wgpu path

- **A second pipeline**, `instanced`, beside the ground pipelines: vertex buffer 0
  per vertex (position, normal, uv; plus joints u16×4 and weights unorm8×4 when
  skinned — 8 more bytes), vertex buffer 1 **per instance** (`VertexStepMode::Instance`,
  three `vec4` affine rows + one `vec4` of params: phase, clip, wind weight,
  feature id). `draw_indexed(0..n, 0, 0..N)`: **one draw per mesh per tile**.
- **Why a vertex buffer and not a storage buffer:** the WebGL2 floor has no
  storage buffers (`max_storage_buffers_per_shader_stage: 0` in wgpu 30's
  `downlevel_webgl2_defaults`) and a 16 KiB uniform binding (256 matrices).
  Instance-step vertex buffers work everywhere; the stride (64 bytes) is under
  WebGL2's 255.
- **Shader.** Shares the view uniform, lighting and aerial perspective with the
  ground (one atmosphere, one sun); skips stitching and imagery passes. Adds
  `alphaMode: MASK` (alpha cutoff) and double-sided, which foliage requires — a
  `MaterialDesc` extension that also benefits b3dm content.
- **Upload**: the instance buffer is created with the mesh, in the same
  `prepare` step, and charged with it (§4.1).
- **Culling.** Per tile, by the traversal, as today: the tile's bounding volume
  must enclose all its instances in all their poses (the spec requires a tile's
  volume to contain its content; §4.4 makes it true for animation).
  Per instance:
  - v1: none. The GPU's clipper discards off-screen instances; the cost is
    vertex work only, bounded by tile selection.
  - later (phase 10, native and WebGPU only): a compute pass culls instances
    against the frustum and a distance band, compacts survivors and writes
    `draw_indexed_indirect` arguments; WebGL2 keeps the draw-all path. Built
    **only if** a measurement says vertex work dominates.
- **Per-instance LOD is the tileset's job.** The 3D Tiles way to thin a forest
  is coarser tiles holding fewer instances (or a cheaper mesh), refined by SSE.
  tuile does not invent a second LOD system inside a tile. A distance fade
  (dithered alpha over the last metres of a tile's range) can hide refinement
  pops without ever leaving a gap.

### 3.4 Who owns time

- **The host.** A `SceneTime` — seconds as f64, or for a film the exact rational
  `frame / fps` — is passed with the view each frame, into the renderer's view
  uniform. The core's animation functions take it as an argument.
- **Game:** Bevy's `Time<Virtual>` (pausable, scalable) feeds it; the game
  decides whether the world freezes on pause.
- **Film:** `frame / fps`. A render is a function of `(stage, time)`
  (`crates/tuile-core/src/determinism.rs`): two farm nodes rendering frame 1,000
  must draw the same pose. With baked poses at a rate that divides the film rate
  (§3.5.2), the sample is exact, not interpolated.
- **Viewer and web:** wall time since session start, monotonic, through the same
  input.
- **What time never does: decide residency** (§4.4).

### 3.5 Animation

#### 3.5.1 Two kinds of motion, one representation

- **Rigid node animation** (a turbine's rotor, a radar, a door): channels on
  node TRS, no skin.
- **Skinned animation** (characters, animals, a flag on bones): joints.

Rigid node animation of a mesh hierarchy is turned into a skin at decode
(*rigidification*): every animated node becomes a joint, every vertex of a
primitive under it is bound with weight 1 to that joint. One GPU path then serves
both, and a rotor costs one joint.

#### 3.5.2 Sampling: CPU, GPU, or baked

| Option | Per-frame cost | WebGL2 | Per-instance phase | Determinism |
|---|---|---|---|---|
| A. CPU sampling per instance, joint matrices uploaded | N × J evaluations and N × J × 48 B upload | yes (uniforms/texture) | yes | exact |
| B. CPU, K shared phase buckets | K × J | yes | quantised to K | exact per bucket |
| C. GPU compute sampling from keyframes | ~0 CPU, a compute dispatch | **no** | yes | GPU-dependent float |
| D. **Baked pose table** (joint matrices per frame per clip in a texture) | 0 CPU, a texture fetch per joint per vertex | yes (vertex texture fetch) | yes, free | exact at sample instants |

A thousand characters with 50 joints is 50,000 matrices a frame for A — 2.4 MB of
upload at 48 bytes and a CPU load the web cannot pay. **D is the default**: at
decode, on the decode thread, the core samples each clip with glTF's exact
interpolation (slerp, cubic spline) at a rate `r` the consumer requests, composes
the joint hierarchy in f64, multiplies by the inverse bind matrices, and writes
3×4 f32 rows. The vertex shader reads the two bracketing frames for
`t_local = (scene_time × speed_i + phase_i) mod duration` and blends them.

- Size: `frames × joints × 48 B` — 2 s at 30 Hz × 64 joints = 184 KB per clip,
  RGBA32F (or half that in RGBA16F once measured acceptable).
- Film: request `r = fps` (or a multiple): the film's instants fall on samples and
  the blend weight is 0 — no interpolation error, bit-stable across runs on one
  GPU model.
- Interactive: the linear blend between samples approximates slerp; at 30 Hz
  the error is invisible for walk cycles; the rate is a config value, not a switch.
- A is kept only for content the host drives frame by frame (a promoted instance
  is the host's; §3.6) — so in practice tuile-wgpu needs D only.
- C is not built: D has the same GPU cost, works on WebGL2, and moves the
  arithmetic to a deterministic CPU step done once.

#### 3.5.3 GPU skinning

Linear blend skinning in the vertex shader: four joints per vertex (`JOINTS_0`
/ `WEIGHTS_0`; a second set is folded into the top four at decode, renormalised,
with a warning), the pose texture indexed by `(clip, frame, joint)`, then the
instance affine, then the tile model matrix. Normals by the same matrices
(rigid-ish joints; no inverse-transpose per vertex). The order follows the JS
client's gray-area resolution (§1.3): pose in node space, then instance, then
node world — so the same content looks the same in both clients.

#### 3.5.4 Per-instance variation

Lockstep forests and crowds look mechanical. Variation, in priority order:

1. the content's own `_PHASE`, `_SPEED`, `_CLIP` instance attributes (allowed by
   `EXT_mesh_gpu_instancing`'s custom attributes — a convention tuile documents,
   not a new extension);
2. otherwise, if the host enables it, a phase derived from a hash of
   `(tile content id, instance index)` — deterministic, identical in every
   client and every film frame;
3. otherwise lockstep, as the JS client.

#### 3.5.5 Morph targets

Lower priority for games over a globe (faces, cloth). The design that fits the
WebGL2 floor is Bevy's own: targets in a texture, weights per instance from the
pose table (a "weights" row beside the joints). Phase 8, only if content needs
it.

#### 3.5.6 Wind for vegetation

No standard covers it, and none is needed: wind is a **continuous function of
inputs**, evaluated per vertex per frame on the GPU — the project rule for
anything that moves (continuous, never stepped).

- Inputs: scene time, a wind field in the view uniform (direction, strength,
  gust frequency; constant per frame, later from weather data), the instance's
  position (phase from its translation, so neighbours sway coherently with a
  travelling gust), and a per-vertex bend weight.
- Bend weight: `COLOR_0.a` or a `_WIND` vertex attribute when the author provides
  it; otherwise height in the mesh frame, normalised by the mesh bounds (trunks
  stiff, crowns free).
- Displacement bounded by the mesh height × a maximum angle, so the envelope in
  §4.4 is computable.
- Applies to non-instanced foliage too (b3dm, glTF), as a material flag.

### 3.6 tuile-bevy: own path or Bevy's?

Bevy has a complete animation stack: skinning (`bevy_pbr`'s `skin.rs`,
`MAX_JOINTS = 256`, storage buffers with a uniform fallback), morph targets in a
texture (`morph.rs`), the `AnimationPlayer` / `AnimationGraph` blending, and
automatic batching of entities sharing a mesh and material
([Bevy animation docs](https://docs.rs/bevy/0.20.0/bevy/animation/index.html),
[GPU-driven rendering, Bevy 0.16](https://bevy.org/news/bevy-0-16/)).

| | tuile-wgpu path (ambient) | Bevy entities (gameplay) |
|---|---|---|
| Scale | 10⁵ instances per view, no ECS cost | 10²–10³ entities comfortably |
| Budget | charged, evicted, deterministic | outside tuile's budget (Bevy's assets) |
| Clients | every client, film included | Bevy only |
| Game features | none: no colliders, no logic | physics, picking, state machines, blending |
| Precision | tile-relative, rebased per tile | `GeoAnchor` against `FloatingOrigin` |

**Recommendation: both, with a seam between them.**

- Tile content is drawn by tuile-wgpu, always.
- A **promotion API**: the host asks for instances matching a feature id or a
  metadata predicate (`class == "vehicle"`) within a radius; tuile-bevy emits
  `InstancePromoted { feature_id, ecef_affine (f64), mesh handle, rig }` events.
  The host spawns its entity (with `GeoAnchor`, Bevy's `SkinnedMesh`,
  `AnimationPlayer`, a collider) and acknowledges; **only then** tuile hides that
  instance through a per-tile visibility bitmask (a `u32` per 32 instances in the
  instance params, or a zero-scale write into the instance buffer). On demotion
  the reverse: unhide first, then the host despawns. Never a frame with neither
  — the same rule as ground.
- **Colliders** for ambient content: the core already holds per-instance f64
  placements at decode; tuile-bevy can expose them (`TileInstances` events on
  load/unload) so the host builds static colliders for trees and buildings near
  the player without promoting them.
- A prerequisite either way: a 3D Tiles content source in `GlobeSources`
  (today: terrain and imagery only).

### 3.7 Other consumers

- **Film / Hydra / USD**: the decoded form maps to USD `PointInstancer`
  (prototypes + positions/orientations/scales) and skel animation; deferred, but
  nothing here blocks it. The film path uses the same tuile-wgpu pipeline, so a
  film inherits instancing and animation as soon as the viewer does.
- **Server**: unaffected beyond the optional i3dm → glb writer (§3.2.5);
  `tuile-server` stays ignorant of animation, like it is of USD.

---

## 4. Budget implications

### 4.1 What an instanced tile costs

Bytes, with the estimator's alignment rules (`budget/cost.rs`):

```
mesh        = mesh_cost(vertices, stride, indices, cpu_copy)   // once per mesh
              stride = 32 (+8 when skinned)
instances   = align(N × 64, 4)                                 // affine + params
rig         = Σ clips frames × joints × 48  (+ mips: none)     // pose texture
morph       = vertices × targets × 12 (positions) [× 2 with normals]
textures    = as today, with mip chains
uniform     = the per-tile uniform, as today
```

`cpu_copy` is `true` today because ground keeps a CPU copy for stitching.
Instanced meshes are not stitched: `false`, unless the host asked for colliders
or picking, in which case the placements (N × 3 × 8 × 4 f64 bytes, 96 B each)
are what is kept, not the vertices.

Two worked examples:

| Tile | Mesh | Instances | Rig | Textures | Total |
|---|---|---|---|---|---|
| Forest: 5,000 trees, 3,000-vertex tree, 4,000 triangles | 96 KB + 48 KB | 320 KB | — | 1 MB → 1.33 MB | ~1.8 MB |
| Plaza: 200 walkers, 8,000-vertex figure, 2 clips × 2 s × 64 joints | 320 KB + 96 KB | 12.8 KB | 368 KB | 2.67 MB | ~3.4 MB |

### 4.2 The missing axis: raster work

The forest tile is 1.8 MB — cheaper than one ground drape at high resolution — and
draws **20 million triangles**. A byte budget would happily hold twenty of them and
the frame would collapse. The budget needs a second, deterministic estimate:

- `Cost { gpu, staging, work }`, with `work = triangles × instances`
  (+ a skinning factor) computed by the same estimator;
- a per-profile ceiling on the **drawn** work (desktop, handheld, headset),
  enforced in the draw set: when exceeded, the budget stops *refining* instanced
  content (raises its SSE) — it never drops coverage, and it never drops a parent
  before the child that replaces it;
- the observer reports drawn work beside drawn bytes, so the ceiling is tuned on
  measurements, not guessed (bench before shipping).

This is the main budget change and the reason the plan places this track after
the budget roadmap: it touches `decide`'s draw set, which A4's incremental
`decide` will have rewritten.

### 4.3 Sharing

An external i3dm model (and, later, any content-addressed glTF) is shared by
every tile that names it — the same situation as imagery, where charging each
user would *"evict geometry to reclaim bytes that do not exist"*
(`DecodedTileContent::byte_size` doc). The model becomes a resource of its own,
keyed by its resolved URI, charged once, held while any drawn or needed tile
references it, and released after the last (per-view reference counts are B7's).
Its pose textures belong to it. A tile then costs only its instance buffer and
uniform.

### 4.4 Determinism: time is not an input to `decide`

- `decide` is a pure function of the view, the need, arrivals and `now` (for
  deadlines). Animation changes pixels, not residency. A frame's selection is the
  same whatever the pose — and a film's selection digests stay bit-identical with
  animation on or off. That is a test (phase 5).
- The **bounding volume must be the envelope over time**: at decode, the core
  computes each instanced mesh's bounds over every baked pose (and the wind
  amplitude bound), transforms them by each instance, and takes the union. A
  content bounding volume smaller than this envelope is enlarged in tuile's arena
  (with a warning in the observer), never trusted, so culling and SSE never flicker
  with a pose.
- Baking poses costs CPU at decode time, on the decode workers, before the arrival
  is recorded — it never runs on the frame. Its cost enters the GreedyDual
  restore cost (B1) of the resource.

### 4.5 Charging and eviction

- Class: `Tiles3dMesh` keeps instanced tiles (their cost includes the instance
  buffer and the work); a new `Class::Model` holds shared models (§4.3). The
  pooled ledger's floors and ceilings get a line for it.
- Staging: mesh + instances + pose texture go through the delivery credit like
  any upload; a large rig is the one case where the pose texture dominates.
- Eviction follows B1–B3 unchanged; a shared model is evictable only when no
  resident tile references it.
- Promoted instances (§3.6) leave tuile's ledger: their bytes are the host's.

---

## 5. Phased plan

Placed **after** the current budget work: the compression budget (its B1–B8),
then A2, A4, B and C of the roadmap. Nothing here starts before; respecting the
order is not optional. Each phase is its own PR (or a stack of them), measured,
with its tests written first and seen failing.

| # | Phase | Size | Depends on |
|---|---|---|---|
| 0 | Prerequisites: `cmpt`, `MaterialDesc` alpha mode + double-sided, `extensionsRequired` check, a 3D Tiles source in tuile-bevy | M | budget roadmap done |
| 1 | Core: `EXT_mesh_gpu_instancing` + embedded i3dm → `InstancedMesh` | M | 0 |
| 2 | tuile-wgpu: instanced pipeline (WebGL2-valid), cost estimator | M | 1 |
| 3 | Budget: `work` axis, drawn-work ceiling, observer channels | M | 2 |
| 4 | External glTF in i3dm, shared `Model` resource | M | 3 |
| 5 | Core: clips, rigidification, baked poses, time-envelope bounds | L | 1 |
| 6 | tuile-wgpu: GPU skinning from the pose table, per-instance phase | M | 2, 5 |
| 7 | Wind | S | 2 |
| 8 | Morph targets (if content needs them) | M | 6 |
| 9 | tuile-bevy: promotion API, colliders events, example scene | M | 6 |
| 10 | GPU instance culling (native/WebGPU), only if measured necessary | M | 3 |
| — | `KHR_animation_pointer` material subset | S | 6 |

### Red tests per phase (written first, seen failing)

- **1.**
  - `instanced_glb_decodes_every_instance`: a three-instance glTF (the public
    sample `SimpleInstancing`, or a hand-built one beside `test_glb()`) yields
    three placements — **fails on main**, which yields one mesh;
  - `i3dm_matches_equivalent_instanced_glb`: an i3dm and the glb that states the
    same instances decode to the same placements within 1 mm (f64 recompute);
  - `i3dm_east_north_up_orients_on_the_ellipsoid`, `i3dm_oct_normals`,
    `i3dm_quantized_positions`, `i3dm_rtc_center` (the public 1.0 instanced
    sample tilesets: orientation, scale, non-uniform scale, RTC, quantized —
    verify their current names and licences before vendoring);
  - `instance_translations_survive_ecef_magnitudes`: instances 6,400 km from the
    origin come out with < 2 mm error (rule 7);
  - malformed counts → typed error, fuzzed with the existing content fuzz target;
  - `cargo check --target wasm32-unknown-unknown -p tuile-core`.
- **2.**
  - readback test: a two-instance scene renders two silhouettes at the expected
    pixels (`readback.rs`), red with `0..1` instances;
  - the WGSL passes the WebGL2 validation of `webgl2.rs`;
  - estimator drift: the acknowledged GPU bytes within 5 % of `cost()`.
- **3.**
  - simulation: a forest-heavy synthetic tileset under the handheld profile keeps
    drawn work under its ceiling and still covers the view (no tile without a
    drawn ancestor);
  - every existing golden (`goldens.txt`, `pattern_goldens.txt`) and film
    selection digest **unchanged** for scenes without instancing.
- **4.** one model referenced by 100 tiles is fetched once, charged once, released
  after the last tile; ledger audit stays equal to the sum of states.
- **5.**
  - clip sampling against analytic values on the Khronos `InterpolationTest`
    sample (STEP, LINEAR, CUBICSPLINE, slerp);
  - rigidification: a node-animated rotor sampled through the joint path equals
    the direct node evaluation;
  - `selection_ignores_animation_time`: the same view at times 0 and 7.3 s gives
    bit-identical `Decisions::digest`;
  - bounds envelope contains every baked pose of `RiggedFigure` / `Fox`;
  - native and wasm32 produce bit-identical pose tables (f64 transcendental
    functions may differ between targets — see risks).
- **6.**
  - GPU-skinned readback equals a CPU-skinned reference frame within a pixel
    tolerance;
  - per-instance phase: two instances with different `_PHASE` differ, two with
    the same are identical;
  - film determinism: one film frame rendered twice → identical bytes; the video
    opened (CLAUDE.md: every render gets opened).
- **7.** wind: displacement is continuous in time (no step between frames at any
  rate), bounded by the declared envelope; a film of a swaying forest opened.
- **9.** promotion: in a headless Bevy app, across the promote/demote frames, the
  instance is drawn by exactly one of tuile and the host on every frame (never
  zero; one frame of both tolerated).

### Acceptance (whole track)

- Desktop (Mac): 100,000 tree instances in view at 60 Hz with the globe; 1,000
  skinned walkers at 60 Hz. iPhone: a third of each at 30 Hz. WebGL2: the same
  scenes run, at reduced counts, with no feature missing. Bench before and after
  each phase, per the project rule.
- A Bevy example: a wind farm and a forest over streamed terrain, turbines
  turning, trees swaying, a few promoted vehicles driven by the host.
- A film of the same scene from the farm, deterministic across two nodes,
  recorded in `videos/README.md` with its parameters.
- `docs/02-3d-tiles-primer.md` and `docs/10-crate-core.md` updated: i3dm and
  `cmpt` move into scope.

---

## 6. Risks

- **The standard may move.** 3D Tiles 2.0's time-dynamic item could define a
  clock or animation semantics. Mitigation: tuile's only time concept is a
  scene-time input; the `_PHASE`/`_CLIP` attributes are documented as a
  convention, easy to map onto a future standard.
- **Gray areas.** Skin × instancing composition (§1.3), animated content whose
  declared bounding volume is too small (§4.4), i3dm with a glTF that is already
  instanced (the C++ engine calls it *"very rare"*). Each gets a fixture and the
  JS client's behaviour as the reference.
- **Raster cost, not bytes, is the real limit.** Without phase 3 a forest tileset
  passes the budget and collapses the frame. Phase 3 is not optional and comes
  before any public demo.
- **Cross-target float determinism.** Pose tables are composed in f64 with
  `sin`/`acos` (slerp); std's libm may differ between native and wasm32 in the
  last bit. If the test in phase 5 fails, use an established pure-Rust libm crate
  for those calls, not a hand-written one. GPU skinning differs between GPU
  vendors: a film is reproducible on one farm GPU model, as today.
- **WebGL2 ceiling.** 16 vertex attributes (3 vertex + 2 skin + 4 instance = 9,
  room left), no storage buffers, no compute: everything in §3 is chosen to fit;
  phase 10's culling is native/WebGPU only, with a draw-all fallback.
- **The `gltf` crate.** No typed `EXT_mesh_gpu_instancing`; raw JSON through the
  `extensions` feature is enough. Should a bug appear, the project rule applies:
  fork under lapoule-dev, fix with a test, pin the revision.
- **Content scarcity and licences.** Few public tilesets carry animated
  instanced content. The Khronos sample assets (`SimpleInstancing`, `RiggedFigure`,
  `Fox`, `InterpolationTest`, `AnimatedMorphCube`) have per-model licences to
  check before they enter `fixtures/`; tuile may have to author its own instanced
  test tilesets (a tool in `tuile-bake` that scatters a model over terrain).
- **Popping on refinement.** Replacing a sparse forest tile by a dense one is a
  visible pop. The dithered fade of §3.3 softens it; it must never be "solved" by
  hiding the parent early.
- **Scope creep into a game engine.** Physics, AI, gameplay animation blending
  belong to the host. tuile stops at ambient motion and at handing instances over.
