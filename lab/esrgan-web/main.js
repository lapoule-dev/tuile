// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
import init, { Upscaler } from "./pkg/esrgan_web.js";

const $ = (id) => document.getElementById(id);
const log = (msg) => { $("log").textContent += msg + "\n"; console.log("[esrgan]", msg); };
const fact = (k, v, cls = "") => {
  const tr = $("facts").insertRow();
  tr.insertCell().textContent = k;
  const td = tr.insertCell();
  td.textContent = v;
  if (cls) td.className = cls;
};
const bytes = async (url) => new Uint8Array(await (await fetch(url)).arrayBuffer());
const floats = async (url) => new Float32Array(await (await fetch(url)).arrayBuffer());

// Images as { w, h, rgba }.
const fromRgb = (rgb, w, h) => {
  const rgba = new Uint8ClampedArray(w * h * 4);
  for (let i = 0; i < w * h; i++) { rgba.set(rgb.subarray(3 * i, 3 * i + 3), 4 * i); rgba[4 * i + 3] = 255; }
  return { w, h, rgba };
};
const fromChw = (chw, w, h) => {
  const rgba = new Uint8ClampedArray(w * h * 4), n = w * h;
  for (let i = 0; i < n; i++) {
    for (let c = 0; c < 3; c++) rgba[4 * i + c] = Math.round(Math.min(1, Math.max(0, chw[c * n + i])) * 255);
    rgba[4 * i + 3] = 255;
  }
  return { w, h, rgba };
};
const fromPng = async (url) => {
  const img = await createImageBitmap(await (await fetch(url)).blob(), { colorSpaceConversion: "none" });
  const c = new OffscreenCanvas(img.width, img.height), g = c.getContext("2d");
  g.drawImage(img, 0, 0);
  return { w: img.width, h: img.height, rgba: g.getImageData(0, 0, img.width, img.height).data };
};

// Bicubic convolution (Keys, a = -0.5), pixel centres aligned, edges clamped.
function bicubic(rgb, w, h, s) {
  const W = w * s, H = h * s, out = new Uint8Array(W * H * 3);
  const k = (x) => { x = Math.abs(x); return x <= 1 ? 1.5 * x ** 3 - 2.5 * x ** 2 + 1 : x < 2 ? -0.5 * x ** 3 + 2.5 * x ** 2 - 4 * x + 2 : 0; };
  const taps = (n, N) => Array.from({ length: N }, (_, o) => {
    const x = (o + 0.5) / s - 0.5, x0 = Math.floor(x);
    return [-1, 0, 1, 2].map((d) => [Math.min(n - 1, Math.max(0, x0 + d)), k(x - (x0 + d))]);
  });
  const tx = taps(w, W), ty = taps(h, H);
  for (let Y = 0; Y < H; Y++) for (let X = 0; X < W; X++) for (let c = 0; c < 3; c++) {
    let v = 0;
    for (const [yy, wy] of ty[Y]) for (const [xx, wx] of tx[X]) v += wy * wx * rgb[3 * (yy * w + xx) + c];
    out[3 * (Y * W + X) + c] = Math.round(Math.min(255, Math.max(0, v)));
  }
  return out;
}

const shown = {};
function draw() {
  const z = Number($("zoom").value);
  for (const [id, im] of Object.entries(shown)) {
    const cw = Math.round(im.w / z), ch = Math.round(im.h / z), x0 = (im.w - cw) >> 1, y0 = (im.h - ch) >> 1;
    const c = $(id);
    c.width = cw; c.height = ch;
    const crop = new ImageData(cw, ch);
    for (let y = 0; y < ch; y++) crop.data.set(im.rgba.subarray(4 * ((y0 + y) * im.w + x0), 4 * ((y0 + y) * im.w + x0 + cw)), 4 * y * cw);
    c.getContext("2d").putImageData(crop, 0, 0);
  }
}
$("zoom").onchange = draw;

// Output against the PyTorch reference: raw floats, and as the 8-bit image both would show.
function compare(a, b) {
  let max = 0, sum = 0, se = 0;
  for (let i = 0; i < a.length; i++) {
    const d = Math.abs(a[i] - b[i]);
    max = Math.max(max, d); sum += d;
    const q = Math.round(Math.min(1, Math.max(0, a[i])) * 255) - Math.round(Math.min(1, Math.max(0, b[i])) * 255);
    se += q * q;
  }
  const mse = se / a.length;
  return { max, mean: sum / a.length, psnr8: mse === 0 ? Infinity : 10 * Math.log10(255 * 255 / mse) };
}

async function main() {
  const result = (window.__result = {});
  if (!navigator.gpu) { fact("WebGPU", "not available in this browser", "bad"); return; }
  const adapter = await navigator.gpu.requestAdapter({ powerPreference: "high-performance" });
  const info = adapter.info ?? {};
  result.adapter = { vendor: info.vendor, architecture: info.architecture, device: info.device, description: info.description };
  const L = adapter.limits;
  result.limits = Object.fromEntries(["maxBufferSize", "maxStorageBufferBindingSize", "maxComputeWorkgroupStorageSize",
    "maxComputeInvocationsPerWorkgroup", "maxComputeWorkgroupSizeX", "maxStorageBuffersPerShaderStage"].map((k) => [k, L[k]]));
  result.features = [...adapter.features].sort();
  fact("Adapter", [info.vendor, info.architecture, info.device, info.description].filter(Boolean).join(" · ") || "(not exposed)");
  fact("Limits", Object.entries(result.limits).map(([k, v]) => `${k} ${v}`).join(", "));
  fact("Features", result.features.join(", "));

  const meta = await (await fetch("data/meta.json")).json();
  $("what").textContent = `Burn 0.22 compiled to wasm, on the browser's GPU · tile ${meta.tile} · ${meta.params.toLocaleString()} parameters · ${meta.num_conv} body convolutions`;
  const [weights, rgb, ref] = await Promise.all([floats("data/weights.bin"), bytes("data/input.rgb"), floats("data/ref.f32")]);
  const w = 256, h = 256;

  let t = performance.now();
  await init();
  const wasmMs = performance.now() - t;
  t = performance.now();
  const net = await Upscaler.create(weights);
  const createMs = performance.now() - t;
  const s = net.scale();
  log(`wasm ${wasmMs.toFixed(0)} ms, device + weights ${createMs.toFixed(0)} ms`);

  t = performance.now();
  let out = await net.run(rgb, w, h);
  const cold = performance.now() - t;
  log(`cold run ${cold.toFixed(0)} ms`);
  const warm = [];
  for (let i = 0; i < 5; i++) {
    t = performance.now();
    out = await net.run(rgb, w, h);
    warm.push(performance.now() - t);
  }
  warm.sort((a, b) => a - b);
  const warmMs = warm[2];
  log(`warm runs ${warm.map((v) => v.toFixed(0)).join(", ")} ms`);

  const d = compare(out, ref);
  Object.assign(result, { wasmMs, createMs, coldMs: cold, warmMs, warmAll: warm, diff: d, referenceSeconds: meta.reference_seconds });
  fact("Timings", `wasm load ${wasmMs.toFixed(0)} ms · device + weights ${createMs.toFixed(0)} ms · cold ${cold.toFixed(0)} ms · warm ${warmMs.toFixed(0)} ms (median of 5, 256² → 1024², readback included) · PyTorch CPU reference ${(meta.reference_seconds * 1000).toFixed(0)} ms`);
  fact("vs PyTorch", `max |Δ| ${d.max.toExponential(2)} · mean |Δ| ${d.mean.toExponential(2)} · PSNR (8-bit) ${d.psnr8 === Infinity ? "∞ (identical)" : d.psnr8.toFixed(1) + " dB"}`,
    d.psnr8 > 50 ? "ok" : "bad");

  Object.assign(shown, {
    "c-input": fromRgb(rgb, w, h),
    "c-bicubic": fromRgb(bicubic(rgb, w, h, s), w * s, h * s),
    "c-esrgan": fromChw(out, w * s, h * s),
    "c-truth": await fromPng("data/truth.png"),
  });
  draw();
  result.done = true;
  log("done");
}

main().catch((e) => { log(`error: ${e?.stack ?? e}`); fact("Error", String(e), "bad"); window.__result = { error: String(e) }; });
