// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Carrying out a [`Command`], whoever gave it.
//!
//! The keys, the steering URLs and a script's commands all end here, so there is one place where
//! "north up" or "freeze" is done and no way for the two to drift apart. URLs
//! arrive on a queue filled by the desktop's event dispatch and are drained by
//! the loop: the camera is only ever moved from the thread that draws it.

use super::{App, DIAGNOSTICS};
use crate::snapshot::Snapshot;
use crate::steer::{self, Command, Request};

impl App {
    /// Drains what arrived from outside since the last turn of the loop.
    ///
    /// A URL that is not a command is dropped with one line and nothing else:
    /// it comes from outside the process, and must not be able to do more than
    /// fail.
    pub(super) fn take_the_commands(&mut self) {
        while let Ok(request) = self.requests.try_recv() {
            match request {
                // A script's command arrives already read and already checked.
                Request::Do(command) => self.apply(command),
                // A script named a layer; which one is the host's list to say.
                Request::Imagery(name) => match steer::layer_named(self.imagery.layers, &name) {
                    Ok(layer) => self.apply(Command::Imagery(layer)),
                    Err(why) => tracing::error!("script ignored: {why}"),
                },
                Request::Url(url) => match steer::parse_url_among(&url, self.imagery.layers) {
                    Ok(commands) => commands.into_iter().for_each(|c| self.apply(c)),
                    Err(why) => tracing::error!("steering URL ignored: {why}"),
                },
            }
        }
    }

    pub(super) fn apply(&mut self, command: Command) {
        match command {
            Command::Goto(goto) => {
                steer::go(&mut self.controller, goto);
                // A place somebody named is not a secret.
                self.located = false;
            }
            Command::NorthUp => {
                let viewport = self.viewport();
                steer::north_up(&mut self.controller, viewport);
            }
            Command::Here => self.locator.request(
                std::time::Instant::now(),
                crate::location::Placement::OVERHEAD,
            ),
            Command::Freeze(on) => {
                self.views.freeze = on;
                tracing::info!("traversal freeze: {on}");
            }
            Command::Wireframe(on) => self.views.wireframe = on,
            Command::Diagnostic(view) => {
                let view = view % DIAGNOSTICS.len();
                if self.views.diagnostic != view {
                    self.views.diagnostic = view;
                    let (name, reads) = DIAGNOSTICS[view];
                    tracing::info!("view: {name} — {reads}");
                }
            }
            Command::CopyLink => self.copy_the_link(),
            Command::PasteLink => self.go_to_the_copied_link(),
            Command::ShowKeys => crate::desktop::alert(
                &format!("{} — keys and flags", self.title),
                &crate::start::usage(),
            ),
            Command::Imagery(layer) => self.drape(layer),
            Command::NextImagery => {
                if let Some(next) = self.imagery.next() {
                    self.drape(next);
                }
            }
        }
    }

    /// Puts the link to the present view on the clipboard: a bookmark to keep
    /// or to send. The person's own key press, so it is theirs to copy
    /// whatever the view shows.
    pub(super) fn copy_the_link(&mut self) {
        let link = steer::link(&steer::view_of(self.controller.target()));
        if crate::desktop::copy(&link) {
            self.say("link to this view copied");
        } else {
            self.say("no clipboard on this platform");
        }
    }

    /// Goes where the link on the clipboard says — a link `C` copied, here or
    /// in another session. Only a `goto`: what is pasted is a place, and a
    /// clipboard is not a way to flip switches.
    fn go_to_the_copied_link(&mut self) {
        let goto = crate::desktop::paste()
            .and_then(|text| steer::parse_url(text.trim()).ok())
            .and_then(|commands| match commands.as_slice() {
                [Command::Goto(goto)] => Some(*goto),
                _ => None,
            });
        match goto {
            Some(goto) => self.apply(Command::Goto(goto)),
            None => self.say("the clipboard holds no link to a view"),
        }
    }

    /// The state a key or a menu item is read against: the one last
    /// published, so that one press delivered twice asks for the same thing
    /// twice — see `crate::menu`.
    pub(super) fn published(&self) -> crate::menu::State {
        let layers = self.imagery.layers.len();
        crate::snapshot::current().map_or(
            crate::menu::State {
                wireframe: self.views.wireframe,
                frozen: self.views.freeze,
                diagnostic: self.views.diagnostic,
                layer: self.imagery.layer,
                layers,
            },
            |snapshot| crate::menu::State::of(&snapshot, layers),
        )
    }

    /// Publishes this frame's view for scripts, and keeps the title bar on it.
    ///
    /// `drawn` is the count of tiles in the frame just presented and whether
    /// all of them were the tiles selected; `None` on a turn of the loop that
    /// drew nothing, which leaves the last counts standing.
    pub(super) fn publish_the_view(&mut self, drawn: Option<(usize, bool)>) {
        if let Some(drawn) = drawn {
            self.drawn = drawn;
        }
        let size = self.active.as_ref().map_or((0, 0), |a| a.size);
        let snapshot = Snapshot {
            wireframe: self.views.wireframe,
            frozen: self.views.freeze,
            located: self.located,
            tiles: self.drawn.0,
            settled: self.drawn.1,
            imagery: self.imagery.layers.get(self.imagery.layer),
            layer: self.imagery.layer,
            diagnostic: self.views.diagnostic,
            ..Snapshot::of(&self.controller, size)
        };
        crate::snapshot::publish(snapshot);
        if let Some(menu) = self.menu.as_mut() {
            menu.show(crate::menu::State::of(&snapshot, self.imagery.layers.len()));
        }
        self.keep_the_title(&snapshot);
    }

    /// The title bar: the application, where the eye is, and — for a few
    /// seconds — the last thing that was said.
    ///
    /// Rewritten a few times a second at most, and only when it changed: a
    /// title set every frame is sixty messages a second to the window server
    /// for a text nobody can read that fast.
    fn keep_the_title(&mut self, snapshot: &Snapshot) {
        /// How often the title may change while the eye moves.
        const EVERY: std::time::Duration = std::time::Duration::from_millis(250);
        /// How long a message stays beside the position.
        const NOTICE: std::time::Duration = std::time::Duration::from_secs(8);

        let now = std::time::Instant::now();
        if now.duration_since(self.title_at) < EVERY {
            return;
        }
        self.title_at = now;
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, since)| now.duration_since(*since) > NOTICE)
        {
            self.notice = None;
        }
        let mut title = format!("{} — {}", self.title, crate::snapshot::title(snapshot));
        // Whose pictures these are, for as long as they are on screen: the
        // title bar is the one piece of text this window has.
        if let Some(layer) = snapshot.imagery {
            title = format!("{title} — {} · {}", layer.name, layer.attribution);
        }
        if let Some((notice, _)) = &self.notice {
            title = format!("{title} — {notice}");
        }
        if title != self.title_shown {
            if let Some(active) = self.active.as_ref() {
                active.window.set_title(&title);
            }
            self.title_shown = title;
        }
    }
}
