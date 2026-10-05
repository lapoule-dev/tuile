// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// The page. It looks at what the buckets hold — films, the packs each is cut
// into, the camera and the tiles a pack carries, the source tiles under that
// camera — and renders a film: its frames split between workers (contiguous
// slices, the farm's rule), each worker walking the packs its slice crosses,
// and their encoded chunks joined into one mp4 by the Rust muxer.
//
// It knows no key: films and packs come from the API's repositories, and a
// pack is only ever a URL handed back by them.

import init, { FilmMuxer, PackView } from "./pkg/tuile_film_web.js";
import { findEncoder } from "./encoder.js";

const $ = (id) => document.getElementById(id);
const status = (text, kind = "") => { $("status").textContent = text; $("status").className = kind; };
const mb = (bytes) => bytes >= 1e9 ? `${(bytes / 1e9).toFixed(2)} Go` : `${(bytes / 1e6).toFixed(0)} Mo`;
const css = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();

// The API: this page's own origin when tuile-pack-api serves it, or ?api=.
const query = new URLSearchParams(location.search);
const API = (query.get("api") ?? location.origin) + "/api";
const get = async (path) => {
  const response = await fetch(`${API}${path}`);
  if (!response.ok) throw new Error(`${path} : HTTP ${response.status} — ${await response.text()}`);
  return response.json();
};
const objectUrl = (project, key) => `${API}/p/${project}/o/${key}`;

await init();

let project = null;     // the selected project's name
let film = null;        // the film being looked at, as the repository gave it
let view = null;        // the PackView of the selected chunk
let path = null;        // its camera samples: [{frame, lon, lat, height, heading, pitch, fovy}]
let layers = [];        // the tile store's layers

// ------------------------------------------------------------------- links

// The address bar always names what is on screen — project, scene, pack and
// frame — so a view can be reopened later, or sent to someone, as it is.
function remember(frame) {
  const q = new URLSearchParams();
  if (query.get("api")) q.set("api", query.get("api"));
  if (project) q.set("project", project);
  if (film) q.set("film", film.id);
  if (film && view) q.set("pack", film.chunks[view.chunk].key.slice(film.id.length + 1));
  if (frame !== undefined) q.set("frame", frame);
  const url = `${location.pathname}?${q}`;
  history.replaceState(null, "", url);
  $("permalink").href = url;
  $("permalink").hidden = !film;
}

// What the address asked for when the page loaded, used once.
let wanted = { film: query.get("film"), pack: query.get("pack"), frame: query.get("frame") };

// ---------------------------------------------------------------- projects

async function start() {
  try {
    const { projects, tiles } = await get("/projects");
    $("projects").innerHTML = "";
    for (const p of projects) {
      const b = document.createElement("button");
      b.textContent = p.name;
      b.title = `${p.store} — ${p.layout}`;
      b.setAttribute("role", "tab");
      b.addEventListener("click", () => openProject(p.name));
      $("projects").append(b);
    }
    if (tiles) {
      layers = (await get("/tiles/catalog")).filter((l) => !l.name.endsWith(".absent"));
      $("layer").innerHTML = layers.map((l) => `<option value="${l.name}">${l.name} — ${l.content_type}</option>`).join("");
      const imagery = layers.find((l) => l.content_type.startsWith("image/"));
      if (imagery) $("layer").value = imagery.name;
    }
    // ?project=…&film=…&pack=…&frame=… opens straight onto that view.
    const first = projects.find((p) => p.name === query.get("project")) ?? projects[0];
    if (first) await openProject(first.name);
    const row = [...$("films").querySelectorAll("tbody tr")].find((r) => r.firstChild.textContent === wanted.film);
    if (row) row.click(); else wanted = {};
  } catch (e) {
    $("films-note").textContent = `API indisponible : ${e.message}`;
    $("films-note").className = "note bad";
  }
}

async function openProject(name) {
  opening++; filming++;
  retire(view);
  view = null; path = null; film = null;
  for (const panel of ["film-panel", "camera-panel", "tiles-panel", "pack-panel"]) $(panel).hidden = true;
  project = name;
  for (const b of $("projects").children) b.setAttribute("aria-selected", b.textContent === name);
  const body = $("films").querySelector("tbody");
  body.innerHTML = "";
  $("films-note").className = "note";
  $("films-note").textContent = "Lecture des packs du bucket…";
  try {
    const films = await get(`/p/${name}/films`);
    $("films-note").textContent = `${films.length} scènes à rendre, ${mb(films.reduce((s, f) => s + f.bytes, 0))} de packs. Rien ici n'est déjà rendu : ce sont les packs cuits, que cette page rend.`;
    for (const f of films) {
      const tr = document.createElement("tr");
      tr.className = "pick";
      tr.innerHTML = `<td>${f.id}</td><td>${f.packs}</td><td>${mb(f.bytes)}</td>`;
      tr.addEventListener("click", () => {
        for (const r of body.children) r.classList.toggle("on", r === tr);
        openFilm(f.id);
      });
      body.append(tr);
    }
  } catch (e) {
    $("films-note").textContent = e.message;
    $("films-note").className = "note bad";
  }
}

// -------------------------------------------------------------------- film

let filming = 0;
async function openFilm(id) {
  $("film-panel").hidden = false;
  $("film-title").textContent = id;
  $("film-note").className = "note";
  $("film-note").textContent = "Lecture de la scène (la plage de chaque pack est lue dans sa table)…";
  const body = $("chunks").querySelector("tbody");
  body.innerHTML = "";
  for (const panel of ["camera-panel", "tiles-panel", "pack-panel"]) $(panel).hidden = true;
  // Leaving the previous scene: its view goes, and any pack still being
  // opened for it is disowned.
  opening++;
  retire(view);
  view = null; path = null; film = null;
  const asked = ++filming;
  try {
    const got = await get(`/p/${project}/films/${id}`);
    if (asked !== filming) return;
    film = got;
  } catch (e) {
    if (asked !== filming) return;
    $("film-note").textContent = e.message;
    $("film-note").className = "note bad";
    return;
  }
  for (const u of film.unreadable) {
    const tr = document.createElement("tr");
    tr.innerHTML = `<td>${u.key.slice(film.id.length + 1)}</td><td colspan="3" class="bad" style="text-align:left">illisible — ${u.why} (${mb(u.bytes)})</td>`;
    body.append(tr);
  }
  if (!film.chunks.length) {
    $("film-note").textContent = `Aucun pack lisible par ce build dans cette scène : ${film.unreadable.length} illisible${film.unreadable.length > 1 ? "s" : ""}.`;
    $("film-note").className = "note bad";
    return;
  }
  const first = film.chunks[0].first, last = film.chunks.at(-1).last;
  const gaps = film.chunks.slice(1).filter((c, i) => c.first !== film.chunks[i].last + 1).length;
  $("film-note").textContent =
    `${film.chunks.length} pack${film.chunks.length > 1 ? "s" : ""}, frames ${first}–${last}` +
    (gaps ? `, ${gaps} trou${gaps > 1 ? "s" : ""} entre packs` : "") +
    `, ${film.others.length} autres objets dans le dossier. Cliquer un pack pour voir sa caméra et ses tuiles.`;
  const rows = [];
  film.chunks.forEach((c, i) => {
    const tr = document.createElement("tr");
    rows.push(tr);
    tr.className = "pick";
    tr.innerHTML = `<td>${c.key.slice(film.id.length + 1)}</td><td>${c.first}–${c.last}</td><td>${mb(c.bytes)}</td><td>${c.scene ?? "—"}</td>`;
    tr.addEventListener("click", () => {
      for (const r of rows) r.classList.toggle("on", r === tr);
      openChunk(i);
    });
    body.append(tr);
  });

  $("pack-panel").hidden = false;
  $("first").value = first; $("last").value = last;
  $("first").min = $("last").min = first; $("first").max = $("last").max = last;
  $("go").disabled = true;
  // The pack and frame the address named, the first time; the first pack
  // otherwise.
  const named = film.chunks.findIndex((c) => c.key.slice(film.id.length + 1) === wanted.pack);
  const at = named >= 0 ? named : 0;
  const frame = named >= 0 && wanted.frame ? Number(wanted.frame) : undefined;
  wanted = {};
  status("Ouverture du pack…");
  for (const r of rows) r.classList.toggle("on", r === rows[at]);
  rows[at].scrollIntoView({ block: "nearest" });
  openChunk(at, frame);
}

// ------------------------------------------------------- a chunk: its pack

// A PackView is a Rust object: freed twice, or freed while one of its async
// reads is still running, it faults. So a view is retired, never freed in
// place — it is dropped from `view` at once, and its memory goes when the
// last read holding it lets go.
const leases = new WeakMap();
function hold(v) { leases.set(v, (leases.get(v) ?? 0) + 1); }
function release(v) {
  const left = (leases.get(v) ?? 1) - 1;
  leases.set(v, left);
  if (left === 0 && v.retired) v.free();
}
function retire(v) {
  if (!v || v.retired) return;
  v.retired = true;
  if (!leases.get(v)) v.free();
}

let opening = 0;
async function openChunk(index, frame) {
  const turn = ++opening;
  const chunk = film.chunks[index];
  // From here nothing may reach the previous pack's view.
  retire(view);
  view = null; path = null;
  $("go").disabled = true;
  $("camera-panel").hidden = false;
  $("camera-title").textContent = `Caméra — ${chunk.key.slice(film.id.length + 1)}`;
  $("cam-at").textContent = "Lecture de la table du pack…";
  let opened;
  try {
    opened = await PackView.open(objectUrl(project, chunk.key));
  } catch (e) {
    if (turn === opening) $("cam-at").textContent = `Pack illisible : ${e.message ?? e}`;
    return;
  }
  // Another pack was asked for while this one was being read: it wins.
  if (turn !== opening) { opened.free(); return; }
  view = opened;
  view.chunk = index;
  const flat = view.cameras(3000);
  path = [];
  for (let i = 0; i < flat.length; i += 7) {
    path.push({ frame: flat[i], lon: flat[i + 1], lat: flat[i + 2], height: flat[i + 3], heading: flat[i + 4], pitch: flat[i + 5], fovy: flat[i + 6] });
  }
  $("cam-frame").min = view.first; $("cam-frame").max = view.last;
  await offerScales();
  if (turn !== opening) return;
  selectFrame(frame ?? view.first);
}

function nearest(frame) {
  return path.reduce((best, s) => Math.abs(s.frame - frame) < Math.abs(best.frame - frame) ? s : best, path[0]);
}

async function selectFrame(frame) {
  if (!view || !path) return;
  frame = Math.min(view.last, Math.max(view.first, Math.round(frame)));
  $("cam-frame").value = frame;
  remember(frame);
  const at = nearest(frame);
  $("cam-at").textContent =
    `lon ${at.lon.toFixed(5)}°, lat ${at.lat.toFixed(5)}°, ${at.height.toFixed(0)} m, cap ${at.heading.toFixed(0)}°, inclinaison ${at.pitch.toFixed(1)}°, champ ${at.fovy.toFixed(0)}°`;
  drawTrack(at);
  drawProfile(at);
  await Promise.all([showPackTiles(frame), showSourceTiles(at)]);
}
$("cam-frame").addEventListener("change", (e) => view && selectFrame(Number(e.target.value)));

// ------------------------------------------------------------------- plots

function frameOf(canvas) {
  const pad = 28, w = canvas.width, h = canvas.height;
  return { pad, w, h, ctx: canvas.getContext("2d") };
}

let trackMap = null;
function drawTrack(at) {
  const { pad, w, h, ctx } = frameOf($("track"));
  ctx.clearRect(0, 0, w, h);
  const lons = path.map((s) => s.lon), lats = path.map((s) => s.lat);
  const [lo, hi, la, ha] = [Math.min(...lons), Math.max(...lons), Math.min(...lats), Math.max(...lats)];
  // Equal metres on both axes: a degree of longitude is shorter by cos(lat).
  const k = Math.cos(((la + ha) / 2) * Math.PI / 180);
  const spanX = Math.max((hi - lo) * k, 1e-6), spanY = Math.max(ha - la, 1e-6);
  const scale = Math.min((w - 2 * pad) / spanX, (h - 2 * pad) / spanY);
  const ox = (w - spanX * scale) / 2, oy = (h - spanY * scale) / 2;
  const X = (lon) => ox + (lon - lo) * k * scale, Y = (lat) => h - oy - (lat - la) * scale;
  trackMap = { X, Y };
  ctx.lineWidth = 2; ctx.strokeStyle = css("--track"); ctx.lineJoin = "round";
  ctx.beginPath();
  path.forEach((s, i) => i ? ctx.lineTo(X(s.lon), Y(s.lat)) : ctx.moveTo(X(s.lon), Y(s.lat)));
  ctx.stroke();
  const dot = (s, colour, r) => { ctx.fillStyle = colour; ctx.beginPath(); ctx.arc(X(s.lon), Y(s.lat), r, 0, 2 * Math.PI); ctx.fill(); };
  dot(path[0], css("--start"), 6);
  dot(path.at(-1), css("--end"), 6);
  // The selected frame, with where it looks.
  const a = (at.heading * Math.PI) / 180;
  ctx.strokeStyle = css("--fg"); ctx.lineWidth = 2;
  ctx.beginPath(); ctx.moveTo(X(at.lon), Y(at.lat)); ctx.lineTo(X(at.lon) + 26 * Math.sin(a), Y(at.lat) - 26 * Math.cos(a)); ctx.stroke();
  dot(at, css("--fg"), 5);
  ctx.fillStyle = css("--muted"); ctx.font = "12px ui-sans-serif, system-ui";
  const km = (spanX / k) * k * 111.32;
  ctx.fillText(`${Math.max(km, spanY * 111.32).toFixed(km > 20 ? 0 : 2)} km`, pad, h - 8);
}

$("track").addEventListener("click", (e) => {
  if (!trackMap || !path) return;
  const r = e.target.getBoundingClientRect();
  const x = (e.clientX - r.left) * (e.target.width / r.width), y = (e.clientY - r.top) * (e.target.height / r.height);
  const hit = path.reduce((best, s) => {
    const d = Math.hypot(trackMap.X(s.lon) - x, trackMap.Y(s.lat) - y);
    return d < best.d ? { d, s } : best;
  }, { d: Infinity, s: path[0] });
  selectFrame(hit.s.frame);
});

function drawProfile(at) {
  const { pad, w, h, ctx } = frameOf($("profile"));
  ctx.clearRect(0, 0, w, h);
  const f0 = path[0].frame, f1 = Math.max(path.at(-1).frame, f0 + 1);
  const hs = path.map((s) => s.height);
  const [h0, h1] = [Math.min(...hs), Math.max(...hs)];
  const X = (f) => pad + ((f - f0) / (f1 - f0)) * (w - 2 * pad);
  const Yh = (v) => h - pad - ((v - h0) / Math.max(h1 - h0, 1e-6)) * (h - 2 * pad);
  const Yp = (v) => h - pad - ((v + 90) / 90) * (h - 2 * pad);   // −90° … 0°
  ctx.strokeStyle = css("--line"); ctx.lineWidth = 1;
  ctx.strokeRect(pad, pad, w - 2 * pad, h - 2 * pad);
  ctx.lineWidth = 2; ctx.strokeStyle = css("--track"); ctx.setLineDash([]);
  ctx.beginPath(); path.forEach((s, i) => i ? ctx.lineTo(X(s.frame), Yh(s.height)) : ctx.moveTo(X(s.frame), Yh(s.height))); ctx.stroke();
  ctx.strokeStyle = css("--end"); ctx.setLineDash([6, 5]);
  ctx.beginPath(); path.forEach((s, i) => i ? ctx.lineTo(X(s.frame), Yp(s.pitch)) : ctx.moveTo(X(s.frame), Yp(s.pitch))); ctx.stroke();
  ctx.setLineDash([]);
  ctx.strokeStyle = css("--fg"); ctx.lineWidth = 1;
  ctx.beginPath(); ctx.moveTo(X(at.frame), pad); ctx.lineTo(X(at.frame), h - pad); ctx.stroke();
  ctx.fillStyle = css("--muted"); ctx.font = "12px ui-sans-serif, system-ui";
  ctx.fillText(`${h1.toFixed(0)} m`, pad + 4, pad + 14);
  ctx.fillText(`${h0.toFixed(0)} m`, pad + 4, h - pad - 6);
  ctx.textAlign = "right";
  ctx.fillText("0°", w - pad - 4, pad + 14);
  ctx.fillText("−90°", w - pad - 4, h - pad - 6);
  ctx.textAlign = "left";
  ctx.fillText(`frame ${f0}`, pad, h - 8);
  ctx.textAlign = "right"; ctx.fillText(`${f1}`, w - pad, h - 8); ctx.textAlign = "left";
}

$("profile").addEventListener("click", (e) => {
  if (!path) return;
  const r = e.target.getBoundingClientRect();
  const t = ((e.clientX - r.left) / r.width * e.target.width - 28) / (e.target.width - 56);
  selectFrame(path[0].frame + Math.min(1, Math.max(0, t)) * (path.at(-1).frame - path[0].frame));
});

// ------------------------------------------------------------------- tiles

const SHOWN = 24;
let tilesTurn = 0;
async function showPackTiles(frame) {
  const turn = ++tilesTurn;
  // This pack's view, for as long as its textures are being read — whatever
  // becomes of `view` meanwhile.
  const pack = view;
  $("tiles-panel").hidden = false;
  $("tiles-title").textContent = `Tuiles — frame ${frame}`;
  const tiles = JSON.parse(pack.frame_tiles(frame));
  const triangles = tiles.reduce((s, t) => s + t.triangles, 0);
  const pixels = tiles.reduce((s, t) => s + t.textureBytes, 0);
  $("pack-tiles-note").textContent =
    `Dans le pack : ${tiles.length} tuiles dessinées, ${triangles.toLocaleString("fr")} triangles, ${mb(pixels)} d'imagerie. Les ${Math.min(SHOWN, tiles.length)} premières :`;
  const gallery = $("pack-tiles");
  for (const img of gallery.querySelectorAll("img")) URL.revokeObjectURL(img.src);
  gallery.innerHTML = "";
  await Promise.all(tiles.slice(0, SHOWN).map(async (t, i) => {
    const fig = document.createElement("figure");
    fig.title = `tuile ${t.id}, drapé ${t.drape}, ${t.triangles} triangles, ${(t.textureBytes / 1e3).toFixed(0)} ko`;
    fig.innerHTML = `<figcaption>${t.triangles} tri.</figcaption>`;
    gallery.append(fig);
    if (!t.textureBytes) { fig.prepend("sans texture"); return; }
    // Superseded before it started: nothing to read, and the view may be gone.
    if (turn !== tilesTurn || pack.retired) return;
    hold(pack);
    try {
      const png = await pack.texture(frame, i);
      if (turn !== tilesTurn) return;
      const img = document.createElement("img");
      img.src = URL.createObjectURL(new Blob([png], { type: "image/png" }));
      fig.prepend(img);
    } catch (e) {
      fig.prepend("illisible");
    } finally {
      release(pack);
    }
  }));
}

// Column and row of the tile holding a point, in a layer's own grid.
function tileAt(layer, z, lon, lat) {
  if (layer.grid === "web-mercator") {
    const n = 2 ** z, s = Math.asinh(Math.tan((lat * Math.PI) / 180));
    return { x: Math.floor(((lon + 180) / 360) * n), y: Math.floor(((1 - s / Math.PI) / 2) * n), dy: 1 };
  }
  // Geographic: two columns per row at level 0, rows counted from the south.
  const n = 2 ** z;
  return { x: Math.floor(((lon + 180) / 180) * n), y: Math.floor(((lat + 90) / 180) * n), dy: -1 };
}

let sourceTurn = 0;
let sourceAt = null;
async function showSourceTiles(at) {
  sourceAt = at ?? sourceAt;
  if (!layers.length || !sourceAt) { $("tiles-note").textContent = "Pas de store de tuiles configuré."; return; }
  const turn = ++sourceTurn;
  const layer = layers.find((l) => l.name === $("layer").value);
  const z = Number($("tz").value);
  const c = tileAt(layer, z, sourceAt.lon, sourceAt.lat);
  $("tiles-note").textContent = `Store source, sous la caméra : ${layer.name} niveau ${z}, autour de ${c.x}/${c.y} (grille ${layer.grid}). Lu tel que stocké, sans appel à la source.`;
  const grid = $("tiles");
  for (const img of grid.querySelectorAll("img")) URL.revokeObjectURL(img.src);
  grid.innerHTML = "";
  const cells = [];
  for (let j = -1; j <= 1; j++) for (let i = -1; i <= 1; i++) cells.push({ x: c.x + i, y: c.y + j * c.dy });
  await Promise.all(cells.map(async ({ x, y }) => {
    const cell = document.createElement("div");
    cell.innerHTML = `<span>${z}/${x}/${y}</span>`;
    grid.append(cell);
    const response = await fetch(`${API}/tiles/${layer.name}/${z}/${x}/${y}`);
    if (turn !== sourceTurn) return;
    if (response.status === 404) { cell.prepend("absente du store"); return; }
    if (!response.ok) { cell.prepend(`HTTP ${response.status}`); return; }
    const blob = await response.blob();
    if (blob.type.startsWith("image/")) {
      const img = document.createElement("img");
      img.src = URL.createObjectURL(blob);
      cell.prepend(img);
    } else {
      cell.prepend(`${(blob.size / 1e3).toFixed(1)} ko — ${layer.content_type.split("/").pop()}`);
    }
  }));
}
$("layer").addEventListener("change", () => showSourceTiles());
$("tz").addEventListener("change", () => showSourceTiles());

// ------------------------------------------------------------------ render

// Says, for each size, which encoder it will get. Encoding is progressive:
// the browser's H.264 encoder when it has one for the size, and otherwise
// rav1e in wasm — any size, but seconds per frame rather than milliseconds.
// The pack's own viewport stays the default: a slower film at the size it was
// baked for, unless a smaller one is asked for.
async function offerScales() {
  const fps = Number($("fps").value), bitrate = Number($("mbps").value) * 1e6;
  // The pack's size is read once: `view` may be another pack's, or none, by
  // the time the browser has answered.
  const [width, height] = [view.width, view.height];
  const turn = opening;
  for (const option of $("scale").options) {
    const scale = Number(option.value);
    const w = even8(width * scale), h = even8(height * scale);
    const browser = !!(await findEncoder(w, h, fps, bitrate));
    if (turn !== opening) return;
    option.dataset.encoder = browser ? "browser" : "rav1e";
    option.textContent = `${w}×${h}${scale === 1 ? " (viewport du pack)" : ""} — ${browser ? "H.264 du navigateur" : "AV1 logiciel, lent"}`;
  }
  $("scale").value = "1";
  $("go").disabled = false;
  describeEncoder();
}

// How many workers a film is split between, from the machine's logical
// cores. The two encoders are limited by different things:
//
// - rav1e is the CPU's: each worker encodes on its own core, so every core
//   but one — kept for the page and the browser's GPU process — is a worker;
// - the browser's encoder is fast and all workers share one GPU, so past a
//   few they queue behind each other: half the cores, four at most.
//
// A browser may report fewer cores than there are (privacy), never more; and
// a number typed in the field is the user's and is left alone.
const CORES = navigator.hardwareConcurrency || 4;
function suggestedWorkers(encoder) {
  const wanted = encoder === "rav1e" ? CORES - 1 : Math.round(CORES / 2);
  return Math.max(1, Math.min(wanted, encoder === "rav1e" ? 16 : 4));
}
let workersTyped = false;
$("workers").addEventListener("input", () => { workersTyped = true; });

function describeEncoder() {
  if (!view) return;
  const encoder = $("scale").selectedOptions[0].dataset.encoder;
  if (!workersTyped) $("workers").value = suggestedWorkers(encoder);
  $("workers").title = `${CORES} cœurs logiques détectés ; ${suggestedWorkers(encoder)} workers proposés pour cet encodeur`;
  const about = `${film.id} : viewport ${view.width}×${view.height}, table de ${(view.table_bytes / 1e6).toFixed(2)} Mo lue pour ce pack (${view.tiles} tuiles).`;
  const soft = $("scale").selectedOptions[0].dataset.encoder === "rav1e";
  status(soft
    ? `${about} Le navigateur n'a pas d'encodeur H.264 à cette taille : repli sur rav1e (AV1 en WebAssembly), de l'ordre de la seconde par image et par worker. Une taille plus petite passe par l'encodeur du navigateur.`
    : about, soft ? "" : "good");
}
$("scale").addEventListener("change", describeEncoder);

// The farm's split: equal spans, the remainder on the last.
function slices(first, last, parts) {
  const total = last - first + 1;
  parts = Math.max(1, Math.min(parts, total));
  const span = Math.floor(total / parts);
  return Array.from({ length: parts }, (_, i) => [first + i * span, i === parts - 1 ? last : first + (i + 1) * span - 1]);
}

// What a slice of the film has to read: each pack it crosses, with the frames
// of that pack the slice wants.
function crossing(first, last) {
  return film.chunks
    .filter((c) => c.last >= first && c.first <= last)
    .map((c) => ({ source: objectUrl(project, c.key), first: Math.max(first, c.first), last: Math.min(last, c.last) }));
}

const even8 = (x) => Math.max(8, Math.round(x / 8) * 8);

let bytes = 0, requests = 0;
function renderStats(totals, frames, wall) {
  const rows = [
    ["Lecture par plages", "fetch"], ["Dépaquetage (LZ4)", "unpack"], ["Décodage PNG (navigateur)", "decode"],
    ["Upload GPU", "upload"], ["Enregistrement + soumission", "record"],
    ["next() complet", "next"], ["Encodage", "encode"],
  ];
  const body = $("stats").querySelector("tbody");
  body.innerHTML = "";
  const row = (cells) => { const tr = document.createElement("tr"); tr.innerHTML = cells; body.append(tr); };
  for (const [label, key] of rows) {
    row(`<td>${label}</td><td>${(totals[key] / Math.max(frames, 1)).toFixed(2)} ms</td><td>${(totals[key] / 1000).toFixed(2)} s</td>`);
  }
  row(`<td>Octets lus</td><td>${(bytes / 1e6 / Math.max(frames, 1)).toFixed(2)} Mo</td><td>${(bytes / 1e6).toFixed(1)} Mo en ${requests} requêtes</td>`);
  row(`<td><b>Mur</b></td><td><b>${(wall * 1000 / Math.max(frames, 1)).toFixed(1)} ms</b></td><td><b>${wall.toFixed(2)} s — ${(frames / wall).toFixed(1)} images/s</b></td>`);
}

$("go").addEventListener("click", async () => {
  if (!view || !film) return;
  const first = Number($("first").value), last = Number($("last").value);
  const fps = Number($("fps").value), bitrate = Number($("mbps").value) * 1e6;
  const scale = Number($("scale").value), supersample = Number($("ss").value);
  const width = even8(view.width * scale), height = even8(view.height * scale);
  // A film with a hole between two packs cannot be one contiguous mp4.
  const covered = crossing(first, last);
  const frames = covered.reduce((s, c) => s + c.last - c.first + 1, 0);
  if (frames !== last - first + 1) {
    status(`Les frames ${first}–${last} ne sont pas toutes dans un pack (${frames} sur ${last - first + 1}).`, "bad");
    return;
  }
  const parts = slices(first, last, Number($("workers").value));

  $("go").disabled = true;
  $("result").hidden = true;
  $("progress").value = 0;
  $("progress").max = frames;
  const previews = $("previews");
  previews.innerHTML = "";
  const contexts = parts.map(() => {
    const c = document.createElement("canvas");
    c.width = 480; c.height = Math.round(480 * height / width);
    previews.append(c);
    return c.getContext("2d");
  });

  const totals = { fetch: 0, unpack: 0, decode: 0, upload: 0, record: 0, next: 0, encode: 0 };
  let done = 0;
  bytes = 0; requests = 0;
  const started = performance.now();
  status(`${parts.length} workers, ${width}×${height}, ${supersample * supersample} échantillons/pixel, ${covered.length} pack${covered.length > 1 ? "s" : ""}…`);

  let results;
  const workers = [];
  try {
    results = await Promise.all(parts.map(([a, b], id) => new Promise((resolve, reject) => {
      const w = new Worker(new URL("./film-worker.js", import.meta.url), { type: "module" });
      workers.push(w);
      w.onmessage = ({ data }) => {
        if (data.type === "frame") {
          for (const k in totals) totals[k] += data[k];
          bytes += data.fetchedBytes; requests += data.requests;
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
      w.postMessage({ id, packs: crossing(a, b), filmFirst: first, width, height, supersample, fps, bitrate });
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
    const muxer = new FilmMuxer(width, height, fps, results[0].record);
    for (const r of results) {
      muxer.check(r.record);
      for (const c of r.chunks) muxer.push(c.index, c.data, c.key);
    }
    const count = muxer.frames();
    const mp4 = muxer.finish();
    const url = URL.createObjectURL(new Blob([mp4], { type: "video/mp4" }));
    $("video").src = url;
    $("download").href = url;
    $("download").download = `film-${film.id.replaceAll("/", "-")}-${first}-${last}-${width}x${height}-ss${supersample}.mp4`;
    $("summary").textContent = `${count} images, ${(mp4.length / 1e6).toFixed(1)} Mo, ${results[0].codec}`;
    $("result").hidden = false;
    status(`Terminé : ${count} images en ${wall.toFixed(1)} s (${(count / wall).toFixed(1)} images/s).`, "good");
  } catch (e) {
    status(`Assemblage refusé : ${e.message ?? e}`, "bad");
  }
  $("go").disabled = false;
});

if (!("gpu" in navigator)) status("Ce navigateur n'expose pas WebGPU : consultation seule.", "bad");
else if (typeof VideoEncoder === "undefined") status("Ce navigateur n'expose pas WebCodecs : consultation seule.", "bad");

start();
