// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Bringing up the window, the adapter, the device and the surface.
//!
//! Runs once. Everything chosen here — the surface format, the sample count,
//! the features asked of the adapter — is a decision the rest of the session
//! cannot revisit, so each one says out loud what it picked and why it might
//! not have got it.
//!
//! The window comes first and at once; the GPU behind it is asked for
//! asynchronously, because that is the only way a browser answers. A desktop
//! and a phone wait for the answer on the spot, as they always did; a page
//! collects it on a later turn of its loop ([`App::take_the_gpu`]), drawing
//! nothing meanwhile — there is nothing yet to draw with.

use std::cell::RefCell;
use std::rc::Rc;

use super::{Active, App};
use glam::DVec3;
use std::sync::Arc;
use tuile_wgpu::{ContentPump, GpuContext, OverlayRenderer, TileRenderer, DEPTH_FORMAT};
use winit::event_loop::ActiveEventLoop;
use winit::window::Window;

/// Everything the device bring-up produces.
pub(super) struct Gpu {
    window: Arc<Window>,
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    format: tuile_wgpu::SurfaceFormat,
    gpu: GpuContext,
    backend: wgpu::Backend,
    /// What the person's choice does with tiles stored compressed, on this
    /// adapter: told to a page's worker beside the plan.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    stored_imagery: tuile_core::texels::StoredPlan,
}

/// The GPU, asked for and not yet collected.
pub(super) struct GpuOnItsWay(Rc<RefCell<Option<Result<Gpu, String>>>>);

impl GpuOnItsWay {
    /// Asks for a GPU that can draw into `window`, and waits for it: a
    /// desktop and a phone have a thread they may block.
    ///
    /// Tried backend by backend, in the order [`attempts`] gives: on Android,
    /// Vulkan and then OpenGL ES, for a device or an emulator whose Vulkan
    /// driver will not give a surface, an adapter or a device.
    #[cfg(not(target_arch = "wasm32"))]
    fn start(
        window: Arc<Window>,
        backends: Option<wgpu::Backends>,
        compression: tuile_wgpu::ImageryCompression,
    ) -> Self {
        let mut refusals = Vec::new();
        let mut answer = Err(String::from("no backend to try"));
        for attempt in attempts(backends, cfg!(target_os = "android")) {
            // Each attempt's surface is dropped before the next is made: a
            // window takes one producer at a time.
            answer = pollster::block_on(bring_up(Arc::clone(&window), attempt, compression));
            match &answer {
                Ok(_) => break,
                Err(why) => {
                    tracing::warn!(?attempt, "no GPU through this backend: {why}");
                    refusals.push(why.clone());
                }
            }
        }
        let answer = answer.map_err(|_| refusals.join("; "));
        Self(Rc::new(RefCell::new(Some(answer))))
    }

    /// Asks for a GPU that can draw into `window`, and returns at once: a
    /// browser answers on a later turn of its event loop.
    #[cfg(target_arch = "wasm32")]
    fn start(
        window: Arc<Window>,
        backends: Option<wgpu::Backends>,
        compression: tuile_wgpu::ImageryCompression,
    ) -> Self {
        let slot = Rc::new(RefCell::new(None));
        let answer = Rc::clone(&slot);
        let viewer = crate::web::imagery::viewer();
        wasm_bindgen_futures::spawn_local(async move {
            let up = bring_up(Arc::clone(&window), backends, compression).await;
            // How the device holds imagery, for the session's worker waiting
            // on it: RGBA8 where there is no device — nothing will draw, and
            // the session must not wait for ever.
            crate::web::imagery::held_by_the_device(
                viewer,
                match &up {
                    Ok(gpu) => {
                        crate::web::imagery::Held::new(gpu.gpu.imagery_plan(), gpu.stored_imagery)
                    }
                    Err(_) => crate::web::imagery::Held::RGBA8,
                },
            );
            *answer.borrow_mut() = Some(up);
            // The loop may be asleep, with nothing else to wake it: a frame
            // asked for is a turn of the loop, which collects the answer.
            window.request_redraw();
        });
        Self(slot)
    }

    /// The answer, once it has come.
    fn take(&self) -> Option<Result<Gpu, String>> {
        self.0.borrow_mut().take()
    }
}

/// The backends to bring the GPU up through, in order: what was asked for,
/// or — with nothing asked — the platform's default, which on Android is
/// Vulkan first and OpenGL ES after it.
///
/// wgpu's default instance there holds both, and takes the first adapter it
/// finds: a Vulkan driver that enumerates and then fails to make a device or
/// a surface would end the session where OpenGL ES would have drawn. So the
/// two are tried one after the other, each in an instance of its own.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(super) fn attempts(
    asked: Option<wgpu::Backends>,
    gl_after_vulkan: bool,
) -> Vec<Option<wgpu::Backends>> {
    match asked {
        Some(backends) => vec![Some(backends)],
        None if gl_after_vulkan => vec![Some(wgpu::Backends::VULKAN), Some(wgpu::Backends::GL)],
        None => vec![None],
    }
}

/// A backend named in `TUILE_BACKEND` — `vulkan`, `gl`, `metal`, `dx12` — or
/// an error naming the ones there are. Unset or empty is the platform's
/// default.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn backend_named(name: Option<&str>) -> Result<Option<wgpu::Backends>, String> {
    let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
        return Ok(None);
    };
    match name.to_ascii_lowercase().as_str() {
        "vulkan" => Ok(Some(wgpu::Backends::VULKAN)),
        "gl" | "gles" | "opengl" => Ok(Some(wgpu::Backends::GL)),
        "metal" => Ok(Some(wgpu::Backends::METAL)),
        "dx12" => Ok(Some(wgpu::Backends::DX12)),
        _ => Err(format!(
            "TUILE_BACKEND={name:?} is not a backend: vulkan, gl, metal or dx12"
        )),
    }
}

/// The instance, the surface on `window`, the adapter and the device.
///
/// `backends` narrows the instance — a page forcing WebGL2, say; `None` is
/// the platform's default. A browser without WebGPU falls back to WebGL2 by
/// itself: the instance is made with WebGPU detected rather than assumed.
///
/// `compression` is the person's choice of how imagery is held: resolved
/// against this adapter, it asks the device for its format's feature, and
/// for nothing when it is off.
async fn bring_up(
    window: Arc<Window>,
    backends: Option<wgpu::Backends>,
    compression: tuile_wgpu::ImageryCompression,
) -> Result<Gpu, String> {
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    if let Some(backends) = backends {
        descriptor.backends = backends;
    }
    let instance = wgpu::util::new_instance_with_webgpu_detection(descriptor).await;
    let surface = instance
        .create_surface(window.clone())
        .map_err(|e| format!("create surface: {e}"))?;
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            apply_limit_buckets: false,
        })
        .await
        .map_err(|e| format!("adapter: {e}"))?;
    // Which GPU, and through which API. Worth a line: every performance
    // question that follows — what a present costs, what a buffer write
    // costs, whether a timing tool applies at all — is answered differently
    // per backend, and guessing which one is in play wastes the whole
    // investigation.
    let info = adapter.get_info();
    tracing::info!(
        backend = %info.backend,
        device = %info.name,
        kind = ?info.device_type,
        driver = %info.driver_info,
        "adapter"
    );
    // What the GPU costs, asked for only where it is on offer.
    //
    // Nothing was requested here at all, so there was no way to time a
    // render pass: every performance question had to be answered from the
    // CPU side, where a frame that is merely *waiting* for the GPU looks
    // exactly like a frame that is idle. Timestamps are what tell those two
    // apart.
    //
    // Intersected with what the adapter actually has rather than named
    // outright, because a hard requirement here would turn a missing
    // stopwatch into a viewer that refuses to start — on someone else's
    // machine, over a diagnostic they were not using.
    let wanted = wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS;
    let features = adapter.features() & wanted;
    // Said out loud either way. A capability that is silently absent is one
    // that gets reported later as a broken tool.
    let missing = wanted - features;
    if missing.is_empty() {
        tracing::info!(?features, "gpu timing available");
    } else {
        tracing::info!(?features, ?missing, "gpu timing partly unavailable");
    }
    // How the imagery is held: the person's choice, on this adapter. Off asks
    // for nothing; a block format asks for its own feature, which the
    // adapter was checked to have.
    let (imagery_plan, imagery_features) = tuile_wgpu::plan_for(compression, &adapter);
    let stored_imagery = compression.stored(tuile_wgpu::texture_caps(&adapter));
    tracing::info!(
        ?compression,
        held = tuile_wgpu::format_name(imagery_plan.target),
        encode = ?imagery_plan.encode,
        caps = ?tuile_wgpu::texture_caps(&adapter),
        "imagery"
    );
    // Everything the adapter has, which is by definition what it supports: on
    // WebGL2 that is the downlevel set raised to what the context reports, and
    // the renderer fits its imagery slots to it.
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("viewer"),
            required_limits: adapter.limits(),
            required_features: features | imagery_features,
            ..Default::default()
        })
        .await
        .map_err(|e| format!("device: {e}"))?;
    let format = tuile_wgpu::surface_format(
        &surface.get_capabilities(&adapter),
        adapter.get_downlevel_capabilities().flags,
    );
    if format.encode {
        tracing::info!(
            surface = ?format.surface,
            render = ?format.render,
            "a linear surface with no sRGB view: the frame is encoded by a pass"
        );
    }
    Ok(Gpu {
        window,
        instance,
        surface,
        format,
        gpu: GpuContext::new(device, queue).with_imagery_plan(imagery_plan),
        backend: info.backend,
        stored_imagery,
    })
}

impl App {
    /// Creates the window and everything that hangs off it.
    pub(super) fn start_window(&mut self, event_loop: &ActiveEventLoop) {
        self.statusbar = super::readout::StatusBar::new();
        // Here and not earlier: a menu wants the application object the event
        // loop creates, and the main thread.
        // With the on-screen controls: a page that brings its own has its
        // own menu too.
        if self.controls {
            let tilesets: Vec<crate::embed::TilesetChoice> = self
                .tilesets
                .slots
                .iter()
                .map(|s| crate::embed::TilesetChoice::new(&s.name, s.asset))
                .collect();
            self.menu = crate::menu::Installed::install(self.imagery.layers, &tilesets);
        }
        if self.statusbar.is_none() {
            tracing::info!("no status-bar item on this platform; the readout is off");
        }
        // Born hidden, shown once the coarse pyramid is on the GPU.
        //
        // A globe drawn before its floor exists is a globe with nothing behind
        // its holes — the one state this whole design prevents, on screen at
        // the moment someone forms a first impression. See
        // [`App::still_warming`], which also decides when to give up waiting.
        //
        // A phone's window is the exception: the system ends an application
        // that shows nothing, so it opens at once on the globe's shell and the
        // loading indicator — see `loading`.
        let attrs = self
            .window_attributes
            .clone()
            .with_title(self.title.clone())
            .with_visible(!self.profile.hold_window_until_warm);
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        self.gpu_on_its_way = Some(GpuOnItsWay::start(
            window,
            self.backends,
            self.imagery_compression,
        ));
        self.take_the_gpu();
    }

    /// Finishes bringing the window up once its GPU has arrived; nothing
    /// before then.
    pub(super) fn take_the_gpu(&mut self) {
        let Some(answer) = self.gpu_on_its_way.as_ref().and_then(GpuOnItsWay::take) else {
            return;
        };
        self.gpu_on_its_way = None;
        match answer {
            Ok(gpu) => self.finish_window(gpu),
            // Said the way a session that cannot start is: once, in the
            // platform's own words. Nothing will ever be drawn, so an
            // application whose window it is ends once that has been said.
            Err(why) => {
                tracing::error!("no GPU to draw with: {why}");
                crate::desktop::drawing(Err(&why));
                self.messages
                    .say(format!("{} cannot draw", self.title), why);
                self.ended_by = Some("no GPU could draw into the window");
            }
        }
    }

    /// Everything that hangs off a window and its device.
    fn finish_window(&mut self, up: Gpu) {
        let Gpu {
            window,
            instance,
            surface,
            format,
            gpu,
            backend,
            stored_imagery: _,
        } = up;
        // The drawable is the whole window, edge to edge; on a phone the
        // inner size is only its safe area, which the controls keep to.
        let size = if self.profile.hold_window_until_warm {
            window.inner_size()
        } else {
            window.outer_size()
        };
        let size = (size.width.max(1), size.height.max(1));
        let scale = window.scale_factor().max(1.0);
        // Said out loud, because everything downstream divides by it and a wrong
        // value is invisible: the ground still draws, the imagery is merely a
        // level off and the type a size off, which reads as a rendering opinion
        // rather than a number to check.
        tracing::info!(
            device_px = format!("{}x{}", size.0, size.1),
            points = format!(
                "{:.0}x{:.0}",
                f64::from(size.0) / scale,
                f64::from(size.1) / scale
            ),
            scale,
            "display"
        );
        let safe = safe_area(&window, size);
        let screen = super::screen::Screen::Window(super::screen::Windowed {
            window,
            instance,
            surface: Some(surface),
        });
        self.finish_screen(screen, format, gpu, size, scale, safe);
        crate::desktop::drawing(Ok(backend.to_str()));
    }

    /// Opens the headless viewer's screen: `eyes` textures of `size` device
    /// pixels at `scale` device pixels per point, on `gpu`. Everything the
    /// frame draws with is built as a window's is.
    #[cfg(feature = "control")]
    pub(crate) fn start_offscreen(
        &mut self,
        gpu: GpuContext,
        size: (u32, u32),
        scale: f64,
        rig: super::Rig,
    ) {
        let size = (size.0.max(1), size.1.max(1));
        let format = tuile_wgpu::SurfaceFormat {
            surface: super::offscreen::FORMAT,
            render: super::offscreen::FORMAT,
            encode: false,
        };
        let screen = super::screen::Screen::Offscreen(super::offscreen::Offscreen::new(
            &gpu, size, rig.eyes,
        ));
        self.rig = rig;
        let safe = (0.0, 0.0, f64::from(size.0), f64::from(size.1));
        self.finish_screen(screen, format, gpu, size, scale.max(1.0), safe);
    }

    /// The renderer, the controls, the pump, the targets and the shell, over
    /// `screen`: everything a frame is drawn with, whatever it is drawn into.
    fn finish_screen(
        &mut self,
        screen: super::screen::Screen,
        format: tuile_wgpu::SurfaceFormat,
        gpu: GpuContext,
        size: (u32, u32),
        scale: f64,
        safe: (f64, f64, f64, f64),
    ) {
        let surface_format = format.render;
        // The loader has been running against the correctness floor since the
        // stream was built; this is the first moment anything knows what the
        // device will actually bind. On the WebGPU baseline it stays at the
        // floor, on Metal it opens up to the fetch-cost ceiling.
        tracing::info!(
            reported = gpu.device.limits().max_sampled_textures_per_shader_stage,
            "imagery layers a drape may bind"
        );
        let renderer = TileRenderer::new(&gpu, surface_format);
        // The viewer's pass carries depth, so the overlay must declare it too.
        let overlay = OverlayRenderer::new(&gpu, surface_format, Some(DEPTH_FORMAT));
        // A host's shapes in the world, drawn into the same frame with the
        // film's own overlay light: see `tuile_wgpu::WorldOverlayRenderer`.
        let world_overlay = tuile_wgpu::WorldOverlayRenderer::new(&gpu, surface_format);
        // The render origin tracks the eye (set each frame); start there.
        // The display's refresh, which each frame's upload deadline is made
        // of; sixty when the window cannot say.
        #[cfg_attr(not(feature = "control"), allow(irrefutable_let_patterns))]
        if let super::screen::Screen::Window(windowed) = &screen {
            if let Some(hz) = windowed
                .window
                .current_monitor()
                .and_then(|m| m.refresh_rate_millihertz())
            {
                self.refresh_hz = (hz / 1000).clamp(24, 240);
            }
        }
        let mut pump = ContentPump::new(self.controller.camera.position);
        // The floor drawn behind everything, each tile in its own drape.
        if crate::backdrop::shell_enabled() {
            pump.set_backdrop_level(Some(crate::backdrop::BASE_LEVEL));
        }
        // Every frame recorded with the session's budget, once there is one.
        if let Some(meter) = self.session.as_ref().and_then(|s| s.meter()) {
            pump.observe(meter);
        }
        let targets = tuile_wgpu::FrameTargets::for_surface(&gpu, format, size);
        // Built and uploaded once for the whole session.
        let shell = tuile_wgpu::prepare(&gpu, &crate::backdrop::shell_content(), DVec3::ZERO);

        let mut active = Active {
            screen,
            format,
            gpu,
            renderer,
            overlay,
            world_overlay,
            pump,
            targets,
            size,
            scale,
            safe,
            readback: tuile_wgpu::Readback::default(),
            shell,
        };
        configure_surface(&mut active);
        active.screen.request_redraw();
        self.active = Some(active);
        // The loader has been running against the correctness floor; this is
        // the first moment anything knows what the device will bind.
        self.fit_the_layer_budget();
        tracing::info!(slots = self.layer_budget.get(), "imagery layers per drape");
    }

    /// Lets go of the surface and shrinks the render targets to nothing: a
    /// phone's application in the background keeps neither.
    pub(super) fn release_the_surface(&mut self) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.screen.release();
        active.targets.resize(&active.gpu, (1, 1));
        tracing::info!("surface released");
    }

    /// Builds the surface again, before any frame is drawn into it.
    pub(super) fn restore_the_surface(&mut self) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if !active.screen.is_released() {
            return;
        }
        match active.screen.restore(&active.gpu, active.size) {
            Ok(()) => {
                active.targets.resize(&active.gpu, active.size);
                configure_surface(active);
                tracing::info!("surface restored");
                // Said again on the next turn, over the new surface.
                self.title_shown.clear();
            }
            Err(e) => tracing::error!("the surface could not be made again: {e}"),
        }
    }
}
/// The window's safe area in device pixels, relative to the drawable:
/// left, top, width, height.
pub(super) fn safe_area(window: &Window, size: (u32, u32)) -> (f64, f64, f64, f64) {
    let whole = (0.0, 0.0, f64::from(size.0), f64::from(size.1));
    // Android draws the window under its bars and says so only through the
    // activity's insets; winit's positions there are not supported.
    #[cfg(target_os = "android")]
    {
        let _ = window;
        crate::native::android_signals::safe_area_of_insets(
            crate::native::android::safe_insets(),
            size.0,
            size.1,
        )
        .unwrap_or(whole)
    }
    #[cfg(not(target_os = "android"))]
    {
        let (Ok(inner), Ok(outer)) = (window.inner_position(), window.outer_position()) else {
            return whole;
        };
        let inner_size = window.inner_size();
        let (x, y) = (
            f64::from(inner.x - outer.x).max(0.0),
            f64::from(inner.y - outer.y).max(0.0),
        );
        let (w, h) = (
            f64::from(inner_size.width).min(f64::from(size.0) - x),
            f64::from(inner_size.height).min(f64::from(size.1) - y),
        );
        if w <= 0.0 || h <= 0.0 {
            return whole;
        }
        (x, y, w, h)
    }
}

pub(super) fn configure_surface(active: &mut Active) {
    active
        .screen
        .configure(&active.gpu, active.format, active.size);
}

/// The same, by parts.
///
/// Taken field by field rather than as a whole `Active` so that a closure
/// needing only the surface can be handed to the backend while another closure
/// holds a mutable borrow of some *other* field — which is exactly what drawing
/// a traced frame does.
pub(super) fn configure(
    surface: &wgpu::Surface<'static>,
    gpu: &GpuContext,
    format: tuile_wgpu::SurfaceFormat,
    size: (u32, u32),
) {
    surface.configure(
        &gpu.device,
        &wgpu::SurfaceConfiguration {
            // COPY_SRC only when a trace is being written: it is free to ask
            // for on every backend that matters, and asking for capabilities a
            // session does not use is how a viewer stops working on someone
            // else's GPU.
            usage: if crate::recording::trace_path().is_some() || crate::recording::capturing() {
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC
            } else {
                wgpu::TextureUsages::RENDER_ATTACHMENT
            },
            format: format.surface,
            width: size.0,
            height: size.1,
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            // What every frame has been shown in so far: sRGB for the 8-bit
            // formats chosen here, on every backend. Wide gamut and HDR change
            // how a frame must be encoded, and are a choice of their own.
            color_space: wgpu::SurfaceColorSpace::Auto,
            // The sRGB view a browser's linear canvas is drawn through; empty
            // everywhere the surface is sRGB itself.
            view_formats: format.view_formats(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Android tries Vulkan, then OpenGL ES; a backend asked for is the only
    /// one tried; elsewhere the platform's default, once.
    #[test]
    fn android_falls_back_from_vulkan_to_gl() {
        assert_eq!(
            attempts(None, true),
            [Some(wgpu::Backends::VULKAN), Some(wgpu::Backends::GL)]
        );
        assert_eq!(
            attempts(Some(wgpu::Backends::GL), true),
            [Some(wgpu::Backends::GL)]
        );
        assert_eq!(attempts(None, false), [None]);
    }

    #[test]
    fn a_backend_is_named_or_refused() {
        assert_eq!(backend_named(None), Ok(None));
        assert_eq!(backend_named(Some(" ")), Ok(None));
        assert_eq!(backend_named(Some("GL")), Ok(Some(wgpu::Backends::GL)));
        assert_eq!(
            backend_named(Some("vulkan")),
            Ok(Some(wgpu::Backends::VULKAN))
        );
        assert!(backend_named(Some("glide"))
            .unwrap_err()
            .contains("vulkan, gl"));
    }
}
