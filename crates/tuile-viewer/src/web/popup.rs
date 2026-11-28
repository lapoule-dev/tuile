// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The budget window on a page: a browser window of its own.
//!
//! A page cannot open an operating-system window, but it can open a browser
//! window — its own document, moved, resized and closed apart from the page
//! — which is the closest a browser comes. The page opens it
//! (`window.open`), puts one canvas in it, and paints on that canvas the
//! budget window's picture, drawn by the viewer with the same panel and the
//! same overlay as a desktop's window and read back
//! (`app::budget_screen`). The numbers are the page's own: the signals the
//! budget publishes in the page, never a second computation.
//!
//! A browser opens a window only in answer to a person's gesture, so the
//! menu's click opens it ([`open`], from the click's handler); a `tuile://`
//! URL opens it too where the browser still allows it, and the viewer says
//! so where it does not. The window closed by the person hides the budget
//! window; the page goes on. The page going away closes it — `pagehide` —
//! and where that never ran (a page that crashed), the window says it is
//! disconnected.

use std::cell::RefCell;

use wasm_bindgen::prelude::*;
use wasm_bindgen::{Clamped, JsCast};

/// The window's name: one budget window per page, found again by name.
const NAME: &str = "tuile-budget";

/// Watches the page from the window: once it is gone, the window says so.
const WATCH: &str = "setInterval(function(){if(!window.opener||window.opener.closed){\
var n=document.getElementById('disconnected');if(n){n.style.display='flex';}\
document.title='Budget (disconnected)';}},1000);";

struct Popup {
    window: web_sys::Window,
    canvas: web_sys::HtmlCanvasElement,
    context: web_sys::CanvasRenderingContext2d,
    /// The wheel, the pointer and the keys, held for as long as the window.
    _listeners: Vec<Closure<dyn FnMut(web_sys::Event)>>,
}

thread_local! {
    /// What the person did in the window since the viewer last looked: the
    /// desktop window's inputs, in device pixels.
    static INPUTS: RefCell<Vec<tuile_ui::budget_board::BoardInput>> = const { RefCell::new(Vec::new()) };
    static POPUP: RefCell<Option<Popup>> = const { RefCell::new(None) };
    /// The page's `pagehide` handler, installed once.
    static HIDE: RefCell<Option<Closure<dyn FnMut()>>> = const { RefCell::new(None) };
}

/// Whether the window is open: opened, and not closed since.
pub(crate) fn is_open() -> bool {
    POPUP.with(|p| {
        p.borrow()
            .as_ref()
            .is_some_and(|p| !p.window.closed().unwrap_or(true))
    })
}

/// Opens the window, `points` CSS pixels, or keeps the one open; says
/// whether one is. Call from a gesture's handler: anywhere else a browser
/// may refuse.
pub(crate) fn open(points: (f64, f64)) -> bool {
    if is_open() {
        return true;
    }
    let Some(page) = web_sys::window() else {
        return false;
    };
    let features = format!(
        "popup,width={},height={}",
        points.0.clamp(320.0, 1_600.0).round(),
        points.1.clamp(160.0, 1_000.0).round()
    );
    let Ok(Some(window)) = page.open_with_url_and_target_and_features("", NAME, &features) else {
        return false;
    };
    let Some(popup) = dress(window) else {
        return false;
    };
    POPUP.with(|p| *p.borrow_mut() = Some(popup));
    HIDE.with(|h| {
        if h.borrow().is_some() {
            return;
        }
        let closure = Closure::<dyn FnMut()>::new(close);
        let _ = page.add_event_listener_with_callback("pagehide", closure.as_ref().unchecked_ref());
        *h.borrow_mut() = Some(closure);
    });
    true
}

/// The window's document made the budget window's: a canvas edge to edge
/// on the panel's background, the note shown once the page is gone. A
/// window found again by name (the page reloaded) is emptied first.
fn dress(window: web_sys::Window) -> Option<Popup> {
    let document = window.document()?;
    document.set_title("Budget");
    let body = document.body()?;
    body.set_inner_html("");
    let _ = body.set_attribute("style", "margin:0;background:#101010;overflow:hidden;");
    let canvas = document
        .create_element("canvas")
        .ok()?
        .dyn_into::<web_sys::HtmlCanvasElement>()
        .ok()?;
    let _ = canvas.set_attribute("style", "display:block;width:100vw;height:100vh;");
    body.append_child(&canvas).ok()?;
    let note = document.create_element("div").ok()?;
    note.set_id("disconnected");
    let _ = note.set_attribute(
        "style",
        "display:none;position:fixed;inset:0;align-items:center;justify-content:center;\
         background:rgba(16,16,16,.85);color:#ccc;\
         font:14px system-ui,-apple-system,'Segoe UI',sans-serif;",
    );
    note.set_text_content(Some("The viewer is gone: this window is disconnected."));
    body.append_child(&note).ok()?;
    let script = document.create_element("script").ok()?;
    script.set_text_content(Some(WATCH));
    body.append_child(&script).ok()?;
    let context = canvas
        .get_context("2d")
        .ok()??
        .dyn_into::<web_sys::CanvasRenderingContext2d>()
        .ok()?;
    let _listeners = listen(&window, &canvas);
    Some(Popup {
        window,
        canvas,
        context,
        _listeners,
    })
}

/// The window's drawable, device pixels, and its density; none when it is
/// not open.
pub(crate) fn size() -> Option<((u32, u32), f64)> {
    POPUP.with(|p| {
        let p = p.borrow();
        let p = p.as_ref()?;
        let scale = p.window.device_pixel_ratio().max(1.0);
        let w = p.window.inner_width().ok()?.as_f64()?;
        let h = p.window.inner_height().ok()?.as_f64()?;
        Some((
            (
                ((w * scale).round() as u32).max(1),
                ((h * scale).round() as u32).max(1),
            ),
            scale,
        ))
    })
}

/// Puts `pixels` (RGBA8, sRGB, `size`) on the window's canvas.
pub(crate) fn show(pixels: &[u8], size: (u32, u32)) {
    POPUP.with(|p| {
        let p = p.borrow();
        let Some(p) = p.as_ref() else {
            return;
        };
        if p.canvas.width() != size.0 || p.canvas.height() != size.1 {
            p.canvas.set_width(size.0);
            p.canvas.set_height(size.1);
        }
        if let Ok(image) =
            web_sys::ImageData::new_with_u8_clamped_array_and_sh(Clamped(pixels), size.0, size.1)
        {
            let _ = p.context.put_image_data(&image, 0.0, 0.0);
        }
    });
}

/// What the person did in the window since the last call.
pub(crate) fn take_inputs() -> Vec<tuile_ui::budget_board::BoardInput> {
    INPUTS.with(|i| std::mem::take(&mut *i.borrow_mut()))
}

/// The window's wheel, pointer and keys, as the desktop window's: device
/// pixels, the same keys.
fn listen(
    window: &web_sys::Window,
    canvas: &web_sys::HtmlCanvasElement,
) -> Vec<Closure<dyn FnMut(web_sys::Event)>> {
    use tuile_ui::budget_board::{BoardInput as In, BoardKey as K};
    let push = |input: In| INPUTS.with(|i| i.borrow_mut().push(input));
    let dpr = {
        let window = window.clone();
        move || window.device_pixel_ratio().max(1.0)
    };
    let mut out = Vec::new();
    let mut on = |target: &web_sys::EventTarget, kind: &str, f: Box<dyn FnMut(web_sys::Event)>| {
        let closure = Closure::<dyn FnMut(web_sys::Event)>::new(f);
        let _ = target.add_event_listener_with_callback(kind, closure.as_ref().unchecked_ref());
        out.push(closure);
    };
    let at = {
        let dpr = dpr.clone();
        move |e: &web_sys::MouseEvent| {
            (
                f64::from(e.offset_x()) * dpr(),
                f64::from(e.offset_y()) * dpr(),
            )
        }
    };
    {
        let dpr = dpr.clone();
        on(
            canvas,
            "wheel",
            Box::new(move |e: web_sys::Event| {
                if let Some(w) = e.dyn_ref::<web_sys::WheelEvent>() {
                    e.prevent_default();
                    // Lines or pages, in pixels: a line is forty points.
                    let k = match w.delta_mode() {
                        1 => 40.0,
                        2 => 400.0,
                        _ => 1.0,
                    };
                    push(In::Wheel(w.delta_y() * k * dpr()));
                }
            }),
        );
    }
    for (kind, pressed) in [
        ("mousemove", None),
        ("mousedown", Some(true)),
        ("mouseup", Some(false)),
    ] {
        let at = at.clone();
        on(
            canvas,
            kind,
            Box::new(move |e: web_sys::Event| {
                if let Some(m) = e.dyn_ref::<web_sys::MouseEvent>() {
                    let (x, y) = at(m);
                    push(match pressed {
                        None => In::Pointer(x, y),
                        Some(true) => In::Press(x, y),
                        Some(false) => In::Release,
                    });
                }
            }),
        );
    }
    on(canvas, "mouseleave", Box::new(move |_| push(In::Leave)));
    on(
        window,
        "keydown",
        Box::new(move |e: web_sys::Event| {
            let Some(k) = e.dyn_ref::<web_sys::KeyboardEvent>() else {
                return;
            };
            let key = match k.key().as_str() {
                "t" | "T" => K::Tables,
                "c" | "C" => K::Cumulative,
                "r" | "R" => K::Reset,
                "i" | "I" => K::Issues,
                "v" | "V" => K::Reservoir,
                "]" | "+" => K::LevelUp,
                "[" | "-" => K::LevelDown,
                "a" | "A" => K::LevelAuto,
                "b" | "B" => K::Below,
                "u" | "U" => K::Above,
                "w" | "W" => K::Span,
                "Tab" | "ArrowDown" => K::Next,
                "ArrowUp" => K::Previous,
                "Backspace" | "Delete" | "Escape" => K::Clear,
                "PageDown" | " " => K::PageDown,
                "PageUp" => K::PageUp,
                "Home" => K::Top,
                "End" => K::Bottom,
                _ => return,
            };
            e.prevent_default();
            push(In::Key(key));
        }),
    );
    out
}

/// Closes the window, if it is open.
pub(crate) fn close() {
    if let Some(popup) = POPUP.with(|p| p.borrow_mut().take()) {
        let _ = popup.window.close();
    }
}
