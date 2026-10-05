// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// One worker, one slice of the film. Rust renders and presents each frame to
// an OffscreenCanvas (WebGPU); this shim turns the canvas into a VideoFrame
// for WebCodecs and nothing else. The encoder lives here only because its
// web-sys bindings are still unstable — every rendering decision is in Rust.

import init, { FilmWorker } from "./pkg/tuile_film_web.js";

const post = (msg, transfer = []) => self.postMessage(msg, transfer);

// High profile, from level 4.0 up: the first one the browser accepts wins.
const CODECS = ["avc1.640028", "avc1.640032", "avc1.640033", "avc1.4d0033", "avc1.42e033"];

async function encoderConfig(width, height, fps, bitrate) {
  for (const codec of CODECS) {
    const config = {
      codec, width, height, bitrate, framerate: fps,
      avc: { format: "avc" },
      latencyMode: "quality",
    };
    const { supported } = await VideoEncoder.isConfigSupported(config);
    if (supported) return config;
  }
  throw new Error(`no H.264 encoder for ${width}×${height}`);
}

self.onmessage = async ({ data }) => {
  const { id, source, first, last, filmFirst, width, height, supersample, fps, bitrate } = data;
  try {
    await init();
    // The source is a URL on the pack API or a Blob shared with the page:
    // either way, this worker reads only the table and its own tiles.
    const canvas = new OffscreenCanvas(width, height);
    const film = await FilmWorker.create(canvas, source, first, last, supersample);

    let avcc = null;
    let failure = null;
    const chunks = [];
    const encoder = new VideoEncoder({
      output: (chunk, meta) => {
        const description = meta?.decoderConfig?.description;
        if (description && !avcc) {
          avcc = new Uint8Array(description.buffer ?? description).slice();
        }
        const bytes = new Uint8Array(chunk.byteLength);
        chunk.copyTo(bytes);
        chunks.push({
          index: Math.round((chunk.timestamp * fps) / 1e6),
          key: chunk.type === "key",
          data: bytes,
        });
      },
      error: (e) => { failure = e; },
    });
    const config = await encoderConfig(width, height, fps, bitrate);
    encoder.configure(config);

    const started = performance.now();
    let n = 0;
    for (;;) {
      if (failure) throw failure;
      const t0 = performance.now();
      const stats = await film.next();
      if (!stats) break;
      const t1 = performance.now();
      const index = stats.frame - filmFirst;
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
    await encoder.flush();
    encoder.close();
    if (failure) throw failure;
    if (!avcc) throw new Error("the encoder gave no avcC description");
    post(
      { type: "done", id, codec: config.codec, avcc, chunks, seconds: (performance.now() - started) / 1000 },
      [avcc.buffer, ...chunks.map((c) => c.data.buffer)],
    );
  } catch (e) {
    post({ type: "error", id, message: String(e?.message ?? e) });
  }
};
