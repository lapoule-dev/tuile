// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// The façade: it ONLY does fetch. The wasm module (../pkg) is the engine — it
// assembles the ion globe (terrain + Bing), drives the SSE traversal, builds
// geometry and drapes the imagery mosaic. We resolve the ion endpoints +
// documents, then run the request/provide/step loop the wasm hands us,
// fetching each terrain and imagery tile and feeding the bytes back.

import init, { Globe } from "../pkg/wasm_globe.js";

const TERRAIN_ASSET = 1; // Cesium World Terrain
const IMAGERY_ASSET = 2; // Bing Aerial
const MAX_ROUNDS = 400; // safety against a runaway loop
const logEl = document.getElementById("log");

function log(msg) {
  logEl.textContent += msg + "\n";
  logEl.scrollTop = logEl.scrollHeight;
}

async function ionEndpoint(asset, token) {
  return await (
    await fetch(`https://api.cesium.com/v1/assets/${asset}/endpoint`, {
      headers: { Authorization: `Bearer ${token}` },
    })
  ).json();
}

async function run(token) {
  logEl.textContent = "";
  log("loading wasm…");
  await init();

  // --- terrain: ion endpoint → layer.json ---
  log("resolving ion terrain endpoint…");
  const ep = await ionEndpoint(TERRAIN_ASSET, token);
  log(`terrain: ${ep.url}`);
  const layerJson = await (
    await fetch(new URL("layer.json", ep.url), {
      headers: { Authorization: `Bearer ${ep.accessToken}` },
    })
  ).text();

  const globe = new Globe(layerJson, ep.url, ep.accessToken);

  // --- imagery: ion endpoint → Bing metadata ---
  log("resolving Bing imagery…");
  const ep2 = await ionEndpoint(IMAGERY_ASSET, token);
  const o = ep2.options ?? {};
  const metaUrl =
    `${(o.url ?? "").replace(/\/$/, "")}/REST/v1/Imagery/Metadata/` +
    `${o.mapStyle ?? "Aerial"}?incl=ImageryProviders&key=${o.key}&uriScheme=https`;
  const metaJson = await (await fetch(metaUrl)).text();
  globe.set_imagery(metaJson);
  log("Bing metadata loaded");

  let terrainFetched = 0;
  let imageryFetched = 0;
  for (let round = 1; round <= MAX_ROUNDS; round++) {
    const res = globe.step();
    if (res.done) {
      log(
        `\nDONE — selected ${res.selected}, resident ${res.resident}, ` +
          `${res.vertices} verts, ${res.triangles} tris, ${res.textures} textures`,
      );
      log(`fetched ${terrainFetched} terrain + ${imageryFetched} imagery tiles in ${round} rounds`);
      break;
    }
    log(
      `round ${round}: ${res.terrain.length} terrain + ${res.imagery.length} imagery · ` +
        `resident ${res.resident}, pending-imagery ${res.pending_imagery}, ${res.vertices} verts`,
    );

    await Promise.all([
      ...res.terrain.map(async (r) => {
        try {
          const resp = await fetch(r.url);
          if (!resp.ok) return globe.fail(r.z, r.x, r.y);
          globe.provide(r.z, r.x, r.y, new Uint8Array(await resp.arrayBuffer()));
          terrainFetched++;
        } catch {
          globe.fail(r.z, r.x, r.y);
        }
      }),
      ...res.imagery.map(async (r) => {
        try {
          const resp = await fetch(r.url);
          if (!resp.ok) return globe.fail_imagery(r.tz, r.tx, r.ty, r.level, r.x, r.y);
          globe.provide_imagery(
            r.tz, r.tx, r.ty, r.level, r.x, r.y,
            new Uint8Array(await resp.arrayBuffer()),
          );
          imageryFetched++;
        } catch {
          globe.fail_imagery(r.tz, r.tx, r.ty, r.level, r.x, r.y);
        }
      }),
    ]);
  }

  document.getElementById("report").textContent = globe.report();
  document.getElementById("report").classList.remove("muted");
}

document.getElementById("run").addEventListener("click", () => {
  const token = document.getElementById("token").value.trim();
  if (!token) {
    log("paste your CESIUM_ION_TOKEN first");
    return;
  }
  run(token).catch((e) => log("ERROR: " + (e?.message ?? e)));
});
