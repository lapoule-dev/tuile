// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// The globe viewer as an ES module: `createViewer(container, options)`.
//
// The viewer itself is Rust compiled to WebAssembly — the same application,
// renderer and controls as the desktop's — drawing with WebGPU, or WebGL2
// where the browser has no WebGPU. Its geometry server runs in a worker this
// module starts. This file only normalises the options, places the canvas and
// the attribution, and turns the wasm module's events into a typed API.

import init, { startViewer } from "./wasm/tuile_viewer.js";

/** No graphics backend this viewer can draw with: neither WebGPU nor WebGL2. */
export class UnsupportedError extends Error {
  constructor(message) {
    super(message);
    this.name = "UnsupportedError";
  }
}

/** The viewer could not start: wrong options, or a session that failed. */
export class StartError extends Error {
  constructor(message) {
    super(message);
    this.name = "StartError";
  }
}

/** The phone's launch colour, the colour of the ground before any arrives. */
const LAUNCH_COLOUR = "rgb(23, 38, 61)";

const EVENTS = ["ready", "loading", "error", "viewChanged", "attributionChanged"];

/** The viewer of the moment: one per page, and a new one replaces it. */
let current = null;

let loaded;
/** Loads and instantiates the wasm module once per page. */
function load() {
  loaded ??= init();
  return loaded;
}

/** `auto` becomes `handheld` on a touch-first, small screen. */
function resolveProfile(profile) {
  if (profile === "desktop" || profile === "handheld") return profile;
  const coarse = globalThis.matchMedia?.("(pointer: coarse)").matches ?? false;
  const small = Math.min(screen.width, screen.height) < 900;
  return coarse && small ? "handheld" : "desktop";
}

/**
 * Creates a viewer in `container`: an element the viewer fills with a canvas
 * of its own, or a canvas to draw into.
 *
 * Resolves once the viewer draws; rejects with `UnsupportedError` when the
 * browser cannot, and with `StartError` when the options are wrong.
 */
export async function createViewer(container, options) {
  if (!(container instanceof HTMLElement)) {
    throw new TypeError("createViewer: the container must be an element");
  }
  const o = options ?? {};
  // While the module loads, the container shows the colour the globe opens on,
  // as the phone's launch screen does: never a black frame. A host's own
  // background is left alone.
  const host = container instanceof HTMLCanvasElement ? container.parentElement : container;
  if (host && getComputedStyle(host).backgroundColor === "rgba(0, 0, 0, 0)") {
    host.style.backgroundColor = LAUNCH_COLOUR;
  }
  await load();
  // Before the new canvas exists, so the two never share the page.
  current?.destroy();

  let canvas = container;
  let ownCanvas = false;
  if (!(container instanceof HTMLCanvasElement)) {
    canvas = document.createElement("canvas");
    canvas.style.cssText = "display:block;width:100%;height:100%;touch-action:none;outline:none";
    canvas.tabIndex = 0;
    container.append(canvas);
    ownCanvas = true;
  }

  // The catalog proxy's and the terrain's addresses, resolved against the
  // page: the worker that asks them has a base URL of its own. (Imagery
  // templates are left as given: resolving one would escape its braces.)
  const resolve = (url) => new URL(url, location.href).href;
  let sources = o.sources;
  if (sources?.catalog?.url) {
    sources = { ...sources, catalog: { ...sources.catalog, url: resolve(sources.catalog.url) } };
  }
  if (typeof sources?.terrain?.url === "string") {
    sources = { ...sources, terrain: { ...sources.terrain, url: resolve(sources.terrain.url) } };
  }
  const normalized = {
    name: o.name ?? "Globe",
    sources,
    profile: resolveProfile(o.profile ?? "auto"),
    deviceMemory: navigator.deviceMemory,
    backend: o.backend ?? "auto",
    view: o.view,
    maxAltitude: o.maxAltitude,
    ui: o.ui ?? true,
    attribution: o.attribution ?? "bottom-right",
    showUnsupportedMessage: o.showUnsupportedMessage ?? true,
    rememberView: o.rememberView ?? true,
    decodeWorkers: o.decodeWorkers,
    imageryCompression: o.imageryCompression ?? "off",
    time: o.time,
    timeOffset: o.timeOffset,
    timeRate: o.timeRate,
    tilesets: o.tilesets ?? [],
  };

  const listeners = new Map(EVENTS.map((kind) => [kind, new Set()]));
  let settle;
  const drawing = new Promise((resolve, reject) => {
    settle = { resolve, reject };
  });
  const onEvent = (kind, detail) => {
    // A session that cannot start yet is said over the view and tried
    // again, as on a phone: it is an `error` event, not a failed creation.
    if (kind === "ready") {
      settle.resolve();
    } else if (kind === "unsupported") {
      settle.reject(new UnsupportedError(detail.message));
      return;
    }
    for (const listener of listeners.get(kind) ?? []) {
      try {
        listener(detail);
      } catch (e) {
        console.error(e);
      }
    }
  };

  // Written out here, literally, so a bundler sees the worker script and
  // ships it with everything it imports.
  const makeWorker = () => new Worker(new URL("./worker.js", import.meta.url), { type: "module" });

  let handle;
  try {
    handle = startViewer(canvas, normalized, makeWorker, onEvent);
  } catch (e) {
    if (ownCanvas) canvas.remove();
    throw new StartError(e?.message ?? String(e));
  }
  const viewer = new Viewer(handle, listeners, () => {
    if (ownCanvas) canvas.remove();
    if (current === viewer) current = null;
  });
  current = viewer;
  try {
    await drawing;
  } catch (e) {
    viewer.destroy();
    throw e;
  }
  return viewer;
}

/** A viewer on the page. */
class Viewer {
  #handle;
  #listeners;
  #cleanup;

  constructor(handle, listeners, cleanup) {
    this.#handle = handle;
    this.#listeners = listeners;
    this.#cleanup = cleanup;
  }

  /** `"webgpu"` or `"webgl2"`: what the viewer draws through. */
  get backend() {
    const b = this.#handle?.backend;
    return b === "webgpu" ? "webgpu" : b ? "webgl2" : undefined;
  }

  /** Puts the eye there at once. */
  setView(view) {
    this.#live().goTo(view.lon, view.lat, view.altitude, view.heading, view.pitch, undefined);
  }

  /** Glides there, over `duration` seconds (2.5 unless said). */
  flyTo(view, { duration = 2.5 } = {}) {
    this.#live().goTo(view.lon, view.lat, view.altitude, view.heading, view.pitch, duration);
  }

  /** The view the last frame was drawn from. */
  getView() {
    return this.#live().getView();
  }

  /** Calls `listener` on each `event`; returns the function that stops it. */
  on(event, listener) {
    const set = this.#listeners.get(event);
    if (!set) throw new TypeError(`unknown event ${JSON.stringify(event)}`);
    set.add(listener);
    return () => set.delete(listener);
  }

  /**
   * The canvas size is observed, so a layout change needs nothing; call this
   * after changing the container's size in a way no layout sees, and the next
   * frame takes the new size.
   */
  resize() {
    this.#live().redraw();
  }

  /** Stops drawing and holds the loads, as a hidden tab does. */
  pause() {
    this.#live().pause();
  }

  /** Draws and loads again. */
  resume() {
    this.#live().resume();
  }

  /** Releases the GPU, the worker and every listener. The viewer is unusable after. */
  destroy() {
    if (!this.#handle) return;
    this.#handle.destroy();
    this.#handle.free();
    this.#handle = null;
    for (const set of this.#listeners.values()) set.clear();
    this.#cleanup();
  }

  #live() {
    if (!this.#handle) throw new Error("the viewer has been destroyed");
    return this.#handle;
  }
}
