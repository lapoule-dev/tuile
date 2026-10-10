// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The menu bar, and the keys: one table for both.
//!
//! Everything the window's keys do, a menu item does, and the other way
//! round — and each ends in a [`Command`], the same values the steering URLs,
//! the scripts and the flags end in. The table is [`KEYS`] and [`layout`]:
//! plain data, read by the window's key handler and by the code that builds
//! the platform's menu, and held to each other by the tests below. A key
//! that exists only in the window, or an item whose key does something else
//! there, does not compile into a passing suite.
//!
//! # A key press and its menu item cannot act twice
//!
//! A menu item's key equivalent with no modifier is the same key the window
//! already handles, and which of the two the system delivers a press to — or
//! whether it delivers it to both — is the system's business. So the answer
//! is made not to matter. What a key or an item asks for is an [`Ask`]:
//! *toggle the wireframe*, *the next layer*. It becomes a command against the
//! state **as last published**, and the command is absolute: *wireframe on*,
//! *layer 2*. Two deliveries of one press are read against the same state —
//! it is published once a frame, and both arrive within one turn of the loop
//! — so they produce the same command twice, and doing an absolute thing
//! twice is doing it once.
//!
//! # The platform
//!
//! The menu itself is built with `muda`, the menu crate already in this
//! workspace's lock file, on macOS with the `application` feature. Elsewhere
//! there is no menu and the keys are all there is, exactly as before.

use crate::embed::ImageryChoice;
use crate::snapshot::Snapshot;
use crate::steer::Command;

/// The diagnostic views, in the order `D` cycles them, as a menu names them.
/// The same order as `app::DIAGNOSTICS`; a test holds the two together.
pub(crate) const DIAGNOSTICS: [&str; 4] = ["Normal", "Unlit", "Coverage", "Geometry"];

/// What a key or a menu item asks for, before it is a command: relative
/// where a person thinks relatively.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Ask {
    /// Something that is already absolute.
    Do(Command),
    ToggleWireframe,
    ToggleFreeze,
    Diagnostic(usize),
    NextDiagnostic,
    Imagery(usize),
    NextImagery,
}

/// As much of the published state as an [`Ask`] is read against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct State {
    pub wireframe: bool,
    pub frozen: bool,
    pub diagnostic: usize,
    /// The imagery layer being draped, and how many the host offers.
    pub layer: usize,
    pub layers: usize,
}

impl State {
    pub(crate) fn of(snapshot: &Snapshot, layers: usize) -> Self {
        Self {
            wireframe: snapshot.wireframe,
            frozen: snapshot.frozen,
            diagnostic: snapshot.diagnostic,
            layer: snapshot.layer,
            layers,
        }
    }
}

impl Ask {
    /// The command this is, in `now`. Always absolute — see the module.
    pub(crate) fn command(self, now: &State) -> Command {
        match self {
            Self::Do(command) => command,
            Self::ToggleWireframe => Command::Wireframe(!now.wireframe),
            Self::ToggleFreeze => Command::Freeze(!now.frozen),
            Self::Diagnostic(view) => Command::Diagnostic(view),
            Self::NextDiagnostic => Command::Diagnostic((now.diagnostic + 1) % DIAGNOSTICS.len()),
            Self::Imagery(layer) => Command::Imagery(layer),
            Self::NextImagery => Command::Imagery((now.layer + 1) % now.layers.max(1)),
        }
    }

    /// Whether an item asking this shows a check mark in `now`; `None` for
    /// an item that is an action and not a state.
    pub(crate) fn checked(self, now: &State) -> Option<bool> {
        match self {
            Self::ToggleWireframe => Some(now.wireframe),
            Self::ToggleFreeze => Some(now.frozen),
            Self::Diagnostic(view) => Some(now.diagnostic == view),
            Self::Imagery(layer) => Some(now.layer == layer),
            Self::Do(_) | Self::NextDiagnostic | Self::NextImagery => None,
        }
    }
}

/// The letter keys. Lower case; the window reads a key without regard to
/// case.
pub(crate) const KEYS: [(char, Ask); 8] = [
    ('w', Ask::ToggleWireframe),
    ('f', Ask::ToggleFreeze),
    ('d', Ask::NextDiagnostic),
    ('n', Ask::Do(Command::NorthUp)),
    ('l', Ask::Do(Command::Here)),
    ('c', Ask::Do(Command::CopyLink)),
    ('v', Ask::Do(Command::PasteLink)),
    ('i', Ask::NextImagery),
];

/// What the key `c` asks for; `None` for a key that is not one of the
/// application's.
///
/// Letters only. The layers had the digits for a while, and lost them: a
/// digit is a different character on every keyboard layout that keeps its
/// digits under Shift, so the menu showed `&`, `é`, `"` beside the first
/// three layers and the window, which reads characters, answered to none of
/// them. A layer is an item of the Imagery menu, and `I` goes round them.
pub(crate) fn key(c: char) -> Option<Ask> {
    let c = c.to_ascii_lowercase();
    KEYS.iter().find(|(key, _)| *key == c).map(|(_, ask)| *ask)
}

/// One line of a menu.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Entry {
    Item {
        title: String,
        ask: Ask,
        /// The key that does the same in the window, shown beside the item.
        key: Option<char>,
    },
    Separator,
}

/// The application's own menus, in the order they sit in the bar. The
/// application menu, Window and Help are the platform's and are added where
/// the platform's menu is built.
pub(crate) fn layout(layers: &[ImageryChoice]) -> Vec<(&'static str, Vec<Entry>)> {
    let item = |title: &str, ask: Ask| Entry::Item {
        title: title.to_owned(),
        ask,
        key: KEYS.iter().find(|(_, a)| *a == ask).map(|(key, _)| *key),
    };
    let mut view = vec![
        item("Wireframe", Ask::ToggleWireframe),
        item("Freeze Selection", Ask::ToggleFreeze),
        Entry::Separator,
    ];
    view.extend(
        DIAGNOSTICS
            .iter()
            .enumerate()
            .map(|(index, name)| item(&format!("Diagnostic View: {name}"), Ask::Diagnostic(index))),
    );
    view.push(item("Next Diagnostic View", Ask::NextDiagnostic));

    let go = vec![
        item("North Up", Ask::Do(Command::NorthUp)),
        item("Centre on Current Location", Ask::Do(Command::Here)),
        Entry::Separator,
        item("Copy Link to This View", Ask::Do(Command::CopyLink)),
        item("Go to Copied Link", Ask::Do(Command::PasteLink)),
    ];

    let mut imagery: Vec<Entry> = layers
        .iter()
        .enumerate()
        .map(|(index, layer)| Entry::Item {
            title: layer.name.clone(),
            ask: Ask::Imagery(index),
            key: None,
        })
        .collect();
    if layers.len() > 1 {
        imagery.push(Entry::Separator);
        imagery.push(item("Next Layer", Ask::NextImagery));
    }
    vec![("View", view), ("Go", go), ("Imagery", imagery)]
}

pub(crate) use platform::Installed;

#[cfg(all(target_os = "macos", feature = "application"))]
mod platform {
    use std::sync::atomic::{AtomicBool, Ordering};

    use muda::accelerator::Accelerator;
    use muda::{
        AboutMetadata, CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
    };

    use super::{layout, Ask, Entry, State};
    use crate::embed::ImageryChoice;
    use crate::steer::{Command, Request};

    /// Set by a click: the platform has just flipped the clicked item's check
    /// mark on its own, whatever the state turns out to be, so the next
    /// [`Installed::show`] must write every mark again.
    static CLICKED: AtomicBool = AtomicBool::new(false);

    /// The menu bar, once it is up. Held because dropping it takes the menus
    /// down, and because the check marks are rewritten as the state changes.
    pub(crate) struct Installed {
        _menu: Menu,
        marks: Vec<(Ask, CheckMenuItem)>,
        shown: Option<State>,
    }

    /// The key equivalent of a bare key. No modifier, on purpose: it is the
    /// key the window answers to, shown where a person looks for it.
    fn bare(key: char) -> Option<Accelerator> {
        key.to_ascii_uppercase().to_string().parse().ok()
    }

    impl Installed {
        /// Builds the menu bar and makes it the application's. Call on the
        /// main thread once the event loop is running. `None`, with a line in
        /// the log, when the platform refuses: a globe without a menu is
        /// still a globe.
        pub(crate) fn install(layers: &'static [ImageryChoice]) -> Option<Self> {
            match Self::build(layers) {
                Ok(installed) => Some(installed),
                Err(why) => {
                    tracing::error!("no menu bar: {why}");
                    None
                }
            }
        }

        fn build(layers: &'static [ImageryChoice]) -> muda::Result<Self> {
            let identity = crate::embed::identity();
            let menu = Menu::new();

            // The application menu: the platform's own items, in its order.
            let app = Submenu::new(&identity.name, true);
            app.append_items(&[
                &PredefinedMenuItem::about(
                    None,
                    Some(AboutMetadata {
                        name: Some(identity.name.clone()),
                        version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                        copyright: Some("Copyright © lapoule.dev. MIT OR Apache-2.0.".to_owned()),
                        ..AboutMetadata::default()
                    }),
                ),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::services(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::hide(None),
                &PredefinedMenuItem::hide_others(None),
                &PredefinedMenuItem::show_all(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::quit(None),
            ])?;
            menu.append(&app)?;

            // The application's own: every item is found again by its place
            // in the layout, which is its id.
            let sections = layout(layers);
            let mut marks = Vec::new();
            let mut asks = Vec::new();
            for (title, entries) in &sections {
                let submenu = Submenu::new(title, true);
                for entry in entries {
                    match entry {
                        Entry::Separator => submenu.append(&PredefinedMenuItem::separator())?,
                        Entry::Item { title, ask, key } => {
                            let id = asks.len().to_string();
                            asks.push(*ask);
                            let key = key.and_then(bare);
                            if ask.checked(&State::default()).is_some() {
                                let item = CheckMenuItem::with_id(id, title, true, false, key);
                                submenu.append(&item)?;
                                marks.push((*ask, item));
                            } else {
                                submenu.append(&MenuItem::with_id(id, title, true, key))?;
                            }
                        }
                    }
                }
                menu.append(&submenu)?;
            }

            let window = Submenu::new("Window", true);
            window.append_items(&[
                &PredefinedMenuItem::minimize(None),
                &PredefinedMenuItem::maximize(None),
                &PredefinedMenuItem::fullscreen(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::bring_all_to_front(None),
            ])?;
            menu.append(&window)?;

            let help = Submenu::new("Help", true);
            let keys = asks.len().to_string();
            asks.push(Ask::Do(Command::ShowKeys));
            help.append(&MenuItem::with_id(keys, "Keys and Flags", true, None))?;
            menu.append(&help)?;

            menu.init_for_nsapp();
            window.set_as_windows_menu_for_nsapp();
            help.set_as_help_menu_for_nsapp();

            // A click becomes a command on the same queue as the URLs and the
            // scripts, read against the state as last published.
            let count = layers.len();
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
                CLICKED.store(true, Ordering::Relaxed);
                let Some(ask) = event.id().0.parse::<usize>().ok().and_then(|i| asks.get(i)) else {
                    return;
                };
                let now = crate::snapshot::current()
                    .map(|snapshot| State::of(&snapshot, count))
                    .unwrap_or_default();
                crate::desktop::queue(Request::Do(ask.command(&now)));
            }));

            Ok(Self {
                _menu: menu,
                marks,
                shown: None,
            })
        }

        /// Makes the check marks say `now`. Cheap to call every frame: the
        /// platform is only spoken to when something changed, or when a click
        /// has just moved a mark by itself.
        pub(crate) fn show(&mut self, now: State) {
            let clicked = CLICKED.swap(false, Ordering::Relaxed);
            if !clicked && self.shown == Some(now) {
                return;
            }
            for (ask, item) in &self.marks {
                if let Some(on) = ask.checked(&now) {
                    item.set_checked(on);
                }
            }
            self.shown = Some(now);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Every key the layout shows is one the platform can show: a key
        /// that does not parse would be an item silently without its key.
        #[test]
        fn every_key_of_the_layout_is_a_key_equivalent() {
            let layers: Vec<ImageryChoice> = (0..9)
                .map(|n| ImageryChoice::new(&format!("l{n}"), &format!("L{n}"), n, ""))
                .collect();
            for (_, entries) in layout(&layers) {
                for entry in entries {
                    if let Entry::Item {
                        key: Some(key),
                        title,
                        ..
                    } = entry
                    {
                        let parsed = bare(key).unwrap_or_else(|| unreachable!("{title}: {key:?}"));
                        assert!(
                            parsed.modifiers().is_empty(),
                            "{title}: {key:?} gained a modifier"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(not(all(target_os = "macos", feature = "application")))]
mod platform {
    use super::State;
    use crate::embed::ImageryChoice;

    /// No menu bar on this platform: the keys are all there is.
    pub(crate) struct Installed;

    impl Installed {
        pub(crate) fn install(_layers: &'static [ImageryChoice]) -> Option<Self> {
            None
        }

        pub(crate) fn show(&mut self, _now: State) {}
    }
}

impl Default for State {
    /// Nothing switched on, the first view, the first layer.
    fn default() -> Self {
        Self {
            wireframe: false,
            frozen: false,
            diagnostic: 0,
            layer: 0,
            layers: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn three() -> Vec<ImageryChoice> {
        vec![
            ImageryChoice::new("aerial", "Aerial", 2, ""),
            ImageryChoice::new("labels", "Aerial with labels", 3, ""),
            ImageryChoice::new("satellite", "Satellite", 9, ""),
        ]
    }

    fn items(layers: &[ImageryChoice]) -> Vec<(String, Ask, Option<char>)> {
        layout(layers)
            .into_iter()
            .flat_map(|(_, entries)| entries)
            .filter_map(|entry| match entry {
                Entry::Item { title, ask, key } => Some((title, ask, key)),
                Entry::Separator => None,
            })
            .collect()
    }

    /// The window's keys and the menu's key equivalents are one table: every
    /// key the window answers to is shown on an item that asks for the same
    /// thing, and no item shows a key the window would read otherwise.
    #[test]
    fn every_key_is_a_menu_item_and_every_shown_key_is_that_key() {
        let layers = three();
        let items = items(&layers);
        let mut reached = 0;
        for (c, _) in KEYS {
            let ask = key(c).expect("a key of the table");
            let shown: Vec<_> = items.iter().filter(|(_, _, k)| *k == Some(c)).collect();
            assert_eq!(shown.len(), 1, "the key {c:?} is on {} items", shown.len());
            assert_eq!(
                shown[0].1, ask,
                "the key {c:?} and the item {:?}",
                shown[0].0
            );
            reached += 1;
        }
        assert_eq!(reached, KEYS.len());
        for (title, ask, shown) in &items {
            if let Some(c) = shown {
                assert_eq!(key(*c), Some(*ask), "{title}");
                // Either case, as the window reads it.
                assert_eq!(key(c.to_ascii_uppercase()), Some(*ask));
            }
        }
    }

    /// No two items share a key, and no key means two things.
    #[test]
    fn no_key_is_used_twice() {
        let nine: Vec<ImageryChoice> = (0..12)
            .map(|n| ImageryChoice::new(&format!("l{n}"), &format!("L{n}"), n, ""))
            .collect();
        let mut seen: HashMap<char, String> = HashMap::new();
        for (title, _, key) in items(&nine) {
            if let Some(key) = key {
                if let Some(other) = seen.insert(key, title.clone()) {
                    unreachable!("{key:?} is on both {other:?} and {title:?}");
                }
            }
        }
        // Letters only: a digit is another character on another keyboard.
        assert!(seen.keys().all(char::is_ascii_lowercase), "{seen:?}");
        let mut table: Vec<char> = KEYS.iter().map(|(key, _)| *key).collect();
        table.sort_unstable();
        table.dedup();
        assert_eq!(table.len(), KEYS.len(), "a letter is in the table twice");
        assert!(KEYS.iter().all(|(key, _)| key.is_ascii_lowercase()));
        for not_a_key in ['1', '&', 'x', ' '] {
            assert_eq!(key(not_a_key), None, "{not_a_key:?}");
        }
    }

    /// Every command a person can give by hand is on a menu: the ones the
    /// keys give, each view, each layer.
    #[test]
    fn every_command_is_reachable_from_the_menu() {
        let layers = three();
        let now = State {
            layers: layers.len(),
            ..State::default()
        };
        let reachable: Vec<Command> = items(&layers)
            .iter()
            .map(|(_, ask, _)| ask.command(&now))
            .collect();
        let mut wanted = vec![
            Command::Wireframe(true),
            Command::Freeze(true),
            Command::NorthUp,
            Command::Here,
            Command::CopyLink,
            Command::PasteLink,
        ];
        wanted.extend((0..DIAGNOSTICS.len()).map(Command::Diagnostic));
        wanted.extend((0..layers.len()).map(Command::Imagery));
        for command in wanted {
            assert!(reachable.contains(&command), "{command:?} is on no menu");
        }
        // And by a key, for everything the key table names.
        for (c, ask) in KEYS {
            assert_eq!(key(c).map(|a| a.command(&now)), Some(ask.command(&now)));
        }
    }

    /// The marks are the state: one view, one layer, and the two switches.
    #[test]
    fn the_check_marks_are_the_state() {
        let layers = three();
        let now = State {
            wireframe: true,
            frozen: false,
            diagnostic: 2,
            layer: 1,
            layers: layers.len(),
        };
        let marked: Vec<String> = items(&layers)
            .into_iter()
            .filter(|(_, ask, _)| ask.checked(&now) == Some(true))
            .map(|(title, _, _)| title)
            .collect();
        assert_eq!(
            marked,
            [
                "Wireframe",
                "Diagnostic View: Coverage",
                "Aerial with labels"
            ]
        );
        // Actions carry no mark, whatever the state.
        for ask in [
            Ask::Do(Command::NorthUp),
            Ask::NextImagery,
            Ask::NextDiagnostic,
        ] {
            assert_eq!(ask.checked(&now), None);
        }
    }

    /// The menu names the views the renderer draws, in the renderer's order.
    #[test]
    fn the_menu_names_the_views_the_window_cycles() {
        let drawn: Vec<String> = crate::app::DIAGNOSTICS
            .iter()
            .map(|(name, _)| name.to_string())
            .collect();
        let named: Vec<String> = DIAGNOSTICS.iter().map(|n| n.to_lowercase()).collect();
        assert_eq!(named, drawn);
    }

    /// One press delivered twice — to the window and to the menu — is one
    /// change: both are read against the same published state, and what they
    /// become is absolute.
    #[test]
    fn a_press_delivered_twice_asks_for_the_same_thing_twice() {
        let now = State {
            wireframe: false,
            frozen: true,
            diagnostic: 3,
            layer: 2,
            layers: 3,
        };
        for (ask, command) in [
            (Ask::ToggleWireframe, Command::Wireframe(true)),
            (Ask::ToggleFreeze, Command::Freeze(false)),
            (Ask::NextDiagnostic, Command::Diagnostic(0)),
            (Ask::NextImagery, Command::Imagery(0)),
            (Ask::Imagery(1), Command::Imagery(1)),
        ] {
            assert_eq!(ask.command(&now), command);
            assert_eq!(ask.command(&now), ask.command(&now));
        }
    }
}
