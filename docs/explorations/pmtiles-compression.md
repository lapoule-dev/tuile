<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# PMTiles compression in the tile store — a study

What should the tile store's PMTiles archives compress, and with what? Three
things are at stake: bytes at rest, bytes on the wire (egress, and the
latency of a first request), and the decode cost on every target tuile runs
on (browser and wasm, desktop, phone, headset, GPU film farm). **Nothing here
is a decision.** The code was read, not changed, and the numbers come from
archives already on this laptop. Scripts are in `lab/pmtiles-compression/`.
Data and results stayed in a scratch directory and are not committed.

**Short answer.**

- **Terrain** is stored uncompressed today. It should be stored **gzip**:
  47 % smaller, and every target decodes it natively or almost for free.
  This first needs the readers to honour the archive's `tile_compression`,
  which none of them does yet.
- **Imagery** stays the **source JPEG, byte for byte**. At matched quality,
  re-encoding a JPEG that is already lossy buys little. It is also a derived
  work of commercial imagery, and it breaks the pack v3 digests.
- **Directories** stay gzip.
- **Dedup** buys nothing: 0 duplicates within an archive, under 1 % across a
  zone's epochs.
- **GPU texture formats** are a load-time question, not a storage one.
- **The container as a whole** (section 6) is a different question.
  - **Packs** are where it pays. A per-block re-encoding inside the pack
    (pack v4) halves an embedded pack: textures go from PNG to lossless
    WebP, and meshes from LZ4 to zstd with a dictionary. Random access
    stays, and the farm's ≈ 2.4 GB download per task drops to ≈ 1.1 GB.
  - **PMTiles archives** need no whole-file compression once their tiles
    are compressed: 0.3–0.5 % is left. A whole-file zstd/xz is for **one**
    use, a zone shipped to a phone as a unit.
  - Never set `Content-Encoding` on a stored `.pmtiles` or `.tuilepack`
    object: Cloudflare then ignores ranges.

## 1. What the code writes today

### Header and writer

All archives are written by `tuile-tile-server`'s `archive::write_in`
(`crates/tuile-tile-server/src/archive.rs:55-102`), over the `pmtiles` crate
0.24 (lapoule-dev fork `tuile-accessors`, `Cargo.toml:125-131`).

| Field | Value | Where it comes from |
|---|---|---|
| `tile_type` | from the catalog: `jpeg` for imagery, `other` (Unknown) for terrain, `unknown` for the `.absent` siblings | `catalog.rs:66-79`, `catalog.rs:113-121`; the writer at `archive.rs:76` |
| `tile_compression` | `none` for every layer of the production catalog | `catalog.rs:84-87` parses it, `archive.rs:77` sets it; `services/stl-tiles/catalog.json` in the deploying repo |
| `internal_compression` | **gzip**, never set by tuile: it is the crate's default (`GzipCompressor::default()`, flate2 level 6) | pmtiles `writer/mod.rs:120`; tuile calls no `internal_compression(...)` |
| `clustered` | true: tiles go in by increasing tile id, and anything out of order is refused | `archive.rs:84-96` |
| bounds, center | unset (whole world, 0/0). Only the zooms are set | `archive.rs:78-80` |
| metadata | `{name, tuile:grid, tuile:layer, tuile:epoch}`, 82–89 bytes gzipped | `archive.rs:66-72` |

Tiles go in through `add_raw_tile` (`archive.rs:96`). Bytes are stored
**exactly as the source delivered them, never recompressed**
(`archive.rs:6-7`, `layer.rs:39-40`).

The terrain source (the ion connector) does deliver gzip, but as HTTP
`Content-Encoding: gzip`, which the transport undoes. The store therefore
holds **raw quantized-mesh** (`tuile-cesium-ion/src/terrain.rs:67-70`).

Since PR #42, `tuile_terrain::decode` refuses to guess gzip from the first
bytes (`crates/tuile-terrain/src/decode.rs:146-157`). A raw tile's first
bytes are an f64, and one tile in 65 536 begins with `1f 8b`. So the
encoding has to come from the container: the archive header or the HTTP
header.

### Directories, dedup and leaves

The crate writes the root alone when it fits: under 16 384 entries and
16 257 compressed bytes. Otherwise it writes leaves of at least 4 096
entries, growing ×1.2 until the root fits (pmtiles `writer/mod.rs:380-416`).
Dedup works on an XxHash3 of the bytes, with run-lengths for identical
consecutive ids (`writer/mod.rs:298-350`).

A reader's first request is 16 KiB: the header plus the root
(`tuile-repository/src/archives.rs:43`, `:483`). It caches 256 parsed leaves
(`:50`).

### Payloads per layer

| Layer | Grid | Payload | Mean tile in the sample |
|---|---|---|---|
| `asset-1` terrain | geographic, zone level 9 | quantized-mesh 1.0 with extensions 1 (oct normals), 2 (water mask) and 4 (metadata) on **every** tile (2 447/2 447) | 19.5 KB (6–47 KB by zone) |
| `asset-2` imagery | web-mercator, zone level 10 | the commercial source's JPEG, 256², 4:2:0, baseline, not Huffman-optimised. Estimated quality ≈ 90 at levels ≤ 12 and ≈ 79 at levels ≥ 13 | 7–30 KiB |
| `asset-3954` imagery | geographic, zone level 9 | JPEG (not present locally, not measured) | — |
| `*.absent` | as its layer | a one-byte marker per absent tile (`catalog.rs:9-16`). The source's "no imagery" placeholder becomes an absence, never a tile (`tuile-bing/src/lib.rs:88`, `:299`) | 1 B |

Film packs are a separate container and not PMTiles: a FlatBuffers table
plus **LZ4** blocks, chosen for GB/s decode on the render path
(`crates/tuile-pack/Cargo.toml:24-27`, `src/lib.rs:28-34`). A v3 pack refers
to store tiles by `(level, x, y, FNV-1a digest of the bytes as the store
held them)` (`tuile-pack/src/lib.rs:96-104`). The film checks that digest
before using a tile (`tuile-film/src/from_store.rs:54-63`).

### Where decoding happens

| Target | Directories | Terrain bytes | Imagery bytes |
|---|---|---|---|
| Native server, bake, film-native (`tuile-tile-server`, pmtiles async reader) | gzip (brotli and zstd too, behind the crate's features) | as stored, `tuile_terrain::decode` (CPU) | `image::load_from_memory` (`tuile-core/src/raster.rs:1519`, `tuile-film-native/src/render.rs:884`), libjpeg-free Rust decoder, CPU |
| Worker / wasm (`tuile-repository`) | **gzip or none only**: anything else is an error (`archives.rs:539-555`) | as stored | as stored |
| Interactive web viewer (`tuile-web`, wasm) | via `tuile-repository` | wasm, `tuile_terrain::decode` | wasm, `image` (`tuile-core` `decode_image`) |
| Film in the browser (`tuile-film-web`) | via `tuile-repository` | wasm | **the browser's `createImageBitmap`**, off-thread, into a GPU texture (`tuile-film-web/src/lib.rs:21-23`) |

**Finding: no reader honours `tile_compression`.** Neither
`tuile-repository` nor the tile server decompresses a tile by the header,
and `service.rs:296-350` answers stored bytes with only the layer's
`content_type`. Today this is harmless, because every layer is `none`. But
it means **a layer cannot be switched to `gzip` by the catalog alone**: the
readers must change first (section 5).

## 2. Data measured

The sample came from what was on disk (`find` over `~/Projects`,
`film-cache/`, `videos/` and the lab). The R2/S3 buckets were not touched:
local data was enough, and nothing was downloaded.

- **Terrain:** six complete z9 zone archives, 2 447 tiles and 47.8 MB of raw
  quantized-mesh at archive levels 10–19 (mostly 13–14). They were
  reassembled from the film block cache (`film-cache/tiles/8m/<size>/...`,
  8 MiB blocks), and `pmtiles verify` accepts them.
- **Imagery:** three archives, 110 tiles sampled, at most 6 per level, over
  20 (archive, level) groups and levels 5–19:
  - one `top` archive (levels 0–9);
  - the z10/503/335 zone (levels 10–19);
  - the largest archive of the lab's z10/513/377 zone (levels 15–19).
- **Dedup:** 21 archives of the lab's z10/513/377 zone (all its epochs), and
  the 11 archives above.

Tools: Pillow 13 (libjpeg-turbo, libwebp, libavif with aom/dav1d),
pillow-jxl-plugin (libjxl), basisu 2.50, jpegtran (libjpeg-turbo 3.2),
python-zstandard 0.25 (libzstd), brotli 1.2, zopfli. Quality is measured on
luma (PSNR-Y, SSIM-Y via scikit-image) and with SSIMULACRA 2 (the Python
port; libjxl's CLI is broken on this machine by a Homebrew library
mismatch). Times are native, single-threaded, Apple Silicon, per tile.

## 3. Results

### 3.1 Terrain (quantized-mesh): per-tile codecs

Each tile is compressed alone, since a tile is fetched alone. The zstd
dictionaries were trained leave-one-zone-out: each zone is measured with a
dictionary that never saw it. The table pools all 2 447 tiles.

| Codec | Size vs raw | Encode ms/tile | Decode ms/tile | Decode MB/s (output) |
|---|---|---|---|---|
| none (today) | 1.000 | — | — | — |
| gzip-1 | 0.553 | 0.40 | 0.053 | 369 |
| gzip-6 | 0.527 | 1.63 | 0.050 | 394 |
| **gzip-9** | **0.526** | 4.90 | 0.046 | 429 |
| brotli-5 | 0.489 | 0.92 | 0.107 | 182 |
| brotli-9 | 0.484 | 1.64 | 0.108 | 180 |
| **brotli-11** | **0.435** | 56.1 | 0.158 | 124 |
| zstd-3 | 0.541 | 0.12 | 0.030 | 646 |
| zstd-9 | 0.522 | 0.64 | 0.031 | 629 |
| zstd-19 | 0.496 | 4.47 | 0.039 | 497 |
| zstd-22 | 0.496 | 4.60 | 0.040 | 484 |
| zstd-19 + 16 KiB dict | 0.483 | 3.20 | 0.041 | 480 |
| zstd-19 + 64 KiB dict | 0.470 | 4.27 | 0.045 | 435 |
| zstd-19 + 110 KiB dict | 0.467 | 4.48 | 0.045 | 433 |

The same codecs on one zone (z9/510/378, 148 tiles), with zopfli added:

| gzip-9 | zstd-19 | **zopfli (5 iterations, gzip-compatible)** | brotli-11 |
|---|---|---|---|
| 0.512 | 0.486 | **0.461** (384 ms/tile) | 0.423 |

What this says:

- **Any compression halves terrain.** Most of the gain is already in gzip.
  Its decode cost, about 50 µs for a 20 KB tile, is noise next to
  `tuile_terrain::decode` and the upsampling.
- **brotli-11 is the densest**: 17 % smaller than gzip-9. That costs 56 ms
  of encode per tile (about 2 minutes for this sample on one core), and
  brotli-5/9 lose most of that edge.
- **zstd without a dictionary is not better than gzip on these tiles**
  (0.496 vs 0.526). Dictionaries close part of the gap (0.467), but they
  must be versioned, shipped to every reader and named in the archive. The
  PMTiles spec has no field for that.
- **zopfli** writes plain gzip that every gzip reader decodes, and lands 10 %
  under gzip-9. It is the "free" densifier for a compaction that has the
  time.

### 3.2 Imagery

Ratios are relative to the source tile of the **same level**. Each cell is
the median over a group's tiles, given as size/source and SSIMULACRA 2
(100 = identical; about 90 is visually lossless; about 70 is "high quality"
with visible differences under scrutiny). Levels are never pooled. A summary
across groups is given only as the median of the per-group medians.

| Group (level, est. quality) | jpegtran -optimize | JXL JPEG reconstruction | JPEG q75 | WebP q75 | AVIF q60 | JXL q75 | KTX2 ETC1S q128 | KTX2 UASTC rdo1 |
|---|---|---|---|---|---|---|---|---|
| top 5 (q90, 2.1 KiB) | 0.56/100 | 0.68/100 | 0.43/88 | 0.16/78 | 0.26/84 | 0.34/87 | 0.57/63 | 2.50/92 |
| top 7 (q90, 26 KiB) | 0.82/100 | 0.67/100 | 0.66/84 | 0.56/70 | 0.50/76 | 0.47/73 | 0.42/31 | 2.42/91 |
| top 9 (q90, 4.1 KiB) | 0.75/100 | 0.71/100 | 0.57/87 | 0.26/73 | 0.37/80 | 0.44/83 | 0.57/61 | 2.71/92 |
| zone A 11 (q90, 29 KiB) | 0.83/100 | 0.67/100 | 0.67/86 | 0.58/75 | 0.48/77 | 0.47/74 | 0.40/37 | 2.16/91 |
| zone A 12 (q90, 23 KiB) | 0.84/100 | 0.70/100 | 0.67/87 | 0.51/75 | 0.47/79 | 0.47/78 | 0.50/54 | 2.61/89 |
| zone A 13 (q79, 12 KiB) | 0.96/100 | 0.89/100 | 0.93/85 | 0.60/67 | 0.70/77 | 0.72/76 | 0.82/59 | 4.38/88 |
| zone A 16 (q79, 9.0 KiB) | 0.94/100 | 0.90/100 | 0.91/83 | 0.47/62 | 0.65/75 | 0.71/76 | 0.99/65 | 5.08/86 |
| zone A 19 (q79, 7.0 KiB) | 0.92/100 | 0.86/100 | 0.88/82 | 0.39/59 | 0.55/73 | 0.63/79 | 1.13/69 | 5.42/84 |
| zone B 15 (q79, 18.6 KiB) | 0.97/100 | 0.85/100 | 0.94/86 | 0.78/74 | 0.68/77 | 0.66/73 | 0.60/46 | 3.37/92 |
| zone B 17 (q79, 15 KiB) | 0.97/100 | 0.85/100 | 0.93/85 | 0.68/70 | 0.69/78 | 0.66/72 | 0.67/53 | 3.95/88 |
| zone B 19 (q92, 15 KiB) | 1.00/100 | 0.82/100 | 0.67/94 | 0.44/62 | 0.52/76 | 0.51/74 | 0.65/60 | 3.48/86 |

Median of the 20 per-group medians, with times:

| Variant | Size/src (range over groups) | PSNR-Y | SSIM-Y | SSIMU2 | Decode ms | Encode ms |
|---|---|---|---|---|---|---|
| source JPEG | 1.000 | — | — | 100 | 0.20 | — |
| jpegtran -optimize (lossless) | 0.930 (0.56–1.00) | ∞ | 1 | 100 | 0.20 | 7 |
| jpegtran -progressive (lossless) | 0.929 (0.73–1.02) | ∞ | 1 | 100 | 0.42 | 5 |
| **JXL JPEG reconstruction** (lossless, **110/110 bit-exact JPEG back**) | **0.850** (0.64–0.90) | ∞ | 1 | 100 | 0.73 to the JPEG, + 0.20 to pixels | 2.3 |
| JPEG q60 / q75 / q85 / q92 | 0.74 / 0.89 / 1.02 / 1.12 | 37 / 47 / 47 / 52 | | 66 / 86 / 89 / 93 | 0.19 | 0.4 |
| WebP q60 / q75 / q90 | 0.41 / 0.50 / 0.96 | 37 / 38 / 44 | | 62 / 69 / 85 | 0.4–0.7 | 4–5 |
| WebP lossless | 4.35 | ∞ | 1 | 100 | 1.23 | 62 |
| AVIF q40 / q60 / q75 (speed 6) | 0.25 / 0.58 / 0.82 | 35 / 43 / 48 | | 46 / 77 / 86 | 0.7–1.0 | 15–17 |
| JXL q75 / q90 | 0.65 / 0.91 | 41 / 48 | | 76 / 90 | 1.3–1.6 | 9–10 |
| KTX2 ETC1S q128 / q255 | 0.70 / 1.26 | 38 / 41 | 0.93 / 0.97 | 61 / 75 | transcode 0.07–0.30 | 130 |
| KTX2 UASTC rdo 1.0 / 3.0 | 3.98 / 3.45 | 46 / 40 | 0.99 / 0.96 | 89 / 76 | transcode 0.37–0.9 | 210 |

Transcode times for Basis (medians, per 256² tile, native):

| | → BC7 | → ASTC 4×4 | → ETC1 | → BC1 |
|---|---|---|---|---|
| ETC1S | 0.30 ms | 0.23 ms | 0.08 ms | 0.13 ms |
| UASTC | 0.45 ms | 0.37 ms | 0.91 ms | 0.61 ms |

What this says:

- **The source is already lossy, so re-encoding it is a generation loss for
  a modest saving.** At SSIMULACRA 2 ≈ 85–90 (near the source), the
  candidates weigh 0.82 (AVIF q75), 0.91 (JXL q90) and 0.96 (WebP q90) of
  the source. Below that, the size falls fast (WebP q75 at 0.50, AVIF q60
  at 0.58), but the score drops to 69–77: a visible softening of a picture
  the film is then asked to grade and match against references. The gain
  is small where the source is q≈79 (levels ≥ 13, the bulk of a film's
  bytes) and larger at the q≈90 upper levels, which are few.
- **Two lossless paths exist.**
  - jpegtran: −7 % median, up to −44 % on the small q90 upper tiles, ≈ −5 %
    at levels ≥ 13. Pixels are identical, but the **bytes change**.
  - JPEG XL's JPEG reconstruction: −15 % median, ≈ −11 to −15 % at levels
    ≥ 13. It gives the **original bytes back, verified on all 110 tiles**.
    That is the only way to shrink imagery that keeps the pack digests.
- **GPU formats are not a storage win.**
  - ETC1S matches the JPEG's size but scores 31–69, clearly worse than the
    JPEG it came from.
  - UASTC looks right (≈ 89) but is 4× larger, even with RDO and KTX2's
    zstd.
  - Their value is elsewhere: no CPU decode (0.1–0.5 ms of transcode instead
    of a 0.2 ms native JPEG decode, more in wasm), and **4–8× less GPU
    memory and upload bandwidth**. A 256² tile is 256 KiB as RGBA8, 64 KiB
    as BC7/ASTC 4×4, 32 KiB as BC1/ETC1, +⅓ with mips. That matters on a
    phone or a headset, where VRAM and bus are what run out.
- **gzip on a JPEG**: 0.984. Never compress a JPEG in the archive.

### 3.3 Directories (internal compression)

Rebuilt with each codec from the archives' own entries:

| Archive (entries) | Stored (gzip-6) root / leaves | none | gzip-9 | zstd-19 | brotli-11 |
|---|---|---|---|---|---|
| terrain zones (148–965) | 452–2 120 B root, no leaves | ×2.0–2.4 | −0.5 % | −1 to −3 % | −12 to −15 % |
| imagery zone (4 179) | 8 843 B root | 20 979 B | 8 763 | 8 668 | 7 292 |
| imagery zone (19 910) | 55 B root + 42 278 B in 5 leaves | 104 269 | 42 283 | 41 970 | 34 591 |

- Every archive seen opens in **one 16 KiB request**: header plus root,
  which is at most 8 970 bytes into the file. A 20 000-tile archive costs one
  more request, for a leaf of about 8.5 KB, that the reader then caches.
- Uncompressed directories would be **worse for latency**. The 4 179-entry
  root would grow to 21 KB, past the 16 257-byte limit, and the writer would
  then have to add leaves: a second request on every cold open.
- zstd saves 1–2 %, which is nothing. brotli saves 15–18 %, a few KB per
  archive, but neither the JS reader nor `tuile-repository` decodes it.
  Parsing a directory takes 0.02–0.4 ms in any codec.

### 3.4 Dedup and run-lengths

- **Within an archive:** across 11 archives and 53 135 entries, there is
  **one** duplicate payload and no run-length > 1. The absence sibling
  already takes everything a duplicate would have been: the "no imagery"
  placeholders and refused tiles.
- **Across a zone's archives:** in the 21 archives of z10/513/377 (66 587
  entries, 865 MB), 663 entries re-store a tile that an older archive holds
  with the same bytes. **Deduplicating would save 0.96 %.** No tile id
  carries two different payloads: in two months of epochs, the source never
  changed a tile here.
- Terrain tiles are never byte-identical, even over flat ground: the header
  holds each tile's own centre and bounds.

Dedup and the leaf layout are therefore not levers. The layout (Hilbert
order, leaves of ≥ 4 096 entries, a 16 KiB root) is already the one the spec
optimises for.

## 4. Constraints

**What the PMTiles spec allows.** `tile_compression` and
`internal_compression` take one of none, gzip, brotli or zstd. `tile_type`
takes unknown, mvt, png, jpeg, webp, avif or mlt. There is **no KTX2 and no
JPEG XL tile type**: either would be `unknown`, with the media type in the
metadata (as the catalog's `content_type` already does). There is no field
for a zstd dictionary.

**What readers support.**

- The reference JS reader decodes gzip, natively through
  `DecompressionStream("gzip")` and with fflate as a fallback. It **throws on
  brotli and zstd** unless the application supplies a decompress function.
- The Rust crate reads all four, behind features.
- `tuile-repository` reads gzip and none only (`archives.rs:539-555`).
- go-pmtiles writes gzip directories.

Gzip directories are the only choice every reader opens.

**HTTP `Content-Encoding` vs in-archive compression.** A PMTiles archive is
read by byte ranges.

- **Never** put a `Content-Encoding` on the `.pmtiles` object itself. Ranges
  would then address the compressed stream, or a CDN would decompress the
  object first.
- In-archive compression is invisible to HTTP. A 206 range response carries
  the stored, compressed bytes, and the client must decode them itself.
- When a server or a Worker cuts the tile out and answers it as its own URL,
  it can **pass the stored bytes through with `Content-Encoding: <the
  archive's tile_compression>`** and `Vary: Accept-Encoding`. The browser,
  or reqwest with the matching feature, then decodes them natively, at no
  wasm cost. The CDN caches the encoded bytes as is.
- Browser support:
  - `Content-Encoding`: gzip and br everywhere. zstd in Chrome ≥ 123,
    Firefox ≥ 126 and Safari ≥ 26.3; older Safari and iOS do not take it.
  - `DecompressionStream`, for the direct range path: gzip everywhere,
    brotli only in recent Firefox, zstd nowhere by default.

**Cloudflare R2 and Workers.**

- R2 serves ranges of an object as raw bytes.
- A Worker that returns a pre-compressed body must set the encoding itself
  (`encodeBody: "manual"` with the header), or Cloudflare may compress it
  again or strip the header.
- Cloudflare does not compress 206 responses, and does not compress media
  types outside its list (`application/vnd.quantized-mesh`, octet-stream).
  This follows Cloudflare's documented behaviour and was not checked against
  the deployed zone. If it holds, terrain is not compressed in transit today,
  whatever the client asks, and **uncompressed terrain costs its full size in
  egress today.**

**wasm binary size per decoder.** Only the Basis transcoder was measured:
489 KiB for the reference engine's emscripten build, all formats included,
in `references/`. The other figures are estimates from the crates and were
not built here.

| Decoder | Cost |
|---|---|
| gzip (`miniz_oxide`) | already linked through flate2: +0 |
| brotli (`brotli-decompressor`) | ≈ 130–150 KiB, its static dictionary alone is ≈ 120 KiB |
| zstd (`ruzstd`) | ≈ 60–100 KiB |
| WebP (`image-webp`) | ≈ 60–100 KiB |
| AVIF | no stable pure-Rust decoder in `image`; dav1d is C |
| JPEG XL (`jxl-oxide`) | several hundred KiB, pre-1.0. JPEG reconstruction is in `jxl-jbr` 0.1, marked unstable |

Rule 8 (stable, widely adopted wasm crates) rules JPEG XL and AVIF out of
the wasm path. It allows miniz_oxide, ruzstd and brotli-decompressor.

**Licence of commercial imagery.** Any pixel re-encoding of the commercial
aerial tiles (lossy JPEG, WebP, AVIF, JXL, KTX2) is **a derived work stored
and redistributed**. A lossless repacking (jpegtran, JXL reconstruction) is
arguably the same work in another container, but it is still a modification
of the delivered file. ⚠ Either needs the provider's terms reread before any
deployed layer: the internal memo "PoC on commercial imagery OK" covers
on-the-fly computation, not stored derivatives. Open ortho sources under
licences that allow derivatives (e.g. Etalab 2.0, CC-BY) can be transcoded
freely, with attribution.

**Pack v3 digests.** A pack names each store tile by the FNV-1a of its bytes
(`from_store.rs:54-63`). Changing how the store holds a tile's bytes renders
every pack baked against it `Renewed`, **unless the digest is taken over the
decoded payload**. That holds if the store reader decompresses before
handing bytes on, or if the codec gives the original bytes back, as JXL
reconstruction does.

## 5. Recommendation

### Per layer

| Layer | Write | Keep | Transcode where |
|---|---|---|---|
| Terrain | **`tile_compression: gzip`**, level 9 at publish. **zopfli** at compaction (another −10 %, still gzip) | — | Nowhere. Served with `Content-Encoding: gzip`, which is what quantized-mesh clients already expect from the wire |
| Imagery, commercial | `jpeg`, `none`, **source bytes untouched** | the source JPEG | Nowhere in storage. On the GPU at load where memory matters (below) |
| Imagery, open ortho | as delivered (JPEG/WebP). A derived **KTX2 ETC1S/UASTC sibling layer** only if mobile and headset make it worth it | the delivered file | at bake, as a separate layer |
| Directories | gzip (unchanged); set bounds and center in the header | — | — |
| `.absent` | unchanged | — | — |

### Per target

- **Browser (tuile-web, tuile-film-web).**
  - Terrain from a tile URL: the browser decodes `Content-Encoding: gzip`,
    off the wasm heap.
  - Terrain from a direct range read: `DecompressionStream("gzip")`, or
    miniz_oxide already linked. No new wasm.
  - Imagery: keep JPEG. film-web already decodes it with `createImageBitmap`.
    tuile-web could do the same instead of `image` in wasm. That is a decode
    change, not a storage one.
- **Desktop and film farm.** Everything as above, gzip through flate2. The
  JPEG decode (≈ 0.2 ms per 256² tile native) is not the bottleneck of a
  render. Packs stay LZ4.
- **Phone and headset.** The cost is GPU memory and upload, not bytes on
  disk. Decode the JPEG (native or `createImageBitmap`), then compress to
  BC7/ASTC/ETC2 **at load, in memory**. A compute-shader block encoder, an
  established one, belongs in the wgpu backend. A transient in-memory
  texture is a decode, not a stored derivative, so the licence question does
  not arise. Pre-baked KTX2 is for open sources only.

### Migration path

1. **Readers first.**
   - `tuile-repository` (`archives.rs`) and `TileStore::get`
     (`store.rs:743`) decode each tile by **its archive's header**
     `tile_compression`, not by the catalog's. gzip goes through flate2
     (already a dependency). brotli and zstd are optional, behind features.
   - The tile server and Workers pass the stored bytes through with
     `Content-Encoding` when the client accepts it, and decode otherwise.
   - The pack digest is computed over the decoded bytes, which is what the
     store hands to `is_baked`. Old packs then stay valid, because raw
     quantized-mesh is what they digested.
   - Ship this everywhere: server, bake, film-native, film-web, Workers.
   - A test: an archive written gzip and an archive written none must give
     the same tile bytes and the same digest. Then remove the decode and
     watch the test fail.
2. **Switch the catalog's terrain layer to `gzip`.** New archives are
   written gzip, old ones stay `none`: archives are immutable and the reader
   follows each header. This needs the writer to compress. The crate's
   `add_tile` compresses with the archive's tile codec (`tile_codec` takes a
   custom one, e.g. gzip-9 or zopfli). `add_raw_tile` stays for bytes
   already in the target encoding. A compaction must work in decoded bytes,
   so that a tile is never gzipped twice and never stored raw under a gzip
   header.
3. **Compaction rewrites old terrain archives as gzip**, or zopfli, which
   compaction has the time for. The bucket shrinks by about half as zones
   are compacted.
4. **Imagery: nothing.** Revisit JXL reconstruction (−15 %, bit-exact,
   digests preserved) once two things hold: a stable wasm decoder exists,
   and the provider's terms are read. It would be a cold-storage form
   transcoded back to JPEG at the edge, about 0.7 ms per tile native.

### Expected gains

| Change | Storage | Egress | Decode cost | Risk |
|---|---|---|---|---|
| Terrain gzip-9 | **−47 %** of terrain bytes | −47 % (no transit compression today) | +0.05 ms/tile, native in browsers | needs step 1 first |
| Terrain zopfli at compaction | −52 to −54 % (one zone: 0.461) | same | same as gzip | compaction CPU, ≈ 0.4 s/tile |
| Terrain brotli-11 (alternative) | −56.5 % | −56.5 % | +0.16 ms native; wasm decoder +≈140 KiB on the range path | JS reader and `tuile-repository` need work; not the first step |
| Terrain zstd-19 + dictionary | −53 % | −53 % | +0.05 ms; dictionary distribution | not in the spec; not recommended |
| Directories zstd / brotli | −1 % / −15 % of a few KB | ≈ 0 | — | breaks the JS reader: no |
| Imagery jpegtran | −7 % median (−5 % at levels ≥ 13) | same | ≈ 0 | bytes change: pack digests, licence |
| Imagery JXL reconstruction | −15 % median (−11 to −15 % at levels ≥ 13) | only if served as JXL, otherwise transcoded at the edge | +0.7 ms native; wasm not stable | licence, wasm; later |
| Imagery lossy re-encode (AVIF q75 / JXL q90) | −18 % / −9 % at SSIMU2 ≈ 86–90 | same | 1–1.6 ms native | generation loss, licence: no |
| Dedup across epochs | −1 % | — | — | not worth it |
| GPU block compression at load (mobile, headset) | 0 | 0 | +GPU encode at load | VRAM ÷4 to ÷8 |

## 6. Compressing the container itself

Sections 1–5 compress what is *inside* an archive. This section asks the
other question: compress the archive, or the film pack, **as a file**.

### 6.1 Where a container moves whole, and where it is read by ranges

| Use | Unit moved | Code |
|---|---|---|
| Farm job, pack in | the **whole pack**, ≈ 2.3–2.4 GB, as ranged GETs of 16 MiB parts, 16 in flight (≈ 150 GETs) | `tuile-farm/src/lib.rs:13-19` (88 s for 2.3 GB with one stream), `:142-156` (`part_bytes` 16 MiB × 16), `:360-404` (download in ordered parts); `job/config.rs:237` (`/tmp/scene.tuilepack`) |
| Pack integrity | FNV-1a over the **whole blob region as stored**, once per open: "seconds per gigabyte" | `tuile-pack/src/lib.rs:351-353`, `:779-790`, `:900-915` |
| Pack migration | the **table only**, by ranges: payloads never downloaded | `tuile-farm/src/bin/tuile-migrate-packs.rs:17-23` |
| Tile server disk cache | **whole archives** copied to local disk and mmapped: awaited if ≤ 4 MiB, in the background if larger, ranges above 256 MiB; 8 GiB budget | `tuile-tile-server/src/disk.rs:4-23`, `:42-58` |
| stl-tiles disk cache | the same mechanism, **12 GiB** | `stl-tiles/src/config.rs:56` (deploying repo) |
| Compaction | reads every tile of a zone's archives, writes one new archive (8 MiB upload parts) | `tuile-tile-server/src/store.rs:98`; `archive.rs:42-102` |
| R2/S3 at rest, backups | whole objects, immutable | — |
| Offline/mobile bundle (future) | a zone, whole | — |
| **Read by ranges** | | |
| Tile reads from the bucket | header + root (16 KiB), leaf, tile | `tuile-repository/src/archives.rs:43`, `:483` |
| Browser film and Worker | packs and archives in **fixed 8 MiB chunks**, each its own URL answered whole and cached at the edge and in the browser — "never a range" | `tuile-repository/src/cached.rs:13-16`, `:33-40`; `tuile-film-web/src/source.rs:36-47` |
| Browser film, a frame's tiles | `Pack::span_of` + `payload_in`: a tile's blocks are contiguous | `tuile-pack/src/lib.rs:960-1030` |

### 6.2 What is in the containers measured

- **Terrain archives:** see section 2. Two zones were measured whole, 10.3
  and 9.4 MB.
- **Imagery archive** z10/503/335, 32.5 MB. **8.0 % of its bytes are JPEG
  headers**: 623 bytes per tile, two distinct headers across 4 179 tiles,
  repeated in every tile.
- **Embedded packs** (`Content::Embedded`, v2): two one-frame packs of 17.2
  and 10.2 MB from a recent sky render, 57 and 47 tiles. These are the kind
  the farm downloads.
  - The table is 10 KB.
  - **PNG textures** (RGBA, 405–779 px) are **83–88 % of the blob**.
  - LZ4 mesh blocks are the rest: positions 4–6 %, indices 6–8 %, uvs 2–3 %.
  - Normals are empty.
- **Reference pack** (`Content::References`, v3): one frame, 874 tiles, all
  table, 474 KB.

No farm-size pack (2.4 GB) was on the laptop. The tuile buckets are not among
the configured credentials (the only R2 profile belongs to another account),
so nothing was downloaded. The farm numbers below are **extrapolated** from
the two embedded packs, on the assumption that a long film's blob has the
same mix of textures and meshes.

### 6.3 Whole-file compression

Ratio of the whole file. Encode and decode MB/s are native and
single-threaded.

| File | gzip-6 | zstd-3 | zstd-19 | zstd-19 long (2²⁷) | brotli-9 | xz-6 |
|---|---|---|---|---|---|---|
| terrain zone, tiles raw (today) | 0.52 | 0.50–0.51 | **0.43–0.44** | 0.43–0.44 | 0.45–0.46 | **0.38–0.39** |
| same, **tiles gzip-9** (section 5) | 0.996 | 0.996 | 0.995 | 0.995 | 0.995 | 0.996 |
| imagery zone (JPEG) | 0.918 | 0.921 | 0.916 | 0.916 | 0.916 | 0.914 |
| embedded pack (PNG + LZ4) | 0.77–0.79 | 0.78–0.80 | **0.65–0.66** | 0.65–0.66 | 0.71–0.73 | 0.65–0.66 |
| reference pack (table only) | 0.23 | 0.21 | **0.17** | 0.17 | 0.17 | 0.15 |
| decode MB/s (terrain raw) | 530–640 | 820 | 680–850 | 720–850 | 250–390 | 26–30 |
| encode MB/s (terrain raw) | 19–20 | 220–250 | 2.8–3.7 | 3.3–3.4 | 11–16 | 1.4 |

What this says:

- **Whole-file beats per-tile on raw terrain**, 0.43 (zstd-19) and 0.39 (xz)
  against 0.526 for per-tile gzip-9. Neighbouring tiles share structure that
  a per-tile codec cannot see.
- **Once tiles are compressed, nothing is left to a whole-file codec**:
  0.995–0.997. The two layers of compression do not add up.
- **Imagery's whole-file gain is exactly the repeated JPEG headers** (8.0 %
  of bytes, 8.4 % saved). No codec finds anything else in it.
- **Long-range mode adds nothing** at these sizes: nothing repeats beyond
  the default window.
- **A pack is the one container with a lot left in it**, 34–35 % with
  zstd-19. Section 6.5 shows where that comes from and how to get more while
  keeping random access.
- **A reference pack's table compresses 6×.** Such a table "can run to more
  than a hundred megabytes" (`tuile-pack/src/lib.rs:2289-2290`).

### 6.4 Seekable framings: random access kept

The file is cut into independently compressed frames of a fixed
uncompressed size. That is what the zstd seekable format does (zstd frames
plus a seek-table skippable frame, 8–12 bytes per frame), and what BGZF does
(gzip members of ≤ 64 KiB). The ratio counts the seek table. A "30 KB read"
is a 30 KiB span at a random offset: the compressed bytes fetched, as a
multiple of 30 KiB, and the native CPU to decode the frames it touches.

| File | Frame | zstd-19 ratio | BGZF-6 ratio | 30 KB read: fetch (×) | decode | index |
|---|---|---|---|---|---|---|
| terrain raw | 64 KiB | 0.49–0.50 | 0.53 | 46 KiB (×1.5) | 0.15 ms | 1.7–1.9 KB |
| terrain raw | 256 KiB | 0.47 | — | 133 KiB (×4.4) | 0.4 ms | 0.45 KB |
| terrain raw | 1 MiB | 0.45–0.46 | — | 475 KiB (×16) | 1.3–1.6 ms | 0.1 KB |
| terrain raw | 8 MiB | 0.43–0.44 | — | 2.0–2.2 MiB (×67–75) | 6–7 ms | 33 B |
| imagery | 64 KiB | 0.924 | 0.926 | 87 KiB (×2.9) | 0.05 ms | 6 KB |
| imagery | 8 MiB | 0.916 | — | 7.1 MiB (×243) | 3.5 ms | 57 B |
| embedded pack | 64 KiB | 0.75–0.77 | 0.80 | 70–73 KiB (×2.4) | 0.12 ms | 1.9–3.2 KB |
| embedded pack | 1 MiB | 0.69–0.71 | — | 0.7 MiB (×23–24) | 1.4 ms | 0.1–0.2 KB |
| embedded pack | 8 MiB | 0.66–0.68 | — | 3.2–3.7 MiB (×110–127) | 6.6–8 ms | 33–45 B |

zstd-9 frames are 3–8 % larger than zstd-19 frames everywhere. On terrain
whose tiles are already gzip, every framing stays at 0.996–0.997, and a read
fetches 3–9× more compressed bytes for nothing.

What this says:

- **Small frames keep random access at a modest price.** At 64 KiB, a terrain
  archive keeps 85 % of whole-file zstd's gain (0.49 against 0.43) for ×1.5
  read amplification and 0.15 ms of CPU. But per-tile brotli-11 reaches 0.435
  with **no** amplification at all, and in a format every reader knows. For
  PMTiles, per-tile compression is the seekable framing that already exists:
  a tile is the natural frame.
- **8 MiB frames match the unit the Worker and the browser film already use**
  (`cached.rs:16`, `source.rs:42`). A chunk is fetched whole anyway, so
  compressing each chunk adds no amplification on that path. It does on the
  range path: a 30 KB tile costs a 2–4 MiB download plus 6–8 ms of decode. The
  same effect is reached with no new format by serving each chunk URL with
  `Content-Encoding: zstd` or `br`, decoded by the browser itself.
- **Aligning frames on tile or directory boundaries** (never splitting a tile)
  would bring the expected frames per read from (F + 30 KiB)/F down to 1. That
  is worth it only if a seekable container is adopted, which this study does
  not recommend for PMTiles.
- **A dictionary per layer** does for a whole-file codec what it does per tile
  (section 3.1): a few percent. For imagery, a 1 KiB dictionary holding the
  two JPEG headers would recover the 8 % per tile, bit-exact. But PMTiles has
  no field for a dictionary, so this is a tuile-only extension. Not
  recommended now.

**Crates.**

| Crate | Status |
|---|---|
| `zeekstd` 0.6 | the seekable format; pre-1.0, on `zstd-safe`, i.e. C libzstd |
| `zstd-seekable` 0.1 | C bindings, last release 2023 |
| `noodles-bgzf` 0.52 | BGZF, widely used in bioinformatics, on flate2 |
| `ruzstd` 0.9 | pure Rust, 72 M downloads, decodes any zstd frame |

No seekable writer is pure Rust and 1.0. On the read side, ruzstd can decode
seekable frames in wasm, and the seek table is a few lines to parse. Rule 8
admits ruzstd and noodles-bgzf in the wasm path, not zeekstd.

### 6.5 Packs: what is inside, and a pack v4

The pack already **is** a seekable container. Each payload is an
independent `Block { offset, stored, raw }` (`schema/tuile_pack.fbs`,
`struct Block`). Meshes are LZ4 (`lib.rs:614-632`). Textures are PNG
"stored as produced" (`TextureFormat::Png`). What is left is to choose a
better codec **per block**:

| Block kind (both packs) | Stored today | Re-encoded, per block | Ratio | Check |
|---|---|---|---|---|
| textures, PNG (83–88 % of blob) | 15.06 + 8.39 MB | **WebP lossless**, method 4 | **0.46–0.49** | pixel-exact, asserted |
| | | PNG re-encoded at maximum effort | 0.79–0.80 | pixel-exact |
| | | zstd-19 over raw RGBA | 1.08–1.12 | — |
| positions, LZ4 | 0.72 + 0.59 MB | zstd-19 / + 64 KiB dictionary | 0.72 / 0.70 | — |
| uvs, LZ4 | 0.38 + 0.31 MB | zstd-19 / + dictionary | 0.07 / **0.012** | — |
| indices, LZ4 | 1.04 + 0.86 MB | zstd-19 / + dictionary | 0.27 / **0.008** | — |
| table (FlatBuffers) | 10 KB | zstd-19 | 0.35 | — |

The mesh dictionary was checked out of sample: trained on one pack, it gives
the same 0.008–0.012 on the other. uvs and indices barely vary between tiles
of the same topology.

Time:

| Codec | Encode | Decode |
|---|---|---|
| WebP lossless, method 4 | 0.23 s per texture | ≈ PNG's, 4.5 vs 5 ms per texture (Pillow) |
| WebP lossless, method 6 | 2.5 s per texture, for 0.5 % more | — |
| method 0 | 35 ms | — (ratio 0.59) |
| zstd, meshes | — | 0.03–0.2 ms per block |

Decoders: `image-webp`, already part of the `image` family, in pure Rust;
`createImageBitmap` in the browser; `zstd` natively, `ruzstd` in wasm.

**Result: a whole pack, re-encoded per block, weighs 0.45–0.46 of today's**
(17.2 → 7.9 MB, 10.2 → 4.6 MB). Whole-file zstd-19 reaches only 0.65–0.66,
and loses random access. Whole-file zstd on top of the per-block
re-encoding would gain almost nothing, by the same logic as terrain.

**Fingerprints.** Two digests are involved, and the rule for both is
"compress around them, never under them".

- `blob_digest` is FNV-1a over the blob region **as stored**
  (`lib.rs:351-353`, `:779-790`). It checks the integrity of what crossed the
  wire, so it stays over the stored bytes of a v4 pack. A blob of half the
  size also halves its "seconds per gigabyte".
- The store digests in `TerrainRef` / `ImageryRef` are FNV-1a over the
  **store tile's bytes** (`tuile-film/src/from_store.rs:54-63`). They must
  stay over the decoded tile (section 5, step 1). A pack never re-encodes
  a store tile anyway: references are not payloads.

**What a pack v4 would look like.** Not v3.1: the layout must say "a reader
of v3 cannot read this", and `READABLE` (`lib.rs:93`) is the mechanism.

- `Tile.texture_format` gains `WebpLossless = 2`. The enum is additive, and
  `raw` stays the decoded size.
- A mesh codec is named once for the pack: a new `Pack.mesh_codec`, LZ4 or
  zstd. A per-block byte is impossible, because `Block` is a fixed-size
  struct.
- A new field `Pack.mesh_dictionary: Block` points at the dictionary, stored
  as the first payload of the blob.
- `VERSION` becomes 4. Readers ship first (1..=4), writers later. A v1–v3
  pack stays readable for ever, as the constant's doc promises.
- Everything else is unchanged: preamble, table, offsets relative to the
  blob, contiguous blocks per tile (so `span_of` still coalesces), FNV over
  the stored blob.

### 6.6 Cloudflare R2 and `Content-Encoding` on a stored object

These points come from the documentation and were not tested against the
deployed buckets.

- R2 never compresses on upload. An object uploaded with
  `Content-Encoding: gzip` is stored as such.
- Through Cloudflare's HTTP front (public bucket, custom domain, a Worker
  `fetch`): "if Cloudflare must decompress a complete encoded response, it
  **ignores `Range`** … and sends the complete, uncompressed body as
  `200 OK`". This applies to cached responses and to origin responses.
  Cloudflare decompresses when the client does not accept the encoding.
- An encoded `206` from an origin is not cached.
- Through the S3 API, a ranged `GetObject` follows S3 semantics: the range
  is over the **stored (encoded) bytes**. This was not verified on R2.
- **Consequence:** an archive or pack object must never carry
  `Content-Encoding`. Ranges and the 8 MiB chunks would break, or be served
  as a 200 of the whole decoded object. Compression lives inside the
  container (per tile, per block). A **transfer** compression goes on
  separate URLs whose body is answered whole: the API's 8 MiB chunk URLs,
  tile URLs.

### 6.7 Decision, per use

| Use | What to compress | Granularity | Format | Expected saving | Range reads |
|---|---|---|---|---|---|
| **Farm transfer** (pack in) | the pack's payloads | per block, inside the pack | **pack v4**: WebP lossless textures, zstd + dictionary meshes | ≈ **2.4 → 1.1 GB per task** (−1.3 GB), GETs ≈ 150 → ≈ 70 at 16 MiB, download time −55 % at the same throughput; FNV −55 % | kept: block = payload |
| Farm transfer, reference packs | the table | whole table, on the wire | the API answers its chunk URLs with `Content-Encoding: zstd` (or `br`) | table ÷6 on the wire, 100 MB → ≈ 17 MB | unchanged: chunks are answered whole |
| **Tile-server and stl-tiles disk caches** | nothing more | — | the store's own per-tile compression (terrain gzip) | the terrain share of 8 / 12 GiB holds ≈ 1.9× more zones | mmap random access kept; whole-file compression would kill it, so no |
| **At rest (R2/S3)** | tiles (section 5); packs (v4) | per tile, per block | gzip terrain, JPEG as is, pack v4 | terrain −47 %, packs −55 %; imagery 0 % | kept |
| Backups, superseded archives (cold) | whole object | whole file | zstd-19 of the archive | 0.4 % on gzip-tiled archives, 8 % on imagery (headers). Not worth an extra format | n/a |
| **CDN ranges** (browser, Worker) | nothing on the object | — | never `Content-Encoding` on `.pmtiles` or `.tuilepack` | — | kept; the 8 MiB chunk URLs may carry `Content-Encoding` (gains only on raw content, ≈ 0 after v4 and gzip tiles) |
| **Mobile/offline bundle** (a zone) | the bundle file | whole file, for the download only | zstd-19 (`zstd` native on iOS/Android, `ruzstd` in wasm; ≥ 680 MB/s decode). The phone unpacks it to a plain archive and mmaps it | over gzip-tiled archives: imagery −8 %, terrain ≈ 0. **Better:** the bundle builder writes terrain tiles **raw** and zstd-19's the whole file, 0.43 against 0.52 (−17 % more on terrain). xz gives 0.39 but decodes at 26–30 MB/s, too slow for a phone | none during the download; the unpacked archive is random-access |

**Migration and compatibility.**

1. **Packs v4.** Teach `tuile-pack` to read v4 first: `READABLE = 1..=4`, a
   WebP texture decode, zstd meshes with a dictionary. Deploy it to the
   farm image, film-native and film-web. Only then let `tuile-bake` write
   v4.
   - Old v2/v3 packs remain readable. Re-baking is not needed; a converter
     (v2 → v4, block by block, payloads only) can shrink the stored ones.
   - **Test:** a v2 pack and its v4 conversion must render the same frame
     to the byte. Then break the WebP decode and watch it fail.
2. **PMTiles archives.** Nothing beyond section 5. No seekable container,
   no whole-file encoding, no `Content-Encoding` on objects.
3. **Mobile bundle.** A bundle format is a new artefact: a whole-file zstd
   of an archive whose terrain tiles are raw. It is decided when offline
   use is built, and it has no impact on the store.

## 7. PoC on production archives

Two archives of the production bucket `stl-tiles`, rewritten locally. Nothing
was uploaded. Script: `lab/pmtiles-compression/poc_transform.py`, with
`contact_sheet.py`. Archives are written with the reference Python
`pmtiles` writer: same tile ids, same clustering, the right `tile_type` and
`tile_compression`. Each one passes `pmtiles verify` and is read back tile by
tile.

**Which archives.** Access was wrangler's existing login. It can fetch
objects but cannot list a bucket, and none of the 342 zone manifests known
from local caches exists in `stl-tiles`. The archives below are therefore
the largest of the three **`top` manifests**, the zones coarser than the
zone level. Whether a zone archive is larger cannot be known without a
listing.

| | Imagery (`asset-2`) | Terrain (`asset-1`) |
|---|---|---|
| Key | `asset-2/top/202610-000001791862421972630375-a85f14746ecf6ca1.pmtiles` | `asset-1/top/d-000001791361825015576247-bd528c4e4d9e65bb.pmtiles` |
| Size, tiles | 96.5 MB, 5 607 tiles, levels 5–9 | 60.3 MB, 1 802 tiles, levels 1–9 |

The bucket itself, from `wrangler r2 bucket info`: **45.5 GB, 71 910
objects**.

### Imagery: JPEG → WebP

The sources here are q≈90, as everywhere at levels ≤ 12 (section 3.2).

The quality ladder was scored on a sample per level, as size/source and
median SSIMULACRA 2:

| q | z7 | z8 | z9 |
|---|---|---|---|
| 80 | 0.65 / 77.6 | 0.47 / 73.8 | 0.66 / 77.3 |
| 85 | 0.78 / 81.8 | 0.60 / 78.2 | 0.77 / 82.1 |
| **90** | 0.92 / 86.4 | 0.77 / 83.1 | 0.94 / 86.3 |
| 95 | 1.17 / 90.4 | 1.11 / 87.5 | 1.17 / 90.1 |
| lossless | 3.00 | 3.12 | 2.92 |

Whole archive, quality measured on every 6th tile, per level:

| Variant | Size | z8 (1 960 tiles) | z9 (3 432 tiles) | SSIMU2 median (p5–p75), z9 | PSNR-Y / SSIM-Y, z9 |
|---|---|---|---|---|---|
| **WebP q90** (closest to the source) | 96.5 → **86.5 MB (0.896)** | 0.889 | 0.901 | 86.0 (78.9–87.2) | 42.7 dB / 0.994 |
| WebP q85 | 96.5 → 70.8 MB (0.734) | 0.724 | 0.740 | 81.8 (74.2–83.3) | 39.8 dB / 0.989 |

Costs:
- Encode: 5.7 ms of CPU per tile (method 4). That is 32 s of CPU for the
  archive, 8 s wall on 4 processes.
- Decode, native (Pillow/libwebp): 1.05 ms per tile median and 1.6 ms p95,
  against **0.21 ms for the source JPEG**, i.e. 5× slower.
- In wasm, decoding with `image-webp` would be an estimated 2–3× native. In
  the browser, `createImageBitmap` decodes natively and off-thread.
- Directories: 14.1 → 13.5 KB.

The contact sheet is
`~/Documents/stl-videos/pmtiles-poc-contact-z9.png`. It shows one tile each
for the most mountain-, coast-, forest- and texture-like z9 tiles: source,
q90, q85, a 3× crop of each, and the difference ×8. At 3× no artefact is
visible at q90. At q85 fine relief loses a little crispness. Levels 5–9 show
no village; that needs the zone levels.

⚠ This re-encodes commercial imagery: a derived work. It is fine as a local
measurement, never as a deployed layer without a licence decision.

### Terrain: gzip-9

| Level | 1 | 2 | 3 | 4 | 5 | 7 | 8 | 9 | **All** |
|---|---|---|---|---|---|---|---|---|---|
| Ratio | 0.164 | 0.102 | 0.064 | 0.047 | 0.040 | 0.064 | 0.078 | 0.081 | **60.3 → 3.85 MB (0.064)** |

- Every one of the 1 802 tiles inflates back to the original bytes.
- Encode takes 2 ms per tile. Decode takes 0.03 ms per tile native
  (0.06 p95); an estimated 0.05–0.1 ms in wasm, or free through
  `DecompressionStream` / `Content-Encoding`.
- zopfli on 200 tiles: 0.927 of gzip-9, at 221 ms per tile.
- Coarse terrain tiles are mostly their **water mask** (up to 64 KiB of
  bytes per tile, extension 2, present on every tile), which deflate
  crushes. That is why this archive shrinks 15×, against 0.526 for the
  zones at levels 10–19 (section 3.1).

### Projection on the bucket

How the 45.5 GB split between layers is not known: there is no listing.

- With terrain between 20 % and 60 % of the bytes, gzip terrain (×0.53 for
  zones, much better for `top`) plus WebP q90 imagery (×0.90) saves about
  **8 to 15 GB**.
- At R2's $0.015 per GB-month, that is **$0.12 to $0.22 a month**. The whole
  bucket costs ≈ $0.68 a month to store.
- R2 egress is free.
- Operation counts do not change: one read per tile or per range, as
  before.

The gain is therefore not in the R2 bill. It is in bytes moved: client
download time, the disk caches (8 / 12 GiB), the farm, and mobile data.
Terrain gzip gets almost all of it, with no quality or licence question.
WebP q90 adds 10 % on imagery, at the price of a derived work and a 5×
CPU decode where the browser does not decode it.

### Faster WebP decoders?

**Method.** The same 1 000 tiles of the production `top` archive (levels
5–9) were decoded as source JPEG and as WebP q90, one decode per tile,
single-threaded, on Apple Silicon. Code:
`lab/pmtiles-compression/decode-bench/` (Rust) and
`decode_bench_inputs.py`. The laptop was heavily loaded by other builds
(load average 20–70), so each tile keeps the fastest of 7 passes and the
table gives the mean of those per-tile minima. Even so, settings within
±15 % of each other are not distinguishable.

| Decoder (version, licence) | Code path | ms/tile, mean of per-tile minima |
|---|---|---|
| **JPEG:** libjpeg-turbo 3.2 via `turbojpeg` 1.5.1 (MIT/Unlicense; C, BSD-style) | NEON SIMD, into a preallocated RGBA buffer | **0.20** |
| — the same, YUV planes | no colour conversion | 0.19 |
| `zune-jpeg` 0.5.15 (MIT/Apache/Zlib) | pure Rust, NEON | 0.29 |
| `jpeg-decoder` 0.3.2 (MIT/Apache) | pure Rust, scalar | 0.56 |
| **WebP:** libwebp (vendored by `libwebp-sys` 0.14.4, MIT; libwebp BSD) | NEON (`WEBP_HAVE_NEON`), RGBA, default fancy upsampling | 1.14 |
| — `no_fancy_upsampling` | | 1.21 (no gain) |
| — `use_threads` | VP8 filtering on a second thread | 1.00 |
| — into a preallocated RGBA / RGB buffer | | 0.93 / 0.97 |
| — YUV 4:2:0 planes (colour conversion left to a shader) | | 0.93 |
| `image-webp` 0.2.4 (MIT/Apache), the decoder behind `image` | pure Rust, no explicit SIMD | 1.77 |
| `zune` | no WebP decoder exists in `zune-image` 0.5 | — |
| **Browser,** Chrome 155: `createImageBitmap(Blob)`, awaited one at a time | native, off the main thread | JPEG 1.73–1.88 / WebP 5.44 |
| Chrome 155: `ImageDecoder` (WebCodecs), awaited one at a time | native | JPEG 2.74 / WebP 2.52 |

**Encoder settings.** All are at q90, scored on 200 tiles against the
source JPEG. Decode is libwebp, RGBA.

| Setting | Size vs `-m 4` | SSIMU2 | Decode |
|---|---|---|---|
| `-m 4` (reference) | 1.000 | 85.6 | 1.14 ms |
| `-m 6` | 0.990 | 85.7 | 1.02 ms |
| `-f 0` (no loop filter) | 1.000 | 85.7 | 1.01 ms |
| `-segments 1` | 0.998 | 85.5 | 1.27 ms |
| `-sharp_yuv` | 1.010 | 85.8 | 1.01 ms |

No alpha is written in any case: the tiles are RGB.

**What this says.**

- **No faster WebP decoder exists.**
  - libwebp with NEON is the fastest: ≈ 0.95–1.15 ms per 256² tile, still
    **≈ 5× libjpeg-turbo** (0.20 ms).
  - The pure-Rust `image-webp`, the only decoder that suits the wasm rule,
    is 1.6–1.9× slower than libwebp.
  - The gap is the format itself: VP8's boolean arithmetic decoding and
    in-loop filter, against JPEG's Huffman coding plus SIMD IDCT. It is not
    a weak implementation.
- **No decoder or encoder setting moves it by more than the measurement
  noise.** Preallocated output, RGB or YUV output, and threads each gain
  0–15 %. `-f 0`, `-segments 1`, `-m 6` and `-sharp_yuv` change size by
  ±1 % and decode by about as much as the noise.
- **The browser makes the native gap moot only for throughput.** One
  decode awaited at a time costs 1.7–5.4 ms, mostly round-trip latency to
  the decoder thread, and it is off the main thread anyway.
  - With `createImageBitmap`, WebP costs 3× JPEG.
  - With `ImageDecoder`, the two are equal (≈ 2.5–2.7 ms).
  - These browser figures were taken while the native benchmark was also
    running. A second run, with four decodes in flight, was cut short when
    the tab was closed. To redo it, serve
    `decode-bench/browser.html` next to `jpeg.bin` and `webp-q90-m4.bin`
    (`python3 -m http.server`) and open it in Chrome.
- **In wasm**, `image-webp` would run at an estimated 2–3× its native
  1.8 ms, i.e. 4–5 ms per tile. That is a reason to hand WebP to the
  browser's decoder rather than decode it in wasm.
- **Avoiding the decode altogether** means GPU block formats (section 3.2,
  KTX2). The trade-off stays the same: a transcode of 0.1–0.5 ms and a
  quarter of the VRAM, against files 1–4× larger, a derived work, and a
  ≈ 500 KiB transcoder.

**Recommendation.** If imagery ever goes WebP:
- **native:** decode with libwebp (`libwebp-sys` with NEON or SSE) into a
  preallocated buffer, or straight to YUV for a shader;
- **web:** `ImageDecoder` / `createImageBitmap` with several decodes in
  flight;
- **wasm:** never `image-webp` on a hot path.

At about 1 ms against 0.2 ms per tile, WebP's decode cost is real but small
next to a tile's network fetch. It is not, alone, a reason to keep JPEG. The
licence question and the 10 % saving are.

## 8. What was not measured

- **wasm decode times and binary sizes.** No wasm was built: other agents
  were compiling on this laptop. Native timings ×1.5–3 is the usual range
  for these decoders without SIMD, but it is an estimate. The Basis
  transcoder's 489 KiB is the only measured size.
- **The bucket's split between terrain and imagery bytes.** The gain on the
  whole store, and on egress per film, depends on it. A listing of the
  bucket by layer would give it.
- **Ocean and sparse coverage**, and the `asset-3954` geographic imagery
  layer: neither is present locally.
- **butteraugli.** libjxl's tools are broken here. SSIMULACRA 2 comes from
  its Python port, whose scores can differ from libjxl's CLI by about a
  point.
- **Basis with mipmaps**, and real on-device GPU encode/transcode times on a
  phone or headset. **Real first-request latency against R2.** Only the
  byte counts that determine it were measured.
- **zopfli** on more than one zone, and brotli or zstd decoders in an actual
  Worker.
- **A farm-size pack** (2.4 GB). The pack figures come from two one-frame
  embedded packs (27 MB in total) and are extrapolated. A long film's blob
  may hold a different mix of textures and meshes, and repeat textures
  across frames differently. The pack v4 ratio should be measured on one
  real farm pack before any decision.
- **Cloudflare's actual behaviour** with ranges on an encoded object, on the
  deployed buckets (section 6.6). The S3-API case is S3 semantics,
  unverified on R2.
- **The seekable writers** (`zeekstd`): neither built nor run. The framings
  were simulated with independent zstd/gzip frames plus the seek table's
  documented size.
- **WebP lossless decode in wasm.** The textures' decode cost was measured
  with Pillow, native.
