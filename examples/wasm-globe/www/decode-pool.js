// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// A pool of decode workers, and the queue in front of it.
//
// The browser counterpart of `tuile_core::offload::ThreadPool`, and sized the
// same way and for the same reason: `hardwareConcurrency - 1`, leaving a core
// for the thread that runs the engine and paints. Decoding a tile is pure CPU
// over a quarter-megabyte; running one per outstanding fetch would put dozens
// of working sets through a cache that holds a few.
//
// Falls back to decoding on the calling thread when Workers or modules are
// unavailable, which is what `Inline` is natively: slower, never wrong.

const DEFAULT_SIZE = () => Math.max(1, (navigator.hardwareConcurrency || 4) - 1);

export class DecodePool {
  /// `metadataJson` is the Bing metadata document — each worker derives its own
  /// tiling scheme from it, so no scheme is ever serialised by hand.
  constructor(metadataJson, size = DEFAULT_SIZE()) {
    this.metadataJson = metadataJson;
    this.size = size;
    this.workers = [];
    this.idle = [];
    this.queue = [];
    this.pending = new Map();
    this.nextId = 1;
    this.available = false;
  }

  /// Spawns the workers and waits for every one to report ready.
  ///
  /// Resolving before they are armed would send `decode` messages to a worker
  /// whose decoder is still null, and the first mosaic would arrive grey.
  async start() {
    try {
      const ready = [];
      for (let i = 0; i < this.size; i++) {
        const worker = new Worker(new URL("./decode-worker.js", import.meta.url), {
          type: "module",
        });
        worker.onmessage = (e) => this._onMessage(worker, e.data);
        this.workers.push(worker);
        ready.push(
          new Promise((resolve, reject) => {
            worker._ready = resolve;
            worker.onerror = (err) => reject(err);
          }),
        );
        worker.postMessage({ type: "init", metadataJson: this.metadataJson });
      }
      await Promise.all(ready);
      this.available = true;
      return this.size;
    } catch (e) {
      // No Workers, no module workers, or the page was opened from `file://`.
      // The caller decodes inline instead; nothing here is load-bearing.
      this.available = false;
      this.terminate();
      return 0;
    }
  }

  /// Decodes one tile, resolving to `{rgba, width, height}`.
  ///
  /// `bytes` is transferred, not copied — the caller must not touch it after
  /// this call, which is why it is handed a fresh buffer straight from `fetch`.
  decode(bytes, level, x, y) {
    return new Promise((resolve, reject) => {
      const id = this.nextId++;
      this.pending.set(id, { resolve, reject });
      const job = { type: "decode", id, bytes, level, x, y };
      const worker = this.idle.pop();
      if (worker) this._dispatch(worker, job);
      else this.queue.push(job);
    });
  }

  _dispatch(worker, job) {
    worker._job = job.id;
    worker.postMessage(job, [job.bytes]);
  }

  _onMessage(worker, msg) {
    if (msg.type === "ready") {
      this.idle.push(worker);
      if (worker._ready) worker._ready();
      return;
    }

    const waiting = this.pending.get(msg.id);
    this.pending.delete(msg.id);
    if (waiting) {
      if (msg.type === "decoded") {
        waiting.resolve({ rgba: msg.rgba, width: msg.width, height: msg.height });
      } else {
        waiting.reject(new Error(msg.error ?? "decode failed"));
      }
    }

    // Take the next job rather than going idle, so a queue drains at the pace
    // the workers set instead of the pace messages happen to arrive.
    const next = this.queue.shift();
    if (next) this._dispatch(worker, next);
    else this.idle.push(worker);
  }

  terminate() {
    for (const worker of this.workers) worker.terminate();
    this.workers = [];
    this.idle = [];
    this.queue = [];
    for (const { reject } of this.pending.values()) {
      reject(new Error("decode pool terminated"));
    }
    this.pending.clear();
    this.available = false;
  }
}
