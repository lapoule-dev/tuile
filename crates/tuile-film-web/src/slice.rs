// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! One Web Worker, one slice of a film.
//!
//! The page hands a worker a contiguous range of frames and the packs it
//! crosses. The worker walks them pack by pack — a pack shares nothing with
//! the next, so each gets its own renderer — fetches ahead the blocks its
//! frames read, renders each frame to an `OffscreenCanvas`, gives it to the
//! encoder, and tells the page as it goes. What it sends back at the end is
//! the slice, encoded, for the page to join with the others.
//!
//! Messages, both ways, are plain objects with a `type`:
//!
//! - in: the order — `packs: [{source, first, last}]`, `filmFirst`, `width`,
//!   `height`, `supersample`, `fps`, `bitrate`, `tone` (how much of the
//!   store's tone correction, 0 to 1), and an `id` echoed back;
//! - out: `preload` (blocks fetched ahead), `frame` (one frame's account,
//!   now and then with a `preview` bitmap), `done` (the encoded slice) or
//!   `error`.

use js_sys::{Array, Object, Uint8Array};
use wasm_bindgen::prelude::*;
use web_sys::{DedicatedWorkerGlobalScope, MessageEvent, OffscreenCanvas};

use crate::encode::Encoder;
use crate::js::{call, get, now, number, object, settled, text};
use crate::worker::FilmWorker;

fn scope() -> DedicatedWorkerGlobalScope {
    js_sys::global().unchecked_into()
}

fn post(message: &Object) {
    let _ = scope().post_message(message);
}

fn post_with(message: &Object, transfer: &Array) {
    let _ = scope().post_message_with_transfer(message, transfer);
}

/// Makes this worker a slice renderer: it waits for its order.
#[wasm_bindgen]
pub fn start_slice() {
    console_error_panic_hook::set_once();
    let on_order = Closure::<dyn FnMut(MessageEvent)>::new(|event: MessageEvent| {
        wasm_bindgen_futures::spawn_local(run(event.data()));
    });
    scope().set_onmessage(Some(on_order.as_ref().unchecked_ref()));
    // The handler lives as long as the worker does.
    on_order.forget();
}

async fn run(order: JsValue) {
    let id = get(&order, "id");
    if let Err(why) = render(&order, &id).await {
        post(&object(&[
            ("type", "error".into()),
            ("id", id),
            ("message", why.into()),
        ]));
    }
}

async fn render(order: &JsValue, id: &JsValue) -> Result<(), String> {
    let (width, height) = (
        number(order, "width") as u32,
        number(order, "height") as u32,
    );
    let (supersample, fps) = (
        number(order, "supersample") as u32,
        number(order, "fps") as u32,
    );
    let film_first = number(order, "filmFirst") as u32;
    let canvas = OffscreenCanvas::new(width, height).map_err(text)?;
    let mut encoder = Encoder::open(width, height, fps, number(order, "bitrate")).await?;

    let started = now();
    let (mut n, mut blocks, mut bytes) = (0u32, 0u32, 0f64);
    for pack in Array::from(&get(order, "packs")).iter() {
        let (first, last) = (number(&pack, "first") as u32, number(&pack, "last") as u32);
        let mut film = FilmWorker::create(
            canvas.clone(),
            get(&pack, "source"),
            first,
            last,
            supersample,
            number(order, "tone") as f32,
        )
        .await
        .map_err(text)?;
        encoder.prepare(&mut film);

        // Every block this slice of the pack reads, fetched before its first
        // frame — into the browser's cache, where the render then finds it.
        let (so_far, to) = (blocks, id.clone());
        let progress = Closure::<dyn FnMut(u32, u32)>::new(move |done: u32, total: u32| {
            if done == total || done % 4 == 0 {
                post(&object(&[
                    ("type", "preload".into()),
                    ("id", to.clone()),
                    ("done", (so_far + done).into()),
                    ("total", (so_far + total).into()),
                ]));
            }
        });
        let ahead = film
            .preload(progress.as_ref().unchecked_ref())
            .await
            .map_err(text)?;
        blocks += ahead.blocks;
        bytes += ahead.bytes;
        post(&object(&[
            ("type", "preload".into()),
            ("id", id.clone()),
            ("done", blocks.into()),
            ("total", blocks.into()),
            ("bytes", bytes.into()),
        ]));

        loop {
            let t0 = now();
            let Some(stats) = film.next().await.map_err(text)? else {
                break;
            };
            let t1 = now();
            encoder
                .frame(&film, &canvas, stats.frame - film_first, n)
                .await?;
            let t2 = now();

            let message = object(&[
                ("type", "frame".into()),
                ("id", id.clone()),
                ("frame", stats.frame.into()),
                ("fetch", stats.fetch_ms.into()),
                ("fetchedBytes", stats.fetched_bytes.into()),
                ("requests", stats.requests.into()),
                ("unpack", stats.unpack_ms.into()),
                ("decode", stats.decode_ms.into()),
                ("upload", stats.upload_ms.into()),
                ("record", stats.record_ms.into()),
                ("next", (t1 - t0).into()),
                ("encode", (t2 - t1).into()),
            ]);
            // A small picture of the frame now and then, for the page to show.
            let preview = match n % 8 {
                0 => preview_of(&canvas).await,
                _ => None,
            };
            match preview {
                Some(bitmap) => {
                    let _ = js_sys::Reflect::set(&message, &"preview".into(), &bitmap);
                    post_with(&message, &Array::of1(&bitmap));
                }
                None => post(&message),
            }
            n += 1;
        }
    }

    let encoded = encoder.finish().await?;
    let record = Uint8Array::from(encoded.record.as_slice());
    let transfer = Array::of1(&record.buffer());
    let chunks = Array::new();
    for chunk in &encoded.chunks {
        let data = Uint8Array::from(chunk.data.as_slice());
        transfer.push(&data.buffer());
        chunks.push(&object(&[
            ("index", chunk.index.into()),
            ("key", chunk.key.into()),
            ("data", data.into()),
        ]));
    }
    post_with(
        &object(&[
            ("type", "done".into()),
            ("id", id.clone()),
            ("codec", encoded.codec.into()),
            ("record", record.into()),
            ("chunks", chunks.into()),
            ("seconds", ((now() - started) / 1000.0).into()),
        ]),
        &transfer,
    );
    Ok(())
}

/// The canvas as it stands, reduced: a bitmap the page can draw.
async fn preview_of(canvas: &OffscreenCanvas) -> Option<JsValue> {
    let options = object(&[
        ("resizeWidth", 480.into()),
        ("resizeQuality", "medium".into()),
    ]);
    let asked = call(
        &js_sys::global(),
        "createImageBitmap",
        &[canvas.clone().into(), options.into()],
    )
    .ok()?;
    settled(asked).await.ok()
}
