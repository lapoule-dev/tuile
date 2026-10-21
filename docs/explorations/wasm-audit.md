<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# WebAssembly implementations — audit

Read-only audit, 2026-10-17, of `origin/main` at `70e17d9` (PR #62 merged), read in a
throwaway detached worktree. No code was changed. It builds on two earlier reports and
does not repeat them:
- the web viewer status, PR #65 (`docs/explorations/web-viewer-status.md`): the build
  matrix, the runtime attempts and the parity table;
- the rebalancing analysis, PR #60 (`docs/explorations/rebalance-and-1x.md`): duplicated
  drivers, caches and renderers across modes.

**Scope note from the owner.** The PMTiles tile store (`tuile-tile-server`,
`tuile-repository`, packs, `tuile-pack-worker`) serves the film pipeline only. The
interactive viewer fetches its sources directly, by design. "The web viewer does not use
the store" is therefore **not** a gap. The store-side wasm code (the pack Worker, the
repository over `fetch`, film-web) is audited on its own merits, as film and demo
infrastructure.

**In-flight work** (read, not duplicated):
- `fix/tuile-web`: uncommitted changes in the worktree `~/Projects/tuile-web-fix`, on top
  of `70e17d9`. They:
  - repair `tuile-web` (new `forward.rs` and `transport.rs`);
  - fix the web-viewer status line and handle `retire`;
  - delete film-web's two dead functions;
  - extend the CI `wasm-guard` to 12 crates under `-D warnings`.
- `feat/web-native-convergence` (`07ba5f9`): **a design document only**
  (`docs/explorations/web-native-convergence.md`). No code yet. It plans:
  - `tuile-viewer` with a web shell: wgpu on WebGPU with a WebGL2 fallback, and
    `web_time`;
  - `tuile-web` reduced to a transport;
  - deleting the THREE.js page, `tuile-web/src/worker.rs` and `examples/wasm-globe`.

## 1. Summary table

| Implementation | Status (build / alive) | Quality | Security | Tests | Verdict |
|---|---|---|---|---|---|
| `tuile-core` (wasm paths) | checks OK, in CI | good; timing stubbed to 0 on wasm; browser budget saturates | n/a | 201 native, 0 on wasm | **keep**, fix timing + budget |
| `tuile-web` | **does not compile** since Aug 18–20 | stale protocol; unbounded `sent_imagery` | token used, not stored | 0 | **fix** (`fix/tuile-web`), then **rescope** to transport (convergence) |
| `examples/web-viewer` (THREE.js) | cannot run (no `pkg`) | a fork of the renderer (GLSL, JS camera, JS residency) | THREE from unpkg without SRI | 0 | **delete** the renderer (convergence); page becomes a package consumer |
| `examples/wasm-globe` | builds, loads, draws nothing | 4th traversal driver | ion token in tile URLs (ion's convention) | 0 | **delete** (convergence) |
| `tuile-wgpu` on wasm | check OK, no consumer | `std::time::Instant` panics on wasm at the first frame | n/a | 40 native | **fix** (convergence: `web_time`) |
| `tuile-planetary`, `-terrain`, `-camera`, `-ui`, `-atmosphere`, `tuile-film` | check OK | pure, `?Send` gates duplicated by hand | n/a | native only (17–58 each) | **keep**; add to wasm guard (`fix/tuile-web`) |
| `tuile-film-web` + `examples/film-web` | builds (2 dead-code warnings, fixed in `fix/tuile-web`); deployed demo | sound film renderer through `tuile-film-gpu`; duplicated fetch/retry/sleep; unbounded Cache Storage | `?api=` points the page at any origin; credential in query | **0** | **keep + fix**; later: its page/shell becomes the film mode of the converged web viewer |
| `tuile-pack-worker` (Cloudflare Worker) | builds (release, measured); deployed at `tuile-demo.lapoule.dev` | thin adapter over `Bench`, good; config re-parsed per request | **anonymous read of whole buckets, privileged credentials, open proxy** | **0** (the `Bench` routes it serves have native tests) | **keep + fix now (security)** |
| `tuile-repository` (wasm half: S3 over fetch, remote store) | compiles as a dep | good error typing; hand-rolled HMAC/SigV4; listings unbounded | route scoping is missing (see the Worker) | 59 native | **keep + fix** |
| `site/` (tuile-site Worker) | static assets only, no wasm, no script | — | static; no security headers | — | **keep**; fix stale claims |
| `tuile-viewer` on wasm | fails (getrandom 0.3 without `wasm_js`, foyer, metrics exporter) | — | — | — | out of scope: convergence |

## 2. Findings, by severity

### Critical / High

**H1. The demo Worker serves any object of every configured bucket to anyone.**
- **Where:**
  - `crates/tuile-repository/src/bench.rs:666` (`ls?prefix=`), `:668-669` (`o/<key>`) and
    `:671-697` (`b8/<n>/<key>`);
  - the only check is `safe()` at `bench.rs:449-455`, which blocks `..` and absolute
    keys. **Nothing restricts a key to the project's film roots** (`Layout::Scenes(roots)`
    is used for `films` only).
- **The deployment.** `GET https://tuile-demo.lapoule.dev/api/projects` answers, today,
  without authentication: projects `tuile` → `bucket stl-track-data` and `stl` →
  `bucket stl-renders`.
  - The bucket names suggest production data (tracks, renders) beyond packs.
  - So anything in those buckets can be read by key, and enumerated with `ls`. `ls` was
    deliberately **not** probed.
- **Fix:**
  - each project declares the prefixes it publishes, and `o/`, `b8/` and `ls` refuse any
    key outside them;
  - remove `ls` from the Worker build, or cap it to the roots;
  - better still, keep public packs in a bucket that holds nothing else.

**H2. The Worker's S3 key is a general one, not a read-only one.**
- **Where:**
  - `crates/tuile-pack-worker/src/bench.rs:300-304` reads `TUILE_STORE_ENDPOINT`,
    `TUILE_STORE_ACCESS_KEY_ID` and `TUILE_STORE_SECRET_ACCESS_KEY`;
  - these are the same variable names the farm, the bake and `tuile-migrate-packs` use
    to *write* (`crates/tuile-farm/src/lib.rs:213-214`, `crates/tuile-bake/src/tiles.rs:155`);
  - one key signs for every project bucket and for the tile-store bucket
    (`bench.rs:316-325, 383-391`).
- **The docs** (`.dev.vars.example:4-5`, `lib.rs:26-28`) ask for a read-only key, but
  nothing enforces it.
- **The deployment notes** say the deployed secret is the Rails stack's R2 key
  (`stl:r2_access_key_id`). Its scope was not checked in Cloudflare.
- **Mitigation in code:** `FetchHttp::send` only ever emits `GET` or `HEAD`
  (`bench.rs:40-45`), so the Worker cannot write even with a write key. The risk is the
  secret itself: one leak (a dashboard, `wrangler` on a laptop, a log) gives write access
  to production buckets.
- **Fix:**
  - mint an R2 API token, *Object Read* only, scoped to the bucket(s) the demo serves;
  - give the Worker distinct variable names (for example `TUILE_BENCH_READ_KEY_ID`) so
    the farm's `.env` cannot be pasted in by mistake;
  - rotate the current secret.

**H3. Anonymous callers spend a privileged upstream token.**
- **Where:**
  - with `[tiles] remote`, the Worker reads the remote store with
    `TUILE_TILES_REMOTE_SECRET` (`bench.rs:411-421`);
  - per the notes, that secret is a load-test token, not the public visitor token used by
    the page.
- **What triggers it:**
  - `GET /api/tiles/<layer>/<z>/<x>/<y>` and `GET /api/projects` both open the store
    (`with_tiles`, `bench.rs:573`), which reads at least the catalog upstream, on every
    request;
  - `POST /api/store/live` asks upstream for up to 512 objects (`LIVE_MANY`) per call.
- **Effect:** the Worker is an open proxy that spends a privileged token's rate limit and
  identity.
- **Fix:**
  - in the Worker, read the remote store with the public credential only;
  - or do not offer `/api/tiles` and `/api/projects` tiles at all when `direct = true`,
    since the page goes to the store itself;
  - add a Cloudflare rate-limiting rule on `/api/*`.

### Medium

**M1. Unbounded or amplified upstream work per anonymous request.**
- **Listings:**
  - `S3Objects::listing` follows every continuation page with no cap
    (`crates/tuile-repository/src/s3.rs:367-387`);
  - it is reachable through `ls`, `films` and `films/<id>`;
  - each page is one subrequest. The Workers limit is 50 subrequests on the free plan and
    1000 on paid; the plan was not checked.
- **`POST /api/store/live`:** 512 objects × up to 5 tries
  (`s3.rs:51` `TRIES`, `bench.rs:577` window 6) can exceed the subrequest limit.
- **Cache keys:** cacheable blocks are keyed by the full URL, query included
  (`pack-worker/src/bench.rs:544-548`). Adding any `?x=` forces a fresh 8 MB bucket read
  and a cache write per request: cache-busting as a cost amplifier.
- **Config re-built per request:** `bench(&env, …)` re-parses the TOML config and rebuilds
  every repository on each request (`bench.rs:292-449`). `toml_edit` and `winnow` are
  **12.7 %** of the Worker's code by size (measured).
- **Fix:**
  - cap listing pages and keys;
  - normalise the cache key: keep only `s` and the path;
  - build the `Bench` once per isolate (`OnceCell`), or take the config as JSON;
  - cap `LIVE_MANY` for the Worker to what its subrequest budget holds.

**M2. Upstream error text and configuration reach anonymous clients.**
- **Where:**
  - `s3.rs:346-352` puts up to 200 characters of the bucket's error body into
    `RepoError::Store`, which `Reply::of` sends to the client;
  - `pack-worker/src/bench.rs:519, 576` answer `misconfigured: {e}`;
  - `/api/projects` names the buckets and the upstream tile-store URL (verified live).
- **Fix:**
  - log the details (`console_log!`) and send generic messages;
  - make project labels configurable, not `bucket <name>`.

**M3. `tuile-web` does not compile, and CI checks only `tuile-core` for wasm.**
- **Where:** `crates/tuile-web/src/worker.rs:210, 244, 269` and `.github/workflows/ci.yml:45-53`.
- Covered by PR #65.
- **Fixed by `fix/tuile-web`** (uncommitted). That branch adds `cargo check` for 12
  crates under `-D warnings`, but:
  - **no wasm clippy** (the workspace lints `unwrap_used` and `panic` never run on the
    wasm-only crates);
  - **no `wasm-pack` / `worker-build` build**;
  - no size budget.

**M4. `tuile-wgpu` panics on wasm32 at its first frame.**
- **Where:** `crates/tuile-wgpu/src/pump.rs:160, 296, 309` call `std::time::Instant::now()`,
  which panics on `wasm32-unknown-unknown`.
- `cargo check` cannot see it, and there is no wasm consumer to trip it.
- **Covered by convergence** (`web_time::Instant`), not yet in code.

**M5. Timing is silently zero in the browser.**
- **Where:**
  - `crates/tuile-core/src/determinism.rs:98-101`: `elapsed_ms()` returns `0` on wasm;
  - `crates/tuile-core/src/metrics.rs:555-569`: `Stamp::elapsed()` returns
    `Duration::ZERO`.
- **Effect:** every load, decode and stitch duration reported by the engine in a browser
  is 0. Instruments read green and are blind.
- **Fix:** `web-time` (`web_time::Instant`): `std::time::Instant` on native,
  `performance.now()` on the web. It is already in the lock file (1.1.0, through winit),
  stable and widely used. Not covered by the in-flight branches: convergence plans it for
  `tuile-viewer` and the pump, not for the core.

**M6. No byte budget for the interactive globe in the browser.**
- **Where:** `crates/tuile-core/src/traversal.rs:392-394`. `interactive_globe`'s
  `4usize.saturating_mul(1 GiB)` saturates to `usize::MAX` on wasm32: the whole address
  space.
- **Effect:** the byte bound never bites (only the 16 000-tile count does), and a tab
  grows until the browser kills it.
- **Fix:** a browser profile with an explicit budget (for example 512 MiB to 1 GiB).
  Partly planned in convergence (`profile`: desktop / handheld / browser); make sure it
  sets `resident_budget_bytes`.

**M7. film-web's Cache Storage grows without bound and is never cleaned.**
- **Where:** `crates/tuile-film-web/src/store.rs:58-93`. The cache `tuile-store-blocks-v2`
  keeps 8 MB blocks forever, until the origin's quota.
- `put` failures are swallowed (`/* full: read, not kept */`). The previous `-v1` cache is
  never deleted (no `caches.delete` anywhere).
- On a phone, one long film can take gigabytes of the origin's storage.
- **Fix:**
  - an LRU index with a size cap;
  - delete stale cache names at startup;
  - read `navigator.storage.estimate()`.

**M8. The wasm-only crates have no tests at all.**
- **Which:** `tuile-web`, `tuile-film-web`, `tuile-pack-worker` and `wasm-globe` have 0
  tests.
- **Why nothing compiles them:** they are whole-crate `#![cfg(target_arch = "wasm32")]`
  (or gated modules), so `cargo test` and `cargo clippy` on the host never compile them.
- **What is missing:**
  - no `wasm-bindgen-test`;
  - no headless browser;
  - no Worker test (miniflare / `wrangler dev`).
- **What is covered:** the `Bench` routes the Worker serves are tested natively in
  `tuile-repository` (59 tests, including key safety, ranges and validators). The Worker
  adapter (`bench.rs` in pack-worker: cache keys, 304 from cache, HEAD) is not.
- `fix/tuile-web` shows the right pattern: `forward.rs` is pure Rust, outside the gate,
  host-tested.
- **Fix:**
  - same pattern for film-web (`store` key/credential logic, block planning) and
    pack-worker (`is_block`, cache-key normalisation);
  - one headless WebGPU smoke test for film-web (see PR #65 §7.6).

### Low

**L1. Hand-rolled cryptography in the wasm path.**
- **Where:** `crates/tuile-repository/src/s3.rs:83-110`: HMAC-SHA256 written by hand, and
  SigV4 at `:166`.
- It is tested against reference vectors (`s3.rs:540, 560`). Still, it is a home-made
  kernel where an established crate exists.
- **Fix:** `hmac` 0.13 (RustCrypto: pure Rust, wasm-clean, MIT/Apache) for the MAC, and
  keep the SigV4 canonicalisation with its vector tests. `aws-sigv4` is too heavy for a
  Worker.

**L2. The same browser plumbing, written five times.**
- **`fetch` clients:**
  - `tuile-web/src/lib.rs:80-120` (`get`);
  - `tuile-film-web/src/source.rs:25-33, 88-114` (typed `web-sys`);
  - `tuile-film-web/src/store.rs:58-93, 183-208` (`Reflect` plus inline JS);
  - `tuile-pack-worker/src/bench.rs:22-65` (`FetchHttp`) and `:69-129` (`RemoteGet`);
  - the JS loops of `wasm-globe`.
- **Retry loops**, each with its own policy:
  - `s3.rs:328` (5 tries, ×2);
  - `pack-worker bench.rs:79` (4, ×3);
  - `film-web store.rs:214` (5, ×3);
  - `film-web source.rs:121` (5, ×3).
- **`sleep`, twice in one crate:** `film-web/src/js.rs:73` and `source.rs:59`.
- **The block size, twice:** hard-coded in `film-web/src/source.rs:42` (`8 << 20`) and
  defined in `tuile_repository::BLOCK` (`bench.rs:65`). They must agree, and nothing
  checks that they do.
- **Fix:** one browser transport (`tuile-web::transport`, started in `fix/tuile-web`) with
  `fetch`, retry and Cache Storage, used by the viewer shell and by film-web.

**L3. Copies across the JS boundary, and 8 MB bodies held whole.**
- **In the browser:** every fetched body is copied out of JS with
  `Uint8Array::new(&buffer).to_vec()`:
  - `film-web store.rs:129` and `source.rs:113, 178`;
  - `tuile-web lib.rs:117`.
- **In the Worker:**
  - an 8 MB block is read whole (`response.bytes()`), then copied again for the cache
    (`pack-worker bench.rs:604`);
  - so two or three 8 MB buffers per cold block, in a 128 MB isolate;
  - its own comments record "Worker exceeded memory limit" (`bench.rs:154-157`).
- The page-to-worker direction is correct: transfers are used (`film-web slice.rs:165-186`,
  `web-viewer engine-worker.js:56-67`).
- **Fix:** in the Worker, stream and tee the upstream body into the response and the
  cache; in the browser, a single copy into wasm memory.

**L4. `u64` to `usize` truncation on wasm32 in the pack reader.**
- **Where:** `crates/tuile-pack/src/lib.rs:885` (`table_len as usize`) and `:972`
  (`blob_start(head)? as usize`).
- On wasm32 a corrupt length above 4 GiB wraps instead of being refused. The flatbuffer is
  still verified (`root_as_pack`), so this is wrong-error, not memory-unsafe.
- **Fix:** `usize::try_from(…)`.

**L5. A panic on server data in film-web.**
- **Where:** `crates/tuile-film-web/src/page.rs:647`. `&u.key[within..]` slices a
  server-provided key by the scene id's length. A shorter key, or one that is not UTF-8
  aligned there, panics the page.
- **Fix:** `strip_prefix`.

**L6. `?api=` sends the demo page to any origin.**
- **Where:** `crates/tuile-film-web/src/page.rs:1920`.
- **Effect:** a link can make `tuile-demo.lapoule.dev` render a film and a store from an
  arbitrary origin: content spoofing on the project's domain. No secret is exposed; the
  page holds none.
- **Fix:** restrict to the same origin, or to an allowlist.

**L7. Errors swallowed when film-web opens the store.**
- **`store/at`:** in `crates/tuile-film-web/src/store.rs:265-271`, an error or malformed
  JSON from `store/at` silently means "the store is behind this API".
- **The credential:** `store.rs:139-140` appends it to the URL without percent-encoding.
- **The token:** it travels in the query string (`?token=`). That is deliberate, to avoid
  CORS preflights, and the token is public, but it lands in every access log on the way.

**L8. THREE.js is imported from unpkg without integrity.**
- **Where:** `examples/web-viewer/www/index.html:49-53`, an import map with no `integrity`.
- Moot once convergence deletes the page.

**L9. No release profile for wasm.**
- **Today:** the workspace profile is `lto = "thin"`, `opt-level = 3`, 16 codegen units,
  `debug = "line-tables-only"` (`Cargo.toml` `[profile.release]`). It is tuned for
  profilable native builds.
- **Measured on the pack Worker** (main, wasm-bindgen 0.2.129, then
  `wasm-opt -Oz`):

  | Profile | Raw | gzip | brotli |
  |---|---|---|---|
  | current release | 1 155 KB | 477 KB | 368 KB |
  | `opt-level = "z"`, fat LTO, `codegen-units = 1`, no debug | 902 KB | 393 KB | 314 KB |

  That is −18 % gzip. Both are far inside the Workers script limits (3 MB gzip on the
  free plan, 10 MB on paid).
- **The film-web module in the main checkout** (built 2026-10-07, wasm-opt applied: no
  name section, and `-Oz` gains only 2 %) is 2.32 MB raw, 879 KB gzip, 663 KB brotli.
- **Older artefacts:**
  - `tuile-web` (2026-08-09): 956 KB raw, 314 KB brotli;
  - `wasm-globe`: 792 KB raw, 267 KB brotli.
- **Fix:** a `[profile.wasm]` that inherits release with the settings above, used by
  `wasm-pack` and `worker-build`.

**L10. Licences: permissive, but one notice is owed.**
- Every crate in the wasm trees is permissive: MIT, Apache-2.0, BSD-2/3, Zlib, Unicode-3.0
  (icu via `url`/`idna`), 0BSD.
- **rav1e** (BSD-2-Clause) ships inside the deployed film-web module. BSD-2 asks for its
  notice in binary distributions, and `NOTICE` has none.
- **Fix:** add the third-party notices for the shipped wasm.

**L11. The demo is deployed by hand, and not from main.**
- It is deployed with `wrangler` from tags and local branches (v0.3.x per the notes),
  while `main` is 0.7.0.
- No CI job builds the Worker or the page.
- **Fix:** a scripted deploy from a tag, with the build in CI.

**L12. The site claims what is not true yet.**
- **Where:** `site/src/pages/architecture.astro`:
  - `:85` "tuile-wgpu … native and in the browser": it has never run in one;
  - `:149` `web-viewer` "the same engine in a browser": it does not build.
- **Also:** the site Worker is static assets only (`site/wrangler.toml`), with no
  security headers (CSP, `X-Content-Type-Options`). Low risk for a docs site.

**L13. The `?Send` split is repeated everywhere.**
- `#[cfg_attr(not(wasm32), async_trait)] #[cfg_attr(wasm32, async_trait(?Send))]` is
  written about 40 times across core, planetary, terrain, bing, repository and
  cesium-ion.
- Test mocks carry only the native half (`tuile-planetary/src/lib.rs:1737, 1757, 1822`),
  so the tests cannot compile for wasm.
- **Fix:** one `tuile_core::maybe_send_async_trait!` macro, or an attribute alias.

### Info (checked, fine)

- **One version of each browser-binding crate** across every wasm tree:
  `wasm-bindgen 0.2.129`, `web-sys`/`js-sys 0.3.106`, `wasm-bindgen-futures 0.4.79`.
- **Duplicates are minor:** `hashbrown` ×3, `thiserror` 1 and 2 (through `mp4`),
  `base64`, `foldhash`.
- **`getrandom`:** 0.3 without `wasm_js` appears only in `tuile-viewer`'s tree
  (convergence).
- **Dead code is stripped:** `nalgebra`, `petgraph` and `image` are in the Worker's
  dependency tree (through `tuile-radiometry` and `tuile-core`) but stripped from its
  binary. By measured code size the top contributors are `core`/`alloc`, then
  `tuile_repository` (13.7 %), the wasm-bindgen describe stubs (12 %, removed by
  wasm-bindgen) and `toml_edit` (10.2 %).
- **Rule 8:**
  - film-web uses `wasm-bindgen`, `web-sys`, `js-sys`, `wasm-bindgen-futures` and
    `console_error_panic_hook`, plus `rav1e` 0.8 (pre-1.0, justified in its
    `Cargo.toml`) and wgpu `webgpu`;
  - WebCodecs is reached through `Reflect` because the `web-sys` bindings sit behind an
    unstable cfg. That is justified in `lib.rs:16-19`;
  - the Worker uses `worker` 0.8, Cloudflare's own crate: pre-1.0, but the only option;
  - `pmtiles` is a patched git fork (documented in the root `Cargo.toml`).
  - No exotic crate otherwise.
- **`unsafe`:** none in the wasm crates (workspace `deny`). Only the generated
  flatbuffers code in `tuile-pack`, allowed and documented; its reader verifies the
  buffer.
- **XSS:** the film-web page sets text with `set_text_content`, and `set_inner_html` is
  only ever called with `""`.
- **No third-party brand in the public API of the wasm crates.**
  - The `CESIUM_ION_TOKEN` placeholder sits in the example pages (both deleted by
    convergence);
  - the ion connector is a connector crate.

## 3. Alignment with the manifesto

- **`tuile-web` + the THREE.js page + `wasm-globe`.** These are a fork:
  - its own consumer bookkeeping, residency, shader and camera;
  - a fourth traversal driver.
  Convergence is the right answer: the engine through `tuile-wgpu` in `tuile-viewer`'s
  web shell, with `tuile-web` as the transport.
- **`tuile-film-web`** shares the *film* engine with native: `tuile-film`,
  `tuile-film-gpu`, `tuile-repository`, `tuile-radiometry` and `tuile-pack` are the
  crates `tuile-film-native` uses. It is not a fork of the film path.
  - **What it forks** is the browser shell: its own page UI in Rust (`page.rs`, 1 979
    lines of DOM), its own `fetch`/retry/cache, and a second JS loader. Against the
    converged web viewer, that is a second web application.
  - **Recommendation:**
    - (1) now, film-web takes its transport from `tuile-web::transport`;
    - (2) after convergence lands, **film playback in the browser becomes a mode of the
      same web package**: one ES module, one wasm module, one worker entry, with a
      `mode: 'film'` that starts the film renderer instead of the interactive one;
    - (3) the renderers merge only when PR #60's renderer convergence does. Until then
      `tuile-film-gpu` stays the film renderer: a mode switch, not a rewrite.
- **`tuile-pack-worker` versus `tuile-tile-server`.** They are not duplicates.
  - `tuile-tile-server` *writes* the PMTiles store, natively.
  - `tuile-pack-worker` is the edge *read* adapter of `tuile_repository::Bench`, the same
    routes `tuile-pack-api` serves natively. That shared-`Bench`, thin-adapter design is
    exactly right.
  - **Keep it.** Fix its security (H1–H3, M1–M2). Do not merge it into the tile server.
- **`site/`** is a static docs Worker with no engine code. **Keep it.**

## 4. Cleanup plan, in order

| # | Item | Size | Covered by |
|---|---|---|---|
| 1 | Rotate the demo Worker's S3 secret. Replace it with an R2 token, Object Read only, scoped to the demo's bucket(s), under distinct variable names (H2) | S (owner action) | — |
| 2 | Scope the Worker's routes: per-project prefix allowlist for `o/`, `b8/` and `ls`; drop `ls` from the Worker; read the remote store with the public credential, or drop `/api/tiles` when `direct = true` (H1, H3) | M | — |
| 3 | Generic error bodies; normalised cache key; `Bench` built once per isolate; listing and `LIVE_MANY` caps sized to the subrequest budget; a CF rate-limit rule (M1, M2) | S | — |
| 4 | Repair `tuile-web`; wasm guard over 12 crates with `-D warnings`; film-web dead code | S | **`fix/tuile-web`** (uncommitted) |
| 5 | CI additions: wasm `clippy`, `wasm-pack build` of film-web, `worker-build` of the Worker, a size budget | S | partly (`fix/tuile-web` adds `check` only) |
| 6 | Host tests for the pure parts of film-web and pack-worker (key/credential logic, cache-key rules); a headless WebGPU smoke test for film-web (M8) | M | — |
| 7 | Core: `web_time` for `metrics::Stamp` and `determinism::elapsed_ms`; an explicit browser byte budget (M5, M6) | S | partly (convergence: `web_time` in viewer/pump, a browser profile) |
| 8 | `tuile-wgpu` pump: `web_time` (M4) | S | **convergence** (design only) |
| 9 | One browser transport (`fetch`, retry, sleep, bounded Cache Storage with cleanup) in `tuile-web`, adopted by film-web; one block-size constant (L2, M7, L7) | M | started by `fix/tuile-web` (`transport.rs`); film-web adoption not covered |
| 10 | `hmac` crate in `s3.rs`; `try_from` in the pack reader; `strip_prefix` in `page.rs`; same-origin `?api=` (L1, L4–L6) | S | — |
| 11 | `[profile.wasm]`; third-party NOTICE for rav1e; site wording; a scripted deploy from a tag (L9–L12) | S | — |
| 12 | Delete the THREE.js page, `tuile-web/src/worker.rs` and `examples/wasm-globe`; the `tuile-viewer` web shell (WebGPU + WebGL2) | L | **convergence** (design only) |
| 13 | Film playback as a mode of the converged web package (one module, one worker entry); renderers merge later with PR #60 | L | — (after 12) |

Items 1 to 3 are urgent: they concern a public, deployed endpoint, and are independent of
everything else.

## 5. Not verified

- **No browser run.**
  - No page was opened.
  - film-web was not rebuilt. Its sizes come from the 2026-10-07 `pkg` in the main
    checkout, not from main.
- **The deployed Worker's version and secrets.**
  - Its version was not checked (the notes say v0.3.3, built from a tag).
  - The key's actual scope was not checked in Cloudflare: H2 rests on the code and the
    deployment notes.
- **The deployed Worker was not probed beyond `GET /api/projects`.**
  - Its 200 answer with the two bucket names was verified.
  - No `ls` was called, and no object was read, so it is unknown what the exposed buckets
    actually contain.
- **The Cloudflare plan (free or paid) is unknown,** so the exact subrequest limit is
  unknown.
- **The pack Worker sizes** come from a local build of main, with `wasm-bindgen` and
  `wasm-opt` run by hand to mimic `worker-build`. They are not from the deployed bundle.
- **No `cargo check` was run for wasm** beyond the pack Worker's release build. The build
  matrix is PR #65's, from the same commit.
- **`fix/tuile-web` is uncommitted working-tree state** and may still change.
  `feat/web-native-convergence` contains no code.
