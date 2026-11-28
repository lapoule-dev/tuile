// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Where the budget window is drawn when it is a window of its own.
//!
//! - **A desktop** (macOS, Linux, Windows): an operating-system window of
//!   its own, opened on the viewer's event loop — its own surface on the
//!   view's device, moved, resized and closed apart from the view. Closing
//!   it hides the budget window (`window=off`) and nothing else; closing the
//!   view ends the viewer, and its window goes with it.
//! - **A page**: a browser window the page opens (`window.open`, its own
//!   document), the panel drawn into a texture by the same overlay and put
//!   on that window's canvas (`crate::web::popup`). The window is opened
//!   from the menu's click, where a browser allows it; a `tuile://` URL
//!   opens it too where the browser still does, and says so where it is
//!   blocked.
//! - **A headless run**: a texture of its own, read back after each
//!   picture, at the panel's own size — what a test looks at.
//!
//! A phone has no second window: the panel stays over its view
//! (`budget_window`).
//!
//! The panel is the same everywhere: the rows `budget_window` makes from
//! the published [`tuile_core::budget::BudgetSignals`] — no second
//! computation — laid out by `tuile_ui::Panel` and drawn by the overlay the
//! view's own controls are drawn by. Only where the pixels go differs.

#[cfg(not(any(target_arch = "wasm32", target_os = "ios", target_os = "android")))]
use std::sync::Arc;

use tuile_wgpu::{
    FrameTargets, GpuContext, OverlayMask, OverlayRenderer, OverlayVertex, ReadbackRing,
    DEPTH_FORMAT, TEXTURE_FORMAT,
};
use winit::event::WindowEvent;
use winit::window::WindowId;

use super::App;

/// Which of the viewer's windows an event is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Route {
    /// The view's own window — or a window the viewer does not know yet,
    /// before the view's is up: what every event was before.
    View,
    /// The budget window's.
    Panel,
    /// A window the viewer no longer has: a budget window just closed,
    /// whose last events are still on their way. Never the view's: a late
    /// close request of a closed budget window must not end the viewer.
    Gone,
}

/// Where the event for window `id` goes, the view's window being `view`
/// and the budget window's `panel`.
pub(super) fn route(id: WindowId, view: Option<WindowId>, panel: Option<WindowId>) -> Route {
    if panel == Some(id) {
        return Route::Panel;
    }
    match view {
        Some(view) if view != id => Route::Gone,
        _ => Route::View,
    }
}

/// What an event on the budget window's own window does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum PanelEvent {
    /// Closed by the person: the budget window is hidden.
    Close,
    /// Its drawable is now this many device pixels.
    Resize((u32, u32)),
    /// Moved to a display of another density.
    Rescale(f64),
    /// The system wants it drawn again.
    Redraw,
    /// The wheel, the pointer, a key: scrolling, a legend, the focus, or
    /// asking the window something — `T` its tables or its board, `C` this
    /// step or the counters summed, `R` summed from now, `I` the levels
    /// with an issue or all of them.
    Input(tuile_ui::budget_board::BoardInput),
    /// Anything else: the panel takes no input.
    Nothing,
}

/// What `event` on the budget window's own window does. Escape closes it,
/// as it would a sheet; it never reaches the view, where it would end the
/// viewer.
pub(super) fn panel_event(event: &WindowEvent, scale: f64) -> PanelEvent {
    use tuile_ui::budget_board::{BoardInput as In, BoardKey as K};
    use winit::keyboard::{Key, NamedKey};
    match event {
        WindowEvent::CloseRequested | WindowEvent::Destroyed => PanelEvent::Close,
        WindowEvent::KeyboardInput { event, .. }
            if event.state.is_pressed() && event.logical_key == Key::Named(NamedKey::Escape) =>
        {
            PanelEvent::Close
        }
        WindowEvent::KeyboardInput { event, .. } if event.state.is_pressed() => {
            let key = match event.logical_key.as_ref() {
                Key::Character("t" | "T") => K::Tables,
                Key::Character("c" | "C") => K::Cumulative,
                Key::Character("r" | "R") => K::Reset,
                Key::Character("i" | "I") => K::Issues,
                Key::Character("v" | "V") => K::Reservoir,
                Key::Character("]" | "+") => K::LevelUp,
                Key::Character("[" | "-") => K::LevelDown,
                Key::Character("a" | "A") => K::LevelAuto,
                Key::Named(NamedKey::Tab | NamedKey::ArrowDown) => K::Next,
                Key::Named(NamedKey::ArrowUp) => K::Previous,
                Key::Named(NamedKey::Backspace | NamedKey::Delete) => K::Clear,
                Key::Named(NamedKey::PageDown | NamedKey::Space) => K::PageDown,
                Key::Named(NamedKey::PageUp) => K::PageUp,
                Key::Named(NamedKey::Home) => K::Top,
                Key::Named(NamedKey::End) => K::Bottom,
                _ => return PanelEvent::Nothing,
            };
            PanelEvent::Input(In::Key(key))
        }
        WindowEvent::MouseWheel { delta, .. } => PanelEvent::Input(In::Wheel(match delta {
            winit::event::MouseScrollDelta::LineDelta(_, y) => -f64::from(*y) * 40.0 * scale,
            winit::event::MouseScrollDelta::PixelDelta(p) => -p.y,
        })),
        WindowEvent::CursorMoved { position, .. } => {
            PanelEvent::Input(In::Pointer(position.x, position.y))
        }
        WindowEvent::CursorLeft { .. } => PanelEvent::Input(In::Leave),
        WindowEvent::MouseInput {
            state,
            button: winit::event::MouseButton::Left,
            ..
        } => PanelEvent::Input(if state.is_pressed() {
            In::Press(f64::NAN, f64::NAN)
        } else {
            In::Release
        }),
        WindowEvent::Resized(size) => PanelEvent::Resize((size.width, size.height)),
        WindowEvent::ScaleFactorChanged { scale_factor, .. } => PanelEvent::Rescale(*scale_factor),
        WindowEvent::RedrawRequested => PanelEvent::Redraw,
        _ => PanelEvent::Nothing,
    }
}

/// What the panel is drawn into.
enum Target {
    /// An operating-system window and its surface, on the view's device.
    #[cfg(not(any(target_arch = "wasm32", target_os = "ios", target_os = "android")))]
    Window {
        window: Arc<winit::window::Window>,
        surface: wgpu::Surface<'static>,
        format: tuile_wgpu::SurfaceFormat,
    },
    /// A texture, read back: a page's window or a headless run's picture.
    #[cfg_attr(
        not(any(feature = "control", target_arch = "wasm32")),
        allow(dead_code)
    )]
    Texture {
        texture: wgpu::Texture,
        ring: ReadbackRing,
        /// The last picture read back, tightly packed RGBA8, sRGB, and its
        /// size.
        pixels: Option<(Vec<u8>, (u32, u32))>,
        /// Whether a picture is waited for (headless) or taken when it is
        /// ready (a page, which may not block).
        wait: bool,
        tag: u64,
    },
}

/// The budget window's own window: where its panel is drawn, and what
/// draws it.
pub(super) struct PanelScreen {
    target: Target,
    targets: FrameTargets,
    overlay: OverlayRenderer,
    /// The panel's layer on top (`tuile_ui::Drawing::on_top`): a legend,
    /// drawn after the panel's text.
    on_top: OverlayRenderer,
    /// The drawable, device pixels.
    pub(super) size: (u32, u32),
    /// Device pixels per point.
    pub(super) scale: f64,
}

/// The panel's background as the clear: the window is the panel, edge to
/// edge, wherever the table ends.
fn clear() -> wgpu::Color {
    let [r, g, b, _] = tuile_ui::PANEL_BACKGROUND;
    wgpu::Color {
        r: f64::from(r),
        g: f64::from(g),
        b: f64::from(b),
        a: 1.0,
    }
}

fn texture(gpu: &GpuContext, size: (u32, u32)) -> wgpu::Texture {
    gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("tuile budget window"),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TEXTURE_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

impl PanelScreen {
    /// On an operating-system window: a surface of its own on `instance`,
    /// in the view's `format`, drawn with the view's device.
    #[cfg(not(any(target_arch = "wasm32", target_os = "ios", target_os = "android")))]
    pub(super) fn window(
        gpu: &GpuContext,
        instance: &wgpu::Instance,
        window: Arc<winit::window::Window>,
        format: tuile_wgpu::SurfaceFormat,
    ) -> Result<Self, String> {
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| format!("the budget window's surface: {e}"))?;
        let inner = window.inner_size();
        let size = (inner.width.max(1), inner.height.max(1));
        super::setup::configure(&surface, gpu, format, size);
        Ok(Self {
            scale: window.scale_factor().max(1.0),
            targets: FrameTargets::for_surface(gpu, format, size),
            overlay: OverlayRenderer::new(gpu, format.render, Some(DEPTH_FORMAT)),
            on_top: OverlayRenderer::new(gpu, format.render, Some(DEPTH_FORMAT)),
            target: Target::Window {
                window,
                surface,
                format,
            },
            size,
        })
    }

    /// On a texture of `size` device pixels at `scale`, read back after
    /// each picture: waited for with `wait` (headless), taken when ready
    /// otherwise (a page).
    #[cfg_attr(
        not(any(feature = "control", target_arch = "wasm32")),
        allow(dead_code)
    )]
    pub(super) fn texture(gpu: &GpuContext, size: (u32, u32), scale: f64, wait: bool) -> Self {
        let size = (size.0.max(1), size.1.max(1));
        Self {
            target: Target::Texture {
                texture: texture(gpu, size),
                ring: ReadbackRing::new(2),
                pixels: None,
                wait,
                tag: 0,
            },
            targets: FrameTargets::new(gpu, TEXTURE_FORMAT, size),
            overlay: OverlayRenderer::new(gpu, TEXTURE_FORMAT, Some(DEPTH_FORMAT)),
            on_top: OverlayRenderer::new(gpu, TEXTURE_FORMAT, Some(DEPTH_FORMAT)),
            size,
            scale: scale.max(1.0),
        }
    }

    /// Its operating-system window's id, if it is one.
    pub(super) fn window_id(&self) -> Option<WindowId> {
        match &self.target {
            #[cfg(not(any(target_arch = "wasm32", target_os = "ios", target_os = "android")))]
            Target::Window { window, .. } => Some(window.id()),
            Target::Texture { .. } => None,
        }
    }

    /// Whether the panel is laid out at its own size (a texture nobody
    /// sizes) rather than in a drawable someone sizes.
    pub(super) fn sized_by_the_panel(&self) -> bool {
        matches!(self.target, Target::Texture { wait: true, .. })
    }

    /// Fits the drawable to `size` device pixels.
    pub(super) fn resize(&mut self, gpu: &GpuContext, size: (u32, u32)) {
        let size = (size.0.max(1), size.1.max(1));
        if size == self.size {
            return;
        }
        self.size = size;
        self.targets.resize(gpu, size);
        match &mut self.target {
            #[cfg(not(any(target_arch = "wasm32", target_os = "ios", target_os = "android")))]
            Target::Window {
                surface, format, ..
            } => super::setup::configure(surface, gpu, *format, size),
            Target::Texture { texture: t, .. } => *t = texture(gpu, size),
        }
    }

    /// Draws `panel` and shows it: presented on a window, read back on a
    /// texture.
    /// The panel first, its triangles then its text; its layer on top
    /// after, so a legend's box hides the panel's text under it.
    pub(super) fn paint(&mut self, gpu: &GpuContext, panel: &tuile_ui::Drawing) {
        set_layer(
            &mut self.overlay,
            gpu,
            &panel.mesh,
            &panel.labels,
            self.size,
        );
        let top = &panel.on_top;
        set_layer(&mut self.on_top, gpu, &top.mesh, &top.labels, self.size);
        let (overlay, on_top) = (&self.overlay, &self.on_top);
        let size = self.size;
        match &mut self.target {
            #[cfg(not(any(target_arch = "wasm32", target_os = "ios", target_os = "android")))]
            Target::Window {
                window,
                surface,
                format,
            } => {
                let presented = tuile_wgpu::draw_to_surface(
                    tuile_wgpu::FrameOnSurface {
                        gpu,
                        surface,
                        targets: &self.targets,
                        clear: clear(),
                        trace: None,
                        record: None,
                    },
                    || super::setup::configure(surface, gpu, *format, size),
                    || window.pre_present_notify(),
                    |pass| {
                        overlay.render(pass);
                        on_top.render(pass);
                    },
                    tuile_wgpu::NOTHING_AFTER,
                );
                if presented == tuile_wgpu::Presented::Invalid {
                    tracing::warn!("the budget window's surface refused a frame");
                }
            }
            Target::Texture {
                texture,
                ring,
                pixels,
                wait,
                tag,
            } => {
                tuile_wgpu::draw_to_texture(
                    tuile_wgpu::FrameOnTexture {
                        gpu,
                        texture,
                        targets: &self.targets,
                        clear: clear(),
                        capture: None,
                    },
                    |pass| {
                        overlay.render(pass);
                        on_top.render(pass);
                    },
                    tuile_wgpu::NOTHING_AFTER,
                );
                *tag += 1;
                let mut encoder = gpu.device.create_command_encoder(&Default::default());
                if ring.capture(gpu, &mut encoder, texture, size, *tag) {
                    gpu.queue.submit([encoder.finish()]);
                    ring.submitted();
                }
                if *wait {
                    take(ring, gpu, true, pixels);
                }
            }
        }
    }

    /// The last picture of a texture, read back: tightly packed RGBA8,
    /// sRGB, and its size. Takes the pictures that became ready since.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(super) fn picture(&mut self, gpu: &GpuContext) -> Option<(&[u8], (u32, u32))> {
        match &mut self.target {
            Target::Texture { ring, pixels, .. } => {
                take(ring, gpu, false, pixels);
                pixels.as_ref().map(|(p, s)| (p.as_slice(), *s))
            }
            #[cfg(not(any(target_arch = "wasm32", target_os = "ios", target_os = "android")))]
            Target::Window { .. } => None,
        }
    }

    /// The last picture read back, without polling the device.
    #[cfg_attr(not(feature = "control"), allow(dead_code))]
    pub(super) fn last_picture(&self) -> Option<(&[u8], (u32, u32))> {
        match &self.target {
            Target::Texture { pixels, .. } => pixels.as_ref().map(|(p, s)| (p.as_slice(), *s)),
            #[cfg(not(any(target_arch = "wasm32", target_os = "ios", target_os = "android")))]
            Target::Window { .. } => None,
        }
    }
}

/// `overlay` to draw `mesh`, then `labels` over it, in a drawable of
/// `size` device pixels.
pub(super) fn set_layer(
    overlay: &mut OverlayRenderer,
    gpu: &GpuContext,
    mesh: &[tuile_ui::Vertex],
    labels: &[tuile_ui::Label],
    size: (u32, u32),
) {
    let vertices: Vec<OverlayVertex> = mesh
        .iter()
        .map(|v| OverlayVertex {
            position: v.position,
            color: v.color,
        })
        .collect();
    overlay.set_geometry(gpu, &vertices, size);
    let masks: Vec<OverlayMask<'_>> = labels
        .iter()
        .map(|label| OverlayMask {
            width: label.mask.width,
            height: label.mask.height,
            coverage: &label.mask.coverage,
            position: label.position,
            color: label.color,
        })
        .collect();
    overlay.set_masks(gpu, &masks);
}

/// A page's window is the page's to close: it goes with its screen, and
/// with the viewer.
#[cfg(target_arch = "wasm32")]
impl Drop for PanelScreen {
    fn drop(&mut self) {
        crate::web::popup::close();
    }
}

/// The newest picture `ring` has ready into `pixels`.
fn take(
    ring: &mut ReadbackRing,
    gpu: &GpuContext,
    wait: bool,
    pixels: &mut Option<(Vec<u8>, (u32, u32))>,
) {
    for frame in ring.ready(gpu, wait) {
        let size = frame.size;
        if let Some(read) = frame.read(1) {
            *pixels = Some((read, size));
        }
    }
}

impl App {
    /// The names of the imagery layers, by index, as the panel shows them.
    pub(super) fn budget_layer_names(&self) -> Vec<String> {
        self.imagery
            .layers
            .get(self.imagery.layer)
            .map(|l| vec![l.name.clone()])
            .unwrap_or_default()
    }

    /// The camera as the reservoir's plan draws it: its place, heading and
    /// the ground its frame sees. `None` while no window shows the
    /// reservoir.
    pub(super) fn reservoir_eye(&self) -> Option<tuile_ui::reservoir::ReservoirEye> {
        if !(self.budget_window.shown() && self.budget_window.reservoir()) {
            return None;
        }
        let cam = &self.controller.camera;
        let g = cam.geodetic();
        let (w, h) = self.viewport();
        // The tiles marked drawn that no frustum sees: measured against the
        // present eyes, then those and the host's views as well.
        let report = tuile_core::metrics::budget().and_then(|s| s.score_held);
        let all = self.eye_views();
        let present = self.eyes().len().min(all.len());
        let (outside_present, outside_any) = report.map_or_else(Default::default, |r| {
            (
                tuile_ui::reservoir::drawn_outside(&r, &all[..present]),
                tuile_ui::reservoir::drawn_outside(&r, &all),
            )
        });
        let selected_in_view = self
            .budget_window
            .selected()
            .map(|t| tuile_ui::reservoir::tile_in_views(t, &all[..present]));
        Some(tuile_ui::reservoir::ReservoirEye {
            selected_in_view,
            truncated: tuile_core::metrics::metrics().traversals_truncated.get(),
            outside_present,
            outside_any,
            lon: g.lon,
            lat: g.lat,
            height_m: g.height,
            heading: cam.heading(),
            footprint: cam.ground_footprint(w / h.max(1.0)),
        })
    }

    /// Opens, draws or closes the budget window's own window, as it is
    /// asked. `event_loop` is the loop's, when the turn has it: an
    /// operating-system window is opened only from there. Nothing here
    /// touches the view: its window, its surface, its session and its
    /// budget are the same whether the budget window is open or not.
    pub(super) fn place_the_budget_window(
        &mut self,
        event_loop: Option<&winit::event_loop::ActiveEventLoop>,
    ) {
        if !self.budget_window.wants_own_window() {
            // Dropped: its window closes with it.
            self.budget_screen = None;
            // A page's window a click opened for an ask that did not show
            // the budget window after all.
            #[cfg(target_arch = "wasm32")]
            if crate::web::popup::is_open() {
                crate::web::popup::close();
            }
            return;
        }
        // A page's window closed by the person.
        #[cfg(target_arch = "wasm32")]
        if self.budget_screen.is_some() && !crate::web::popup::is_open() {
            self.budget_screen = None;
            self.budget_window.closed();
            return;
        }
        if self.budget_screen.is_none() {
            self.open_the_budget_window(event_loop);
        }
        // What the person did in a page's window since the last turn.
        #[cfg(target_arch = "wasm32")]
        for input in crate::web::popup::take_inputs() {
            if self.budget_window.input(input) {
                self.publish_the_view(None);
            }
        }

        let names = self.budget_layer_names();
        let eye = self.reservoir_eye();
        self.budget_window.set_eye(eye);
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let Some(screen) = self.budget_screen.as_mut() else {
            return;
        };
        #[cfg(target_arch = "wasm32")]
        if let Some((size, scale)) = crate::web::popup::size() {
            screen.scale = scale;
            screen.resize(&active.gpu, size);
        }
        let signals = tuile_core::metrics::budget();
        // A texture nobody sizes is laid out at the panel's own size.
        let frame = if screen.sized_by_the_panel() {
            (16_384, 16_384)
        } else {
            screen.size
        };
        let laid =
            self.budget_window
                .lay_out(signals.as_ref(), frame, (0.0, 0.0), screen.scale, &names);
        if laid {
            if let Some(panel) = self.budget_window.own_layout() {
                if screen.sized_by_the_panel() {
                    screen.resize(&active.gpu, panel.size);
                }
                screen.paint(&active.gpu, panel);
            }
        }
        #[cfg(target_arch = "wasm32")]
        if let Some((pixels, size)) = screen.picture(&active.gpu) {
            crate::web::popup::show(pixels, size);
        }
    }

    /// Opens the budget window's own window, at the panel's size; says why
    /// where it cannot be, and hides the budget window then.
    fn open_the_budget_window(&mut self, event_loop: Option<&winit::event_loop::ActiveEventLoop>) {
        let names = self.budget_layer_names();
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let signals = tuile_core::metrics::budget();
        let natural = self
            .budget_window
            .natural_size(signals.as_ref(), active.scale, &names);
        let opened: Result<Option<PanelScreen>, String> = match &active.screen {
            #[cfg(not(any(target_arch = "wasm32", target_os = "ios", target_os = "android")))]
            super::screen::Screen::Window(view) => {
                let Some(event_loop) = event_loop else {
                    // On the next turn that has the loop.
                    return;
                };
                // At the panel's size, clamped to a window a screen holds;
                // in points, so a display of another density keeps it.
                let points =
                    |px: u32, lo: f64, hi: f64| (f64::from(px) / active.scale).clamp(lo, hi);
                let attrs = winit::window::Window::default_attributes()
                    .with_title(format!("Budget — {}", self.title))
                    .with_inner_size(winit::dpi::LogicalSize::new(
                        points(natural.0, 320.0, 1_600.0),
                        points(natural.1, 160.0, 1_000.0),
                    ))
                    // As narrow as the board's narrowest section, one class wide:
                    // narrower would cut its text.
                    .with_min_inner_size(winit::dpi::LogicalSize::new(680.0, 200.0))
                    .with_resizable(true);
                event_loop
                    .create_window(attrs)
                    .map_err(|e| format!("the budget window: {e}"))
                    .and_then(|window| {
                        PanelScreen::window(
                            &active.gpu,
                            &view.instance,
                            Arc::new(window),
                            active.format,
                        )
                    })
                    .map(Some)
            }
            #[cfg(target_arch = "wasm32")]
            super::screen::Screen::Window(_) => {
                let _ = event_loop;
                let points = (
                    f64::from(natural.0) / active.scale,
                    f64::from(natural.1) / active.scale,
                );
                if crate::web::popup::open(points) {
                    let (size, scale) =
                        crate::web::popup::size().unwrap_or((natural, active.scale));
                    Ok(Some(PanelScreen::texture(&active.gpu, size, scale, false)))
                } else {
                    Err(
                        "the browser did not open the budget window (a pop-up blocker?): \
                         open it from the menu, Budget › Budget Window"
                            .to_owned(),
                    )
                }
            }
            // A phone keeps the panel over its view: never asked.
            #[cfg(any(target_os = "ios", target_os = "android"))]
            super::screen::Screen::Window(_) => {
                let _ = (event_loop, natural);
                Ok(None)
            }
            #[cfg(feature = "control")]
            super::screen::Screen::Offscreen(_) => {
                let _ = event_loop;
                Ok(Some(PanelScreen::texture(
                    &active.gpu,
                    natural,
                    active.scale,
                    true,
                )))
            }
        };
        match opened {
            Ok(Some(screen)) => {
                tracing::info!(
                    size = format!("{}x{}", screen.size.0, screen.size.1),
                    "budget window opened"
                );
                self.budget_screen = Some(screen);
                self.budget_window.again();
            }
            Ok(None) => {}
            Err(why) => {
                tracing::warn!("{why}");
                self.budget_window.closed();
                self.messages.say("Budget window".to_owned(), why);
            }
        }
    }

    /// An event on the budget window's own window.
    pub(super) fn on_panel_event(&mut self, event: &WindowEvent) {
        let scale = self.budget_screen.as_ref().map_or(1.0, |s| s.scale);
        match panel_event(event, scale) {
            PanelEvent::Close => {
                tracing::info!("budget window closed");
                self.budget_window.closed();
                self.budget_screen = None;
                return;
            }
            PanelEvent::Resize(size) => {
                if let (Some(active), Some(screen)) =
                    (self.active.as_ref(), self.budget_screen.as_mut())
                {
                    screen.resize(&active.gpu, size);
                }
            }
            PanelEvent::Rescale(scale) => {
                if let Some(screen) = self.budget_screen.as_mut() {
                    screen.scale = scale.max(1.0);
                }
            }
            PanelEvent::Redraw => {}
            PanelEvent::Input(input) => {
                if self.budget_window.input(input) {
                    // The menu's marks follow.
                    self.publish_the_view(None);
                }
            }
            PanelEvent::Nothing => return,
        }
        // Drawn again at once: a window being resized is drawn while it is,
        // whatever the loop is doing.
        self.budget_window.again();
        self.place_the_budget_window(None);
    }

    /// The budget window's last picture, when it is drawn into a texture
    /// of its own (a headless run): tightly packed RGBA8, sRGB.
    #[cfg(feature = "control")]
    pub(crate) fn budget_window_picture(&self) -> Option<(&[u8], (u32, u32))> {
        self.budget_screen.as_ref()?.last_picture()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> WindowId {
        WindowId::from(n)
    }

    /// The budget window's events go to it, the view's to the view; a
    /// window the viewer no longer has — a budget window just closed — is
    /// nobody's: its late close request never ends the viewer.
    #[test]
    fn each_window_has_its_own_events() {
        let (view, panel) = (id(1), id(2));
        assert_eq!(route(view, Some(view), Some(panel)), Route::View);
        assert_eq!(route(panel, Some(view), Some(panel)), Route::Panel);
        assert_eq!(route(panel, Some(view), None), Route::Gone);
        // Before the view's window is up: everything is the view's, as it
        // always was.
        assert_eq!(route(view, None, None), Route::View);
    }

    /// The pixels in the alert colour (vermillion) and the waste colour
    /// (reddish purple), roughly: the shapes the attention rules draw.
    fn coloured(pixels: &[u8]) -> (usize, usize) {
        let near = |p: &[u8], c: [u8; 3]| (0..3).all(|k| p[k].abs_diff(c[k]) < 24);
        let alert = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| near(&p[..], [0xD5, 0x5E, 0x00]))
            .count();
        let waste = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| near(&p[..], [0xCC, 0x79, 0xA7]))
            .count();
        (alert, waste)
    }

    /// The board drawn as the budget window draws it — the panel's own
    /// texture, the overlay — for a healthy step and for the stall of the
    /// captures: the stall is drawn in the alert's colour, the healthy step
    /// nowhere. `TUILE_BOARD_PICTURES=<dir>` writes both as PNG.
    #[test]
    fn the_board_draws_a_stall_in_red_and_a_healthy_step_without() {
        let gpu = match futures_executor::block_on(GpuContext::headless()) {
            Ok(gpu) => gpu,
            Err(e) => {
                assert!(
                    std::env::var("TUILE_REQUIRE_GPU").is_err(),
                    "TUILE_REQUIRE_GPU is set and there is no usable adapter: {e}"
                );
                eprintln!("skipping: no usable adapter ({e})");
                return;
            }
        };
        let fixtures = tuile_ui::budget_board::fixtures::healthy;
        let mut texts = tuile_ui::budget_board::TextCache::default();
        let mut picture = |name: &str, s: &tuile_core::budget::BudgetSignals| {
            let view = tuile_ui::budget_board::BoardView {
                cumulative: false,
                from: None,
                layers: &["Aerial".to_owned()],
                stalls: &tuile_ui::budget_board::StallWatch::at_once(),
                width: None,
                issues_only: false,
                decide: Some(tuile_ui::budget_board::DecideSummary {
                    mean_ms: 0.9,
                    max_ms: 2.4,
                    steps: 240,
                    publications: 120,
                }),
            };
            let board = tuile_ui::budget_board::draw(Some(s), &view, &mut texts, 2.0);
            let mut screen = PanelScreen::texture(&gpu, board.size, 2.0, true);
            screen.paint(&gpu, &board);
            let (pixels, size) = screen.last_picture().expect("read back");
            if let Ok(dir) = std::env::var("TUILE_BOARD_PICTURES") {
                let path = std::path::Path::new(&dir).join(format!("{name}.png"));
                image::save_buffer(
                    &path,
                    pixels,
                    size.0,
                    size.1,
                    image::ExtendedColorType::Rgba8,
                )
                .expect("written");
            }
            coloured(pixels)
        };
        let (alert, waste) = picture("board-healthy", &fixtures());
        assert_eq!(
            (alert, waste),
            (0, 0),
            "a healthy step drawn in alert or waste colours"
        );
        let (alert, waste) = picture(
            "board-stalled",
            &tuile_ui::budget_board::fixtures::stalled(),
        );
        assert!(
            alert > 200,
            "the stall is not drawn in the alert's colour: {alert}"
        );
        assert!(
            waste > 200,
            "the drapes held for nothing are not drawn as waste: {waste}"
        );
    }

    /// A legend's box hides the board under it: drawn after the board's
    /// text, not under it. A box of blank lines over rows of numbers is
    /// dark inside, where the same rows without it are written.
    /// `TUILE_BOARD_PICTURES=<dir>` also writes a real legend over a row.
    #[test]
    fn a_legend_hides_the_board_under_it() {
        use tuile_ui::budget_board::{BoardControl, BoardInput, Hit, Legend, Swatch};
        let gpu = match futures_executor::block_on(GpuContext::headless()) {
            Ok(gpu) => gpu,
            Err(e) => {
                assert!(
                    std::env::var("TUILE_REQUIRE_GPU").is_err(),
                    "TUILE_REQUIRE_GPU is set and there is no usable adapter: {e}"
                );
                eprintln!("skipping: no usable adapter ({e})");
                return;
            }
        };
        let scale = 2.0;
        let stalls = tuile_ui::budget_board::StallWatch::at_once();
        let view = tuile_ui::budget_board::BoardView {
            cumulative: false,
            from: None,
            layers: &["Aerial".to_owned()],
            stalls: &stalls,
            width: None,
            issues_only: false,
            decide: None,
        };
        let mut texts = tuile_ui::budget_board::TextCache::default();
        let s = tuile_ui::budget_board::fixtures::stalled();
        let board = tuile_ui::budget_board::draw(Some(&s), &view, &mut texts, scale);
        let (fw, fh) = (f64::from(board.size.0), f64::from(board.size.1));
        let picture = |d: &tuile_ui::Drawing| {
            let mut screen = PanelScreen::texture(&gpu, d.size, scale, true);
            screen.paint(&gpu, d);
            let (pixels, size) = screen.last_picture().expect("read back");
            (pixels.to_vec(), size)
        };
        // The pixels of `rect` brighter than the box could ever be: ink.
        let ink = |(pixels, size): &(Vec<u8>, (u32, u32)), [x, y, w, h]: [f64; 4]| {
            let mut n = 0;
            for py in y as u32..(y + h) as u32 {
                for px in x as u32..(x + w) as u32 {
                    let i = ((py * size.0 + px) * 4) as usize;
                    n += usize::from(pixels[i..i + 3].iter().any(|&c| c > 0x60));
                }
            }
            n
        };
        // A row in the upper half, left of the middle: its box below it
        // lies over the rows that follow.
        let row = board
            .hits
            .iter()
            .find(|h| {
                let [x, y, w, _] = h.rect;
                w >= 200.0 && x + 400.0 < fw && y > fh / 8.0 && y < fh / 2.0
            })
            .expect("a row with a legend")
            .clone();
        let blank = Hit {
            pick: None,
            rect: row.rect,
            lines: vec![
                Legend {
                    swatch: Swatch::None,
                    text: " ".repeat(200),
                };
                3
            ],
        };
        // The board's words well inside the box — under its top line, left
        // of its right edge — and only them: the bars are triangles, which
        // the box hides whichever layer it is in.
        let [hx, hy, _, hh] = row.rect;
        let top = hy + hh + 4.0;
        let words: Vec<[f64; 4]> = board
            .labels
            .iter()
            .map(|l| {
                let [x, y] = l.position.map(f64::from);
                [x, y, f64::from(l.mask.width), f64::from(l.mask.height)]
            })
            .filter(|&[x, y, w, h]| {
                x >= hx + 16.0 && x + w <= hx + 800.0 && y >= top + 8.0 && y + h <= top + 90.0
            })
            .collect();
        let ink_in = |p: &(Vec<u8>, (u32, u32))| words.iter().map(|&r| ink(p, r)).sum::<usize>();
        let bare = picture(&board);
        assert!(
            ink_in(&bare) > 100,
            "no board text where the box goes: {words:?}"
        );
        let mut covered = board.clone();
        tuile_ui::budget_board::tooltip(&mut covered, &blank, board.size, &mut texts, scale);
        let shown = picture(&covered);
        assert_eq!(
            ink_in(&shown),
            0,
            "the board's text shows through the legend's box"
        );
        if let Ok(dir) = std::env::var("TUILE_BOARD_PICTURES") {
            let mut control = BoardControl::default();
            let [x, y, w, h] = row.rect;
            control.input(BoardInput::Pointer(x + w / 2.0, y + h / 2.0));
            let d = control.present(board.clone(), board.size, scale, &mut texts);
            let (pixels, size) = picture(&d);
            let path = std::path::Path::new(&dir).join("board-tooltip.png");
            image::save_buffer(
                &path,
                &pixels,
                size.0,
                size.1,
                image::ExtendedColorType::Rgba8,
            )
            .expect("written");
        }
    }

    /// Closing the budget window's window closes it; resizing sizes it;
    /// nothing else on it does anything — no key reaches the view.
    #[test]
    fn the_budget_windows_events() {
        assert_eq!(
            panel_event(&WindowEvent::CloseRequested, 1.0),
            PanelEvent::Close
        );
        assert_eq!(panel_event(&WindowEvent::Destroyed, 1.0), PanelEvent::Close);
        assert_eq!(
            panel_event(
                &WindowEvent::Resized(winit::dpi::PhysicalSize::new(640, 300)),
                1.0
            ),
            PanelEvent::Resize((640, 300))
        );
        assert_eq!(
            panel_event(&WindowEvent::RedrawRequested, 1.0),
            PanelEvent::Redraw
        );
        assert_eq!(
            panel_event(&WindowEvent::Focused(true), 1.0),
            PanelEvent::Nothing
        );
    }
}
