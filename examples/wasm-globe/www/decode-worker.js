// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// One decode worker: its own instance of the wasm module, decoding and
// reprojecting imagery tiles off the main thread.
//
// A separate instance, NOT shared memory. The alternative — real wasm threads
// over `SharedArrayBuffer` — would let the main thread hand this one a closure,
// and costs `+atomics`, a nightly `build-std`, and COOP/COEP headers on
// whatever serves the page. Here the job is shaped as data instead: encoded
// bytes in, RGBA out, and the only thing crossing is an ArrayBuffer. Plain
// `wasm-pack` output on stable, no headers.
//
// The price is one wasm instance per worker (~a megabyte) and the Bing metadata
// parsed once per worker. Both are paid at startup, once.

import init, { ImageryDecoder } from "../pkg/wasm_globe.js";

let decoder = null;

self.onmessage = async (e) => {
  const msg = e.data;

  if (msg.type === "init") {
    // `init()` is idempotent per worker; the decoder holds the tiling scheme,
    // derived from the same metadata document the main thread gave the Globe,
    // so both sides cannot drift.
    await init();
    decoder = new ImageryDecoder(msg.metadataJson);
    self.postMessage({ type: "ready" });
    return;
  }

  if (msg.type === "decode") {
    const { id, bytes, level, x, y } = msg;
    try {
      const tile = decoder.decode(new Uint8Array(bytes), level, x, y);
      const width = tile.width;
      const height = tile.height;
      // `rgba` consumes the tile and moves the buffer out; transferring it
      // hands the memory over rather than copying a quarter-megabyte per tile.
      const rgba = tile.rgba;
      self.postMessage({ type: "decoded", id, rgba, width, height }, [rgba.buffer]);
    } catch (err) {
      self.postMessage({ type: "failed", id, error: String(err?.message ?? err) });
    }
  }
};
