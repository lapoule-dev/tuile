// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// One worker, one slice of the film. Rust renders each frame and presents it
// to an OffscreenCanvas (WebGPU); this shim hands the picture to an encoder
// and collects what comes out. Every rendering decision is in Rust.
//
// Encoding is progressive. The browser's own H.264 encoder (WebCodecs) reads
// the canvas directly and is used whenever it exists for the film's size. When
// it does not, the frame is read back as I420 — converted on the GPU — and
// encoded to AV1 by rav1e, compiled into the same wasm as the renderer.

import init, { FilmWorker, SoftEncoder } from "./pkg/tuile_film_web.js";
import { findEncoder } from "./encoder.js";

const post = (msg, transfer = []) => self.postMessage(msg, transfer);

// Both encoders answer to the same four calls: prepare(film) once per pack,
// frame(film, canvas, index, n) after each render, finish(), and result().

// The browser's encoder. It lives on the JS side only because its web-sys
// bindings are still behind an unstable flag.
function browserEncoder(config, fps) {
  let record = null, failure = null;
  const chunks = [];
  const encoder = new VideoEncoder({
    output: (chunk, meta) => {
      const description = meta?.decoderConfig?.description;
      if (description && !record) record = new Uint8Array(description.buffer ?? description).slice();
      const bytes = new Uint8Array(chunk.byteLength);
      chunk.copyTo(bytes);
      chunks.push({ index: Math.round((chunk.timestamp * fps) / 1e6), key: chunk.type === "key", data: bytes });
    },
    error: (e) => { failure = e; },
  });
  encoder.configure(config);
  return {
    name: config.codec,
    prepare() {},
    async frame(film, canvas, index, n) {
      if (failure) throw failure;
      const frame = new VideoFrame(canvas, {
        timestamp: Math.round((index * 1e6) / fps),
        duration: Math.round(1e6 / fps),
      });
      // A key frame first, then one every two seconds of film.
      encoder.encode(frame, { keyFrame: n % (2 * fps) === 0 });
      frame.close();
      while (encoder.encodeQueueSize > 4) {
        await new Promise((r) => encoder.addEventListener("dequeue", r, { once: true }));
      }
    },
    async finish() {
      await encoder.flush();
      encoder.close();
      if (failure) throw failure;
      if (!record) throw new Error("the encoder gave no avcC description");
    },
    result: () => ({ record, chunks }),
  };
}

// rav1e. Its packets are numbered from 0 in the order frames went in, so the
// film's index of this worker's first frame places them.
function softEncoder(width, height, fps, bitrate) {
  const encoder = new SoftEncoder(width, height, fps, bitrate);
  const record = encoder.config();
  const chunks = [];
  let base = null;
  const drain = () => {
    for (let packet; (packet = encoder.next_packet()); ) {
      chunks.push({ index: base + packet.index, key: packet.key, data: packet.take() });
      packet.free();
    }
  };
  return {
    name: "av01 (rav1e)",
    prepare(film) { film.enable_i420(); },
    async frame(film, canvas, index) {
      base ??= index;
      encoder.push(await film.read_i420());
      drain();
    },
    async finish() {
      encoder.finish();
      drain();
      encoder.free();
    },
    result: () => ({ record, chunks }),
  };
}

self.onmessage = async ({ data }) => {
  const { id, packs, filmFirst, width, height, supersample, fps, bitrate, budget } = data;
  try {
    await init();
    const canvas = new OffscreenCanvas(width, height);
    const config = await findEncoder(width, height, fps, bitrate);
    const encoder = config ? browserEncoder(config, fps) : softEncoder(width, height, fps, bitrate);

    const started = performance.now();
    const loaded = { blocks: 0, bytes: 0, inMemory: true };
    let n = 0;
    // This worker's slice of the film, pack by pack. A pack shares nothing
    // with the next — its own tiles, its own imagery — so each gets its own
    // renderer, and only its table and the tiles of the frames wanted here
    // are read from it.
    for (const pack of packs) {
      const film = await FilmWorker.create(canvas, pack.source, pack.first, pack.last, supersample);
      encoder.prepare(film);
      // Every block this slice of the pack reads, fetched before its first
      // frame: rendering then waits on no network. Held in the module's
      // memory when they fit the budget, left to the browser's cache when
      // they do not.
      const ahead = await film.preload(budget, (done, total) => {
        if (done === total || done % 4 === 0) post({ type: "preload", id, done: loaded.blocks + done, total: loaded.blocks + total });
      });
      loaded.blocks += ahead.blocks;
      loaded.bytes += ahead.bytes;
      loaded.inMemory &&= ahead.in_memory;
      ahead.free();
      post({ type: "preload", id, done: loaded.blocks, total: loaded.blocks, bytes: loaded.bytes, inMemory: loaded.inMemory });
      for (;;) {
        const t0 = performance.now();
        const stats = await film.next();
        if (!stats) break;
        const t1 = performance.now();
        await encoder.frame(film, canvas, stats.frame - filmFirst, n);
        const t2 = performance.now();

        const msg = {
          type: "frame", id,
          frame: stats.frame,
          selected: stats.selected, entered: stats.entered, left: stats.left,
          fetch: stats.fetch_ms, fetchedBytes: stats.fetched_bytes, requests: stats.requests,
          unpack: stats.unpack_ms, decode: stats.decode_ms, upload: stats.upload_ms,
          record: stats.record_ms, next: t1 - t0, encode: t2 - t1,
        };
        stats.free();
        if (n % 8 === 0) {
          const preview = await createImageBitmap(canvas, { resizeWidth: 480, resizeQuality: "medium" });
          msg.preview = preview;
          post(msg, [preview]);
        } else {
          post(msg);
        }
        n++;
      }
      film.free();
    }
    await encoder.finish();
    const { record, chunks } = encoder.result();
    post(
      { type: "done", id, codec: encoder.name, record, chunks, seconds: (performance.now() - started) / 1000 },
      [record.buffer, ...chunks.map((c) => c.data.buffer)],
    );
  } catch (e) {
    post({ type: "error", id, message: String(e?.message ?? e) });
  }
};
