// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

use std::ops::Range;

use futures_util::stream::{self, StreamExt, TryStreamExt};

use js_sys::{ArrayBuffer, Promise, Uint8Array};
use tuile_film::{blob_start, PREAMBLE};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Blob, Request, RequestInit, Response, Window, WorkerGlobalScope};

/// Where a pack's bytes are read from, a range at a time.
///
/// Never the whole pack: a worker reads the table once, then only the tiles
/// its frames bring in. A `Blob` is sliced; a URL is the pack's block URL on
/// the bench's API — `…/b/{block}/<key>` — read a fixed block at a time.
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

/// The unit a pack is read in over HTTP: the API's fixed blocks, each a URL
/// of its own answered whole. Never a range: a ranged reply is kept by no
/// cache one can count on, and a range is whatever its asker computed. The
/// same blocks answer whichever frames, slices and worker counts a film is
/// rendered with, so a block read once — by this worker, another, or an
/// earlier render — comes from the browser's cache, and one nobody here has
/// read comes from the edge's.
const BLOCK: u64 = 8 << 20;
/// Where a block's number goes in a pack's URL.
const BLOCK_PLACEHOLDER: &str = "{block}";

/// The size of a block, for whoever plans which to fetch.
pub const BLOCK_BYTES: u64 = BLOCK;
/// Blocks in flight at once, per read.
const BLOCKS_AT_ONCE: usize = 2;
/// Tries at a block before giving up. A server under a film's load fails a
/// request now and then; the same request a moment later goes through.
const TRIES: u32 = 5;

fn err(e: impl std::fmt::Debug) -> JsError {
    JsError::new(&format!("{e:?}"))
}

async fn sleep(ms: i32) {
    let promise = Promise::new(&mut |resolve, _| {
        let global = js_sys::global();
        let set = js_sys::Reflect::get(&global, &"setTimeout".into()).ok();
        if let Some(set) = set.and_then(|f| f.dyn_into::<js_sys::Function>().ok()) {
            let _ = set.call2(&global, &resolve, &ms.into());
        }
    });
    let _ = JsFuture::from(promise).await;
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

    /// One block: its bytes, and the object's size when the reply says.
    /// `Err((true, …))` is a failure worth trying again.
    async fn get(url: &str) -> Result<(Vec<u8>, Option<u64>), (bool, String)> {
        let hard = |e: JsValue| (false, format!("{e:?}"));
        let request = Request::new_with_str_and_init(url, &RequestInit::new()).map_err(hard)?;
        // A network failure is the kind that passes.
        let response: Response = JsFuture::from(fetch(&request).map_err(hard)?)
            .await
            .map_err(|e| (true, format!("{e:?}")))?
            .unchecked_into();
        let status = response.status();
        if status != 200 {
            return Err((status >= 500 || status == 429, format!("HTTP {status}")));
        }
        let total = response
            .headers()
            .get("x-object-size")
            .ok()
            .flatten()
            .and_then(|v| v.trim().parse().ok());
        let buffer: ArrayBuffer = JsFuture::from(response.array_buffer().map_err(hard)?)
            .await
            .map_err(|e| (true, format!("{e:?}")))?
            .unchecked_into();
        Ok((Uint8Array::new(&buffer).to_vec(), total))
    }

    /// One block of a pack, tried again on a passing failure. `template` is
    /// the pack's block URL, with `{block}` where the number goes.
    async fn block(template: &str, index: u64) -> Result<Vec<u8>, JsError> {
        let url = template.replace(BLOCK_PLACEHOLDER, &index.to_string());
        let start = index * BLOCK;
        let mut wait = 150;
        for attempt in 1..=TRIES {
            match Self::get(&url).await {
                Ok((bytes, total)) => {
                    // Every block is whole but the last, which is as long as
                    // the object allows.
                    let wanted =
                        total.map_or(BLOCK, |t| (start + BLOCK).min(t).saturating_sub(start));
                    let whole =
                        bytes.len() as u64 == wanted || (total.is_none() && !bytes.is_empty());
                    if whole {
                        return Ok(bytes);
                    }
                    if attempt == TRIES {
                        return Err(JsError::new(&format!(
                            "{url}: {} bytes, not the block's {wanted}",
                            bytes.len()
                        )));
                    }
                }
                Err((retry, why)) if !retry || attempt == TRIES => {
                    return Err(JsError::new(&format!(
                        "{url}: {why}, after {attempt} tries"
                    )));
                }
                Err(_) => {}
            }
            sleep(wait).await;
            wait *= 3;
        }
        Err(JsError::new(&format!("{url}: could not be read")))
    }

    /// One block of a pack by its number, for a reader fetching ahead.
    pub async fn fetch_block(&self, index: u64) -> Result<Vec<u8>, JsError> {
        match self {
            Source::Url(url) => Self::block(url, index).await,
            Source::Blob(_) => self.read(index * BLOCK..(index + 1) * BLOCK).await,
        }
    }

    /// `range` of the pack. A URL is read in whole blocks, which the
    /// browser's cache answers when it has seen them.
    pub async fn read(&self, range: Range<u64>) -> Result<Vec<u8>, JsError> {
        let wanted = (range.end - range.start) as usize;
        if wanted == 0 {
            return Ok(Vec::new());
        }
        match self {
            Source::Blob(blob) => {
                let part = blob
                    .slice_with_f64_and_f64(range.start as f64, range.end as f64)
                    .map_err(err)?;
                let buffer: ArrayBuffer = JsFuture::from(part.array_buffer())
                    .await
                    .map_err(err)?
                    .unchecked_into();
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
            Source::Url(url) => {
                if !url.contains(BLOCK_PLACEHOLDER) {
                    return Err(JsError::new(&format!(
                        "{url}: a pack's URL names its blocks with {BLOCK_PLACEHOLDER}"
                    )));
                }
                // Whole blocks, a few at a time, in order.
                let blocks = range.start / BLOCK..range.end.div_ceil(BLOCK);
                let parts: Vec<Vec<u8>> = stream::iter(blocks.clone())
                    .map(|index| Self::block(url, index))
                    .buffered(BLOCKS_AT_ONCE)
                    .try_collect()
                    .await?;
                let mut out = Vec::with_capacity(wanted);
                for (index, part) in blocks.zip(parts.iter()) {
                    let base = index * BLOCK;
                    let from = (range.start.max(base) - base) as usize;
                    let to =
                        (range.end.min(base + part.len() as u64).saturating_sub(base)) as usize;
                    if from > to || to > part.len() {
                        return Err(JsError::new(&format!(
                            "{url}: bytes {}-{} are past its end",
                            range.start, range.end
                        )));
                    }
                    out.extend_from_slice(&part[from..to]);
                }
                if out.len() != wanted {
                    return Err(JsError::new(&format!(
                        "{url}: asked for {wanted} bytes at {}, got {}",
                        range.start,
                        out.len()
                    )));
                }
                Ok(out)
            }
        }
    }

    /// The file up to its blob region: everything `Pack::open_table` needs.
    pub async fn head(&self) -> Result<Vec<u8>, JsError> {
        let preamble = self.read(0..PREAMBLE as u64).await?;
        let start = blob_start(&preamble).map_err(|e| JsError::new(&e.to_string()))?;
        self.read(0..start).await
    }
}
