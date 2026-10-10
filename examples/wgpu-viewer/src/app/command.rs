// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Carrying out a [`Command`], whoever gave it.
//!
//! The keys and the steering URLs both end here, so there is one place where
//! "north up" or "freeze" is done and no way for the two to drift apart. URLs
//! arrive on a queue filled by the desktop's event dispatch and are drained by
//! the loop: the camera is only ever moved from the thread that draws it.

use super::App;
use crate::steer::{self, Command};

impl App {
    /// Drains the steering URLs that arrived since the last turn of the loop.
    ///
    /// A URL that is not a command is dropped with one line and nothing else:
    /// it comes from outside the process, and must not be able to do more than
    /// fail.
    pub(super) fn take_the_commands(&mut self) {
        while let Ok(url) = self.urls.try_recv() {
            match steer::parse_url(&url) {
                Ok(commands) => commands.into_iter().for_each(|c| self.apply(c)),
                Err(why) => tracing::error!("steering URL ignored: {why}"),
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
            Command::Report => self.report_the_view(),
        }
    }

    /// Writes the present view, as the URL that returns to it, where a script
    /// can read it back.
    ///
    /// **Not when the view was centred on the current location.** A report is
    /// asked for by another program, the file can be read by any program, and
    /// the two together would turn the permission the person gave *this*
    /// application into the machine's position for whoever asks. The view is
    /// reportable again as soon as a `goto` has put it somewhere named.
    fn report_the_view(&mut self) {
        let Some(file) = crate::host::report_file(&crate::host::Machine) else {
            self.say("view not reported: this machine has no caches directory");
            return;
        };
        if self.located {
            // And the last report goes: a reader must not take an old answer
            // for this one.
            let _ = std::fs::remove_file(&file);
            self.say("view not reported: it is centred on the current location");
            return;
        }
        let link = steer::link(&steer::view_of(self.controller.target()));
        let written = file
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&file, format!("{link}\n")));
        match written {
            Ok(()) => self.say(&format!("view written to {}", file.display())),
            Err(e) => self.say(&format!("view not reported: {e}")),
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
}
