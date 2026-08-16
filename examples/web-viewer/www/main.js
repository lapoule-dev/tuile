// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// The page: a THREE renderer in front of the engine, and nothing more.
//
// It owns the camera, the GPU resources and the frame loop. It owns no
// decision about *what* to draw — that comes from the worker, which runs the
// same GeometryServer and the same traversal the native wgpu viewer runs. The
// division is the point: if this page and the native viewer disagree about what
// is on screen, the disagreement is in a renderer, not in the engine, and that
// makes this page a differential test rather than a second implementation.
//
// Drag: orbit. Wheel: zoom.

import * as THREE from "three";
import { groundMaterial } from "./ground-material.js";

// ECEF is metres from the Earth's centre, so the whole scene sits ~6.4e6 from
// the origin. In f32 that spacing resolves to about half a metre, which is
// visible crawling on a hillside. Everything below is arranged so f32 never
// holds an absolute ECEF coordinate — see `render()`.
const WGS84_A = 6378137.0;

const state = {
  // Camera in f64, always. THREE's own camera stays at the origin.
  eye: [0, 0, 0],
  target: [0, 0, 0],
  up: [0, 0, 1],
  fovy: (45 * Math.PI) / 180,
  // Orbit parameters over the globe.
  lon: 1.0 * (Math.PI / 180),
  lat: 42.7 * (Math.PI / 180),
  altitude: 2.0e6,
  yaw: 0,
  pitch: -Math.PI / 4,
};

const tiles = new Map(); // key → {mesh, origin}
const textures = new Map(); // imagery coord key → THREE.Texture
let selected = new Set();

const status = document.getElementById("status");
const canvas = document.getElementById("view");
const renderer = new THREE.WebGLRenderer({ canvas, antialias: true });
renderer.setPixelRatio(Math.min(devicePixelRatio, 2));

const scene = new THREE.Scene();
scene.background = new THREE.Color(0x05070d);
const camera = new THREE.PerspectiveCamera(45, 1, 1, 5.0e7);
// The ground material does its own lighting, exactly as the native shader
// does, so there are no THREE lights: a `DirectionalLight` here would apply to
// nothing and only invite the two to drift apart.
const sunDir = new THREE.Vector3(-1, -0.6, -0.8).normalize();

// --- geodesy, the two conversions this page genuinely needs -----------------

function geodeticToEcef(lon, lat, height) {
  // Sphere, not ellipsoid: this positions a camera, and the difference never
  // reaches a pixel. Tile geometry comes from the engine, which uses WGS84.
  const r = WGS84_A + height;
  const cl = Math.cos(lat);
  return [r * cl * Math.cos(lon), r * cl * Math.sin(lon), r * Math.sin(lat)];
}

function enuFrame(lon, lat) {
  const east = [-Math.sin(lon), Math.cos(lon), 0];
  const up = [
    Math.cos(lat) * Math.cos(lon),
    Math.cos(lat) * Math.sin(lon),
    Math.sin(lat),
  ];
  const north = [
    up[1] * east[2] - up[2] * east[1],
    up[2] * east[0] - up[0] * east[2],
    up[0] * east[1] - up[1] * east[0],
  ];
  return { east, north, up };
}

function updateCamera() {
  const ground = geodeticToEcef(state.lon, state.lat, 0);
  const { east, north, up } = enuFrame(state.lon, state.lat);
  // Orbit in the local horizon frame, so "up" stays the local vertical however
  // far the camera has travelled over the globe.
  const cp = Math.cos(state.pitch);
  const dir = [
    east[0] * Math.sin(state.yaw) * cp + north[0] * Math.cos(state.yaw) * cp + up[0] * Math.sin(state.pitch),
    east[1] * Math.sin(state.yaw) * cp + north[1] * Math.cos(state.yaw) * cp + up[1] * Math.sin(state.pitch),
    east[2] * Math.sin(state.yaw) * cp + north[2] * Math.cos(state.yaw) * cp + up[2] * Math.sin(state.pitch),
  ];
  state.eye = [
    ground[0] - dir[0] * state.altitude,
    ground[1] - dir[1] * state.altitude,
    ground[2] - dir[2] * state.altitude,
  ];
  state.target = ground;
  state.up = up;
}

// --- input ------------------------------------------------------------------

let dragging = false;
let last = [0, 0];
canvas.addEventListener("pointerdown", (e) => {
  dragging = true;
  last = [e.clientX, e.clientY];
  canvas.setPointerCapture(e.pointerId);
});
canvas.addEventListener("pointerup", (e) => {
  dragging = false;
  canvas.releasePointerCapture(e.pointerId);
});
canvas.addEventListener("pointermove", (e) => {
  if (!dragging) return;
  const dx = e.clientX - last[0];
  const dy = e.clientY - last[1];
  last = [e.clientX, e.clientY];
  // Pan scales with altitude: the same gesture should move the same fraction of
  // the screen whether the camera is in orbit or on a hillside.
  const scale = (state.altitude / WGS84_A) * 0.6 + 0.0002;
  state.lon -= dx * scale * 0.01;
  state.lat = Math.max(-1.5, Math.min(1.5, state.lat + dy * scale * 0.01));
});
canvas.addEventListener(
  "wheel",
  (e) => {
    e.preventDefault();
    state.altitude = Math.max(300, Math.min(3.0e7, state.altitude * Math.exp(e.deltaY * 0.001)));
  },
  { passive: false },
);

// --- the engine, in its worker ---------------------------------------------

const worker = new Worker(new URL("./engine-worker.js", import.meta.url), { type: "module" });
let ready = false;
let awaitingBatch = false;
let batchId = 0;
let stats = { selected: 0, visited: 0, culled: 0 };
let priming = null;

worker.onmessage = (e) => {
  const msg = e.data;
  if (msg.type === "ready") {
    ready = true;
    return;
  }
  if (msg.type === "error") {
    status.textContent = "engine error: " + msg.message;
    return;
  }
  if (msg.type === "batch") {
    awaitingBatch = false;
    for (const m of msg.batch) apply(m);
  }
};

function apply(m) {
  switch (m.kind) {
    case "add":
      addTile(m);
      break;
    case "evict":
      for (const key of m.tiles) dropTile(key);
      break;
    case "select":
      selected = new Set(m.tiles);
      stats = { selected: m.selected, visited: m.visited, culled: m.culled };
      // Only what the traversal chose is drawn. Everything else stays resident
      // and invisible — that is what makes a later frame free rather than a
      // re-fetch, and it mirrors the native viewer exactly.
      for (const [key, tile] of tiles) tile.mesh.visible = selected.has(key);
      break;
    case "priming":
      // The same wait the native viewer holds its window through, shown rather
      // than hidden: a warm-up that stalls must look like a stall, not like a
      // slow network. `unavailable` are tiles the source does not serve — they
      // are resolved, never pending, and waiting on them would never end.
      priming = m;
      break;
    case "closed":
      status.textContent = "the engine stopped";
      break;
  }
}

function addTile(m) {
  dropTile(m.tile);

  const group = new THREE.Group();
  // Held in f64 beside the object; THREE's own `position` is f32 and is
  // rewritten every frame from this and the live camera.
  group.userData.origin = m.origin;

  cacheTextures(m.imagery);
  for (const mesh of m.meshes) {
    const geometry = new THREE.BufferGeometry();
    geometry.setAttribute("position", new THREE.BufferAttribute(mesh.positions, 3));
    if (mesh.normals) {
      geometry.setAttribute("normal", new THREE.BufferAttribute(mesh.normals, 3));
    } else {
      geometry.computeVertexNormals();
    }
    if (mesh.uvs) geometry.setAttribute("uv", new THREE.BufferAttribute(mesh.uvs, 2));
    geometry.setIndex(new THREE.BufferAttribute(mesh.indices, 1));

    // The whole mosaic, blended in one pass by the same rule the native
    // backend uses — up to twelve layers, each masked to its own coverage
    // rectangle. Picking a single "widest" layer, which this did before, drew
    // a multi-part mosaic at the resolution of its coarsest part.
    const material = groundMaterial({
      table: m.layerTable,
      emptyCoverage: m.emptyCoverage,
      layers: m.imagery,
      resolve: (coord) => textures.get(coord) ?? null,
      sunDir,
    });
    group.add(new THREE.Mesh(geometry, material));
  }

  group.visible = selected.has(m.tile);
  scene.add(group);
  tiles.set(m.tile, { mesh: group, origin: m.origin });
}

/// Uploads the pixels of any layer seen for the first time.
///
/// Layers cross from the worker once per imagery coordinate — a tile that
/// drapes a coordinate some earlier tile already carried names it and nothing
/// more. So this is a cache fill, not a choice: which layers a tile draws, and
/// where, is decided by the shader from the coverage rectangles the engine sent.
function cacheTextures(layers) {
  if (!layers) return;
  for (const layer of layers) {
    if (!layer.rgba || textures.has(layer.coord)) continue;
    const data = new THREE.DataTexture(layer.rgba, layer.width, layer.height, THREE.RGBAFormat);
    data.colorSpace = THREE.SRGBColorSpace;
    // The engine's v grows southward, matching the imagery tile's own
    // rectangle; flipping here would mirror every tile against its coverage
    // rectangle and smear the mosaic seams.
    data.flipY = false;
    data.wrapS = THREE.ClampToEdgeWrapping;
    data.wrapT = THREE.ClampToEdgeWrapping;
    data.minFilter = THREE.LinearFilter;
    data.magFilter = THREE.LinearFilter;
    data.needsUpdate = true;
    textures.set(layer.coord, data);
  }
}

function dropTile(key) {
  const tile = tiles.get(key);
  if (!tile) return;
  scene.remove(tile.mesh);
  // THREE does not free GPU memory on its own; a streaming renderer that skips
  // this leaks a buffer per evicted tile, which over a session is the whole
  // budget the engine was carefully staying inside.
  tile.mesh.traverse((node) => {
    if (node.geometry) node.geometry.dispose();
    if (node.material) node.material.dispose();
  });
  tiles.delete(key);
}

// --- the frame loop ---------------------------------------------------------

function render() {
  requestAnimationFrame(render);

  const w = canvas.clientWidth;
  const h = canvas.clientHeight;
  if (canvas.width !== w || canvas.height !== h) {
    renderer.setSize(w, h, false);
    camera.aspect = w / Math.max(1, h);
    camera.updateProjectionMatrix();
  }

  updateCamera();

  if (ready) {
    worker.postMessage({
      type: "view",
      view: {
        eye: state.eye,
        dir: normalize(sub(state.target, state.eye)),
        up: state.up,
        width: w,
        height: h,
        fovy: state.fovy,
      },
    });
    // One outstanding drain at a time: asking again before the last answer
    // arrived would queue work the page cannot consume any faster.
    if (!awaitingBatch) {
      awaitingBatch = true;
      worker.postMessage({ type: "drain", id: ++batchId });
    }
  }

  // THE rebase. The camera sits at the scene origin and every tile is placed
  // relative to it, the subtraction done in f64 *here* before anything becomes
  // f32. Positions inside a tile are already relative to that tile's origin, so
  // no f32 value in the pipeline ever exceeds a tile's own extent.
  camera.position.set(0, 0, 0);
  const dir = normalize(sub(state.target, state.eye));
  camera.up.set(state.up[0], state.up[1], state.up[2]);
  camera.lookAt(dir[0], dir[1], dir[2]);
  for (const tile of tiles.values()) {
    tile.mesh.position.set(
      tile.origin[0] - state.eye[0],
      tile.origin[1] - state.eye[1],
      tile.origin[2] - state.eye[2],
    );
  }

  renderer.render(scene, camera);

  if (priming && !priming.settled) {
    status.textContent =
      `warming the coarse pyramid — ${tiles.size} of ${priming.expected} held, ` +
      `${priming.outstanding} outstanding` +
      (priming.unavailable > 0 ? `, ${priming.unavailable} not served` : "");
  } else {
    status.textContent =
      `alt ${(state.altitude / 1000).toFixed(1)} km · ` +
      `${stats.selected} selected of ${tiles.size} resident · ` +
      `${stats.visited} visited, ${stats.culled} culled · ` +
      `${textures.size} textures`;
  }
}

function sub(a, b) {
  return [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
}
function normalize(v) {
  const n = Math.hypot(v[0], v[1], v[2]) || 1;
  return [v[0] / n, v[1] / n, v[2] / n];
}

/// Why a token is refused, or null if it looks like one.
///
/// Not paranoia about format — it is about *which error you get*. A field
/// holding something that is not a token produces a 401 from ion, and a 401 is
/// indistinguishable from an expired or unauthorised token, so the message
/// blames the account instead of the paste. Seen for real: a console error
/// message ended up in this field (the clipboard had moved on between the copy
/// and the paste) and the page reported nothing but Unauthorized.
///
/// An ion token is a JWT: three base64url segments separated by dots, and no
/// whitespace anywhere.
function rejectToken(token) {
  if (!token) return "paste your CESIUM_ION_TOKEN first";
  if (/\s/.test(token)) return "that has spaces in it — it is not a token. Did the clipboard change?";
  const parts = token.split(".");
  if (parts.length !== 3 || parts.some((p) => p.length === 0)) {
    return `that is not a JWT (${parts.length} dot-separated part(s), expected 3)`;
  }
  return null;
}

document.getElementById("run").addEventListener("click", () => {
  const token = document.getElementById("token").value.trim();
  const refused = rejectToken(token);
  if (refused) {
    status.textContent = refused;
    return;
  }
  status.textContent = "starting the engine…";
  worker.postMessage({ type: "start", token, maxSse: 2.0 });
});

render();
