// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The browser side of tuile: a `fetch`-backed transport, so the same engine
//! that runs on a laptop runs in a Web Worker.
//!
//! # Why a crate rather than an example
//!
//! `tuile-core` takes no backend, and the browser is a backend like any other —
//! `web-sys` is as much a transport dependency as `reqwest` is. This crate is
//! the browser's counterpart to `tuile-native-fetchers`: it implements the same
//! two seams ([`TileFetcher`] and [`IonHttp`]) against `window.fetch`, and adds
//! nothing else to the engine's vocabulary.
//!
//! # Where it runs
//!
//! In a **Worker**, not on the page's thread. That is the entire point: the
//! engine's traversal, its decodes and its resamples are CPU work, and on the
//! main thread they are work the browser cannot paint through. `fetch` is
//! available in a worker through `WorkerGlobalScope`, which is why
//! [`global_fetch`] looks for either scope rather than assuming a `Window`.
//!
//! # Single-threaded, and honest about it
//!
//! A `Response` is a JS object and JS objects are not `Send`. The engine's
//! traits are already declared `?Send` under `target_arch = "wasm32"`, so the
//! futures here never claim otherwise, and the server is driven by
//! `spawn_local` rather than a work-stealing runtime. Nothing here would be
//! made faster by pretending: a worker has one thread, and parallelism comes
//! from running several workers, not from a scheduler inside one.

#![cfg(target_arch = "wasm32")]

pub mod worker;

use async_trait::async_trait;
use bytes::Bytes;
use url::Url;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use tuile_cesium_ion::{HttpResponse, IonError, IonHttp};
use tuile_core::fetch::{FetchError, Fetched, TileFetcher};

/// `fetch` from whichever global this code is running under.
///
/// A Worker has no `window`; a page has no `WorkerGlobalScope`. Asking for the
/// wrong one is the classic way for browser code to work in a demo page and
/// fail in the worker it was written for, so both are tried and the failure is
/// a stated error rather than a panic in JS.
fn global_fetch(request: &web_sys::Request) -> Result<js_sys::Promise, JsValue> {
    let global = js_sys::global();
    if let Some(window) = global.dyn_ref::<web_sys::Window>() {
        return Ok(window.fetch_with_request(request));
    }
    if let Some(worker) = global.dyn_ref::<web_sys::WorkerGlobalScope>() {
        return Ok(worker.fetch_with_request(request));
    }
    Err(JsValue::from_str(
        "no fetch: neither a Window nor a WorkerGlobalScope",
    ))
}

/// Turns whatever JS threw into something printable. `JsValue` has no `Display`
/// and its `Debug` is noisy, so this is the one place the conversion happens.
fn js_message(value: &JsValue) -> String {
    value
        .as_string()
        .or_else(|| {
            value
                .dyn_ref::<js_sys::Error>()
                .map(|e| String::from(e.message()))
        })
        .unwrap_or_else(|| format!("{value:?}"))
}

/// The bytes of one response, or the reason there are none.
async fn get(url: &Url, bearer: Option<&str>) -> Result<(u16, Bytes, Option<u64>), String> {
    let opts = web_sys::RequestInit::new();
    opts.set_method("GET");
    // Tiles are served cross-origin (ion, Bing) and answer with permissive CORS
    // headers; `Cors` is what lets the bytes be *read* rather than merely
    // fetched opaquely, which `NoCors` would give us.
    opts.set_mode(web_sys::RequestMode::Cors);

    let request =
        web_sys::Request::new_with_str_and_init(url.as_str(), &opts).map_err(|e| js_message(&e))?;
    if let Some(token) = bearer {
        request
            .headers()
            .set("Authorization", &format!("Bearer {token}"))
            .map_err(|e| js_message(&e))?;
    }

    let promise = global_fetch(&request).map_err(|e| js_message(&e))?;
    let response: web_sys::Response = JsFuture::from(promise)
        .await
        .map_err(|e| js_message(&e))?
        .dyn_into()
        .map_err(|e| js_message(&e))?;

    let status = response.status();
    let max_age = response
        .headers()
        .get("cache-control")
        .ok()
        .flatten()
        .as_deref()
        .and_then(parse_max_age);

    let buffer = JsFuture::from(response.array_buffer().map_err(|e| js_message(&e))?)
        .await
        .map_err(|e| js_message(&e))?;
    // One copy out of the JS heap into the wasm heap. Unavoidable without
    // shared memory, and the only copy on this path.
    let bytes = Bytes::from(js_sys::Uint8Array::new(&buffer).to_vec());
    Ok((status, bytes, max_age))
}

/// `max-age` from a `Cache-Control` header, when it states one.
///
/// Deliberately tolerant: anything unparsable means "no stated lifetime", which
/// is the same answer as a missing header and is always safe — the caller then
/// invents nothing and re-asks.
fn parse_max_age(header: &str) -> Option<u64> {
    header.split(',').find_map(|part| {
        let part = part.trim();
        let rest = part.strip_prefix("max-age")?.trim_start();
        rest.strip_prefix('=')?.trim().parse().ok()
    })
}

/// Fetches tiles with the browser's own `fetch`.
///
/// Stateless, which is what lets it satisfy the engine's `Send + Sync` bound on
/// a target where JS values are neither: it holds no `Response`, no `Window`,
/// nothing from the JS heap — each call reaches for the global scope itself.
#[derive(Debug, Default, Clone, Copy)]
pub struct WebFetcher;

#[async_trait(?Send)]
impl TileFetcher for WebFetcher {
    async fn fetch(&self, url: &Url) -> Result<Bytes, FetchError> {
        Ok(self.fetch_cacheable(url).await?.value)
    }

    async fn fetch_cacheable(&self, url: &Url) -> Result<Fetched<Bytes>, FetchError> {
        let (status, bytes, max_age) = get(url, None).await.map_err(|message| FetchError::Io {
            url: url.clone(),
            message,
        })?;
        // 404 is a fact about the data — the ocean has no terrain tile — and the
        // engine has a distinct arm for it that stops the re-asking. Collapsing
        // it into a transport error would make every absent tile a permanent
        // retry.
        if status == 404 {
            return Err(FetchError::NotFound(url.clone()));
        }
        if !(200..300).contains(&status) {
            return Err(FetchError::Status {
                status,
                url: url.clone(),
            });
        }
        Ok(Fetched {
            value: bytes,
            ttl: max_age.map(std::time::Duration::from_secs),
        })
    }
}

/// The same transport, wearing the ion API's hat.
///
/// ion needs a bearer token and reports its own status codes, so it has its own
/// seam; both land on the same [`get`].
#[derive(Debug, Default, Clone, Copy)]
pub struct WebIonHttp;

#[async_trait(?Send)]
impl IonHttp for WebIonHttp {
    async fn get(&self, url: &Url, bearer: Option<&str>) -> Result<HttpResponse, IonError> {
        let (status, body, max_age) = get(url, bearer)
            .await
            .map_err(|e| IonError::Transport(format!("{url}: {e}")))?;
        Ok(HttpResponse {
            status,
            body,
            max_age: max_age.map(std::time::Duration::from_secs),
        })
    }
}
