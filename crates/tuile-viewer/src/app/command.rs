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
        #[cfg(target_os = "android")]
        super::importing::take_from_the_activity();
        while let Ok(request) = self.requests.try_recv() {
            // A configuration file, or an import link: see `importing`.
            #[cfg(not(target_arch = "wasm32"))]
            if self.take_an_import(&request) {
                continue;
            }
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
                Request::Pasted(text) => self.go_to_the_link(text),
                // Taken above; a page has no files to import.
                Request::Import { .. } | Request::ImportFile(_) | Request::ImportFailed(_) => {}
            }
        }
    }

    /// A steering URL from inside the process — a steering script's — read
    /// as one from outside is.
    pub(super) fn queue_url(&mut self, url: String) {
        match steer::parse_url_among(&url, self.imagery.layers) {
            Ok(commands) => commands.into_iter().for_each(|c| self.apply(c)),
            Err(why) => tracing::error!("steering URL ignored: {why}"),
        }
    }

    pub(super) fn apply(&mut self, command: Command) {
        match command {
            Command::Goto(goto) => {
                // Where it leads, before the eye leaves: a sunset in the
                // link is the one there.
                let to = steer::destination(&self.controller, goto);
                if let Some(time) = goto.time {
                    self.retime(time, &to);
                }
                if !goto.says_where() {
                    return;
                }
                match goto.seconds.filter(|s| *s > 0.0) {
                    Some(seconds) => {
                        let from = steer::view_of(self.controller.target());
                        self.flight = Some(super::flight::Flight::new(
                            from,
                            to,
                            web_time::Instant::now(),
                            seconds,
                        ));
                    }
                    None => {
                        self.flight = None;
                        steer::go(&mut self.controller, goto);
                    }
                }
                // A place somebody named is not a secret.
                self.located = false;
            }
            Command::NorthUp => {
                let viewport = self.viewport();
                steer::north_up(&mut self.controller, viewport);
            }
            Command::Here => self.locator.request(
                web_time::Instant::now(),
                crate::location::Placement::OVERHEAD,
            ),
            Command::Tileset { asset, on } => self.switch_tileset(asset, on),
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
            // An application that is its own window was started with flags;
            // a page's viewer was started by its host's script, and has none.
            Command::ShowKeys if self.profile.owns_its_window => crate::desktop::alert(
                &format!("{} — keys and flags", self.title),
                &crate::start::usage(),
            ),
            Command::ShowKeys => crate::desktop::alert(
                &format!("{} — keys and gestures", self.title),
                &crate::start::keys(),
            ),
            Command::OpenConfiguration => crate::desktop::choose_configuration(),
            Command::Imagery(layer) => self.drape(layer),
            Command::Clock { instant, rate } => {
                let wall = super::wall_seconds();
                self.atmosphere.clock.set(instant, wall);
                self.atmosphere.clock.set_rate(rate, wall);
                if rate != 0.0 {
                    self.resume_rate = rate;
                }
            }
            Command::Record(on) => self.record(on),
            Command::ClockNow => {
                self.atmosphere.clock.now(super::wall_seconds());
                self.resume_rate = 1.0;
                self.clock_event = None;
            }
            Command::NextImagery => {
                if let Some(next) = self.imagery.next() {
                    self.drape(next);
                }
            }
        }
    }

    /// Sets the scene's clock as a link says, for the view `to` it leads to.
    fn retime(&mut self, time: steer::Retime, to: &crate::start::StartView) {
        let wall = super::wall_seconds();
        self.atmosphere.clock = steer::retimed(self.atmosphere.clock, Some(time), to, wall);
        let rate = match time {
            steer::Retime::Start(settings) => {
                self.clock_event = settings.start.event();
                settings.rate
            }
            steer::Retime::Rate(rate) => rate,
        };
        if rate != 0.0 {
            self.resume_rate = rate;
        }
    }

    /// Puts the link to the present view on the clipboard: a bookmark to keep
    /// or to send, with the scene's clock. The person's own key press, so it
    /// is theirs to copy whatever the view shows.
    pub(super) fn copy_the_link(&mut self) {
        let link = steer::link_at(
            &steer::view_of(self.controller.target()),
            &self.clock_state(),
        );
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
        match crate::desktop::paste() {
            crate::desktop::Pasted::Now(text) => self.go_to_the_link(text),
            // The text comes back on the queue: see `take_the_commands`.
            crate::desktop::Pasted::Later => {}
        }
    }

    /// Goes where `text`, from the clipboard, says — if it is a link to a
    /// view and nothing else.
    fn go_to_the_link(&mut self, text: Option<String>) {
        let goto = text
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

    /// The clock as published: this frame's instant, its rate, and what a
    /// pause would run it at again.
    fn clock_state(&self) -> crate::menu::Clock {
        crate::menu::Clock {
            instant: self.instant,
            rate: self.atmosphere.clock.rate(),
            resume: self.resume_rate,
            event: self.clock_event,
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
                clock: self.clock_state(),
                recording: self.is_recording(),
                tilesets: self.tilesets.bits(),
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
        // The view's size, above the credits bar: what the centre is picked
        // in. The whole frame when there is no bar.
        let size = self.active.as_ref().map_or((0, 0), |_| {
            let (w, h) = self.viewport();
            (w as u32, h as u32)
        });
        let snapshot = Snapshot {
            wireframe: self.views.wireframe,
            frozen: self.views.freeze,
            located: self.located,
            tiles: self.drawn.0,
            settled: self.drawn.1,
            imagery: self.imagery.layers.get(self.imagery.layer),
            layer: self.imagery.layer,
            diagnostic: self.views.diagnostic,
            clock: self.clock_state(),
            recording: self.is_recording(),
            tilesets: self.tilesets.bits(),
            ..Snapshot::of(&self.controller, size)
        };
        crate::snapshot::publish(snapshot);
        if let Some(menu) = self.menu.as_mut() {
            menu.show(crate::menu::State::of(&snapshot, self.imagery.layers.len()));
        }
        self.keep_the_title(&snapshot);
    }

    /// The imagery on the GPU, by the format it is held in: `imagery BC7
    /// 31 MiB (360 × 88 KiB)`, and the RGBA8 beside it where some had to
    /// stay RGBA8.
    fn imagery_held(&self) -> Option<String> {
        let active = self.active.as_ref()?;
        let mut held = active.gpu.imagery.lock().ok()?.live_textures_by_format();
        Some(held_line(active.gpu.imagery_plan().target, &mut held))
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

        let now = web_time::Instant::now();
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
        let mut title = format!(
            "{} — {} — {}",
            self.title,
            crate::snapshot::title(snapshot),
            clock_line(
                self.instant,
                self.atmosphere.clock.rate(),
                snapshot.view.lon.to_radians()
            )
        );
        // Whose pictures these are, for as long as they are on screen: the
        // title bar is the one piece of text this window has.
        if let Some(layer) = snapshot.imagery {
            title = format!("{title} — {} · {}", layer.name, layer.attribution);
        }
        // Whose 3D Tiles is not said here: the credits bar below the
        // picture says it, on every shell (see `credits`).
        // How the imagery is held, and what it holds: the one line that
        // tells a compressed session from an uncompressed one.
        let held = self.imagery_held();
        if let Some(held) = &held {
            title = format!("{title} — {held}");
        }
        if let Some((notice, _)) = &self.notice {
            title = format!("{title} — {notice}");
        }
        let waiting = self.waiting_note(now);
        if let Some(waiting) = &waiting {
            title = format!("{title} — {waiting}");
        }
        if title != self.title_shown {
            if let Some(active) = self.active.as_ref() {
                active.screen.set_title(&title);
            }
            // Where there is no title bar, the part a person needs: whose
            // pictures these are, and the last thing said.
            let mut caption = snapshot
                .imagery
                .map(|layer| format!("{} · {}", layer.name, layer.attribution))
                .unwrap_or_default();
            if let Some(held) = &held {
                caption = format!("{caption} — {held}");
            }
            if let Some((notice, _)) = &self.notice {
                caption = format!("{caption} — {notice}");
            }
            if let Some(waiting) = waiting {
                caption = format!("{caption} — {waiting}");
            }
            crate::desktop::caption(&caption);
            crate::desktop::view(snapshot);
            self.title_shown = title;
        }
    }
}

/// `imagery BC7 31 MiB (360 × 88 KiB)`: the plan's format first, then any
/// other the imagery is held in — each with its texture count and what one
/// texture holds on average, mip chain included, which is where a quarter
/// shows.
fn held_line(
    plan: tuile_core::texels::ImageryFormat,
    held: &mut [(tuile_core::texels::ImageryFormat, usize, usize)],
) -> String {
    held.sort_by_key(|(format, _, _)| *format != plan);
    let mib = |bytes: usize| bytes as f64 / f64::from(1u32 << 20);
    let mut parts: Vec<String> = held
        .iter()
        .map(|(format, bytes, count)| {
            format!(
                "{} {:.0} MiB ({count} × {:.0} KiB)",
                tuile_wgpu::format_name(*format),
                mib(*bytes),
                *bytes as f64 / 1024.0 / (*count).max(1) as f64
            )
        })
        .collect();
    if parts.is_empty() {
        parts.push(format!("{} 0 MiB", tuile_wgpu::format_name(plan)));
    }
    format!("imagery {}", parts.join(" + "))
}

/// `2026-06-21 19:42 UTC · solar 19:43 · ×60`: the instant drawn, the local
/// apparent solar time under the eye, and the rate when it is not real time.
pub(crate) fn clock_line(instant: f64, rate: f64, lon: f64) -> String {
    let utc = tuile_atmosphere::clock::format_rfc3339(instant);
    let solar = tuile_atmosphere::clock::solar_time_hours(instant, lon);
    let minutes = (solar * 60.0).floor() as u32;
    let mut line = format!(
        "{} {} UTC · solar {:02}:{:02}",
        &utc[..10],
        &utc[11..16],
        minutes / 60 % 24,
        minutes % 60
    );
    if rate == 0.0 {
        line.push_str(" · paused");
    } else if rate != 1.0 {
        line.push_str(&format!(" · ×{rate}"));
    }
    line
}

#[cfg(test)]
mod held_tests {
    use super::held_line;
    use tuile_core::texels::ImageryFormat;

    /// The plan's format leads, the others follow, sizes in whole MiB.
    #[test]
    fn the_held_line_names_each_format() {
        assert_eq!(held_line(ImageryFormat::Bc7, &mut []), "imagery BC7 0 MiB");
        let mut held = [
            (ImageryFormat::Rgba8, 3 << 20, 9),
            (ImageryFormat::Bc7, 87_408 * 360, 360),
        ];
        assert_eq!(
            held_line(ImageryFormat::Bc7, &mut held),
            "imagery BC7 30 MiB (360 × 85 KiB) + RGBA8 3 MiB (9 × 341 KiB)"
        );
        assert_eq!(
            held_line(
                ImageryFormat::Rgba8,
                &mut [(ImageryFormat::Rgba8, 349_524 * 360, 360)]
            ),
            "imagery RGBA8 120 MiB (360 × 341 KiB)"
        );
    }
}
