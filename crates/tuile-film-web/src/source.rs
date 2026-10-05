// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use std::ops::Range;

use js_sys::{ArrayBuffer, Promise, Uint8Array};
use tuile_film::{blob_start, PREAMBLE};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Blob, Request, RequestInit, Response, Window, WorkerGlobalScope};

/// Where a pack's bytes are read from, a range at a time.
///
/// Never the whole pack: a worker reads the table once, then only the tiles
/// its frames bring in. A `Blob` (a file the user picked) is shared between
/// workers without a copy; a URL is read with `Range` requests, which is how
/// `tuile-pack-api` serves the farm's packs.
#[derive(Clone)]
pub enum Source {
    Blob(Blob),
    Url(String),
}

fn fetch(request: &Request) -> Result<Promise, JsValue> {
    let global = js_sys::global();
    if let Some(worker) = global.dyn_ref::<WorkerGlobalScope>() {
        return Ok(worker.fetch_with_request(request));
    }
    Ok(global
        .unchecked_into::<Window>()
        .fetch_with_request(request))
}

impl Source {
    pub fn from_js(value: &JsValue) -> Result<Self, JsError> {
        if let Some(url) = value.as_string() {
            return Ok(Source::Url(url));
        }
        value
            .dyn_ref::<Blob>()
            .map(|b| Source::Blob(b.clone()))
            .ok_or_else(|| JsError::new("a pack source is a URL or a Blob"))
    }

    pub async fn read(&self, range: Range<u64>) -> Result<Vec<u8>, JsError> {
        let wanted = (range.end - range.start) as usize;
        let buffer: ArrayBuffer = match self {
            Source::Blob(blob) => {
                let part = blob
                    .slice_with_f64_and_f64(range.start as f64, range.end as f64)
                    .map_err(|e| JsError::new(&format!("{e:?}")))?;
                JsFuture::from(part.array_buffer())
                    .await
                    .map_err(|e| JsError::new(&format!("{e:?}")))?
                    .unchecked_into()
            }
            Source::Url(url) => {
                let init = RequestInit::new();
                let request = Request::new_with_str_and_init(url, &init)
                    .map_err(|e| JsError::new(&format!("{e:?}")))?;
                request
                    .headers()
                    .set("Range", &format!("bytes={}-{}", range.start, range.end - 1))
                    .map_err(|e| JsError::new(&format!("{e:?}")))?;
                let response: Response =
                    JsFuture::from(fetch(&request).map_err(|e| JsError::new(&format!("{e:?}")))?)
                        .await
                        .map_err(|e| JsError::new(&format!("{url}: {e:?}")))?
                        .unchecked_into();
                // 206 is the only answer that is the range: a 200 is the whole
                // pack, which must never be pulled into a worker by accident.
                if response.status() != 206 {
                    return Err(JsError::new(&format!(
                        "{url}: HTTP {} for bytes {}-{} (a ranged server is needed)",
                        response.status(),
                        range.start,
                        range.end - 1
                    )));
                }
                JsFuture::from(
                    response
                        .array_buffer()
                        .map_err(|e| JsError::new(&format!("{e:?}")))?,
                )
                .await
                .map_err(|e| JsError::new(&format!("{e:?}")))?
                .unchecked_into()
            }
        };
        let bytes = Uint8Array::new(&buffer).to_vec();
        if bytes.len() != wanted {
            return Err(JsError::new(&format!(
                "asked for {wanted} bytes at {}, got {}",
                range.start,
                bytes.len()
            )));
        }
        Ok(bytes)
    }

    /// The file up to its blob region: everything `Pack::open_table` needs.
    pub async fn head(&self) -> Result<Vec<u8>, JsError> {
        let preamble = self.read(0..PREAMBLE as u64).await?;
        let start = blob_start(&preamble).map_err(|e| JsError::new(&e.to_string()))?;
        self.read(0..start).await
    }
}
