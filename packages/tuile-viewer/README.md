<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# @lapoule-dev/tuile-viewer

A streaming terrain and imagery globe for any web page or application.

The viewer is the same Rust application as the desktop and phone viewers, compiled to
WebAssembly. It has the same renderer, on-screen controls, menu, gestures, keys, loading
indicator, caption and alerts, and the same guarantees: the ground is never drawn black,
and new tiles replace old ones without a gap. It draws with WebGPU, or with WebGL2 where the
browser has no WebGPU. Its geometry server runs in a Web Worker, so decoding never blocks
the page. **The page never holds a token**: tiles come through a catalog proxy that keeps
it on the server.

## Try it

From a checkout of the repository, with `TUILE_ION_TOKEN` in its `.env`:

```sh
scripts/try-viewer.sh web            # http://localhost:8091/examples/web-viewer/
scripts/try-viewer.sh web --open     # … and open it in Chrome
scripts/try-viewer.sh web --lan      # for a phone on the same Wi-Fi (WebGL2 over HTTP)
scripts/try-viewer.sh web --lan --https   # … with WebGPU, location and clipboard (mkcert)
```

Add `?backend=webgl2` to the page's URL to force WebGL2. The script builds the package
and starts `tuile-catalog-proxy`, which serves the page and holds the token; it never
reaches the browser, on this machine or on a phone.

## Install

```sh
npm install @lapoule-dev/tuile-viewer
```

The package is an ES module and resolves its own worker and wasm files with
`new URL(…, import.meta.url)`. It works:

- with Vite, webpack 5, esbuild or Rollup, with no extra configuration;
- from a plain `<script type="module">` that serves the package directory as is.

To build it from this repository, run `packages/tuile-viewer/build.sh`. It needs the
`wasm32-unknown-unknown` target and `wasm-bindgen-cli` at the version in `Cargo.lock`.

## Use

```js
import { createViewer, UnsupportedError } from "@lapoule-dev/tuile-viewer";

try {
  const viewer = await createViewer(document.querySelector("#globe"), {
    view: { lon: 6.86, lat: 45.83, altitude: 12000, heading: 120, pitch: 35 },
    sources: {
      catalog: { url: "/catalog/" },
      terrain: { asset: 1 },
      imagery: [{ key: "aerial", name: "Aerial", asset: 2, attribution: "© …" }],
    },
  });
  viewer.on("viewChanged", (view) => console.log(view));
  viewer.flyTo({ lon: 7.66, lat: 45.98, altitude: 9000 });
} catch (e) {
  if (!(e instanceof UnsupportedError)) throw e;
}
```

`createViewer(container, options)` fills `container` with a canvas, or draws into it if it
is a canvas. It resolves once the viewer draws.

## Options

| Option | Meaning |
|---|---|
| `sources.catalog` | `{ url }`: the catalog proxy that resolves asset numbers to tile endpoints and holds the catalog's token on the server. Relative to the page unless absolute. Required only when a source is named by `asset`. |
| `sources.terrain` | `{ asset }`, the terrain from the catalog, or `{ url }`, the address of a quantized-mesh `layer.json` (relative to the page unless absolute; no token, and the server must answer with `Access-Control-Allow-Origin`). Its tiles are addressed as the `layer.json` says: TMS, row 0 in the south. |
| `sources.imagery` | The layers, in order. Each has `key`, `name` and `attribution`, plus either a catalog `asset` or a URL `template` (`…/{z}/{x}/{y}.png`) with an optional `scheme` (`webmercator` or `geographic`) and `maxLevel`. |
| `name` | Your application's name, used in messages. |
| `profile` | `auto` (the default), `desktop` or `handheld`: memory budget, upload pacing and frame rate. |
| `backend` | `auto` (the default: WebGPU, else WebGL2), `webgpu` or `webgl2`. |
| `view` | Where the eye opens: `lon`, `lat`, `altitude`, and optionally `heading` and `pitch`. Without it, where the last viewer of this `name` on this site was left. |
| `maxAltitude` | The highest the eye may go, metres above the ellipsoid (`1` to `1e8`). A zoom out slows into it; a `view`, `setView` or `flyTo` above it is held to it. By default twice the distance at which the whole globe fits the view: 36 043 km. |
| `ui` | Whether the compass, zoom controls and menu are drawn (`true` by default). |
| `attribution` | The bottom corner of the caption — whose pictures are on screen, and the last thing the viewer said: `bottom-right` (the default) or `bottom-left`. |
| `showUnsupportedMessage` | Whether a viewer that cannot draw says so in the container (`true` by default). |
| `rememberView` | Whether a viewer created without `view` opens where the last one was left, kept in the site's local storage (`true` by default). |
| `decodeWorkers` | The most Web Workers that decode imagery and terrain beside the geometry server's: one per core less one, up to this (`4` by default). `0` decodes on the geometry server's own worker. They exchange transferred buffers: no `SharedArrayBuffer`, so no cross-origin isolation headers are needed. |
| `imageryCompression` | How imagery is held on the GPU: `"off"` (the default: RGBA8), `"auto"`, `"bc7"` or `"astc"`. Compressed imagery takes a quarter of the memory. `"auto"` picks the best format the page's device samples: under WebGPU, BC7 or ASTC 4×4 encoded on the GPU; under WebGL2, which has no compute, ASTC 4×4 (phones) or BC7 (desktops) encoded in the decode workers. A device that holds neither stays RGBA8, and a tile that cannot be encoded is held as RGBA8, never left out. The caption says what is held: `imagery ASTC 4x4 23 MiB (276 × 85 KiB)`. |
| `time` | The instant the scene is lit for — where the sun, the Moon and the stars stand: `"now"` (the default), `"sunset"` or `"sunrise"` (the next one over the opening `view`), or an RFC 3339 instant (`"2026-06-21T19:42:00Z"`). |
| `timeOffset` | Added to `time`: a signed duration, `"-30m"`, `"-3h30m"`, `"45m"`, `"90s"`. |
| `timeRate` | Scene seconds a second: `1` (real time, the default), `30`, `0` (frozen), negative to run backwards. |

`time`, `timeOffset` and `timeRate` are the desktop viewer's `--time`,
`--time-offset` and `--time-rate`, and the parameters of the same names in
its `tuile://goto?…` links, read by one parser: a value one accepts, the
others accept. The example page (`examples/web-viewer`) takes them from its
query with the view, under the link's names, so the query of a link opens
the same view at the same moment:

```text
?lon=0.1418&lat=42.9365&altitude=3100&heading=290&pitch=0&time=sunset&timeOffset=-30m&timeRate=30
```

— half an hour before sunset at the Pic du Midi, an hour of the scene every
two minutes. In a query `+` reads as a space: write an instant's zone as `Z`
(or `%2B02:00`), and a positive offset without its sign.

## The viewer

| Member | Does |
|---|---|
| `backend` | `"webgpu"` or `"webgl2"`. |
| `setView(view)` | Puts the eye there at once. |
| `flyTo(view, { duration })` | Glides there: up, across and down. |
| `getView()` | The view of the last frame. |
| `on(event, listener)` | Subscribes; returns the function that unsubscribes. |
| `resize()` | Draws a frame now. The canvas size is observed, so this is rarely needed. |
| `pause()` / `resume()` | Holds drawing and loading, as a hidden tab does (that one is automatic). |
| `diagnostic(view)` | Draws a diagnostic view until another is asked — `normal`, `unlit`, `coverage`, `geometry`, `levels`, `lag` or `cap`, the views the `D` key cycles. `levels` colours each tile by the imagery level it wears, one hue a level round a wheel of eight (red 8 and 16, orange 9, yellow 10, green 11, cyan 12, blue 13, violet 14, magenta 15); `lag` colours each tile by how far under the level it asked for it wears (green at it, yellow one under, orange two, red three or more, blue finer than asked); `cap` colours each tile by the levels its terrain's level denies its imagery — the screen asks finer than the terrain level plus the imagery cap allows, so the tile is soft while green in `lag` (green none, yellow one, orange two, red three or more). Tiles are edged dark, and a mesh drawn in another's place is hatched. |
| `destroy()` | Releases the GPU device, the worker and every listener, and removes the canvas the viewer created. |

`debug` is reserved for the graphics budget's control API (budgets, GPU figures, frame
readback, scripted moves). It is not available yet.

## Events

| Event | Detail |
|---|---|
| `ready` | `{ backend }`: the viewer draws. |
| `loading` | `{ shown, progress }`: the loading indicator came or went, or its progress moved. |
| `error` | `{ title, message }`: something the person should hear about, such as a session that could not start. It is also shown over the view, and the viewer keeps retrying. |
| `viewChanged` | `{ lon, lat, altitude, heading, pitch }`, a few times a second at most. |
| `attributionChanged` | `{ layer, name, attribution }`: whose pictures are on screen. |

Errors:

- `createViewer` rejects with `UnsupportedError` when the browser can draw with neither
  WebGPU nor WebGL2, or not with the backend that was asked for; the container says why;
- it rejects with `StartError` when the options are wrong.

## What the person sees

Everything is the viewer's own, drawn from the same Rust code as the desktop and the phone:

- the compass, tilt handle and zoom buttons, in the safe area;
- the menu, from a button in the top right corner: the desktop's View, Go and Imagery menus
  item for item, with the key of each, plus Keys and About;
- mouse and keys (drag, right-drag, wheel, N, L, C, V, I, W, D, F) and touch (one finger,
  pinch, twist, two-finger tilt, double tap, hold for the current location);
- the loading indicator while the first view is covered, and in a corner after a jump;
- the caption along the bottom: whose pictures are on screen, and what was just said;
- alerts over the view: a session that cannot start, a location refused, the keys.

The current location uses the browser's Geolocation API, which asks the person first and
needs a secure page (HTTPS, or `localhost`).

## Browser support

**WebGPU first, WebGL2 as the fallback.** Current Chrome, Edge and Safari have one or the
other, and so does Firefox. Known differences on WebGL2:

- a drape binds at most 15 imagery layers per pass instead of up to 25, so a few tiles
  are drawn in two passes;
- there are no GPU timestamps;
- surfaces are always drawn through an sRGB view.

The picture is otherwise the same: the same shaders, translated by wgpu.

## The catalog proxy and its token

A page cannot keep a secret: any token it holds, anyone can read. So the package takes
none. The page names a **catalog proxy** in `sources.catalog.url`; the proxy holds the
token and answers the catalog's endpoints and tiles without it.

- `tuile-catalog-proxy` (`examples/catalog-proxy`) is that proxy, the same binary for
  development and production. It answers only for the assets it is given (`--assets`),
  only three routes, and refuses to pass on any reply that would carry the token, an asset
  token or an imagery key. The imagery tiles themselves carry no key and are fetched from
  the imagery service directly.
- It reads the token from the environment variable `TUILE_ION_TOKEN`.
- **In production** the token is a secret of the deployment, never part of the front end:
  declare it as a secret configuration value (for example `pulumi config set --secret
  catalogToken …`), have the stack put it in the secret store of the platform (a secret
  manager entry, or a Kubernetes secret), and expose it to the proxy's process as
  `TUILE_ION_TOKEN`. Serve the proxy behind TLS, set `--public-url` to its public address
  and `--allow-origin` to the pages that may use it.

## Not yet

- No persistent tile cache in the browser. The worker keeps tiles in memory, and a browser
  store comes with the graphics budget.
- No atmosphere's sky model beyond the shared aerial perspective, no overlays and no
  stereo yet: they come with the shared layers.
- One viewer per page at a time: a second `createViewer` destroys the first.

## License

MIT OR Apache-2.0.
