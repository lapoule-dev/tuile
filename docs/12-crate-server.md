# 12 — `tuile-server` spec

`tuile-server` is the **network middleware** for the core's logical geometry server (see `01-architecture.md`) — the geometry server is not a network server, it is this crate that exposes it on the network — plus a static "Martin of 3D Tiles" mode for interop and CDN. One binary, multiple sources, a removable cache, deployable from the laptop to the Cloudflare Worker.

## The two modes

### Streaming mode (the primary mode)

Streaming mode is NOT defined here: it is defined in the core, as a trait (`GeometryStream`, see `01-architecture.md`). This crate only implements its **network binding**: one WebSocket session per viewer — the client sends `Hello` / `ViewerState` / `Ack` / `Cancel`; the middleware runs one core `GeometryServer` per session and serializes its outputs (`Select` / `Content` / `Evict` / `Error`).

- **The serialization format for geometry on the wire is glb**: standard, compact, compressions (Draco/meshopt/KTX2) preserved. No homegrown binary format. The client binding (`WsStream`) decodes with the core's `content::decode()` — decoding always lives on the consumer side.
- The message envelope is minimal and versioned from the first byte (compact header + payload; exact format frozen in M2).
- Transport v1: WebSocket (axum/tokio-tungstenite native; web-sys on wasm clients — stable crates only). WebTransport/Connect = post-v1 tracks.
- Consumers, in order: 1) `wgpu-viewer --remote ws://…` — end-to-end proof and test tool; 2) **USD, notably the Hydra renderer** via the `tuile-hydra` facade (post-v1) — the server still does not know USD: it pushes 3D Tiles, the client materializes.
- Sessions: state bounded per session (client-side residency tracked via `Ack`/`Evict`), inactivity timeout, configurable max number of sessions.
- The server cache remains a source-bytes cache, shared with the static mode (the server decodes nothing in this mode: traversal + byte shuffling — a session costs little, it scales).
- Workers: streaming requires persistent sessions → Durable Objects track, non-blocking. Streaming may be native-only in M2; Workers serve the static mode.

### Static mode

Cacheable HTTP GET: tileset.json + contents. This is interop (any OGC 3D Tiles client: existing web viewers, three.js 3DTilesRenderer…) and CDN/Workers deployment. From the Tuile consumer's point of view, this mode is just the pull implementation of the same protocol (`HttpPullStream`: the `GeometryServer` runs on the client side and pulls the files via GET). Philosophy of THIS mode: **the server does nothing the CDN could do — except when there is no CDN.**

## Internal architecture

```
Router (axum native | worker cloudflare)
   ├── GET /ws/{id} (streaming) ──► Session { GeometryServer (core) + glb serialization }
   ▼
Static service (shared logic, independent of the HTTP framework)
   ├── TilesetRegistry: id → Source (resolution, lazy validation of tileset.json via core)
   ├── TileCache (trait, see core): Noop | MemoryLru | Disk | CacheApi/R2
   └── Storage (trait): LocalFs | ObjectStore (S3/GCS/R2 via the object_store crate)
```

The service logic is written ONCE, framework-agnostic: `async fn get_tileset_json(...)`, `async fn get_content(...)` functions taking `&dyn Storage`, `&dyn TileCache` and returning a `ServiceResponse { status, headers, body }` type. axum and worker are only adapters. This is what makes the Workers target trivial instead of a fork.

## Routes

| Route | Behavior |
|---|---|
| `GET /healthz` | 200, version, uptime |
| `GET /tilesets` | list of registered tilesets (JSON) |
| `GET /tilesets/{id}/tileset.json` | from Storage; `Cache-Control: max-age=60`; ETag |
| `GET /tilesets/{id}/{path...}` | content (glb/b3dm/subtree); cache→storage; `immutable, max-age=31536000`; sha256 ETag; Range supported; correct Content-Type (`model/gltf-binary`, etc.) |
| `GET /metrics` | `metrics` feature: Prometheus counters (cache hits/miss, bytes served, latencies) |

CORS: permissive by default (`*`) because the consumers are third-party web viewers; restrictable via config.

## Configuration

Minimal CLI + optional TOML file:

```bash
tuile serve ./data                          # fs, NoopCache, :8080
tuile serve s3://bucket/prefix --cache lru --cache-budget 1GiB
tuile serve ./data --cache disk --cache-dir /var/cache/tuile --port 9000
```

```toml
[server]      port = 8080, cors = ["*"]
[cache]       kind = "lru" | "noop" | "disk", budget = "512MiB", dir = "..."
[[tilesets]]  id = "demo", source = "s3://bucket/demo/"
```

Principle: zero mandatory config — `tuile serve <dir>` must work immediately, auto-discovery of the directory's `tileset.json` files (each subfolder containing a tileset.json becomes an id).

## Cloudflare Workers target (`cloudflare` feature)

- `worker` crate, built via `worker-build`. The shared Service is compiled as-is (it is wasm-clean as long as Storage/Cache are).
- Storage: R2 binding. Cache: `NoopCache` by default (the Cloudflare CDN + immutable headers do the work); Cache API as an option.
- No DiskCache nor persistent MemoryLru (ephemeral isolates) — this is exactly the "cache removable depending on the runtime environment" case.
- Ship `examples/cloudflare/`: wrangler.toml, end-to-end instructions (create R2 bucket, upload fixture, deploy, consume from a standard web viewer).

## Explicitly out of scope

The server NEVER depends on `tuile-usd` and exposes no USD route. It serves the 3D Tiles standard, period. USDZ export lives in the `tuile-usd` lib/CLI (see 13-crate-usd.md); if an HTTP export service ever becomes necessary, it will be a separate binary composing the lib — never a feature of the tile server.

## Security and robustness

- Path traversal: the `{path...}` are resolved WITHIN the tileset prefix, rejection of `..`, strict validation. Dedicated test.
- Limits: configurable max response size, storage timeout, simple per-IP 429 as an optional feature (not in v1 core).
- Never panic on network input: everything is `Result`, errors → clean status codes (400/404/413/502).

## Tests

- Streaming: end-to-end WS session on a fixture (Hello → ViewerState → reception of Select/Content consistent with the local traversal on the same camera — parity tested programmatically); cancellation; session timeout; malformed messages → clean Error, never a panic.
- Integration (native): ephemeral server on the fixtures, assertions on status/headers/ETag/304/Range/CORS/path traversal.
- Consumer conformance: `tests/html/` pages (a reference 3D Tiles web viewer + three.js 3DTilesRenderer, via CDN) pointing at localhost — documented manual verification, to run before each release.
- Cache: exact LRU budget, hit/miss, Noop passthrough, key includes the tileset's ETag (invalidation if the source changes).
