// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The menu, on a page: the desktop's menu bar, from the same table.
//!
//! A page has no menu bar, so the menus open from a button in the view's top
//! right corner (the top left is the loading indicator's, after a jump).
//! Their items are `crate::menu::layout` — the very entries the desktop's
//! bar is built from, with the key beside each — and a click is
//! the same [`Ask`] read against the state as last published, queued where a
//! desktop's click is. The check marks follow that state as the desktop's
//! do. What the desktop's application menu and Help menu add, the page adds
//! too where it means something: *About* and *Keys*. Hide, Services, Quit and
//! the Window menu are the operating system's and have no page equivalent.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use super::screen::{element, FONT};
use crate::embed::ImageryChoice;
use crate::menu::{layout, Ask, Entry, State};
use crate::steer::{Command, Request};

/// What an item of the page's menu does.
#[derive(Clone, Copy)]
enum Action {
    Ask(Ask),
    About,
}

/// The menu, once it is on the page. Dropping it takes it off.
pub(crate) struct Installed {
    button: web_sys::HtmlElement,
    panel: web_sys::HtmlElement,
    /// The check mark of each item that is a state, and the state it shows.
    marks: Vec<(Ask, web_sys::HtmlElement)>,
    shown: Option<State>,
    /// The handlers, held for as long as the elements they listen on.
    _handlers: Vec<Closure<dyn FnMut(web_sys::Event)>>,
}

/// The elements a click changes, shared with the handlers.
#[derive(Clone)]
struct Parts {
    panel: web_sys::HtmlElement,
    open: Rc<RefCell<bool>>,
}

impl Parts {
    fn show(&self, open: bool) {
        *self.open.borrow_mut() = open;
        let _ = self
            .panel
            .style()
            .set_property("display", if open { "block" } else { "none" });
    }

    /// Closes the menu and hands the keys back to the canvas.
    fn close(&self) {
        self.show(false);
        if let Some(canvas) = super::screen::host()
            .and_then(|host| host.query_selector("canvas").ok().flatten())
            .and_then(|c| c.dyn_into::<web_sys::HtmlElement>().ok())
        {
            let _ = canvas.focus();
        }
    }
}

impl Installed {
    /// Puts the menu on the page, over the view. `None` where there is no
    /// page to put it on.
    pub(crate) fn install(
        layers: &'static [ImageryChoice],
        tilesets: &[crate::embed::TilesetChoice],
    ) -> Option<Self> {
        let host = super::screen::host()?;
        let button = element(
            "button",
            &format!(
                "position:absolute;right:calc(12px + env(safe-area-inset-right,0px));\
                 top:calc(12px + env(safe-area-inset-top,0px));width:36px;height:36px;\
                 border-radius:18px;border:0;padding:0;cursor:pointer;z-index:1;\
                 background:rgba(38,42,50,.78);color:#e8e8e8;{FONT}font-size:18px;line-height:36px;\
                 box-shadow:0 1px 4px rgba(0,0,0,.4)"
            ),
        )?;
        button.set_text_content(Some("☰"));
        button.set_attribute("aria-label", "Menu").ok()?;
        button.set_attribute("aria-haspopup", "menu").ok()?;
        let panel = element(
            "div",
            &format!(
                "position:absolute;right:calc(12px + env(safe-area-inset-right,0px));\
                 top:calc(54px + env(safe-area-inset-top,0px));display:none;z-index:1;\
                 min-width:230px;max-height:calc(100% - 70px);overflow:auto;padding:6px 0;\
                 background:rgba(36,38,44,.96);color:#eee;border-radius:10px;\
                 box-shadow:0 8px 28px rgba(0,0,0,.5);{FONT}font-size:13px;user-select:none"
            ),
        )?;
        panel.set_attribute("role", "menu").ok()?;
        let parts = Parts {
            panel: panel.clone(),
            open: Rc::default(),
        };
        let mut handlers = Vec::new();
        let mut marks = Vec::new();

        let mut sections = layout(layers, tilesets);
        // What the desktop's Help menu holds, and its application menu's
        // About, after the application's own.
        sections.push((
            "Help",
            vec![Entry::Item {
                title: "Keys".to_owned(),
                ask: Ask::Do(Command::ShowKeys),
                key: None,
            }],
        ));
        let count = layers.len();
        for (index, (title, entries)) in sections.iter().enumerate() {
            if index > 0 {
                let line = rule()?;
                panel.append_child(&line).ok()?;
            }
            let heading = element(
                "div",
                "padding:6px 14px 2px;font-size:11px;font-weight:600;letter-spacing:.04em;\
                 text-transform:uppercase;color:rgba(255,255,255,.5)",
            )?;
            heading.set_text_content(Some(title));
            panel.append_child(&heading).ok()?;
            for entry in entries {
                match entry {
                    Entry::Separator => {
                        let line = rule()?;
                        panel.append_child(&line).ok()?;
                    }
                    Entry::Item { title, ask, key } => {
                        let (row, mark) = item(title, *key)?;
                        if ask.checked(&State::default()).is_some() {
                            marks.push((*ask, mark));
                        }
                        handlers.push(on_click(&row, &parts, Action::Ask(*ask), count)?);
                        panel.append_child(&row).ok()?;
                    }
                }
            }
        }
        let line = rule()?;
        panel.append_child(&line).ok()?;
        let (about, _) = item(&format!("About {}", super::system::name()), None)?;
        handlers.push(on_click(&about, &parts, Action::About, count)?);
        panel.append_child(&about).ok()?;

        // The button opens and closes the menu; Escape closes it.
        let toggle = {
            let parts = parts.clone();
            Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
                event.stop_propagation();
                let open = !*parts.open.borrow();
                if open {
                    parts.show(true);
                } else {
                    parts.close();
                }
            })
        };
        button
            .add_event_listener_with_callback("click", toggle.as_ref().unchecked_ref())
            .ok()?;
        handlers.push(toggle);
        let escape = {
            let parts = parts.clone();
            Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
                let escape = event
                    .dyn_ref::<web_sys::KeyboardEvent>()
                    .is_some_and(|k| k.key() == "Escape");
                if escape {
                    parts.close();
                }
            })
        };
        panel
            .add_event_listener_with_callback("keydown", escape.as_ref().unchecked_ref())
            .ok()?;
        button
            .add_event_listener_with_callback("keydown", escape.as_ref().unchecked_ref())
            .ok()?;
        handlers.push(escape);
        // A press anywhere else on the view closes it, as a menu does.
        let outside = {
            let parts = parts.clone();
            Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
                if *parts.open.borrow() {
                    parts.show(false);
                }
            })
        };
        host.add_event_listener_with_callback("pointerdown", outside.as_ref().unchecked_ref())
            .ok()?;
        handlers.push(outside);
        // Presses on the menu itself are the menu's.
        let inside = Closure::<dyn FnMut(web_sys::Event)>::new(|event: web_sys::Event| {
            event.stop_propagation();
        });
        for target in [&button, &panel] {
            target
                .add_event_listener_with_callback("pointerdown", inside.as_ref().unchecked_ref())
                .ok()?;
        }
        handlers.push(inside);

        host.append_child(&button).ok()?;
        host.append_child(&panel).ok()?;
        Some(Self {
            button,
            panel,
            marks,
            shown: None,
            _handlers: handlers,
        })
    }

    /// Makes the check marks say `now`; the page is only touched when
    /// something changed.
    pub(crate) fn show(&mut self, now: State) {
        if self.shown == Some(now) {
            return;
        }
        for (ask, mark) in &self.marks {
            let on = ask.checked(&now).unwrap_or(false);
            mark.set_text_content(Some(if on { "✓" } else { "" }));
        }
        self.shown = Some(now);
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        self.button.remove();
        self.panel.remove();
    }
}

/// A line between groups.
fn rule() -> Option<web_sys::HtmlElement> {
    element(
        "div",
        "height:1px;margin:5px 0;background:rgba(255,255,255,.12)",
    )
}

/// One item: a check-mark column, the title, and the key that does the same
/// in the view.
fn item(title: &str, key: Option<char>) -> Option<(web_sys::HtmlElement, web_sys::HtmlElement)> {
    let row = element(
        "div",
        "display:flex;align-items:center;gap:8px;padding:5px 14px 5px 8px;cursor:pointer;\
         white-space:nowrap",
    )?;
    row.set_attribute("role", "menuitem").ok()?;
    let mark = element("span", "width:14px;text-align:center;color:#4da3ff")?;
    let label = element("span", "flex:1")?;
    label.set_text_content(Some(title));
    let shortcut = element("span", "color:rgba(255,255,255,.45);padding-left:18px")?;
    if let Some(key) = key {
        shortcut.set_text_content(Some(&key.to_ascii_uppercase().to_string()));
    }
    row.append_child(&mark).ok()?;
    row.append_child(&label).ok()?;
    row.append_child(&shortcut).ok()?;
    Some((row, mark))
}

/// What the pointer does on `row`: lights it, or runs the item's action and
/// closes the menu.
fn on_click(
    row: &web_sys::HtmlElement,
    parts: &Parts,
    action: Action,
    layers: usize,
) -> Option<Closure<dyn FnMut(web_sys::Event)>> {
    let parts = parts.clone();
    let handler = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
        // The row under the pointer is lit, as a menu's is; a click is the
        // item's action, and the menu closes.
        let lit = match event.type_().as_str() {
            "pointerenter" => Some("rgba(77,163,255,.28)"),
            "pointerleave" => Some(""),
            _ => None,
        };
        if let Some(background) = lit {
            if let Some(row) = event
                .current_target()
                .and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok())
            {
                let _ = row.style().set_property("background", background);
            }
            return;
        }
        parts.close();
        match action {
            Action::Ask(ask) => {
                let now = crate::snapshot::current()
                    .map(|snapshot| State::of(&snapshot, layers))
                    .unwrap_or_default();
                crate::desktop::queue(Request::Do(ask.command(&now)));
            }
            Action::About => about(),
        }
    });
    for kind in ["click", "pointerenter", "pointerleave"] {
        row.add_event_listener_with_callback(kind, handler.as_ref().unchecked_ref())
            .ok()?;
    }
    Some(handler)
}

/// What the desktop's About box says.
fn about() {
    let layer = crate::snapshot::current()
        .and_then(|s| s.imagery)
        .map(|layer| format!("\n\nImagery: {} · {}", layer.name, layer.attribution))
        .unwrap_or_default();
    crate::desktop::alert(
        &super::system::name(),
        &format!(
            "Version {}\nCopyright © lapoule.dev. MIT OR Apache-2.0.{layer}",
            env!("CARGO_PKG_VERSION")
        ),
    );
}
