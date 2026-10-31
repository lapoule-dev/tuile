<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Compressing 3D Tiles content textures on the GPU — a study

Status: study, 31 Oct. 2026. Nothing here is built. It is placed **after
the compression budget (CB1–CB10)** and after the 3D Tiles work parked
behind it (#173, "one ledger for the whole scene"); §6 says why.

The question (Laurent): *a 3D Tiles tile served uncompressed — Google's
photorealistic tiles are glb with JPEG textures — is decoded to RGBA8 and
held as RGBA8. How do we "recompress" it so that the GPU holds BC7 / ASTC
(or ETC2) instead?*

Short answer: **almost all of the machinery exists**, because the imagery
path was built on a seam that does not care where a texture came from. A
content texture marked with the consumer's plan would already be encoded by
`upload_texture` and paced by the pump. What is missing is not the encoder
but everything around it that imagery never needed: arbitrary sizes, atlases,
glTF samplers, unused images, textures 16× larger than a 256² drape tile,
and a budget that can pace a single encode longer than a frame. That last
point is the whole reason it waits for CB.

Scope: the interactive consumers (tuile-viewer native and web, tuile-bevy,
a game embedding tuile-wgpu). Unchanged rules: compression is the client's
choice, `Off` by default in the API and byte-identical; nothing in the bake
or the film; never black ground; `tuile-core` wgpu-free and wasm-clean
(`gpu-compressed-textures.md` §1).

## 1. What the imagery path does today, and what of it carries over

### 1.1 Entry points and choice of format

| Step | Where | What |
|---|---|---|
| Caps | `crates/tuile-wgpu/src/compress.rs:39-58` | `GpuTextureCaps { bc, astc, compute }` from the adapter's features, downlevel flags and limits (`compute` = texcomp's kernels run here; false on WebGL2). |
| Choice → plan | `crates/tuile-core/src/texels.rs:331-340` (`select`) | compute+BC → BC7 on the GPU; compute+ASTC → ASTC 4×4 on the GPU; ASTC without compute → ASTC on the CPU; BC without compute → BC7 on the CPU; else RGBA8. `Off` → RGBA8 whatever the device. |
| Device features | `compress.rs:62-101` (`plan_features`, `plan_for`) | only the chosen format's feature, plus `TIMESTAMP_QUERY` when the GPU encodes. `Off` requests nothing. |
| Context | `crates/tuile-wgpu/src/context.rs:328-349` (`with_imagery_plan`) | a plan the device cannot sample falls back to RGBA8; a GPU plan creates the `Compressor`. |
| Marking | `crates/tuile-planetary/src/lib.rs:858` | the imagery loader marks each decoded tile `.planned(plan)` (`content.rs:328-333`): `Texels::Rgba8 { resident: Bc7 }`, so the budget charges the resident format before the GPU sees it. |
| Upload | `crates/tuile-wgpu/src/prepare.rs:595-609` (`upload_texture`) | `Blocks` → `upload_blocks`; `Rgba8` → CPU mip chain, then `encode` if marked for a block format, else `upload_rgba8`. |
| GPU encode | `prepare.rs:613-631`, `compress.rs:312-380` | texcomp `GpuEncoder::encode_with`, whole chain in one compute pass, submitted at once, timestamps around the pass where available. |
| CPU encode (WebGL2) | `texels.rs:430-461` (`encode_on_cpu`) | in the decode worker: mip chain, UASTC level "Fastest", transcode to ASTC 4×4 (lossless) or BC7. |
| Stored KTX 2.0 | `crates/tuile-core/src/stored.rs:53-80` | UASTC transcoded to the held format, or unpacked bit-exact to RGBA8 under `Off`; recognised by content, never by a setting. |
| Pacing | `crates/tuile-wgpu/src/pump.rs:499-520`, `compress.rs:184-216` | `EncodePace` (4 ms of GPU per frame) divided by ns/block; **the first encode of a frame always goes, however large** (`pump.rs:513-516`). |

### 1.2 Mips, sRGB, alignment, fallback

- **Mips:** built on the CPU by `raster::mip_chain` (`crates/tuile-core/src/raster.rs:1739-1758`) — triangle filter, `max(1, w/2)` per level, so non-power-of-two sizes already work — because GPU blits leak render passes on Metal (`prepare.rs:586-594`). The kernels encode each level.
- **sRGB:** every texture is sRGB: `Rgba8UnormSrgb` (`context.rs:39`), `texture_format(.., true)` for blocks (`prepare.rs:678`). One filterable `texture_2d<f32>` binding takes any of them (`shader.wgsl:126, 213`).
- **Block alignment:** level 0 must be whole 4×4 blocks. Anything else is refused: `upload_blocks` (`prepare.rs:647-653`), `encode_on_cpu` (`texels.rs:436-437`), and the GPU encoder returns an error, so `encode` gives `None` (`prepare.rs:623`). Smaller mips are copied at their physical block size (`prepare.rs:698-704`). The WebGPU spec imposes this for every compressed format: the texture's width and height must be multiples of the block size ([WebGPU, GPUTextureDescriptor validation](https://www.w3.org/TR/webgpu/#abstract-opdef-validating-gputexturedescriptor)).
- **Fallback:** an encode that cannot be done uploads RGBA8 (`prepare.rs:602-606`); blocks that cannot be held give the slate stand-in, never black (`prepare.rs:597-598`, `content.rs:384-391`); a drape is acked in the format each tile actually went resident in (`prepare.rs:508-511`).
- **Never black:** the texture is complete before anything binds it; the replaced resource is released after (`gpu-compressed-textures.md` §7).

### 1.3 What already reaches 3D Tiles content

More than expected:

- `prepare()` sends **every owned texture** through `upload_texture` (`prepare.rs:388-392`). A content texture marked `Rgba8 { resident: Bc7 }` would be encoded today, with RGBA8 fallback.
- The pump's encode accounting **already counts mesh textures**: `blocks_to_encode` sums `m.textures` marked for the encoder's format (`pump.rs:1088-1095`).
- `DecodedTileContent::cost` sums each owned texture's `cost()` (`content.rs:164-175`), which already reads the resident format off the texels.
- `PreparedTile` charges what was actually uploaded (`prepare.rs:393`), so a fallback is acked at RGBA8.

The one missing link is the marking: `content::decode` builds textures with `DecodedTexture::from_rgba8` (`content.rs:587`) and the tileset loader passes no plan (`crates/tuile-core/src/tiles3d.rs:143-155`). **A one-line `.planned(plan)` in the loader would "work"** — and would be wrong, for the reasons of §2 and §6. It is the trap of this subject.

### 1.4 Reusable as is / to adapt / new

| As is | To adapt | New |
|---|---|---|
| `ImageryCompression`, `select`, caps, device features | `Texels`'s sRGB-only assumption (normal/ORM maps need linear, §2.5) | an alignment step for sizes not multiple of 4 (§2.1) |
| `Compressor` + timestamps; `encode_on_cpu`; `stored::decode` | `EncoderScratch` sized for one 256² tile at setup (`compress.rs:258-259`): content textures are often larger | glTF samplers (§2.3) |
| the fallback rules and their tests' structure | the plan has to reach the tileset loader (`tiles3d.rs:154`) and the web decode workers | decoding only referenced images (§2.7) |
| `drape_cost` / `tile_blocks` formulas (any size, `div_ceil` per level, `cost.rs:78-99`) | the pacing unit: a whole tile with several large textures (§4.1) | atlas-aware pre-processing, if measurements ask for it (§2.2) |

## 2. How 3D Tiles textures differ from imagery tiles

Imagery is a world of 256² opaque tiles on a grid, clamped at the edge,
one per resource. glTF content is none of that.

### 2.1 Arbitrary sizes

glTF puts no constraint on image size. A texture whose level 0 is not a
multiple of 4 falls back to RGBA8 today — silently, as a `fell_back` count.
On a dataset where most textures are e.g. 1000×750 or 509×1023, "compression
on" would compress nothing. Two ways to make any size encodable:

1. **Pad** to the next multiple of 4 by replicating the last row and column,
   and scale the UVs by `w/W'`, `h/H'` (in the material uniform, or rewritten
   at decode). Texels stay exact. **Correct only with `ClampToEdge`** or with
   UVs inside [0, 1]: with `Repeat` the period becomes `W'`, and the padded
   column shows at every wrap.
2. **Resample** level 0 to the next multiple of 4 (≤ 3 texels larger on each
   axis, < 0.6 % at 509 texels). UVs and wrap semantics unchanged; every texel
   moves by a sub-texel resample (slight blur, comparable to one mip filter).

Resampling is the only one that is correct for every sampler; padding is
lossless only where the sampler clamps. The decision is a measurement
(§7, TX3): PSNR of each against the decoded JPEG on real textures, the
winner kept, the loser deleted (no switch). The resize must happen in the
core at decode (render-agnostic, wasm-clean), not in tuile-wgpu.

Non-power-of-two mips are already handled (`raster.rs:1753-1754`, wgpu's
`max(1, w >> l)` rule; `drape_cost` uses `div_ceil` per level, `cost.rs:84`).

For reference, KHR_texture_basisu requires KTX images to be multiples of 4
but not powers of two ("`pixelWidth` and `pixelHeight` MUST be multiples of
4"; "Exporters may opt to make the dimensions powers of 2" —
[KHR_texture_basisu](https://github.com/KhronosGroup/glTF/blob/main/extensions/2.0/Khronos/KHR_texture_basisu/README.md)):
a producer that writes basisu has already solved this; we only have to solve
it for JPEG/PNG sources.

### 2.2 Atlases: block bleeding and mip bleeding

Photogrammetry textures are atlases: many UV charts packed into one image,
with gutters between them and often unused (black or arbitrary) texels
around. Two separate effects:

- **Mip bleeding** exists today, in RGBA8: the CPU mip chain filters across
  chart borders, so at coarse levels a chart's edge mixes with its gutter.
  The usual remedy is padding/dilation that grows with the number of mip
  levels used ([a summary of the atlas-bleeding problem and its padding rule](https://bugnet.io/blog/how-to-fix-texture-bleeding-and-seams-in-an-atlas);
  [NVIDIA, *Improve Batching Using Texture Atlases*, 2004, "Using Mip-Maps with Atlases"](https://www.jmeiners.com/shamans/papers/graphics/texture_atlas.pdf)).
  The producer of the atlas usually dilates already; we do not know how much
  for Google's tiles (§7, TX0 measures it).
- **Block bleeding** is new with compression: a 4×4 block that straddles a
  chart edge is fitted with endpoints shared between the chart's colour and
  the gutter's. Our BC7 encoder uses modes 6 (one subset) and 1 (two subsets,
  64 partitions) only (`gpu-compressed-textures.md` §4), and our ASTC is
  single-partition: a block mixing a bright façade and a black gutter is
  exactly the weak case already measured for ASTC (bright points on black,
  36.4 dB, §4 there). The error lands on the chart's outermost 1–3 texels —
  where seams between charts are drawn.

The fix, if the measurement asks for it, is a **coverage-aware dilation**
before the mip chain and the encode: rasterise the mesh's UV triangles into
a coverage mask (the core has the UVs at decode), push-pull fill the
uncovered texels from the covered ones, then mip and encode. It helps the
RGBA8 path's mip bleeding too, so it changes `Off` pictures and must be
argued on its own merit, not smuggled in with compression. The quality gate
must be computed **on covered texels only** (a gutter's error is invisible)
and separately on a 2-texel border band of each chart, where seams are.

### 2.3 Samplers: wrap modes the engine ignores

The engine binds one global sampler: `ClampToEdge` on both axes, linear,
trilinear, anisotropy 16 (`context.rs:271-285`), chosen for draped imagery
seams. glTF's sampler defaults are `REPEAT` on both axes, and a material may
also choose `MIRRORED_REPEAT` or `CLAMP_TO_EDGE`
([glTF 2.0, samplers](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html#samplers)).
Content textures inherit the imagery sampler because the decoder keeps only
the image index (`content.rs:539-541`: `info.texture().source().index()`;
the texture's sampler is dropped) and reads `TEXCOORD_0` only
(`content.rs:527-529`).

For compression this matters in one place: the alignment choice of §2.1
(padding breaks `Repeat`). It is a correctness gap independent of
compression, to fix first (§7, TX2): a sampler per distinct glTF sampler
(at most 3 × 3 wrap pairs × filter choices, cached on the context), chosen
per material's bind group (bind groups are per material already,
`prepare.rs:781-849`).

### 2.4 Alpha

`content.rs` ignores `alphaMode` (the compatibility audit lists it). For
compression:

- **`OPAQUE`** (photogrammetry, Google's tiles as far as anyone renders them
  opaque): the alpha channel must be ignored by the encoder — the texcomp
  BC7 kernel keeps opaque alpha at 255 and spends no bits on it; ASTC uses
  CEM 8 (opaque). A JPEG source has no alpha anyway (`content.rs:567-572`
  fills 255).
- **`MASK` / `BLEND`**: BC7 and ASTC 4×4 both carry alpha at the same 8 bpp
  (texcomp's alpha gates: BC7 ≥ 55 dB, ASTC ≥ 50 dB on the synthetic tile,
  `gpu-compressed-textures.md` §8). A `MASK` cutoff is sensitive to alpha
  error near the threshold: gate it by "texels that flip side of the cutoff",
  not by PSNR.
- **BC1 / BC3** are not worth adding: BC1's 1-bit alpha and 4 bpp tier was
  dropped deliberately (÷4 only, `gpu-compressed-textures.md` §1); BC3 is the
  same 8 bpp as BC7 at lower quality
  ([Microsoft, *Texture Block Compression in Direct3D 11*](https://learn.microsoft.com/en-us/windows/win32/direct3d11/texture-block-compression-in-direct3d-11)).
  A 4 bpp tier (BC1, ETC2 RGB) would halve memory again on opaque
  photogrammetry; it reopens a decision of 24 Oct. and is not proposed here.

### 2.5 Normal and ORM maps (later)

Google's tiles carry base colour only (they are rendered unlit by design —
[Cesium community, *Google photorealistic 3D tiles and KHR_materials_unlit*](https://community.cesium.com/t/unreal-5-1-problem-with-google-photorealistic-3d-tiles-when-ignoring-khr-materials-unlit/25592)).
Other tilesets will carry normal and occlusion/roughness/metallic maps. These
are **linear**, not sRGB (KHR_texture_basisu requires
`KHR_DF_TRANSFER_LINEAR` for non-colour data, same source as §2.1), and
two-channel normals want BC5 / EAC RG11 / ASTC with a two-channel mode. The
seam today is sRGB-only (`texture_format(.., true)`); a texture's colour
space must become part of `Texels`/`ImageryFormat` before any non-colour map
is compressed. Out of scope until materials are decoded at all.

### 2.6 Quality: recompressing JPEG

A photogrammetry JPEG already carries 8×8 DCT artefacts; BC7 adds a second
lossy pass at 8 bpp. The imagery gates are a reasonable prior (BC7 worst
NASA tile 40.3 dB, ΔE2000 p99 5.35; ASTC 36.4 dB, 9.40 — §8 there), but
photogrammetry differs: hard façade edges, shadows, and atlas gutters
(§2.2). Expectation, to verify: BC7 ≥ 38 dB, ASTC 4×4 ≥ 34 dB on covered
texels against the decoded JPEG; the `select` preference for BC7 where both
exist carries over. The gate is computed the way §8 did it (GPU decode by
`textureLoad`, PSNR + ΔE2000 + a hole check), on an **openly licensed**
photogrammetry tileset fixture (Google's tiles may not be stored, §3.3).

### 2.7 Textures per tile, unused images, dedup

- `decode_glb` decodes **every image** in the glb (`content.rs:448, 455`),
  and `prepare` uploads every one (`prepare.rs:388-392`), referenced by a
  material or not. With compression each unused image would also cost an
  encode. Decoding only the images a drawn primitive's material references
  (and remapping `base_color_texture`) is a pure win under every plan,
  `Off` included.
- An image referenced by several materials is already decoded once (by
  image index). Two glTF textures over the same image with different
  samplers must still share one GPU texture (sampler ≠ texture).
- No sharing across tiles: each tile owns its atlas (`content.rs:77-84`).
- An unsupported image format fails the **whole tile** (`content.rs:585`;
  KTX2/WebP — "retried forever" per the audit). Under REPLACE that is a hole
  held open by the parent; a texture the consumer cannot read should become
  the slate stand-in on that material, the geometry still drawn (as
  `prepare.rs:597-598` does for blocks).

### 2.8 Size of the unit

A drape tile is 256², 5,463 blocks with its chain, ~9.5 ms of BC7 on an M2
(`gpu-compressed-textures.md` §9.1). A content texture is commonly larger,
and a tile may carry several. With `tile_blocks` (`compress.rs:169-171`):

| Level 0 | Blocks, with chain | RGBA8 held | BC7/ASTC held | BC7 GPU time at 1.74 µs/block | Frames at a 4 ms share |
|---|---|---|---|---|---|
| 256² | 5,463 | 349,524 B | 87,408 B | ≈ 9.5 ms | 2.4 |
| 512² | 21,847 | 1,398,100 B | 349,552 B | ≈ 38 ms | 9.5 |
| 1024² | 87,383 | 5,592,404 B | 1,398,128 B | ≈ 152 ms | 38 |
| 2048² | 349,527 | 22,369,620 B | 5,592,432 B | ≈ 608 ms | 152 |

(RGBA8 and block bytes from `drape_cost`, `cost.rs:78-99`; the per-block cost
is the measured 1.7–1.9 µs of §9.1 there. ASTC's assumed cost is about a
fifth, `compress.rs:177-182`, not yet measured in the viewer.)

**Today's pacing lets the first encode of a frame through however large**
(`pump.rs:513-516`): a single 1024² texture would be a ~150 ms frame, and a
tile is uploaded as one unit with all its textures. This is the core reason
the work waits for CB6 (resumable encode, "share + one tile") and **CB10
(split encodes over block-row ranges)**: for content textures "one tile" is
not a bounded amount of work, and only row-range splitting bounds it.

## 3. Where to recompress

### 3.1 (a) On the client at upload, GPU encode — the imagery path

The texture is decoded (JPEG → RGBA8) in the decode worker as today, marked
with the plan, carried as RGBA8, encoded by the GPU at upload.

- **Memory:** resident ÷4. Transient: the RGBA8 chain staged plus the
  blocks while the GPU encodes (§9.2 there: 349,524 + 87,408 per 256² tile;
  6.99 MB per 1024² texture), plus the encoder scratch, which must be sized
  for the largest admitted encode — the argument for row ranges (CB10) again.
- **Time:** GPU only, 1.74 µs/block BC7 (§2.8). CPU: the mip chain, which
  O-CB moves off the render thread; for content it belongs in the decode job
  that already runs `content::decode`.
- **Latency to first pixel:** each texture waits for its encode budget. A
  1024² BC7 texture at a 4 ms share is ~38 frames (0.63 s at 60 Hz). The
  parent tile (REPLACE) or nothing (ADD) covers the wait.
- **Variant (a′), RGBA8 first, swap to blocks later:** upload RGBA8 at once
  (first pixel as fast as `Off`), encode in the background, swap when done —
  new texture complete and bound before the RGBA8 one is released. First
  pixel unchanged; peak memory is RGBA8 + blocks for the life of the encode
  queue, which under pressure is exactly when it hurts. It is "re-encoding
  resident RGBA8", listed outside CB (§9.2 there). To decide by measurement
  (TX4), not by preference.
- **Determinism:** the kernels use float arithmetic; their output is not
  guaranteed bit-identical across GPU vendors. The budget's decisions do not
  depend on the bytes (only on block counts), so `decide` stays
  deterministic; pictures differ within the quality gate. The film never
  compresses.
- **Platforms:** every device with compute and BC or ASTC (`texels.rs`
  capability matrix): desktop, Apple, Android/Quest Vulkan, WebGPU.

### 3.2 (b) In the decode worker, CPU encode — the WebGL2 path

`encode_on_cpu` (`texels.rs:430-461`) in the worker that decoded the glb;
blocks cross to the page.

- **Memory:** blocks cross, so transfer, staging and resident are all ÷4;
  the worker's peak is RGBA8 chain + UASTC + blocks (CB7's
  `decoded_reserved` = workers × per-encode peak).
- **Time:** UASTC "Fastest" measured at 16–25 ms per 256² tile in desktop
  Chrome (§5.3 there): about 0.33–0.5 s for a 1024² texture with its chain,
  per worker, off the page's thread. Native would be faster but has the GPU
  path; this one is for WebGL2 only.
- **Latency:** adds the encode to the decode, per tile, before anything is
  drawn; no frame cost on the page.
- **Determinism:** pure-Rust integer code: identical output on every
  platform.
- **Quality:** UASTC level 0 then transcode: ASTC lossless from UASTC, BC7
  "within a small loss" (§4 there); below the GPU BC7 encoder.
- **Constraint:** no CPU encode on the geometry worker (CB7: no pool → the
  plan resolves to RGBA8 there, counted as a fallback). If content is decoded
  on the geometry worker, it must move to the decode pool first.

### 3.3 (c) Offline, KTX 2.0 / UASTC in the glb (KHR_texture_basisu)

The tileset is written once with its images as KTX 2.0 UASTC, referenced
through `KHR_texture_basisu`; the client only transcodes (`stored.rs`).

- **Memory:** blocks from the network on: transfer = resident = ÷4 of
  RGBA8, no transient beyond the file, no encoder scratch.
- **Time:** transcoding UASTC to BC7 or ASTC is block-for-block and cheap
  ("particularly fast and simple", [Basis Universal README](https://github.com/BinomialLLC/basis_universal));
  no GPU encode lane at all. Our transcode cost per block in tuile has not
  been measured yet — TX6 does.
- **Latency to first pixel:** the best of the three once bytes arrive.
- **Network:** worse. UASTC is 8 bpp before supercompression, several times
  a JPEG's size; KHR_texture_basisu allows UASTC with Zstandard
  supercompression and ETC1S with BasisLZ (same source as §2.1). texcomp's
  KTX 2.0 reader accepts supercompression scheme 0 only (texcomp v0.2.0,
  `src/ktx2.rs:27-28, 109-112`), and has no ETC1S. Reading real-world
  basisu glb therefore needs a Zstd decoder (a pure-Rust, wasm-clean crate)
  and, for ETC1S files, a transcoder we do not have. Writing our own
  tilesets, we choose UASTC + Zstd.
- **Quality:** the encoder runs offline, so it can take the slow, high
  quality levels (and RDO for size), unlike the worker's "Fastest".
- **Determinism:** total — the blocks are in the file.
- **Licence:** **only for tilesets we may store.** Google's Map Tiles API
  policies forbid pre-fetching, storing or caching content outside the
  agreement's limited conditions, and list offline uses among prohibited
  uses ([Map Tiles API policies](https://developers.google.com/maps/documentation/tile/policies)).
  Google's photorealistic tiles can only take (a) or (b). Own and open
  tilesets (photogrammetry we tile, pg2b3dm/py3dtiles outputs, Cesium-tiled
  data whose licence allows it — Cesium itself recommends KTX2 textures for
  runtime performance, [Cesium, *Tiling photogrammetry*](https://cesium.com/learn/3d-tiling/ion-tile-photogrammetry/))
  can take (c), naturally inside the "3D Tiles in PMTiles" step planned
  after the budget.
- **Under `Off`:** UASTC unpacks bit-exact to RGBA8 (`stored.rs`), so a
  consumer that did not opt in still draws it — but the pixels are the
  UASTC encode's, not the source JPEG's. A film reading such a store sees
  compressed pixels; the store's producer must keep the JPEG source for any
  film use (the "nothing in the film" rule applies to the store's content
  too). This repeats the PMTiles study's conclusion for imagery
  (`compressed-imagery-in-pmtiles.md`).
- **Where it is written:** not in tuile-bake (no compression in the bake);
  in the tool that writes the 3D Tiles store.

### 3.4 Comparison

Per 1024² texture with its chain, BC7 target:

| | (a) GPU at upload | (a′) RGBA8 then swap | (b) CPU in worker | (c) offline UASTC |
|---|---|---|---|---|
| Network | JPEG | JPEG | JPEG | UASTC (+Zstd): larger |
| Crosses worker → GPU | 5.59 MB RGBA8 | 5.59 MB | 1.40 MB blocks | 1.40 MB blocks |
| Resident | 1.40 MB | 5.59 → 1.40 MB | 1.40 MB | 1.40 MB |
| Transient peak | + 5.59 MB + 1.40 MB + scratch | RGBA8 + blocks held until swap | worker: RGBA8 + UASTC + blocks | file only |
| CPU | mip chain | mip chain | mip + UASTC encode (≈ 0.3–0.5 s wasm) | transcode only |
| GPU | ≈ 152 ms, spread by the lane | same, later | none | none |
| First pixel | after encode budget | as `Off` | after CPU encode | fastest |
| Quality | best (GPU BC7) | same | UASTC Fastest → BC7 | best (slow offline levels) |
| Determinism | per device | per device | bit-exact | bit-exact |
| Google tiles | yes | yes | yes | **no** (storage forbidden) |
| Platforms | compute + BC/ASTC | same | WebGL2 | all that sample BC7/ASTC; RGBA8 unpack otherwise |

ETC2: not proposed, as for imagery — every target with ETC2 also has ASTC
or BC (`gpu-compressed-textures.md` §1). WebGL2 phones expose ASTC
(`WEBGL_compressed_texture_astc`); a device with ETC2 only would get RGBA8.

## 4. What the budget must charge

### 4.1 The unit

A 3D Tiles content tile is one resource of class `Tiles3dMesh`
(`crates/tuile-core/src/budget/types.rs:21-30`): mesh and owned textures
together, charged by `DecodedTileContent::cost` (`content.rs:164-175`). That
is right for residency (a tile's textures live and die with it) and keeps
swap groups whole (CB1's rule). It is wrong as the **encode** unit, which
must be smaller than a frame: the lane admits the tile, then its textures'
block-row ranges are encoded over as many frames as the share allows (CB6
resumable progress + CB10 ranges), the tile becoming drawable — and acked —
only when every texture is complete, its parent drawn meanwhile.

### 4.2 Cost per path (CB2's fields applied to content)

CB2 already names `DecodedTileContent::cost` among the functions it
changes. For content, per owned texture `w × h`, summed with the mesh (whose
own extras are zero):

| Path | `gpu` (resident) | `transfer` | `transient` | `encode_blocks` |
|---|---|---|---|---|
| RGBA8 (`Off`, or fallback) | RGBA8 chain | = `gpu` | 0 | 0 |
| (a) GPU encode | blocks chain | RGBA8 chain | RGBA8 chain (+ blocks until done, CB4's rule) | `tile_blocks(f, w', h')` |
| (a′) RGBA8 then swap | RGBA8 chain, then blocks | RGBA8 chain | blocks chain for the swap | `tile_blocks` |
| (b) CPU encode | blocks chain | blocks chain | 0 on the GPU; worker peak in `decoded_reserved` | 0 |
| (c) stored UASTC | blocks chain | blocks chain | 0 | 0 |

`w', h'` are the aligned sizes (§2.1): the cost must be of what is
uploaded, so the alignment happens before the cost is computed, at decode.
Unused images (§2.7) are never decoded, so never charged. A tile whose
texture fell back is acked at RGBA8 for that texture — "a fallback ack is
not drift" (`gpu < actual ≤ transfer`, CB2) holds per tile because the
bound is a sum.

### 4.3 Reservations

CB4 sets `reserved[Drape]` from `EncoderScratch::bytes()` once at setup.
Content textures share the same encoder and scratch; the reservation must
cover the largest encode unit admitted. With CB10's row ranges that is a
fixed number of block rows; without them it would be the largest texture
ever seen (22 MB of RGBA8 for a 2048² one), which is not a reservation one
can make up front. One more reason for CB10 first.

### 4.4 Classes and the one ledger

The encode lane (CB5) is per frame, not per class: drapes and content
textures compete for the same GPU time, in the plan's order, without
overtaking. The "one ledger for the whole scene" step (#173's articulation,
plan of 31 Oct.) must have landed first, otherwise a tileset with its own
cloned `BudgetConfig` would run a second encode lane against the same GPU.

## 5. Memory to expect

Exact: resident texture bytes ÷4.000 for any size whose level 0 is a
multiple of 4 (from `drape_cost`); with resampling to the next multiple of
4, ÷3.98 or better at 509² and above. Per 1 Mpx of base texture: 5.59 MB →
1.40 MB, **4.19 MB saved**.

What that is per Google tile depends on numbers nobody has recorded: images
per tile, their sizes, their alignment, the share of textures in a tile's
resident bytes (mesh bytes are untouched). Public documentation does not give
Google's texture sizes or encodings (searches of Google's and Cesium's
documentation turned up nothing; [Photorealistic 3D Tiles overview](https://developers.google.com/maps/documentation/tile/3d-tiles-overview)
only says they are OGC 3D Tiles). TX0 measures them live, in the viewer's
own session, keeping counts and sizes only — no pixels or tiles stored.

## 6. Why after CB, not before

1. **The pacing would blow frames.** `EncodePace` lets the first encode of
   each frame through regardless of size (`pump.rs:513-516`); a 1024²
   content texture would cost ~150 ms in one frame, a tile with several
   more. CB6 (resumable encode, bound "share + one tile") and CB10 (row
   ranges, bound "share + one range") are the mechanisms content needs.
2. **The ledger would under-count four times over, at larger sizes.**
   `plan` caps on resident bytes while RGBA8 crosses (CB2), and the transient
   peak and scratch are uncharged (CB4). Imagery's error is 262 KB per tile;
   content's would be 4.2 MB per 1 Mpx texture.
3. **The imagery path is not accepted yet.** Its settle regression (5.0 s →
   10.2 s) is open until CB8. Adding a second, heavier producer of encode
   work before then makes both unmeasurable.
4. **The defaults flip in CB9.** Content compression should follow the same
   client choice, so it inherits CB9's defaults rather than inventing its
   own (one `ImageryCompression` governs all base-colour textures; no
   second switch).
5. **3D Tiles is parked until the budget closes** (#173, 31 Oct.), and the
   compatibility stack (unsupported required extensions refused, Draco,
   KTX2/WebP, materials, samplers) comes with it. TX1–TX2 below belong to
   that stack and can land there; compression (TX3+) only after.

Doing it before CB would mean writing a second pacing path beside the one
CB removes — exactly the debt the plan forbids.

## 7. Phased plan (after CB10 and the one-ledger step)

Rules as for CB: `Off` byte-identical (goldens, `det!` digests, film
goldens) except where a step is a correctness change stated as such; never
black; no switch; tests seen red first; every render opened; before/after
measured (M-sim, M-run on a Google flight and on an open fixture, M-web).

**TX0 — measure (instrument only).** An observer on the tileset loader
records per decoded tile: image count, referenced count, sizes, `% 4`
alignment, sampler wrap modes, `alphaMode`, `extensionsUsed`, texture bytes
vs mesh bytes. Written to the session's MCAP (no CSV), Parquet for the
analysis notebook. Counts only: no pixel, no tile kept (Google policy, §3.3;
Laurent to confirm that counting sizes in a live session is within the
"visualization" use). Output: the distribution table that sets TX3's choice
and the acceptance numbers of TX8. Also: an openly licensed photogrammetry
tileset chosen as the quality fixture.

**TX1 — decode only referenced images** (compatibility stack). Red: "a glb
with an unreferenced image decodes and uploads nothing for it"; "two
materials on one image share one texture". Under `Off`, content costs drop
where images were unused; goldens of fixtures that use all their images
unchanged.

**TX2 — glTF samplers and texture coordinates honoured** (compatibility
stack). A sampler cache on the context, per-material sampler binding;
`TEXCOORD_n` from `texCoord`. Red: "a `REPEAT` material samples across the
wrap, a `CLAMP` one does not" (rendered, opened). An `Off` picture change,
stated.

**TX3 — content textures follow the client's plan.** The plan reaches the
tileset loader (as `GlobeOptions::imagery_plan` does for imagery); the core
aligns level 0 to multiples of 4 (pad+UV or resample, chosen on TX0's data
and a PSNR comparison, loser deleted); the mip chain built in the decode job;
`.planned(plan)` on each content texture. Encoding through the existing
`upload_texture`, admitted by CB5's lane in CB10's row ranges, the tile
drawable after its last texture. Red: "a 1000×750 texture is held as BC7,
not RGBA8"; "a 2048² texture never puts more than share + one range on a
frame"; "the parent is drawn every frame until the child's last texture is
complete"; "a failed texture is RGBA8, the tile drawn, acked as such";
`no_black_ground` and `fast_zoom_never_empties` on a tileset scenario under
Off / BC7 / ASTC.

**TX4 — (a) vs (a′).** Both measured on the Google flight: first pixel,
peak (peak + reserved + settling ≤ hard), every-upload-landed. Keep the
winner. Expectation: (a) under memory pressure, (a′) gives a faster first
picture; it may be that (a′) only wins on headsets.

**TX5 — atlas quality.** The quality gate on covered texels and chart
borders (§2.2, §2.6) on the open fixture, GPU-decoded as in §8 there.
Coverage-aware dilation only if the border band fails the gate; if kept, its
`Off` effect argued separately.

**TX6 — WebGL2 and stored.** CPU encode for content in the decode workers
under CB7's reservation. `KHR_texture_basisu` read: UASTC (+ Zstd) through
`stored::decode`, the core `source` as fallback when present, the slate
stand-in when not — never a failed tile. Transcode cost per block measured.

**TX7 — offline writing**, inside the "3D Tiles in PMTiles" step, for
tilesets whose licence allows storage only: UASTC + Zstd at a slow quality
level, written by the store's producer, never by tuile-bake, source JPEG
kept for the film.

**TX8 — acceptance** (numbers provisional, fixed from TX0 the way §9.2 set
imagery's):

| | Target |
|---|---|
| Resident texture bytes per tile, BC7/ASTC | ≤ 0.26 × `Off` (÷4 minus alignment) |
| Textures held as blocks, of those the plan targets | ≥ 99 % (fallbacks counted, reasons recorded) |
| GPU encode per frame | ≤ share + one row range, every frame (asserted in sim and M-run) |
| Frame p99 | ≤ `Off` + 1 ms (60 Hz desktop); headset profile asserted at 90 Hz |
| Every upload landed, Google flight | ≤ `Off` × 1.2 |
| Quality, covered texels vs decoded JPEG | BC7 ≥ 38 dB, ΔE2000 p99 ≤ 6.5; ASTC ≥ 34 dB — each reset 1 dB under the worst measured |
| Holes, black patches | 0 on every run, native and web, filmed on Laurent's phone |

## 8. Open questions for Laurent

1. Counting texture sizes of Google's tiles in a live session (TX0): within
   the "visualization only" use of the Map Tiles API policies?
2. One `ImageryCompression` for drapes and content textures (proposed), or
   a separate client choice for content?
3. A 4 bpp tier for opaque photogrammetry (BC1 / ETC2 RGB / ASTC 6×6) — the
   24 Oct. "÷4 only" decision stands unless reopened.

## Sources

- [KHR_texture_basisu](https://github.com/KhronosGroup/glTF/blob/main/extensions/2.0/Khronos/KHR_texture_basisu/README.md) — multiples of 4, ETC1S/BasisLZ and UASTC/Zstd, colour spaces, fallback `source`.
- [glTF 2.0 specification, samplers](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html#samplers) — wrap modes, `REPEAT` by default.
- [WebGPU, GPUTextureDescriptor validation](https://www.w3.org/TR/webgpu/#abstract-opdef-validating-gputexturedescriptor) — compressed sizes in whole blocks.
- [Microsoft, Texture Block Compression in Direct3D 11](https://learn.microsoft.com/en-us/windows/win32/direct3d11/texture-block-compression-in-direct3d-11) — BC1/BC3/BC5/BC7.
- [Basis Universal](https://github.com/BinomialLLC/basis_universal) — UASTC transcoding to BC7/ASTC.
- [Map Tiles API policies](https://developers.google.com/maps/documentation/tile/policies) — no storing or pre-fetching, no offline use.
- [Photorealistic 3D Tiles overview](https://developers.google.com/maps/documentation/tile/3d-tiles-overview).
- [Cesium, Tiling photogrammetry](https://cesium.com/learn/3d-tiling/ion-tile-photogrammetry/) — KTX2 recommended for runtime performance.
- [Cesium community, Google tiles and KHR_materials_unlit](https://community.cesium.com/t/unreal-5-1-problem-with-google-photorealistic-3d-tiles-when-ignoring-khr-materials-unlit/25592).
- [NVIDIA, Improve Batching Using Texture Atlases (2004)](https://www.jmeiners.com/shamans/papers/graphics/texture_atlas.pdf); [atlas bleeding and padding](https://bugnet.io/blog/how-to-fix-texture-bleeding-and-seams-in-an-atlas).
- In the repo: `docs/explorations/gpu-compressed-textures.md` (§4, §5, §8, §9), `docs/explorations/graphics-budget-roadmap.md`, `docs/explorations/compressed-imagery-in-pmtiles.md`; the budget plan of 31 Oct. (CB1–CB10, the 3D Tiles parking note).
