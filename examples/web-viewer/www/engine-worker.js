// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// The worker shim. It holds the engine and does nothing else.
//
// Every decision — what to load, what to draw, what to drop — is made by the
// Rust `WorkerEngine` inside this worker: the real GeometryServer, the real SSE
// traversal, the real planetary loader. This file is a mailbox, and it is
// deliberately this thin. Logic here would be logic the native viewer does not
// have, and the two would drift.
//
// The page pulls rather than the worker pushing: `drain` is answered once per
// animation frame with whatever accumulated. A worker mid-decode therefore
// cannot stall a frame, and a page that drops a frame cannot lose a tile.

import init, { WorkerEngine } from "../pkg/tuile_web.js";

let engine = null;

self.onmessage = async (e) => {
  const msg = e.data;

  try {
    if (msg.type === "start") {
      await init();
      engine = await WorkerEngine.start(msg.token, msg.maxSse ?? 2.0);
      self.postMessage({ type: "ready" });
      return;
    }

    if (!engine) return;

    if (msg.type === "view") {
      const v = msg.view;
      engine.set_view(
        v.eye[0], v.eye[1], v.eye[2],
        v.dir[0], v.dir[1], v.dir[2],
        v.up[0], v.up[1], v.up[2],
        v.width, v.height, v.fovy,
      );
      return;
    }

    if (msg.type === "drain") {
      const batch = engine.drain();
      // The typed arrays inside came straight out of the wasm heap; listing
      // them as transferables hands the memory to the page instead of copying
      // a mesh's worth of vertices per tile.
      self.postMessage({ type: "batch", id: msg.id, batch }, transferables(batch));
    }
  } catch (err) {
    self.postMessage({ type: "error", message: String(err?.message ?? err) });
  }
};

/// Every ArrayBuffer in a batch, so `postMessage` moves rather than clones.
function transferables(batch) {
  const out = [];
  for (const m of batch) {
    if (m.kind !== "add") continue;
    for (const mesh of m.meshes ?? []) {
      for (const key of ["positions", "normals", "uvs", "indices"]) {
        if (mesh[key]) out.push(mesh[key].buffer);
      }
    }
    for (const layer of m.imagery ?? []) {
      if (layer.rgba) out.push(layer.rgba.buffer);
    }
  }
  return out;
}
