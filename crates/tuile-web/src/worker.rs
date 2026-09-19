// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The engine, running inside a Web Worker.
//!
//! This is the browser's equivalent of `wgpu-viewer`'s background thread: the
//! **real** [`GeometryServer`], the real traversal, the real planetary loader —
//! not a reimplementation, not a subset. The page never sees a tile coordinate;
//! it sends a camera and receives geometry.
//!
//! # What crosses, and in which direction
//!
//! ```text
//!   page  ──set_view(eye, dir, up, viewport, fovy)──▶  worker
//!   page  ◀──drain(): Add | Drop | Stats ────────────  worker
//! ```
//!
//! Pull, not push. The worker does not post messages when it feels like it; the
//! page asks once per animation frame and takes whatever has accumulated. That
//! is what keeps the two loops independent — a worker mid-decode cannot stall a
//! frame, and a page that skips a frame cannot lose a tile.
//!
//! # Anti-jitter
//!
//! Positions are `f32` **relative to the tile's own ECEF origin**, and the
//! origin travels beside them as `f64`. Absolute ECEF in `f32` has a resolution
//! of metres at the Earth's radius, which is visible shimmer on a mountainside;
//! rebasing is what buys back the precision, and the page recomputes the model
//! matrix from the live camera each frame rather than asking the worker for it.
//!
//! # Imagery crosses once
//!
//! Twenty terrain tiles routinely drape the same imagery tile. Pixels are sent
//! the first time a coordinate appears and never again — afterwards the tile
//! carries only the coordinate, and the page looks up the texture it already
//! holds. Without this the same quarter-megabyte would cross twenty times.

use std::collections::HashSet;
use std::sync::Arc;

use wasm_bindgen::prelude::*;

use tuile_core::content::TileContent;
use tuile_core::protocol::{ClientMessage, GeometryStream, ServerMessage};
use tuile_core::raster::ImageryCoord;
use tuile_core::runtime::in_process_with;
use tuile_core::source::TileId;
use tuile_core::traversal::{Config, ViewState};

use crate::{WebFetcher, WebIonHttp};

/// Cesium World Terrain, and Bing Aerial. The same two assets the native viewer
/// resolves, named here so the page needs to know neither.
const TERRAIN_ASSET: u64 = 1;
const IMAGERY_ASSET: u64 = 2;

/// The engine's handle, held by the worker's JS shim.
#[wasm_bindgen]
pub struct WorkerEngine {
    stream: tuile_core::protocol::InProcessStream,
    /// Imagery coordinates whose pixels have already crossed. See the module
    /// note: this is what stops one texture crossing once per tile that drapes
    /// it.
    sent_imagery: HashSet<ImageryCoord>,
}

#[wasm_bindgen]
impl WorkerEngine {
    /// Resolves the ion assets, assembles the globe and starts the server on
    /// this worker's event loop.
    ///
    /// `spawn_local` rather than a runtime: a worker is one thread, and the
    /// server is one future by construction (`runtime.rs` spawns nothing). The
    /// browser's event loop is a perfectly good executor for it.
    ///
    /// The token is used and not stored: it reaches `IonClient` and this
    /// function's frame ends. Nothing here logs it.
    pub async fn start(token: String, max_sse: f64) -> Result<WorkerEngine, JsError> {
        // First line of the first entry point: a panic before this is a bare
        // `RuntimeError: unreachable` in the console, with no message and a
        // stack of raw wasm offsets. After it, the panic says what and where.
        console_error_panic_hook::set_once();
        let ion = tuile_cesium_ion::IonClient::new(Arc::new(WebIonHttp), token.clone());
        let terrain = tuile_cesium_ion::IonTerrainSource::new(
            tuile_cesium_ion::IonClient::new(Arc::new(WebIonHttp), token),
            TERRAIN_ASSET,
        );
        let layer = terrain
            .layer()
            .await
            .map_err(|e| JsError::new(&format!("ion terrain: {e}")))?;

        let endpoint = match ion
            .asset_endpoint(IMAGERY_ASSET)
            .await
            .map_err(|e| JsError::new(&format!("ion imagery: {e}")))?
        {
            tuile_cesium_ion::AssetEndpoint::Imagery(e) => e,
            _ => return Err(JsError::new("ion asset 2 is not imagery")),
        };
        let o = &endpoint.options;
        let meta_url = tuile_bing::BingMetadata::metadata_url(
            o.url.as_deref().ok_or_else(|| JsError::new("bing url"))?,
            o.map_style.as_deref().unwrap_or("Aerial"),
            o.key.as_deref().ok_or_else(|| JsError::new("bing key"))?,
        );
        let bing =
            tuile_bing::BingImageryProvider::from_metadata_url(Arc::new(WebFetcher), &meta_url)
                .await
                .map_err(|e| JsError::new(&format!("bing metadata: {e}")))?;

        // The Bing provider goes in bare. `CachedImagery` exists to put a
        // `ContentStore` in front of a provider, and there is no store here yet
        // (IndexedDB is its own piece of work); the sharing that actually
        // matters — one decoded texture per coordinate, however many terrain
        // tiles drape it — lives in `PlanetaryLoader`'s own cache and is
        // unaffected.
        let (tree, loader, _detail, _heights) = tuile_planetary::globe(
            terrain,
            bing,
            layer,
            tuile_planetary::GlobeOptions::default(),
        );

        let config = Config {
            maximum_screen_space_error: max_sse.max(1.0),
            ..Config::default()
        };
        let (stream, server) = in_process_with(tree, loader, config);
        wasm_bindgen_futures::spawn_local(async move {
            server.run().await;
            // The server returning is never routine here — the page holds the
            // only sender, so this means the stream closed under it.
            tracing::error!("the geometry server stopped; no more tiles will arrive");
        });

        Ok(WorkerEngine {
            stream,
            sent_imagery: HashSet::new(),
        })
    }

    /// Hands the server the camera. One call per frame; the server coalesces.
    #[allow(clippy::too_many_arguments)]
    pub fn set_view(
        &self,
        eye_x: f64,
        eye_y: f64,
        eye_z: f64,
        dir_x: f64,
        dir_y: f64,
        dir_z: f64,
        up_x: f64,
        up_y: f64,
        up_z: f64,
        width: f64,
        height: f64,
        fovy: f64,
    ) -> Result<(), JsError> {
        let view = ViewState::perspective(
            glam::DVec3::new(eye_x, eye_y, eye_z),
            glam::DVec3::new(dir_x, dir_y, dir_z).normalize_or_zero(),
            glam::DVec3::new(up_x, up_y, up_z).normalize_or_zero(),
            glam::DVec2::new(width, height),
            fovy,
        );
        self.stream
            .send(ClientMessage::ViewerState { views: vec![view], generation: 0 })
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Everything the server has produced since the last call, as a JS array.
    ///
    /// Non-blocking: it drains what is ready and returns. A frame that finds
    /// nothing simply draws what it already had, which is the correct behaviour
    /// for a streaming renderer and the reason this is a poll rather than a
    /// callback.
    pub fn drain(&mut self) -> js_sys::Array {
        let out = js_sys::Array::new();
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        loop {
            match self.stream.poll_message(&mut cx) {
                std::task::Poll::Ready(Some(msg)) => {
                    if let Some(value) = self.encode(msg) {
                        out.push(&value);
                    }
                }
                // `None` is the server gone; the page is told once, through the
                // same channel as everything else.
                std::task::Poll::Ready(None) => {
                    let obj = js_sys::Object::new();
                    set(&obj, "kind", &JsValue::from_str("closed"));
                    out.push(&obj);
                    break;
                }
                std::task::Poll::Pending => break,
            }
        }
        out
    }

    /// Turns one server message into the object the page consumes.
    fn encode(&mut self, msg: ServerMessage) -> Option<JsValue> {
        let obj = js_sys::Object::new();
        match msg {
            ServerMessage::Select { tiles, stats } => {
                set(&obj, "kind", &JsValue::from_str("select"));
                let ids = js_sys::Array::new();
                for (tile, _sse) in &tiles {
                    ids.push(&JsValue::from_str(&tile_key(*tile)));
                }
                set(&obj, "tiles", &ids);
                set(&obj, "selected", &JsValue::from_f64(stats.selected as f64));
                set(&obj, "visited", &JsValue::from_f64(stats.visited as f64));
                set(&obj, "culled", &JsValue::from_f64(stats.culled as f64));
            }
            ServerMessage::Evict { tiles } => {
                set(&obj, "kind", &JsValue::from_str("evict"));
                let ids = js_sys::Array::new();
                for tile in &tiles {
                    ids.push(&JsValue::from_str(&tile_key(*tile)));
                }
                set(&obj, "tiles", &ids);
            }
            // A take-back of stand-ins only. Forwarded under its own kind so
            // the page can drop exactly the meshes it flagged as fills — an
            // "evict" here would also drop real content still queued page-side,
            // which is the failure the variant exists to prevent.
            ServerMessage::Retire { tiles } => {
                set(&obj, "kind", &JsValue::from_str("retire"));
                let ids = js_sys::Array::new();
                for tile in &tiles {
                    ids.push(&JsValue::from_str(&tile_key(*tile)));
                }
                set(&obj, "tiles", &ids);
            }
            // A stand-in reaches the page through the same door as content and
            // says so, so THREE can drop it the moment the real tile lands — and
            // so a page held together by approximations can be seen to be.
            ServerMessage::Fill { tile, content } => {
                set(&obj, "kind", &JsValue::from_str("add"));
                set(&obj, "fill", &JsValue::from_bool(true));
                set(&obj, "tile", &JsValue::from_str(&tile_key(tile)));
                let origin = js_sys::Array::new();
                origin.push(&JsValue::from_f64(content.local_origin_ecef.x));
                origin.push(&JsValue::from_f64(content.local_origin_ecef.y));
                origin.push(&JsValue::from_f64(content.local_origin_ecef.z));
                set(&obj, "origin", &origin);
                let meshes = js_sys::Array::new();
                for mesh in &content.meshes {
                    let m = js_sys::Object::new();
                    set(&m, "positions", &flat3(&mesh.positions));
                    if let Some(normals) = &mesh.normals {
                        set(&m, "normals", &flat3(normals));
                    }
                    if let Some(uvs) = &mesh.uvs {
                        set(&m, "uvs", &flat2(uvs));
                    }
                    set(&m, "indices", &js_sys::Uint32Array::from(&mesh.indices[..]));
                    meshes.push(&m);
                }
                set(&obj, "meshes", &meshes);
                set(&obj, "imagery", &js_sys::Array::new());
            }
            ServerMessage::Content { tile, content } => {
                let TileContent::Decoded(decoded) = content else {
                    // `Raw` never reaches a consumer in the in-process binding.
                    return None;
                };
                set(&obj, "kind", &JsValue::from_str("add"));
                set(&obj, "tile", &JsValue::from_str(&tile_key(tile)));
                // f64, and it must stay f64: this is the whole anti-jitter
                // mechanism. The page keeps it in double precision and folds it
                // against the camera each frame.
                let origin = js_sys::Array::new();
                origin.push(&JsValue::from_f64(decoded.local_origin_ecef.x));
                origin.push(&JsValue::from_f64(decoded.local_origin_ecef.y));
                origin.push(&JsValue::from_f64(decoded.local_origin_ecef.z));
                set(&obj, "origin", &origin);

                let meshes = js_sys::Array::new();
                for mesh in &decoded.meshes {
                    let m = js_sys::Object::new();
                    set(&m, "positions", &flat3(&mesh.positions));
                    if let Some(normals) = &mesh.normals {
                        set(&m, "normals", &flat3(normals));
                    }
                    if let Some(uvs) = &mesh.uvs {
                        set(&m, "uvs", &flat2(uvs));
                    }
                    let indices = js_sys::Uint32Array::from(&mesh.indices[..]);
                    set(&m, "indices", &indices);
                    meshes.push(&m);
                }
                set(&obj, "meshes", &meshes);

                // The packed table, exactly as the native backend uploads it.
                //
                // The page used to receive one object per real layer and pad
                // the rest itself — which meant it also owned a copy of the
                // slot count, of the empty-coverage sentinel and of the
                // identity placement. Four hand-written copies of a rule
                // `imagery_layer_table` already states, and nothing tested any
                // of them. It is documented as "the contract *between*
                // backends"; sending its output is what makes that true rather
                // than aspirational.
                // The floor rather than a detected count: this worker does not own the
                // WebGL context that would report one, and the page it posts to
                // is free to split the layers into passes of whatever width it
                // can bind. Sending the packed table at the floor is the widest
                // shape every consumer can read.
                let table = tuile_core::raster::imagery_layer_table(
                    &decoded.imagery,
                    tuile_core::raster::MIN_IMAGERY_SLOTS,
                );
                set(
                    &obj,
                    "layerTable",
                    &js_sys::Float32Array::from(table.as_flattened()),
                );
                // And the sentinel it pads with, so a consumer that has to mask
                // a slot for its own reasons masks it the same way.
                set(
                    &obj,
                    "emptyCoverage",
                    &js_sys::Float32Array::from(&tuile_core::raster::EMPTY_COVERAGE[..]),
                );

                // Identity and pixels only: which texture belongs in which slot,
                // in slot order, and the bytes the first time a coordinate is
                // seen.
                let layers = js_sys::Array::new();
                for layer in &decoded.imagery {
                    let l = js_sys::Object::new();
                    set(&l, "coord", &JsValue::from_str(&coord_key(layer.coord)));
                    if self.sent_imagery.insert(layer.coord) {
                        set(&l, "width", &JsValue::from_f64(layer.texture.width as f64));
                        set(
                            &l,
                            "height",
                            &JsValue::from_f64(layer.texture.height as f64),
                        );
                        let rgba = js_sys::Uint8Array::from(&layer.texture.rgba8[..]);
                        set(&l, "rgba", &rgba);
                    }
                    layers.push(&l);
                }
                set(&obj, "imagery", &layers);
            }
            // The warm-up's own progress. Relayed rather than swallowed so the
            // page can hold its globe back exactly as the native viewer holds
            // its window back — and, more importantly, so a warm-up that stalls
            // is visible instead of looking like a slow network.
            ServerMessage::Priming(p) => {
                set(&obj, "kind", &JsValue::from_str("priming"));
                set(&obj, "total", &JsValue::from_f64(p.total as f64));
                set(
                    &obj,
                    "outstanding",
                    &JsValue::from_f64(p.outstanding as f64),
                );
                set(
                    &obj,
                    "unavailable",
                    &JsValue::from_f64(p.unavailable as f64),
                );
                set(&obj, "expected", &JsValue::from_f64(p.expected() as f64));
                set(&obj, "settled", &JsValue::from_bool(p.settled()));
            }
            ServerMessage::Error { tile, message } => {
                set(&obj, "kind", &JsValue::from_str("error"));
                if let Some(tile) = tile {
                    set(&obj, "tile", &JsValue::from_str(&tile_key(tile)));
                }
                set(&obj, "message", &JsValue::from_str(&message));
            }
        }
        Some(obj.into())
    }
}

/// A tile's identity as the page's `Map` key. Strings because a JS `Map` keyed
/// by object compares by reference, which would never hit.
fn tile_key(tile: TileId) -> String {
    let (z, x, y) = tile.terrain_coord();
    format!("{z}/{x}/{y}")
}

fn coord_key(coord: ImageryCoord) -> String {
    format!("{}/{}/{}", coord.level, coord.x, coord.y)
}

/// `Vec<[f32; 3]>` is already contiguous; this reinterprets rather than repacks.
///
/// `as_flattened` is the safe primitive for exactly this — the workspace forbids
/// `unsafe_code`, and a manual `Vec<f32>` copy would allocate a second buffer per
/// mesh for no gain.
fn flat3(values: &[[f32; 3]]) -> js_sys::Float32Array {
    js_sys::Float32Array::from(values.as_flattened())
}

fn flat2(values: &[[f32; 2]]) -> js_sys::Float32Array {
    js_sys::Float32Array::from(values.as_flattened())
}

fn set(obj: &js_sys::Object, key: &str, value: &JsValue) {
    // Defining a property on a fresh object cannot fail; a failure here would
    // mean the JS heap is gone, and nothing after it would run either.
    let _ = js_sys::Reflect::set(obj, &JsValue::from_str(key), value);
}
