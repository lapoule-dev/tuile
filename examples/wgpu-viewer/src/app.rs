// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The winit application: owns the window, surface and render loop, feeds
//! pixel gestures to the render-agnostic [`tuile_camera::CameraController`] and
//! drives the geometry stream through the [`ContentPump`].

use glam::DVec2;
use std::sync::Arc;
use tuile_camera::CameraController;
use tuile_core::protocol::{ClientMessage, GeometryStream, InProcessStream};
use tuile_core::source::TileId;
use tuile_wgpu::{ContentPump, GpuContext, TileRenderer, DEPTH_FORMAT};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

/// Everything known before the window exists.
pub struct ViewerConfig {
    pub stream: InProcessStream,
    pub controller: CameraController,
    pub title: String,
}

struct Active {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    surface_format: wgpu::TextureFormat,
    gpu: GpuContext,
    renderer: TileRenderer,
    pump: ContentPump,
    stream: InProcessStream,
    depth: wgpu::TextureView,
    size: (u32, u32),
}

pub struct App {
    config: Option<ViewerConfig>,
    controller: CameraController,
    title: String,
    active: Option<Active>,
    // Input state.
    cursor: (f64, f64),
    dragging: bool,
    tilting: bool,
    wireframe: bool,
    freeze: bool,
    last_log: std::time::Instant,
}

impl App {
    pub fn new(config: ViewerConfig) -> Self {
        Self {
            controller: config.controller.clone(),
            title: config.title.clone(),
            config: Some(config),
            active: None,
            cursor: (0.0, 0.0),
            dragging: false,
            tilting: false,
            wireframe: false,
            freeze: false,
            last_log: std::time::Instant::now(),
        }
    }

    fn viewport(&self) -> (f64, f64) {
        self.active
            .as_ref()
            .map_or((1920.0, 1080.0), |a| (a.size.0 as f64, a.size.1 as f64))
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.active.is_some() {
            return;
        }
        let config = self.config.take().expect("config consumed once");
        let attrs = Window::default_attributes().with_title(self.title.clone());
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        let size = window.inner_size();
        let size = (size.width.max(1), size.height.max(1));

        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(window.clone())
            .expect("create surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .expect("adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("viewer"),
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .expect("device");

        let caps = surface.get_capabilities(&adapter);
        let surface_format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let gpu = GpuContext::new(device, queue);
        let renderer = TileRenderer::new(&gpu, surface_format);
        // The render origin tracks the eye (set each frame); start there.
        let pump = ContentPump::new(self.controller.camera.position);
        let depth = make_depth(&gpu, size);

        let active = Active {
            window,
            surface,
            surface_format,
            gpu,
            renderer,
            pump,
            stream: config.stream,
            depth,
            size,
        };
        configure_surface(&active);
        active.window.request_redraw();
        self.active = Some(active);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        if self.active.is_none() {
            return;
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(new) => {
                if let Some(active) = self.active.as_mut() {
                    active.size = (new.width.max(1), new.height.max(1));
                    configure_surface(active);
                    active.depth = make_depth(&active.gpu, active.size);
                    active.window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                match event.logical_key {
                    Key::Character(ref c) if c.eq_ignore_ascii_case("w") => {
                        self.wireframe = !self.wireframe;
                    }
                    Key::Character(ref c) if c.eq_ignore_ascii_case("f") => {
                        self.freeze = !self.freeze;
                        eprintln!("traversal freeze: {}", self.freeze);
                    }
                    Key::Named(NamedKey::Escape) => event_loop.exit(),
                    _ => {}
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = state == ElementState::Pressed;
                match button {
                    MouseButton::Left => self.dragging = pressed,
                    MouseButton::Right => self.tilting = pressed,
                    _ => {}
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let vp = self.viewport();
                let prev = self.cursor;
                let cur = (position.x, position.y);
                if self.dragging {
                    // Drag the globe: the grabbed point follows the cursor.
                    self.controller.drag(prev, cur, vp);
                } else if self.tilting {
                    // Right-drag: vertical = tilt, horizontal = heading.
                    self.controller.tilt((cur.1 - prev.1) * 0.005, vp);
                    self.controller.rotate_heading((cur.0 - prev.0) * 0.005, vp);
                }
                self.cursor = cur;
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let amount = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y / 50.0,
                };
                self.controller.zoom(amount, self.cursor, self.viewport());
            }
            WindowEvent::RedrawRequested => {
                self.render();
                if let Some(active) = self.active.as_ref() {
                    active.window.request_redraw();
                }
            }
            _ => {}
        }
    }
}

impl App {
    fn render(&mut self) {
        // Ease the camera toward the gesture target (smooth motion).
        self.controller.update(0.3);
        let cam = self.controller.camera;
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let viewport = DVec2::new(active.size.0 as f64, active.size.1 as f64);

        if !self.freeze {
            let _ = active.stream.send(ClientMessage::ViewerState {
                views: vec![cam.view_state(viewport)],
            });
        }
        active.pump.pump(&mut active.stream, &active.gpu, 8);
        // Anti-jitter: render origin = eye, so the f32 the GPU sees is small.
        let origin = cam.position;
        active.pump.rebase(&active.gpu.queue, origin);

        let aspect = active.size.0 as f32 / active.size.1 as f32;
        // Headlight: light travels along the view direction (behind the camera).
        let sun = cam.direction.as_vec3();
        active.renderer.set_view(
            &active.gpu.queue,
            &tuile_wgpu::ViewUniform {
                view_proj: cam.view_proj(origin, aspect),
                sun_dir: [sun.x, sun.y, sun.z, 0.0],
                params: [0.5, 0.0, 0.0, 0.0],
            },
        );

        // For any selected terrain tile not yet uploaded, fall back to its
        // nearest ready ancestor so refinement never flashes the background.
        let tiles = active.pump.visible_resolved(|id| {
            let (z, x, y) = id.terrain_coord();
            (z > 0).then(|| TileId::from_terrain(z - 1, x / 2, y / 2))
        });
        let rendered = tiles.len();

        let frame = match active.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => {
                f
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                configure_surface(active);
                return;
            }
            wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Timeout => return,
            wgpu::CurrentSurfaceTexture::Validation => {
                eprintln!("surface validation error");
                return;
            }
        };
        let view_tex = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = active
            .gpu
            .device
            .create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("viewer"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view_tex,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        // Space black.
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.0,
                            g: 0.0,
                            b: 0.0,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &active.depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                ..Default::default()
            });
            active
                .renderer
                .render(&mut pass, tiles.iter().copied(), self.wireframe);
        }
        active.gpu.queue.submit([encoder.finish()]);
        frame.present();

        if self.last_log.elapsed().as_secs_f32() > 1.0 {
            self.last_log = std::time::Instant::now();
            let s = &active.pump.stats;
            eprintln!(
                "alt {:.0} km | selected {} | rendered {} | prepared {} | missing {} | visited {} culled {} | {:.0} MiB GPU",
                cam.altitude() / 1000.0,
                active.pump.selection.len(),
                rendered,
                active.pump.prepared_count(),
                active.pump.missing(),
                s.visited,
                s.culled,
                active.pump.gpu_bytes as f32 / (1024.0 * 1024.0),
            );
            for err in active.pump.errors.drain(..) {
                eprintln!("server: {err}");
            }
        }
    }
}

fn configure_surface(active: &Active) {
    active.surface.configure(
        &active.gpu.device,
        &wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: active.surface_format,
            width: active.size.0,
            height: active.size.1,
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
        },
    );
}

fn make_depth(gpu: &GpuContext, size: (u32, u32)) -> wgpu::TextureView {
    gpu.device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("viewer depth"),
            size: wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&Default::default())
}
