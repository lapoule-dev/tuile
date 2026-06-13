// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The winit application: owns the window, surface and render loop, drives
//! the geometry stream through the [`ContentPump`].

use crate::camera::OrbitCamera;
use glam::{DVec2, DVec3, Vec3};
use std::sync::Arc;
use tuile_core::protocol::{ClientMessage, GeometryStream, InProcessStream};
use tuile_wgpu::{ContentPump, GpuContext, TileRenderer, DEPTH_FORMAT};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

/// Everything known before the window exists.
pub struct ViewerConfig {
    pub stream: InProcessStream,
    pub camera: OrbitCamera,
    pub render_origin: DVec3,
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
    camera: OrbitCamera,
    render_origin: DVec3,
    title: String,
    active: Option<Active>,
    // Input state.
    cursor: Option<(f64, f64)>,
    orbiting: bool,
    panning: bool,
    wireframe: bool,
    freeze: bool,
    last_log: std::time::Instant,
}

impl App {
    pub fn new(config: ViewerConfig) -> Self {
        let camera = OrbitCamera::new(config.camera.target, config.camera.distance);
        Self {
            render_origin: config.render_origin,
            title: config.title.clone(),
            camera,
            config: Some(config),
            active: None,
            cursor: None,
            orbiting: false,
            panning: false,
            wireframe: false,
            freeze: false,
            last_log: std::time::Instant::now(),
        }
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

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
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
        let pump = ContentPump::new(self.render_origin);
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
        let Some(active) = self.active.as_mut() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(new) => {
                active.size = (new.width.max(1), new.height.max(1));
                configure_surface(active);
                active.depth = make_depth(&active.gpu, active.size);
                active.window.request_redraw();
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
                    MouseButton::Left => self.orbiting = pressed,
                    MouseButton::Right => self.panning = pressed,
                    _ => {}
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let pos = (position.x, position.y);
                if let Some((px, py)) = self.cursor {
                    let (dx, dy) = (pos.0 - px, pos.1 - py);
                    if self.orbiting {
                        self.camera.orbit(dx * 0.005, -dy * 0.005);
                    } else if self.panning {
                        self.camera.pan(dx, dy);
                    }
                }
                self.cursor = Some(pos);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let amount = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y / 50.0,
                };
                self.camera.zoom(amount);
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
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let viewport = DVec2::new(active.size.0 as f64, active.size.1 as f64);

        // Feed the camera to the geometry server unless frozen, then pump
        // GPU uploads (budget per frame to avoid hitches).
        if !self.freeze {
            let _ = active.stream_send(ClientMessage::ViewerState {
                views: vec![self.camera.view_state(viewport)],
            });
        }
        active.pump.pump(&mut active.streamed(), &active.gpu, 8);

        let aspect = active.size.0 as f32 / active.size.1 as f32;
        let sun = Vec3::new(-0.4, -0.8, -0.45).normalize();
        let view = self.camera.view_uniform(self.render_origin, aspect, sun);
        active.renderer.set_view(&active.gpu.queue, &view);

        let frame = match active.surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                configure_surface(active);
                return;
            }
            Err(e) => {
                eprintln!("surface error: {e:?}");
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
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.45,
                            g: 0.62,
                            b: 0.82,
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
                .render(&mut pass, active.pump.visible(), self.wireframe);
        }
        active.gpu.queue.submit([encoder.finish()]);
        frame.present();

        if self.last_log.elapsed().as_secs_f32() > 1.0 {
            self.last_log = std::time::Instant::now();
            eprintln!(
                "selected {} | prepared {} | pending {} | missing {} | {:.1} MiB GPU",
                active.pump.selection.len(),
                active.pump.prepared_count(),
                active.pump.pending_uploads(),
                active.pump.missing(),
                active.pump.gpu_bytes as f32 / (1024.0 * 1024.0),
            );
            for err in active.pump.errors.drain(..) {
                eprintln!("server: {err}");
            }
        }
    }
}

impl Active {
    fn stream_send(&self, msg: ClientMessage) -> Result<(), tuile_core::protocol::StreamError> {
        self.stream.send(msg)
    }
    fn streamed(&mut self) -> &mut InProcessStream {
        &mut self.stream
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
