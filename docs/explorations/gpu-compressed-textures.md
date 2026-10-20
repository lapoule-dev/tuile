<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# GPU-compressed imagery in the viewer

Status: design, 21 Oct. 2026. Nothing is built yet. This note is the first PR of
roadmap item A6. The PRs that follow it are listed in §10.
Scope: the interactive viewer, native and web (`tuile-wgpu`, `tuile-viewer`,
`tuile-web`, the decode jobs in `tuile-planetary`). The film renderer
(`tuile-film*`) is out of scope, and its goldens must stay byte-identical (§8.4).

Numbers marked *(to measure)* are engineering estimates. They are not
measurements, and the measurement plan in §9 exists to replace them.

## 1. Decisions this note builds on (Laurent, 21 Oct.)

- **Encoded on the fly, client side.** Tiles arrive as they do today (JPEG/PNG
  from the sources). They are decoded, then encoded to the GPU's native block
  format just before upload. Nothing is stored compressed, and the stocks and
  stl-tiles don't change. No derived imagery layer is stored anywhere, which
  keeps Bing imagery within its terms.
- **Two encoders.**
  1. A **WGSL compute encoder** in `tuile-wgpu` is the fast path wherever the
     adapter has compute and a block format the encoder writes.
  2. A **pure-Rust CPU encoder ported from Basis Universal**, running in the
     decode workers, covers the rest (WebGL2, adapters without compute, block
     formats the GPU encoder doesn't write yet).
- **Selection comes strictly from what wgpu reports** (features, downlevel
  capabilities, limits): the GPU encoder, else the CPU encoder, else
  uncompressed. There is no flag and no switch. Environment variables may tune
  quality and effort, and never choose a path.
- **The CPU encoder is its own crate in its own repository** under the
  lapoule-dev organisation (coordinator message, 21 Oct.). tuile depends on it
  by git tag. §5.1 covers the repository and its name.

## 2. Where the viewer stands today

| Step | Today | File |
|---|---|---|
| Decode | `image` 0.25 (jpeg, png) → `to_rgba8` | `tuile-core/src/raster.rs:1610` `decode_image` |
| Reproject | Web Mercator → geographic spacing, CPU | `raster.rs:1635` `decode_and_reproject` |
| Job | `ImageryDecode` (`Portable`, kind `"imagery"`): decode + reproject + opaque-black check; the output is the raw RGBA plus w, h and flags | `tuile-planetary/src/jobs.rs:46,68,130` |
| Workers | native `ThreadPool` (`tuile-decode-N`); web `WebDecode` workers (`runDecodeWorker`), with an inline fallback | `tuile-core/src/offload.rs:280`, `tuile-viewer/src/web/decode.rs:63,149` |
| Web wire | the geometry worker posts raw RGBA8 to the page | `tuile-web/src/wire.rs:603,650,877,920` |
| Mips | CPU, `raster::mip_chain` (Triangle filter), on the uploading thread. GPU blit chains were rejected because of the Metal render-pass leak (gfx-rs/wgpu#8768) | `tuile-wgpu/src/prepare.rs:554-584` |
| Upload | one `Rgba8UnormSrgb` texture per imagery tile, one `write_texture` per level, shared by `(source, coord)` | `prepare.rs:564`, `context.rs:36,88,142` |
| Device | the only features requested are the timestamp ones. **Nothing detects BC, ETC2 or ASTC**, and the viewer has no compute pipeline | `tuile-viewer/src/app/setup.rs:133-191` |
| Cost | `budget::cost::drape_cost(w, h, TextureFormat)` already knows `Rgba8`, `Block16` and `Block8`. Every caller hard-codes `Rgba8` | `tuile-core/src/budget/cost.rs:65-103`, `content.rs:161-253` |
| Ack | the pump acknowledges a drape with `drape.cost().gpu`, the core's estimate. The measured `GpuImagery.bytes` appears only in `ImageryTextures::live()` | `pump.rs:537`, `context.rs:97-112` |

Imagery tiles are 256² almost everywhere; Bing uses the provider's width. The
ground shader uses alpha (`raster.wgsl:45`, `mix(.., texel.rgb, mask * texel.a)`),
so a layer with partial coverage needs an alpha-capable format.

Film does not use `tuile-wgpu`. It shares only core's decode
(`decode_and_reproject`) and `DecodedTexture`, and it builds its own mips by
compute in `tuile-film-gpu`.

## 3. Formats per backend and what wgpu 29 exposes

wgpu exposes three relevant features: `TEXTURE_COMPRESSION_BC`, `_ETC2` and
`_ASTC`, plus their HDR/3D variants, which are not needed here. Read in
`wgpu-hal` 29.0.4:

| Platform / backend | BC (BC1–7) | ETC2 | ASTC LDR | Compute | Note |
|---|---|---|---|---|---|
| macOS, Metal (Apple silicon) | yes | yes (Apple7+) | yes | yes | `metal/adapter.rs:720-725`: `format_bc = (os == macOS)` |
| iOS / iPadOS / visionOS, Metal | **no** | yes | yes | yes | wgpu reports no BC off macOS, whatever the hardware does |
| Windows DX12 / Vulkan desktop | yes | rarely | rarely | yes | NVIDIA/AMD/Intel desktop: BC only |
| Android Vulkan | rarely | yes | mostly yes (Mali, Adreno, PowerVR) | yes | |
| Linux GL fallback (`TUILE_BACKEND`) | if s3tc + rgtc + bptc | if ES3 compat | if KHR_astc_ldr | GL 4.3 / ES 3.1 | `gles/adapter.rs:497-575` |
| Browser WebGPU, desktop | `texture-compression-bc` | on Apple, `-etc2` too | on Apple, `-astc` too | yes | |
| Browser WebGPU, Android | rarely | `-etc2` | `-astc` | yes | |
| Browser WebGL2 (wgpu GL) | only if **all four** of `WEBGL_compressed_texture_s3tc`, `_s3tc_srgb`, `EXT_texture_compression_rgtc` and `_bptc` are present | `WEBGL_compressed_texture_etc` | `WEBGL_compressed_texture_astc` | **none** | the CPU encoder is the only path |

Formats the viewer uses, all sRGB so that sampling stays exactly what
`Rgba8UnormSrgb` does today:

| Format | wgpu name | bpp | vs RGBA8 | Alpha | Use |
|---|---|---|---|---|---|
| BC7 | `Bc7RgbaUnormSrgb` | 8 | 4× | full | default where BC exists |
| ASTC 4×4 | `Astc { B4x4, UnormSrgb }` | 8 | 4× | full | default where ASTC exists and BC does not |
| ETC2 RGBA8 (EAC alpha) | `Etc2Rgba8UnormSrgb` | 8 | 4× | full | ETC2-only adapters, tiles with alpha |
| ETC2 RGB8 | `Etc2Rgb8UnormSrgb` | 4 | 8× | none | ETC2-only adapters, opaque tiles |
| BC1 | `Bc1RgbaUnormSrgb` | 4 | 8× | 1-bit | opaque tiles, only if the 8× tier is wanted (⚑2) |

Order of preference for one adapter: **BC7, then ASTC 4×4, then ETC2**, then
uncompressed. The decode job already scans each tile for opaque black, so it can
flag "fully opaque" in the same scan. An opaque tile may then take the 4 bpp
tier (BC1 or ETC2 RGB8) when ⚑2 allows it. A tile with alpha always takes an
alpha-capable 8 bpp format.

Constraints these formats impose, which the plan honours:

- A block-compressed texture can be neither a render target nor a storage
  texture. The GPU encoder writes blocks into a **storage buffer**, then
  `copy_buffer_to_texture` fills each level. `bytes_per_row` is `blocks_x × 16`,
  which is 1,024 bytes at 256² and is padded to 256 for the small levels.
- Level 0 must be a multiple of 4. Levels below 4×4 are stored as one padded
  block, which wgpu handles through the level's physical size. Tile widths that
  aren't multiples of 4 (none today) stay uncompressed.
- The device must **request** the chosen feature. `bring_up` (`setup.rs:175`)
  gains the selected compression feature, intersected with the adapter's
  features as the timestamp features already are.

## 4. The GPU encoder (WGSL, `tuile-wgpu`)

### 4.1 Shape

- **Input:** the RGBA8 levels as they are uploaded today, written once into a
  per-frame scratch texture or buffer (staging is unchanged, §7).
- **Kernel:** one invocation per 4×4 block, in 8×8 workgroups (64 invocations,
  within every downlevel limit). It writes `vec4<u32>` per BC7 block (or
  `vec2<u32>` per BC1 block) into a storage buffer.
- **Copy:** `copy_buffer_to_texture` into the final `Bc7RgbaUnormSrgb` texture,
  in the same command buffer.
- **Batching:** every imagery tile the pump uploads in one frame is encoded in
  **one compute pass**: one dispatch per (tile, level), or one dispatch over a
  block table. This keeps the pass count at one per frame, whatever the Metal
  leak turns out to do with compute passes (⚑5).
- **Mips:** the CPU `mip_chain` stays as it is, and the encoder runs on each
  level. Building mips by compute (as `tuile-film-gpu` does) is a later
  optimisation, and it is not needed to get the bytes down.

### 4.2 BC7 mode subset

Imagery is mostly opaque and photographic: smooth gradients, with some sharp
edges (roads, coastlines, field boundaries).

| Mode | Subsets | Endpoints | Indices | Why |
|---|---|---|---|---|
| 6 | 1 | RGBA 7.7.7.7 + p-bit | 4-bit | smooth blocks, and alpha. The workhorse |
| 1 | 2 (64 partitions) | RGB 6.6.6 + shared p-bit | 3-bit | edge blocks |
| 5 | 1, rotation | RGB 7.7.7, A 8 | 2+2-bit | blocks with non-trivial alpha (layer edges) |

- **Per block:** PCA or bounding-box endpoints for mode 6, then least-squares
  refinement (two iterations).
- **Mode 1:** a cheap partition shortlist, the 8–16 partitions with the best
  luma split, then the same fit.
- **Mode 5:** tried only when the block's alpha isn't constant.
- **Choice:** the lowest RGB error wins.

Modes 0, 2, 3, 4 and 7 are left out. They help mostly on alpha-heavy or
three-colour content, which imagery rarely is. Expected quality is about
40–44 dB PSNR on photographic 256² tiles, vs about 45–47 for the slow offline
encoders *(to measure)*.

**BC1** is range fit on the principal axis plus one refinement, done in the same
pass. It ships with the BC7 kernel because it is small and gives the 8× tier on
BC adapters (⚑2).

The WGSL is our own code. Published compute and ISPC encoders inform the
algorithm, which creates no obligation (rule 4); nothing is transliterated.

### 4.3 ETC2 and ASTC in compute

- **ETC2 RGB8 (and EAC alpha): feasible.**
  - ETC1 individual/differential modes plus the ETC2 planar mode cover most
    photographic blocks. T and H modes add little on imagery and can come
    later.
  - EAC alpha is a separate 64-bit half-block with a small search.
  - Cost: about one BC7-sized kernel.
- **ASTC 4×4 LDR: feasible for a fixed configuration.**
  - Configuration: a single partition, CEM 8 (RGB direct) or CEM 12 (RGBA
    direct), a 4×4 weight grid with one fixed quantisation chosen to fill the
    128 bits, and fixed BISE packing.
  - This is the subset UASTC's simplest modes use.
  - Quality is below a full ASTC search, but it is uniform and fast.
- **Order:** BC7/BC1 first (PR 2), because that covers macOS, Windows and
  desktop WebGPU. ETC2 and ASTC come in a later PR (PR 5). Until then iOS,
  Android and ETC2/ASTC-only WebGPU go through the CPU encoder, as the
  selection rule already says: "a compressed format the GPU encoder writes".

### 4.4 Capability detection

`tuile_wgpu::texcomp::select(features, downlevel, limits, format_features)`
returns one of:

- `Gpu(BlockFormat)`: `DownlevelFlags::COMPUTE_SHADERS`, at least one storage
  buffer per compute stage, and workgroup limits of 64 or more. The format's
  feature must be present and `get_texture_format_features` must report it
  filterable. The GPU encoder must implement the format.
- `Cpu(BlockFormat)`: the format's feature is present and filterable, and the
  CPU crate implements it.
- `Uncompressed`.

`select` is a pure function of what the adapter reported, so the capability
matrix (Metal macOS, Metal iOS, Vulkan desktop, Vulkan Android, DX12, GL,
WebGL2 with and without the BC extensions, WebGPU desktop and Android) is a unit
test over synthetic `Features`/`DownlevelCapabilities`. A forced-backend run
(`TUILE_BACKEND=gl`, and WebGL2 in web-bench) checks the live path.

## 5. The CPU encoder: a Rust port of Basis Universal

### 5.1 Repository, name and licence

- **Repository:** `lapoule-dev/texcomp`, private. Apache-2.0 doesn't require
  publishing a port, and nothing in the licence analysis argues for public
  (⚑1 has the name).
- **Crate:** `texcomp`. Free on crates.io as of 21 Oct. Its public surface is
  our own types: `BlockFormat`, `Image<'_>`, `Quality`, `encode(..) -> Blocks`.
- **Licensing:**
  - our code: dual MIT OR Apache-2.0, with SPDX headers;
  - every ported item: an Apache-2.0 attribution comment at its head, and a
    prominent "modified" notice as Apache-2.0 §4(b) requires;
  - `NOTICE` names upstream Basis Universal **v2.50**, tag `v2_50`, commit
    `9bebe16726b3a61c8c213eeee3b7cffb462ef34e`, Apache-2.0, © Binomial LLC;
  - the upstream `LICENSE` is kept as `LICENSE-APACHE-UPSTREAM`.
- **Naming:** the README names Basis Universal nominatively, as the project it
  ports. The crate name and every type stay generic. ("Basis Universal" is a
  Binomial trademark; nominative use only.)
- **CI:** test, clippy `-D warnings`, `cargo check --target wasm32-unknown-unknown`,
  fmt. Pure Rust, no `build.rs`, no C toolchain, no `unsafe`. No dependencies
  beyond `thiserror`; `rayon` stays out because the callers own the threads.
- **tuile's side:** tuile depends on it by git tag, and the main session tags
  its releases. Inside tuile it is used only by `tuile-planetary` (the decode
  job). `tuile-core` gets the enum and the texel container (§6), never the
  encoder.

### 5.2 What gets ported

The input arrives as RGBA, gets encoded, and goes into a GPU texture. It never
touches a disk or a network. Supercompression (ETC1S codebooks, RDO, Zstd,
`.basis`/`.ktx2` containers) exists to make files small, which is a cost we
don't pay. So the port takes the **block layer** of Basis and leaves the
container layer:

| Part | Upstream (v2.50) | Ported | Why |
|---|---|---|---|
| UASTC LDR 4×4 encoder | `encoder/basisu_uastc_enc.cpp` (≈150 KB) + the ASTC/BC7 helpers it uses | **yes**, effort levels 0–2 first, higher levels later | the high-quality intermediate, about 8 bpp |
| UASTC → ASTC 4×4 | `transcoder/basisu_transcoder.cpp` (UASTC part), `basisu_astc_helpers.h` | **yes** | lossless by construction (UASTC is an ASTC subset) |
| UASTC → BC7 | same | **yes** | upstream states 0.75–1.5 dB below ASTC |
| UASTC → ETC1 / ETC2 RGB(A) + EAC | same, with the ETC1 hint bits | **yes** | |
| UASTC → BC1 | same, with the BC1 hint bits | **yes** (cheap, ⚑2) | |
| UASTC unpack (reference decode) | transcoder | **yes**, in tests and as the quality instrument | |
| ETC1S encoder (frontend, backend, codebooks) | `basisu_frontend.cpp`, `basisu_backend.cpp`, `basisu_etc.cpp` | **no** (⚑3) | heavy clustering encoder, lower quality; its only benefit is a small file |
| RDO, Zstd, KTX2/.basis/.DDS, XUASTC, XUBC7, HDR, PVRTC, ATC, FXT1, OpenCL, SSE/SPMD kernels, resamplers, image loaders | | **no** | containers, transmission and formats wgpu doesn't expose |

Sizing *(to measure)*: about 6–9k lines of Rust plus generated tables.
Upstream's per-format transcoder tables for UASTC are small; the large `.inc`
tables in `transcoder/` serve ETC1S and aren't needed.

### 5.3 Correctness against upstream

- **Reference vectors** are captured once from upstream `basisu` v2.50 on a
  remote builder (never in CI, never on the laptop's critical path). For each
  fixture tile and each effort level:
  - the raw UASTC blocks (`-uastc -uastc_level N`, no RDO, no Zstd);
  - each transcode to ASTC 4×4, BC7, ETC1, ETC2 RGBA and BC1.

  They are committed as fixtures with the exact command lines and the commit
  hash.
- **Transcoders:** integer code, so the tests require **bit-exact** output.
- **UASTC encoder:** bit-exact where upstream is deterministic, which is
  single-threaded and integer through the levels we port. Any block that
  differs must still meet the block's PSNR as upstream encodes it, within
  0.1 dB, and the test reports the count of differing blocks. If a level turns
  out to rely on float ordering that can't be reproduced, the test for that
  level drops to the quality bound and the note records why.
- **wasm32:** `cargo check` in CI, plus the reference tests run under
  `wasm32-wasip1` if a runner is cheap to add.

### 5.4 Speed budget

- A 256² tile with its mips is about 87k texels, or 5,461 blocks.
- UASTC level 0–1 runs at roughly 3–10 Mtexel/s per native thread *(to
  measure)*, which is 10–30 ms per tile.
- wasm runs 1.5–3× slower.
- The transcode is a few ms.

That fits a decode worker, where today's JPEG decode plus reprojection already
takes several ms. A slow encode only delays a tile, which the ancestor climb
already covers (§8.1).

## 6. Integration: decode workers, wire, upload

### 6.1 The target format is the host's

- The device decides the plan (§4.4) once, at `bring_up`.
- **Native:** the plan goes to the runtime, through the viewer's profile/config,
  before the first load.
- **Web:** the page owns the device, and the geometry worker and decode
  workers don't. The page sends the plan to the geometry worker in the same
  host message family as `Retarget` (D-A6). The geometry worker passes it with
  each `ImageryDecode` job.
- A plan change (device lost, re-created on another adapter) sends the message
  again. Tiles already resident keep the format they were made with.

### 6.2 Core types (texels, not encoders)

`DecodedTexture { width, height, rgba8 }` becomes
`DecodedTexture { width, height, texels: Texels }` with:

- `Texels::Rgba8(Vec<u8>)`: today's form. The GPU path and the uncompressed
  path keep it.
- `Texels::Blocks { format: TextureFormat, levels: Vec<Vec<u8>> }`: the CPU
  path's output, with the mips already encoded.

`TextureFormat` is the one the budget already has (`budget/cost.rs:65`), grown
to name the concrete formats (BC7, BC1, ASTC 4×4, ETC2 RGB8/RGBA8) while keeping
its 16-byte/8-byte block arithmetic. Core gains no dependency. Film reads
`Texels::Rgba8` only and is handed exactly the bytes it has today (§8.4).

### 6.3 The decode job (`tuile-planetary/src/jobs.rs`)

- `ImageryDecode` gains `target: Option<BlockFormat>`. `None` gives today's
  output, byte for byte.
- With `Some(f)`, the job decodes, reprojects and checks for opaque black
  (unchanged). It then builds the mip chain in the worker, moving it off the
  uploading thread, and encodes each level to UASTC and transcodes it to `f`.
- The output is serialised as a new tagged layout: the format byte, then
  w, h, flags and the level lengths.
- **If the encode fails or overruns** its per-tile time cap (an env-tunable
  value, not a switch), the job returns `Texels::Rgba8`. The tile is then drawn
  uncompressed, and it is never missing (§8.1).
- Native pool and web workers run the same `jobs::serve`, so one
  implementation covers both. The web inline fallback (`decode.rs:68`) runs it
  too.

### 6.4 Wire and upload

- `tuile-web/src/wire.rs` `texture()`/reader gain the format tag and the levels.
  The page receives blocks, 4× smaller to post across the worker boundary.
- `prepare.rs` `upload_texture` branches on `Texels`:
  - `Blocks` creates a texture of that format and makes one `write_texture`
    per level, with block-row strides;
  - `Rgba8` under a `Gpu(f)` plan writes to the encoder's scratch, queues the
    encode into the frame's compute pass and copies into an `f` texture;
  - `Rgba8` under `Uncompressed` takes today's path.

  The bind group layout doesn't change: a `texture_2d<f32>` filterable binding
  accepts any filterable format, so `PreparedTile::dress` and the shader are
  untouched.
- glTF base-colour textures take the same path once it exists. That is a
  follow-up, not this item.

## 7. Budget: real bytes

The budget code itself doesn't change. Its **cost inputs** do:

- `DrapeContent::cost` (`content.rs:223`) and `DecodedTileContent::cost`
  (`content.rs:161`) stop hard-coding `Rgba8`.
- **CPU path:** the format comes from `Texels::Blocks`. Then
  `gpu = drape_cost(w, h, Block16|Block8).gpu` and the staging is the blocks'
  staging. Both drop 4× or 8×.
- **GPU path:** the texels are RGBA, but the resident texture is `f`. The
  runtime holds the host's plan (§6.1) and passes it to `cost`. Then
  `gpu = drape_cost(.., f).gpu` (resident) and
  `staging = drape_cost(.., Rgba8).staging` (what crosses the bus this frame).
  `Cost { gpu, staging }` already separates the two, which is what makes this
  work.
- **Drape billing (49fc6e4):** the drape's charged share (`ppm` of the whole)
  multiplies the true whole, which is now the compressed one. The rule doesn't
  change.
- **Drift check:** a test charges a drape, uploads it on a headless device and
  compares the cost to the measured `GpuImagery.bytes`, within the 5 %
  `within_drift`. This makes estimate and reality agree by test rather than by
  hope. The pump's ack (`pump.rs:537`) keeps using `cost().gpu`, which becomes
  right.
- **The GPU encoder's scratch** (the RGBA scratch plus the block buffer for one
  frame's batch) is a fixed transient allocation. It is sized from the pump's
  per-frame upload pacing and is reported as a fixed overhead, not per tile.
- The **budget agents** (A1, A3) work in `tuile-core/src/budget`. Nothing here
  edits that directory except, if needed, extending `TextureFormat`'s variants
  in `cost.rs`. That is coordinated through the main session before it lands.

## 8. Never black ground, and what must not move

### 8.1 Black ground

- **Encode failure** (CPU) gives an RGBA tile, never a missing one (§6.3).
- **GPU path failure:** the encode is wrapped in a validation error scope.
  If it fails, the tile is re-uploaded as RGBA in the next frame, and the
  fallback texture is created before the failed one is dropped.
- **Slow encode:** a tile still in a worker is a tile not yet resident. The
  consumer's climb to the nearest resident ancestor covers it, exactly as it
  covers a slow network fetch today. Nothing here refuses to draw a fallback.
- **Swap order:** unchanged, new before old. A compressed texture is created,
  written, encoded and copied in the same submission that first binds it, and
  the replaced tile is released only after.
- **Test:** `no_black_ground.rs` and `fast_zoom_never_empties.rs` run under
  each plan the test machine can produce (`Gpu(Bc7)`, `Cpu(Bc7)`, `Cpu(Etc2)` /
  `Cpu(Astc)` where the adapter has them, and `Uncompressed`). One more case
  injects an encode failure on every second tile.

### 8.2 No seams from compression

Block artefacts at tile borders could read as seams. Imagery edges already
clamp (`ClampToEdge`), and blocks never straddle tiles (sizes are multiples of
4). `no_seams.rs` runs under the compressed plans with its current thresholds.

### 8.3 Colour

Encoders measure error on the sRGB-encoded values, the same space the texels
are stored in, and the hardware decodes to linear exactly as it does for
`Rgba8UnormSrgb`. Radiometric calibration is film-side and untouched.

### 8.4 Film goldens

- Film never calls `upload_texture` and never runs the `ImageryDecode` job with
  a target.
- The `DecodedTexture` field change is a type change, not a byte change. Film's
  readers move from `.rgba8` to `Texels::Rgba8`.
- Every PR runs the film golden tests (`tuile-film-gpu/tests/headless.rs`,
  frozen PNGs) and requires them **byte-identical**.

## 9. Quality gate and measurements

### 9.1 Instruments

Add `tuile_core::raster::quality` (or a test-support module), pure Rust, wasm
clean, with:

- **PSNR** over RGB, and over Y;
- **ΔE2000** (CIEDE2000) per texel from sRGB through CIELAB D65, giving the
  mean, the p99 and the max.

The arithmetic for both is a few lines, and colour-space conversion is
reference math, not a numeric kernel (cf. "established crates for numerics").
If `palette` is acceptable as a dev-dependency, ΔE comes from it instead (⚑4).

### 9.2 Fixtures

Committed imagery tiles with clear licences:

- NASA Blue Marble / GIBS tiles (public domain);
- IGN BD ORTHO tiles (Licence Ouverte 2.0);
- three synthetic patterns: a gradient, a checker with edges, and an
  alpha-edged layer.

Bing tiles are never committed (⚑6).

### 9.3 Gates, per encoder and format, on every fixture tile and every mip level

| Format | PSNR RGB ≥ | ΔE2000 mean ≤ | ΔE2000 p99 ≤ |
|---|---|---|---|
| BC7, ASTC 4×4 | 38 dB | 1.0 | 4.0 |
| ETC2 RGBA8 | 36 dB | 1.5 | 5.0 |
| ETC2 RGB8, BC1 | 33 dB | 2.5 | 7.0 |

These are starting values *(to calibrate)*. The first PR that implements each
encoder measures it on the fixtures. It then sets the gate about 1 dB below the
measured worst tile, so the gate catches regressions without being a wish. A
GPU encoder test reads the texture back through a decode-to-RGBA render (the
hardware decoder), so the gate measures what is actually sampled.

### 9.4 Viewer measurements (PR 4)

Each measurement is taken before and after, on the same trajectory:

- headless viewer (`examples/wgpu-viewer/examples/headless`, the `control`
  verdict's `worst_resident_bytes`, `ImageryTextures::live()`), native Metal;
- `scripts/web-bench.py`, under WebGPU and WebGL2.

Recorded per run: GPU imagery bytes resident (live and by level), bytes moved
per frame, upload time per tile (CPU `write_texture` time plus GPU encode
timestamps), decode-worker time per tile, and frame time p50/p99.

**Visual check:** the same view, uncompressed and compressed, screenshots side
by side, a ΔE2000 map of the drape region, both opened before the PR asks for
review.

### 9.5 Expected gains *(to measure)*

256² tile with its full mip chain:

| | RGBA8 | BC7 / ASTC 4×4 / ETC2 RGBA | BC1 / ETC2 RGB |
|---|---|---|---|
| Resident GPU bytes | 349,524 | 87,408 (4×) | 43,704 (8×) |
| Staging per tile, CPU path | 360,192 | 90,368 | ≈45 k |
| Staging per tile, GPU path | 360,192 | 360,192 (+ fixed scratch) | 360,192 |
| Web worker → page post | 262,144 (level 0 only) | 87,408 (all levels) | 43,704 |

At a fixed budget, the imagery share holds 4× (or 8×) as many tiles, which
means a sharper ground at the same memory. Upload bandwidth drops 4× on the
CPU path. On the GPU path it stays the same, and the encode adds about
0.05–0.2 ms of GPU time per tile *(to measure)*.

## 10. Order of the PRs

1. **This note.**
2. **tuile: GPU compute encoder (BC7 modes 6/1/5, BC1), `texcomp::select`, the
   quality instruments and fixtures.** Built in `tuile-wgpu`, not wired into
   upload yet. Tests: round trip through hardware decode within §9.3, the
   capability matrix, and a forced-backend GL run.
3. **`lapoule-dev/texcomp`: the port, split into several PRs in that
   repository.**
   - 3a: skeleton, CI, licence/NOTICE, reference vectors, UASTC unpack and
     UASTC → ASTC;
   - 3b: UASTC → BC7, ETC1/ETC2 + EAC, BC1;
   - 3c: the UASTC encoder, levels 0–2.

   The main session tags `v0.1.0` after 3c.
4. **tuile integration**: `Texels`, the job target, the wire, upload, device
   features, the cost inputs, the measurements and the visual check. May split
   into 4a (native) and 4b (web) if the diff passes about 800 lines.
5. **tuile: GPU ETC2 and ASTC kernels**, which move iOS, Android and mobile
   WebGPU off the CPU path.

PR 2 and the 3x PRs don't depend on each other and proceed in parallel. PR 4
needs PR 2 and `texcomp` v0.1.0.

## ⚑ Open questions for Laurent

1. **Repository name.** The proposal is `lapoule-dev/texcomp` (crate
   `texcomp`), private. Alternatives: `blockcomp`, `gpu-texcomp`. Until the
   name is settled, work proceeds under `texcomp` as the working name, and the
   repository is not created until it is confirmed.
2. **The 8× tier.** Should opaque tiles take BC1 / ETC2 RGB8 (4 bpp, about
   5 dB below BC7) to halve again, or does BC7/ASTC (4×) stay the only tier?
   The proposal is 4× only, with the 8× tier left for the budget to pick under
   pressure later. As a quality tuning it could be an env value, not a switch.
3. **ETC1S.** The decision named "UASTC/ETC1S". Nothing is transmitted, so
   ETC1S only costs quality and encoder time. The proposal is UASTC only, with
   ETC1S left out unless you want a stored or transmitted form later.
4. **ΔE source.** Hand-written CIEDE2000 in a test-support module (small,
   reference formula), or `palette` as a dev-dependency?
5. **Metal and compute passes.** The render-pass leak (gfx-rs/wgpu#8768): is it
   known whether compute passes leak too? The plan batches one compute pass
   per frame either way. PR 2 measures it.
6. **Fixture imagery.** NASA (public domain) + IGN BD ORTHO (Licence Ouverte)
   + synthetic patterns. Is that acceptable, and is there an orthophoto set
   you prefer?
