// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the viewer asks of the page it runs in: `crate::desktop`'s services,
//! answered by the page.
//!
//! What a phone shows over its window, the page shows over its canvas
//! ([`super::screen`]): the caption along the bottom — whose pictures these
//! are, the last thing said — and the alerts. What a desktop says in its
//! title bar, the page also says to its host's script, through the one
//! callback the package installs: `ready` (or `unsupported`), `loading`,
//! `viewChanged`, `attributionChanged` and `error`. Steering comes the other
//! way, through the same queue a desktop's URLs and scripts use. The
//! clipboard is the browser's, and the last view is kept in the page's local
//! storage. One viewer at a time per page, so all of it is the page's.

use std::cell::{Cell, RefCell};
use std::sync::mpsc::{channel, Receiver, Sender};

use wasm_bindgen::JsValue;

use crate::desktop::Pasted;
use crate::snapshot::Snapshot;
use crate::steer::Request;

thread_local! {
    /// The package's event callback: `(kind, detail)`.
    static EVENTS: RefCell<Option<js_sys::Function>> = const { RefCell::new(None) };
    /// Where steering from the page is queued for the render loop.
    static OUTBOX: RefCell<Option<Sender<Request>>> = const { RefCell::new(None) };
    /// The host application's name, for its messages, its menu and the key
    /// its last view is kept under.
    static NAME: RefCell<String> = const { RefCell::new(String::new()) };
    /// The backend the window draws through, once it does.
    static BACKEND: RefCell<Option<String>> = const { RefCell::new(None) };
    /// What was last said, so each event is said on a change only.
    static LOADING: Cell<Option<(bool, Option<u8>)>> = const { Cell::new(None) };
    static ATTRIBUTION: RefCell<Option<String>> = const { RefCell::new(None) };
    /// Whether the view is kept for the next viewer of this name.
    static REMEMBERS: Cell<bool> = const { Cell::new(false) };
}

/// Installs the page's event callback for the viewer named `name`, which
/// keeps its view for the next one when `remembers`, and forgets what the
/// last viewer said.
pub(crate) fn connect(events: js_sys::Function, name: &str, remembers: bool) {
    EVENTS.with(|e| *e.borrow_mut() = Some(events));
    NAME.with(|n| *n.borrow_mut() = name.to_owned());
    REMEMBERS.with(|r| r.set(remembers));
    BACKEND.with(|b| *b.borrow_mut() = None);
    LOADING.with(|l| l.set(None));
    ATTRIBUTION.with(|a| *a.borrow_mut() = None);
}

/// Removes it, and what the viewer laid over the page: a destroyed viewer
/// says nothing more.
pub(crate) fn disconnect() {
    EVENTS.with(|e| *e.borrow_mut() = None);
    OUTBOX.with(|o| *o.borrow_mut() = None);
    super::screen::detach();
}

/// The host application's name.
pub(crate) fn name() -> String {
    NAME.with(|n| n.borrow().clone())
}

/// The backend the window draws through, once it does.
pub(crate) fn backend() -> Option<String> {
    BACKEND.with(|b| b.borrow().clone())
}

/// Calls the page's callback with `kind` and an object built by `detail`.
fn emit(kind: &str, detail: impl FnOnce(&js_sys::Object)) {
    let Some(events) = EVENTS.with(|e| e.borrow().clone()) else {
        return;
    };
    let object = js_sys::Object::new();
    detail(&object);
    if let Err(e) = events.call2(&JsValue::NULL, &JsValue::from_str(kind), &object) {
        tracing::warn!("the page's {kind} handler threw: {e:?}");
    }
}

fn set(object: &js_sys::Object, key: &str, value: impl Into<JsValue>) {
    // Defining a property on a fresh object cannot fail.
    let _ = js_sys::Reflect::set(object, &JsValue::from_str(key), &value.into());
}

/// The queue the render loop drains.
pub(crate) fn listen() -> Receiver<Request> {
    let (tx, rx) = channel();
    OUTBOX.with(|o| *o.borrow_mut() = Some(tx));
    rx
}

/// Queues a request from the page, and wakes the loop to take it.
pub(crate) fn send(request: Request) {
    OUTBOX.with(|o| {
        if let Some(outbox) = o.borrow().as_ref() {
            // A closed queue is a viewer already destroyed.
            let _ = outbox.send(request);
        }
    });
    super::page::poke();
}

/// A message the person asked for — the keys, the about box: over the view.
pub(crate) fn alert(title: &str, text: &str) {
    if !super::screen::try_alert(title, text) {
        tracing::info!("{title}\n\n{text}");
    }
}

/// Something went wrong that the person must hear about: over the view, as
/// the phone presents it, and an `error` event for the host's script.
/// `false` while another message is still up; it is offered again.
pub(crate) fn try_alert(title: &str, text: &str) -> bool {
    if !super::screen::try_alert(title, text) {
        return false;
    }
    emit("error", |o| {
        set(o, "title", title);
        set(o, "message", text);
    });
    true
}

/// The caption along the bottom of the view.
pub(crate) fn caption(text: &str) {
    super::screen::caption(text);
}

/// The caption lifted off the bottom by the credits bar's height.
pub(crate) fn caption_lift(pixels: u32) {
    super::screen::caption_lift(pixels);
}

/// Puts `text` on the browser's clipboard. A key press or a menu click is
/// the person's own gesture, which is what the browser asks before it lets a
/// page write there.
pub(crate) fn copy(text: &str) -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    let written = window.navigator().clipboard().write_text(text);
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = wasm_bindgen_futures::JsFuture::from(written).await {
            tracing::warn!("the browser refused the clipboard: {e:?}");
        }
    });
    true
}

/// Asks the browser for the clipboard's text, which it gives once it has
/// asked the person; the text arrives on the steering queue.
pub(crate) fn paste() -> Pasted {
    let Some(window) = web_sys::window() else {
        return Pasted::Now(None);
    };
    let read = window.navigator().clipboard().read_text();
    wasm_bindgen_futures::spawn_local(async move {
        let text = match wasm_bindgen_futures::JsFuture::from(read).await {
            Ok(text) => text.as_string(),
            Err(e) => {
                tracing::warn!("the browser refused the clipboard: {e:?}");
                None
            }
        };
        send(Request::Pasted(text));
    });
    Pasted::Later
}

/// The window draws: `ready`, with the backend. Or it never will: the note
/// in the view, and `unsupported`, which the package turns into the typed
/// error a host catches.
pub(crate) fn drawing(outcome: Result<&str, &str>) {
    match outcome {
        Ok(backend) => {
            BACKEND.with(|b| *b.borrow_mut() = Some(backend.to_owned()));
            emit("ready", |o| set(o, "backend", backend));
        }
        Err(why) => {
            super::screen::unsupported(why);
            emit("unsupported", |o| set(o, "message", why));
        }
    }
}

/// The indicator: `loading`, when it comes or goes, or its progress moves by a
/// whole percent.
pub(crate) fn loading(shown: bool, progress: Option<f32>) {
    let now = (shown, progress.map(|p| (p.clamp(0.0, 1.0) * 100.0) as u8));
    if LOADING.with(|l| l.replace(Some(now))) == Some(now) {
        return;
    }
    emit("loading", |o| {
        set(o, "shown", shown);
        set(
            o,
            "progress",
            progress.map_or(JsValue::NULL, |p| f64::from(p).into()),
        );
    });
}

/// The view: `viewChanged`, `attributionChanged` when the layer's owner
/// changes — and the view kept for the next viewer, as it moves.
///
/// Kept as it moves rather than on the way out, as a desktop keeps it: a tab
/// that is closed or navigated away from gives the page no turn of its loop
/// to write anything. A few times a second at most, a few dozen bytes.
pub(crate) fn view(snapshot: &Snapshot) {
    if REMEMBERS.with(Cell::get) {
        let link = (!snapshot.located).then(|| crate::steer::link(&snapshot.view));
        keep_the_view(link.as_deref());
    }
    let v = snapshot.view;
    emit("viewChanged", |o| {
        set(o, "lon", v.lon);
        set(o, "lat", v.lat);
        set(o, "altitude", v.altitude);
        set(o, "heading", v.heading);
        set(o, "pitch", v.pitch);
    });
    let Some(layer) = snapshot.imagery else {
        return;
    };
    let changed = ATTRIBUTION.with(|a| {
        let mut said = a.borrow_mut();
        let changed = said.as_deref() != Some(layer.key.as_str());
        *said = Some(layer.key.clone());
        changed
    });
    if changed {
        emit("attributionChanged", |o| {
            set(o, "layer", layer.key.as_str());
            set(o, "name", layer.name.as_str());
            set(o, "attribution", layer.attribution.as_str());
        });
    }
}

/// Where the last view of the viewer named `name` is kept in the page's
/// local storage: one per application, as a desktop keeps one per
/// application.
fn last_view_key(name: &str) -> String {
    format!("tuile-viewer/last-view/{name}")
}

/// The page's local storage, where the browser lets the page have one: a
/// private window, a sandboxed frame or a refusal leave none, and the viewer
/// then simply remembers nothing.
fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

/// The link to the view the last session of `name` ended on, if it left one.
pub(crate) fn remembered_view(name: &str) -> Option<String> {
    storage()?.get_item(&last_view_key(name)).ok().flatten()
}

/// Where the 3D Tiles tilesets a person switched on are kept for the next
/// session of the viewer named `name`: their asset numbers, comma-separated.
fn tilesets_on_key(name: &str) -> String {
    format!("tuile-viewer/tilesets-on/{name}")
}

/// Keeps `on`, the tilesets a person switched on, for the next session;
/// none on forgets the entry.
pub(crate) fn keep_tilesets_on(on: &[i64]) {
    let Some(storage) = storage() else {
        return;
    };
    let key = tilesets_on_key(&name());
    let _ = if on.is_empty() {
        storage.remove_item(&key)
    } else {
        let line: Vec<String> = on.iter().map(i64::to_string).collect();
        storage.set_item(&key, &line.join(","))
    };
}

/// The tilesets the last session of the viewer named `name` left switched
/// on.
pub(crate) fn tilesets_kept_on(name: &str) -> Vec<i64> {
    storage()
        .and_then(|s| s.get_item(&tilesets_on_key(name)).ok().flatten())
        .and_then(|text| crate::start::asset_list("tilesets-on", &text).ok())
        .unwrap_or_default()
}

/// Keeps `link` as the view the next session of this viewer opens on, or —
/// with `None` — forgets the last one.
pub(crate) fn keep_the_view(link: Option<&str>) {
    let Some(storage) = storage() else {
        return;
    };
    let key = last_view_key(&name());
    let _ = match link {
        Some(link) => storage.set_item(&key, link),
        None => storage.remove_item(&key),
    };
}
