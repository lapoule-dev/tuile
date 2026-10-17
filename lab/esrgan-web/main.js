// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// The variant comes from the URL, since a WebGPU device's float type is fixed once Burn has used it:
// changing a selector reloads the page with the new query.
const CHOICES = {
  pkg: { "pkg": "wasm: release (thin LTO, O3)", "pkg-fusion": "wasm: + fusion", "pkg-opt": "wasm: + wasm-opt -O3", "pkg-fusion-opt": "wasm: fusion + wasm-opt -O3" },
  dtype: { f32: "compute f32", f16: "compute f16" },
  algo: { direct: "conv: Burn direct", gemm: "conv: im2col + matmul" },
  quant: { none: "Burn int8: off", tensor: "Burn int8: per tensor", block: "Burn int8: per block" },
  w: { f32: "weights file f32", f16: "weights file f16", q8: "weights file int8" },
};
const DEFAULTS = { pkg: "pkg", dtype: "f32", algo: "direct", quant: "none", w: "f32" };
const query = new URLSearchParams(location.search);
const variant = Object.fromEntries(Object.entries(DEFAULTS).map(([k, d]) => [k, CHOICES[k][query.get(k)] ? query.get(k) : d]));
const { default: init, Upscaler } = await import(`./${variant.pkg}/esrgan_web.js`);

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

// The weights through the Cache API: a second visit reads them from disk, not the network.
async function cachedBytes(url) {
  const t = performance.now();
  let hit = false, res;
  try {
    const cache = await caches.open("esrgan-weights-v1");
    res = await cache.match(url);
    hit = !!res;
    if (!res) { res = await fetch(url); await cache.put(url, res.clone()); }
  } catch { res = await fetch(url); }
  const b = new Uint8Array(await res.arrayBuffer());
  return { bytes: b, hit, ms: performance.now() - t };
}

function selectors() {
  const box = document.getElementById("variant");
  for (const [k, opts] of Object.entries(CHOICES)) {
    const label = document.createElement("label"), sel = document.createElement("select");
    for (const [v, text] of Object.entries(opts)) sel.add(new Option(text, v, false, v === variant[k]));
    sel.onchange = () => { const q = new URLSearchParams(variant); q.set(k, sel.value); location.search = q.toString(); };
    label.append(sel);
    box.append(label);
  }
}

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

  selectors();
  result.variant = variant;
  const meta = await (await fetch("data/meta.json")).json();
  $("what").textContent = `Burn 0.22 compiled to wasm, on the browser's GPU · tile ${meta.tile} · ${meta.params.toLocaleString()} parameters · ${meta.num_conv} body convolutions`;
  const wfile = `data/weights.${variant.w}.esrw`;
  const [wts, rgb, ref] = await Promise.all([cachedBytes(wfile), bytes("data/input.rgb"), floats("data/ref.f32")]);
  const w = 256, h = 256;

  let t = performance.now();
  await init();  // given no argument, wasm-bindgen fetches the .wasm with instantiateStreaming
  const wasmMs = performance.now() - t;
  t = performance.now();
  const net = await Upscaler.create(wts.bytes, variant.dtype, variant.algo, variant.quant);
  const createMs = performance.now() - t;
  const s = net.scale();
  log(`weights ${wfile} ${wts.bytes.length} B (${wts.hit ? "Cache API hit" : "network"}, ${wts.ms.toFixed(0)} ms) · wasm ${wasmMs.toFixed(0)} ms · device + weights ${createMs.toFixed(0)} ms · computes in ${net.dtype()} · fusion ${Upscaler.fusion()}`);

  t = performance.now();
  let r = await net.run(rgb, 1, w, h);
  const cold = performance.now() - t;
  const firstResultMs = performance.now();  // since navigation start
  log(`cold run ${cold.toFixed(0)} ms (gpu ${r.gpuMs?.toFixed(0)} ms) · first result ${firstResultMs.toFixed(0)} ms after navigation`);
  const warm = [], gpu = [];
  for (let i = 0; i < 7; i++) {
    t = performance.now();
    r = await net.run(rgb, 1, w, h);
    warm.push(performance.now() - t);
    gpu.push(r.gpuMs);
  }
  const median = (a) => [...a].sort((x, y) => x - y)[a.length >> 1];
  const warmMs = median(warm), gpuMs = median(gpu.filter((v) => v != null));
  log(`warm runs ${warm.map((v) => v.toFixed(0)).join(", ")} ms · gpu ${gpu.map((v) => v?.toFixed(0)).join(", ")} ms`);
  const out = r.out;

  // four tiles in one batch: what a tile costs when the GPU has more to chew on
  const rgb4 = new Uint8Array(rgb.length * 4);
  for (let i = 0; i < 4; i++) rgb4.set(rgb, i * rgb.length);
  // opt-in (?batch=4): the direct convolution on four tiles runs past the GPU watchdog and loses the device
  let batch4Ms = null, batch4GpuMs = null;
  if (query.get("batch") === "4") try {
    await net.run(rgb4, 4, w, h);
    t = performance.now();
    const r4 = await net.run(rgb4, 4, w, h);
    batch4Ms = performance.now() - t; batch4GpuMs = r4.gpuMs;
    log(`batch of 4: ${batch4Ms.toFixed(0)} ms (gpu ${batch4GpuMs?.toFixed(0)} ms), ${(batch4Ms / 4).toFixed(0)} ms a tile`);
  } catch (e) { log(`batch of 4 failed: ${e}`); }

  // the largest |activation| after each layer, in the dtype the network computes in (f16 overflows at 65504)
  const peaks = await net.probe(rgb, w, h);
  const peak = Math.max(...peaks), finite = peaks.every(Number.isFinite) && out.every(Number.isFinite);
  log(`activation peaks ${[...peaks].map((v) => v.toPrecision(3)).join(" ")}`);

  const d = compare(out, ref);
  Object.assign(result, {
    dtype: net.dtype(), fusion: Upscaler.fusion(), weightsBytes: wts.bytes.length, weightsCacheHit: wts.hit, weightsMs: wts.ms,
    wasmMs, createMs, coldMs: cold, coldGpuMs: null, warmMs, gpuMs, warmAll: warm, gpuAll: gpu, batch4Ms, batch4GpuMs,
    firstResultMs, peak, finite, diff: d, referenceSeconds: meta.reference_seconds,
  });
  fact("Variant", `${net.dtype()} · ${variant.algo} · Burn int8 ${variant.quant} · weights ${variant.w} (${(wts.bytes.length / 1e6).toFixed(2)} MB${wts.hit ? ", from Cache API" : ""}) · fusion ${Upscaler.fusion()} · ${variant.pkg}`);
  fact("Timings", `wasm load ${wasmMs.toFixed(0)} ms · device + weights ${createMs.toFixed(0)} ms · cold ${cold.toFixed(0)} ms · warm ${warmMs.toFixed(0)} ms, GPU ${gpuMs?.toFixed(1)} ms (median of 7, 256² → 1024², readback included) · batch of 4 ${batch4Ms?.toFixed(0)} ms · first result ${firstResultMs.toFixed(0)} ms after navigation · PyTorch CPU ${(meta.reference_seconds * 1000).toFixed(0)} ms`);
  fact("Activations", `peak |a| ${peak.toPrecision(4)} over ${peaks.length} layers · ${finite ? "all finite" : "NON-FINITE values"}`, finite && peak < 6e4 ? "ok" : "bad");
  fact("vs PyTorch", `max |Δ| ${d.max.toExponential(2)} · mean |Δ| ${d.mean.toExponential(2)} · PSNR (8-bit) ${d.psnr8 === Infinity ? "∞ (identical)" : d.psnr8.toFixed(1) + " dB"}`,
    d.psnr8 > 40 ? "ok" : "bad");

  Object.assign(shown, {
    "c-input": fromRgb(rgb, w, h),
    "c-bicubic": fromRgb(bicubic(rgb, w, h, s), w * s, h * s),
    "c-esrgan": fromChw(out, w * s, h * s),
    "c-truth": await fromPng("data/truth.png"),
  });
  $("cap-esrgan").textContent = `Real-ESRGAN compact ×4, Burn on WebGPU: ${net.dtype()}, ${variant.algo}, weights ${variant.w}${variant.quant !== "none" ? ", Burn int8 " + variant.quant : ""}`;
  draw();
  result.done = true;
  log("done");
}

main().catch((e) => { log(`error: ${e?.stack ?? e}`); fact("Error", String(e), "bad"); window.__result = { error: String(e) }; });
