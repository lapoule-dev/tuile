// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// The page: picks a pack, splits its frames between workers (contiguous
// slices, the farm's rule), shows what they report, and joins their encoded
// chunks into one mp4 with the Rust muxer — which refuses slices whose
// encoders disagree on the parameter sets.

import init, { FilmMuxer, PackInfo } from "./pkg/tuile_film_web.js";

const $ = (id) => document.getElementById(id);
const status = (text, kind = "") => { $("status").textContent = text; $("status").className = kind; };

let source = null;   // a Blob: the File, or the fetched bytes
let info = null;

await init();

if (!("gpu" in navigator)) status("Ce navigateur n'expose pas WebGPU.", "bad");
if (typeof VideoEncoder === "undefined") status("Ce navigateur n'expose pas WebCodecs.", "bad");

async function load(blob, name) {
  status(`Lecture de ${name} (${(blob.size / 1e6).toFixed(1)} Mo)…`);
  $("go").disabled = true;
  try {
    // Opens and verifies the pack once, here: a bad pack fails before any
    // worker starts.
    const bytes = new Uint8Array(await blob.arrayBuffer());
    info = PackInfo.read(bytes);
    source = blob;
    $("first").value = info.first;
    $("last").value = info.last;
    $("first").min = $("last").min = info.first;
    $("first").max = $("last").max = info.last;
    status(`${name} : frames ${info.first}–${info.last}, ${info.tiles} tuiles, viewport ${info.width}×${info.height}, scène ${info.scene}`, "good");
    $("go").disabled = false;
  } catch (e) {
    status(`Pack illisible : ${e.message ?? e}`, "bad");
  }
}

$("file").addEventListener("change", (e) => {
  const file = e.target.files[0];
  if (file) load(file, file.name);
});

async function fromUrl(url) {
  status(`Téléchargement de ${url}…`);
  const response = await fetch(url);
  if (!response.ok) { status(`${url} : HTTP ${response.status}`, "bad"); return; }
  await load(await response.blob(), url.split("/").pop());
}
$("url").addEventListener("change", (e) => e.target.value && fromUrl(e.target.value));
const fromQuery = new URLSearchParams(location.search).get("pack");
if (fromQuery) { $("url").value = fromQuery; fromUrl(fromQuery); }

// The farm's split: equal spans, the remainder on the last.
function slices(first, last, parts) {
  const total = last - first + 1;
  parts = Math.max(1, Math.min(parts, total));
  const span = Math.floor(total / parts);
  return Array.from({ length: parts }, (_, i) => [first + i * span, i === parts - 1 ? last : first + (i + 1) * span - 1]);
}

const even8 = (x) => Math.max(8, Math.round(x / 8) * 8);

function renderStats(totals, frames, wall) {
  const rows = [
    ["Dépaquetage (LZ4)", "unpack"], ["Décodage PNG (navigateur)", "decode"],
    ["Upload GPU", "upload"], ["Enregistrement + soumission", "record"],
    ["next() complet", "next"], ["Encodage (attente file)", "encode"],
  ];
  const body = $("stats").querySelector("tbody");
  body.innerHTML = "";
  for (const [label, key] of rows) {
    const tr = document.createElement("tr");
    tr.innerHTML = `<td>${label}</td><td>${(totals[key] / Math.max(frames, 1)).toFixed(2)} ms</td><td>${(totals[key] / 1000).toFixed(2)} s</td>`;
    body.append(tr);
  }
  const tr = document.createElement("tr");
  tr.innerHTML = `<td><b>Mur</b></td><td><b>${(wall * 1000 / Math.max(frames, 1)).toFixed(1)} ms</b></td><td><b>${wall.toFixed(2)} s — ${(frames / wall).toFixed(1)} images/s</b></td>`;
  body.append(tr);
}

$("go").addEventListener("click", async () => {
  const first = Number($("first").value), last = Number($("last").value);
  const fps = Number($("fps").value), bitrate = Number($("mbps").value) * 1e6;
  const scale = Number($("scale").value), supersample = Number($("ss").value);
  const width = even8(info.width * scale), height = even8(info.height * scale);
  const parts = slices(first, last, Number($("workers").value));
  const total = last - first + 1;

  $("go").disabled = true;
  $("result").hidden = true;
  $("progress").value = 0;
  $("progress").max = total;
  const previews = $("previews");
  previews.innerHTML = "";
  const contexts = parts.map(() => {
    const c = document.createElement("canvas");
    c.width = 480; c.height = Math.round(480 * height / width);
    previews.append(c);
    return c.getContext("2d");
  });

  const totals = { unpack: 0, decode: 0, upload: 0, record: 0, next: 0, encode: 0 };
  let done = 0;
  const started = performance.now();
  status(`${parts.length} workers, ${width}×${height}, ${supersample * supersample} échantillons/pixel…`);

  let results;
  const workers = [];
  try {
    results = await Promise.all(parts.map(([a, b], id) => new Promise((resolve, reject) => {
      const w = new Worker(new URL("./film-worker.js", import.meta.url), { type: "module" });
      workers.push(w);
      w.onmessage = ({ data }) => {
        if (data.type === "frame") {
          for (const k in totals) totals[k] += data[k];
          done++;
          $("progress").value = done;
          if (data.preview) { contexts[id].drawImage(data.preview, 0, 0, contexts[id].canvas.width, contexts[id].canvas.height); data.preview.close(); }
          if (done % 4 === 0) renderStats(totals, done, (performance.now() - started) / 1000);
        } else if (data.type === "done") {
          resolve(data);
        } else if (data.type === "error") {
          reject(new Error(`worker ${id} (frames ${a}–${b}) : ${data.message}`));
        }
      };
      w.onerror = (e) => reject(new Error(`worker ${id} : ${e.message}`));
      w.postMessage({ id, source, first: a, last: b, filmFirst: first, width, height, supersample, fps, bitrate });
    })));
  } catch (e) {
    workers.forEach((w) => w.terminate());
    status(e.message, "bad");
    $("go").disabled = false;
    return;
  }
  workers.forEach((w) => w.terminate());
  const wall = (performance.now() - started) / 1000;
  renderStats(totals, done, wall);

  try {
    const muxer = new FilmMuxer(width, height, fps, results[0].avcc);
    for (const r of results) {
      muxer.check(r.avcc);
      for (const c of r.chunks) muxer.push(c.index, c.data, c.key);
    }
    const frames = muxer.frames();
    const mp4 = muxer.finish();
    const url = URL.createObjectURL(new Blob([mp4], { type: "video/mp4" }));
    $("video").src = url;
    $("download").href = url;
    $("download").download = `film-${info.scene.slice(0, 12)}-${first}-${last}-${width}x${height}-ss${supersample}.mp4`;
    $("summary").textContent = `${frames} images, ${(mp4.length / 1e6).toFixed(1)} Mo, ${results[0].codec}`;
    $("result").hidden = false;
    status(`Terminé : ${frames} images en ${wall.toFixed(1)} s (${(frames / wall).toFixed(1)} images/s).`, "good");
  } catch (e) {
    status(`Assemblage refusé : ${e.message ?? e}`, "bad");
  }
  $("go").disabled = false;
});
