// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a page shows over its canvas: the caption, the alerts, and the note
//! that the browser cannot draw.
//!
//! The browser's counterpart of the phone's UIKit glue (`crate::desktop`'s
//! iOS `system`): a label along the bottom that says whose pictures are on
//! screen and the last thing said, and a message presented over the view,
//! one at a time, dismissed with a button. Made of the page's own elements,
//! laid over the canvas in the element that holds it, and taken down with
//! the viewer. The canvas keeps every pointer and key it had: nothing here
//! takes an event except the alert's button.

use std::cell::RefCell;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

/// Which bottom corner the caption keeps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Left,
    Right,
}

/// The elements over the canvas, while a viewer lives.
struct Screen {
    /// The element that holds the canvas; everything here is laid over it.
    host: web_sys::HtmlElement,
    caption: web_sys::HtmlElement,
    /// The alert on screen, if one is, and its button's handler.
    alert: Option<(web_sys::HtmlElement, Closure<dyn FnMut()>)>,
    /// Whether the page wants the note when nothing can be drawn.
    note_unsupported: bool,
}

thread_local! {
    static SCREEN: RefCell<Option<Screen>> = const { RefCell::new(None) };
}

fn document() -> Option<web_sys::Document> {
    web_sys::window()?.document()
}

/// A new element of `tag` with `style`, or `None` where there is no document.
pub(super) fn element(tag: &str, style: &str) -> Option<web_sys::HtmlElement> {
    let element = document()?
        .create_element(tag)
        .ok()?
        .dyn_into::<web_sys::HtmlElement>()
        .ok()?;
    let _ = element.set_attribute("style", style);
    Some(element)
}

/// The type every message and label uses: the platform's own face, as the
/// phone's labels are the system font.
pub(super) const FONT: &str = "font-family:system-ui,-apple-system,'Segoe UI',sans-serif;";

/// Lays the caption over `canvas`, in the element that holds it.
///
/// The holder is given a position if it has none, so that what is laid over
/// the canvas is placed against it and not against the page.
pub(crate) fn attach(canvas: &web_sys::HtmlCanvasElement, side: Side, note_unsupported: bool) {
    detach();
    let Some(host) = canvas
        .parent_element()
        .and_then(|p| p.dyn_into::<web_sys::HtmlElement>().ok())
    else {
        tracing::warn!("the canvas is in no element: no caption and no alerts on the page");
        return;
    };
    let positioned = web_sys::window()
        .and_then(|w| w.get_computed_style(&host).ok().flatten())
        .and_then(|style| style.get_property_value("position").ok())
        .is_some_and(|position| position != "static");
    if !positioned {
        let _ = host.style().set_property("position", "relative");
    }
    // Along the bottom of the safe area, clear of the home indicator, in the
    // phone's 11-point type: see the phone's `caption`.
    let align = match side {
        Side::Left => "left",
        Side::Right => "right",
    };
    let Some(caption) = element(
        "div",
        &format!(
            "position:absolute;left:12px;right:12px;\
             bottom:calc(4px + env(safe-area-inset-bottom,0px));{FONT}font-size:11px;\
             line-height:16px;color:rgba(255,255,255,.85);text-shadow:0 1px 2px rgba(0,0,0,.6);\
             text-align:{align};white-space:nowrap;overflow:hidden;text-overflow:ellipsis;\
             pointer-events:none;user-select:none"
        ),
    ) else {
        return;
    };
    let _ = host.append_child(&caption);
    SCREEN.with(|s| {
        *s.borrow_mut() = Some(Screen {
            host,
            caption,
            alert: None,
            note_unsupported,
        });
    });
}

/// Takes everything down: the viewer is gone.
pub(crate) fn detach() {
    if let Some(screen) = SCREEN.with(|s| s.borrow_mut().take()) {
        screen.caption.remove();
        if let Some((alert, _)) = screen.alert {
            alert.remove();
        }
    }
}

/// The element the canvas is in, while a viewer lives.
pub(super) fn host() -> Option<web_sys::HtmlElement> {
    SCREEN.with(|s| s.borrow().as_ref().map(|screen| screen.host.clone()))
}

/// Says `text` along the bottom of the view.
pub(crate) fn caption(text: &str) {
    SCREEN.with(|s| {
        if let Some(screen) = s.borrow().as_ref() {
            screen.caption.set_text_content(Some(text));
        }
    });
}

/// Lifts the caption `pixels` device pixels off the bottom: above the
/// credits bar, which already keeps clear of the home indicator. 0 puts it
/// back along the bottom of the safe area.
pub(crate) fn caption_lift(pixels: u32) {
    let ratio = web_sys::window().map_or(1.0, |w| w.device_pixel_ratio().max(0.1));
    let bottom = if pixels == 0 {
        "calc(4px + env(safe-area-inset-bottom,0px))".to_owned()
    } else {
        format!("{:.1}px", 4.0 + f64::from(pixels) / ratio)
    };
    SCREEN.with(|s| {
        if let Some(screen) = s.borrow().as_ref() {
            let _ = screen.caption.style().set_property("bottom", &bottom);
        }
    });
}

/// Presents `title` and `text` over the view, with a button that dismisses
/// them; `false` while another message is still up, which the caller keeps
/// for a later turn — as the phone does.
pub(crate) fn try_alert(title: &str, text: &str) -> bool {
    SCREEN.with(|s| {
        let mut screen = s.borrow_mut();
        let Some(screen) = screen.as_mut() else {
            return false;
        };
        if screen.alert.is_some() {
            return false;
        }
        let Some(card) = alert_card(title, text) else {
            return false;
        };
        let Some(button) = card.query_selector("button").ok().flatten() else {
            return false;
        };
        let dismiss = Closure::<dyn FnMut()>::new(dismiss);
        let _ = button.add_event_listener_with_callback("click", dismiss.as_ref().unchecked_ref());
        let _ = screen.host.append_child(&card);
        if let Some(button) = button.dyn_ref::<web_sys::HtmlElement>() {
            let _ = button.focus();
        }
        screen.alert = Some((card, dismiss));
        true
    })
}

/// Takes the alert down, and gives the keys back to the canvas.
fn dismiss() {
    // The handler being run is the one held here: it is dropped on a later
    // turn, never inside its own call.
    let taken = SCREEN.with(|s| s.borrow_mut().as_mut().and_then(|s| s.alert.take()));
    let Some((card, handler)) = taken else {
        return;
    };
    card.remove();
    if let Some(canvas) = SCREEN
        .with(|s| s.borrow().as_ref().map(|s| s.host.clone()))
        .and_then(|host| host.query_selector("canvas").ok().flatten())
        .and_then(|c| c.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = canvas.focus();
    }
    let _ = web_sys::window().map(|w| {
        let later = Closure::once_into_js(move || drop(handler));
        w.set_timeout_with_callback(later.unchecked_ref())
    });
}

/// The message as the phone's alert shows it: a title, the text, one button.
///
/// Lines that begin with spaces are a table — the keys, the flags — and keep
/// their columns in a fixed-width face, under their headings; a message
/// without one is set as centred text, as the phone sets it.
fn alert_card(title: &str, text: &str) -> Option<web_sys::HtmlElement> {
    let veil = element(
        "div",
        "position:absolute;inset:0;display:flex;align-items:center;justify-content:center;\
         background:rgba(0,0,0,.35);z-index:2",
    )?;
    // A table is as wide as its columns, up to the view's width; a sentence
    // wraps at a reading width.
    let table = text.lines().any(|line| line.starts_with("  "));
    let width = if table {
        "calc(100% - 32px)"
    } else {
        "min(420px,calc(100% - 32px))"
    };
    let card = element(
        "div",
        &format!(
            "{FONT}max-width:{width};max-height:calc(100% - 32px);\
             display:flex;flex-direction:column;background:rgba(36,38,44,.96);color:#eee;\
             border-radius:14px;box-shadow:0 8px 32px rgba(0,0,0,.5);overflow:hidden"
        ),
    )?;
    card.set_attribute("role", "alertdialog").ok()?;
    let heading = element(
        "div",
        "padding:16px 16px 4px;font-size:15px;font-weight:600;text-align:center",
    )?;
    heading.set_text_content(Some(title));
    let body = element(
        "div",
        "padding:4px 16px 14px;font-size:13px;line-height:1.4;overflow:auto;\
         overflow-wrap:anywhere",
    )?;
    for line in text.lines() {
        let preformatted = line.starts_with("  ");
        let row = element(
            "div",
            match (preformatted, table) {
                (true, _) => {
                    "font-family:ui-monospace,Menlo,Consolas,monospace;font-size:11.5px;\
                     white-space:pre;min-height:1em"
                }
                (false, true) => "white-space:pre-wrap;min-height:1em;font-weight:600",
                (false, false) => "white-space:pre-wrap;min-height:1em;text-align:center",
            },
        )?;
        row.set_text_content(Some(line));
        body.append_child(&row).ok()?;
    }
    let button = element(
        "button",
        &format!(
            "{FONT}font-size:15px;color:#4da3ff;background:none;border:0;\
             border-top:1px solid rgba(255,255,255,.12);padding:11px;cursor:pointer"
        ),
    )?;
    button.set_text_content(Some("OK"));
    card.append_child(&heading).ok()?;
    card.append_child(&body).ok()?;
    card.append_child(&button).ok()?;
    veil.append_child(&card).ok()?;
    Some(veil)
}

/// Fills the view with why nothing will be drawn, where the page asked for
/// that. Left in place after the viewer is destroyed: it is the last thing
/// the container has to say.
pub(crate) fn unsupported(why: &str) {
    let (host, wanted) = SCREEN.with(|s| {
        s.borrow().as_ref().map_or((None, false), |s| {
            (Some(s.host.clone()), s.note_unsupported)
        })
    });
    let (Some(host), true) = (host, wanted) else {
        return;
    };
    let Some(note) = element(
        "div",
        &format!(
            "position:absolute;inset:0;display:flex;align-items:center;justify-content:center;\
             padding:16px;text-align:center;{FONT}font-size:14px;color:#ddd;background:#000"
        ),
    ) else {
        return;
    };
    note.set_text_content(Some(&format!("This globe cannot be drawn here: {why}")));
    let _ = host.append_child(&note);
}
