// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The globe in Bevy's render world.
//!
//! Nothing of the ground is re-implemented here. tuile-wgpu's
//! [`ContentPump`] — the graphics budget's executor, the one place the
//! anti-black-ground rules live — runs on Bevy's device and queue, and its
//! [`TileRenderer`] draws through a [`HostPass`] into the camera's own
//! colour and depth, from a system in Bevy's 3D schedule:
//!
//! 1. **extract** — the [`TuileCamera`](crate::TuileCamera)'s `f64` pose,
//!    its viewport, the floating origin, the preload views;
//! 2. **prepare** (`RenderSystems::PrepareResources`) — the pump's frame: the
//!    camera sent, this frame's share of the uploads taken, the render origin
//!    moved to the eye;
//! 3. **draw** (in `Core3d`, after the prepass and before the main pass) —
//!    the draw set resolved and painted: backdrop shell, coarse level,
//!    exact tiles, fallback ancestors, depth-tested against what the host
//!    has drawn and written back into the host's depth.
//!
//! The ground is drawn **first** by default: the host's own meshes, in its
//! own pipelines, are then depth-tested against it by Bevy's main pass. What
//! a host draws before it — a depth prepass — is respected too: tuile's pass
//! imports the host's depth before it draws.
//!
//! # Never a black frame
//!
//! From the very first frame, before the session is up, the backdrop shell
//! covers the whole planet; the session's floor and its fallbacks take over
//! as they arrive (`tuile_wgpu::Drawn`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bevy::core_pipeline::schedule::Core3d;
use bevy::core_pipeline::Core3dSystems;
use bevy::prelude::*;
use bevy::render::camera::ExtractedCamera;
use bevy::render::renderer::{RenderAdapter, RenderContext, RenderDevice, RenderQueue, ViewQuery};
use bevy::render::sync_world::RenderEntity;
use bevy::render::view::{ExtractedView, ViewDepthStencilTexture, ViewTarget};
use bevy::render::{Extract, ExtractSchedule, Render, RenderApp, RenderSystems};
use glam::{DMat4, DVec2, DVec3};
use tuile_camera::GlobeCamera;
use tuile_core::texels::ImageryCompression;
use tuile_wgpu::{
    view_proj_from_world, ContentPump, DepthDirection, GpuContext, HostFrame, HostPass,
    PreparedTile, TargetLayout, TileRenderer, ViewUniform,
};

use crate::atmosphere::TuileAtmosphere;
use crate::budget::BACKDROP_LEVEL;
use crate::camera::{FloatingOrigin, PreloadViews, TuileCamera};
use crate::ground::SharedSurface;
use crate::session::{SessionSlot, Started};
use crate::status::{PumpFacts, SessionState, TuileLight, TuileStatus};
use tuile_core::surface::Surface;

/// The colour bare ground is painted: a deep, desaturated ocean blue, the
/// colour most of the planet is — and never black.
pub const BARE_GROUND: [f32; 4] = [0.09, 0.15, 0.24, 1.0];

/// The display refresh a frame's upload deadline is made of.
const REFRESH_HZ: u32 = 60;

/// One globe camera, as the render world sees it.
#[derive(Component, Debug, Clone, Copy)]
pub(crate) struct GlobeView {
    pose: GlobeCamera,
    /// Physical pixels.
    viewport: (u32, u32),
    /// Physical pixels per logical one.
    scale: f64,
}

/// What the main world says about this frame.
#[derive(Resource, Debug, Clone, Default)]
pub(crate) struct GlobeFrame {
    origin: FloatingOrigin,
    preload: PreloadViews,
    light: TuileLight,
    /// The atmosphere as it runs this frame and the wall instant it is drawn
    /// at, or `None` before the main world has started it.
    sky: Option<(tuile_wgpu::Atmosphere, f64)>,
    /// The air the ground is seen through, with the atmosphere on.
    aerial: tuile_atmosphere::AerialAir,
}

/// The session, as far as the render world has it.
enum Session {
    Starting(SessionSlot),
    Running(Box<Started>),
    Failed,
}

/// Everything the globe is drawn with, behind a lock: a tile remembers the
/// origin it was last rebased onto in a cell, so the whole is `Send` but not
/// `Sync`, and the render world's systems take it in turn.
#[derive(Resource)]
pub(crate) struct GlobeRender(Mutex<Globe>);

struct Globe {
    gpu: Option<GpuContext>,
    pump: ContentPump,
    /// The whole planet, flat and blue, behind everything.
    shell: Option<PreparedTile>,
    /// One renderer and one pass per target shape: a camera rendering in
    /// HDR and one in sRGB draw through pipelines of their own.
    targets: HashMap<TargetLayout, (TileRenderer, HostPass)>,
    session: Session,
    compression: ImageryCompression,
    status: Arc<Mutex<TuileStatus>>,
    /// The drawn ground, as the main world reads it ([`crate::TuileGround`]),
    /// and the snapshot last handed over.
    ground: SharedSurface,
    surface: Surface,
}

impl GlobeRender {
    pub(crate) fn new(
        slot: SessionSlot,
        compression: ImageryCompression,
        status: Arc<Mutex<TuileStatus>>,
        ground: SharedSurface,
    ) -> Self {
        let mut pump = ContentPump::new(DVec3::ZERO);
        pump.set_backdrop_level(Some(BACKDROP_LEVEL));
        // The decoded meshes, shared with the server's own tier, for the
        // ground the host reads on the CPU.
        pump.keep_contents(true);
        Self(Mutex::new(Globe {
            gpu: None,
            pump,
            shell: None,
            targets: HashMap::new(),
            session: Session::Starting(slot),
            compression,
            status,
            ground,
            surface: Surface::default(),
        }))
    }
}

pub(crate) fn add_to(app: &mut App, globe: GlobeRender) {
    let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
        return;
    };
    render_app
        .insert_resource(globe)
        .init_resource::<GlobeFrame>()
        .add_systems(ExtractSchedule, extract_globe)
        .add_systems(
            Render,
            advance_globe.in_set(RenderSystems::PrepareResources),
        )
        .add_systems(
            Core3d,
            draw_globe
                .after(Core3dSystems::Prepass)
                .before(Core3dSystems::MainPass),
        );
}

fn extract_globe(
    mut commands: Commands,
    cameras: Extract<Query<(RenderEntity, &TuileCamera, &Camera)>>,
    origin: Extract<Res<FloatingOrigin>>,
    preload: Extract<Res<PreloadViews>>,
    light: Extract<Res<TuileLight>>,
    atmosphere: Extract<Res<TuileAtmosphere>>,
    mut frame: ResMut<GlobeFrame>,
) {
    for (entity, camera, bevy_camera) in &cameras {
        let Some(size) = bevy_camera.physical_viewport_size() else {
            continue;
        };
        commands.entity(entity).insert(GlobeView {
            pose: camera.pose,
            viewport: (size.x, size.y),
            scale: f64::from(bevy_camera.target_scaling_factor().unwrap_or(1.0)),
        });
    }
    frame.origin = **origin;
    frame.preload.clone_from(&preload);
    frame.light = **light;
    frame.sky = atmosphere.running();
    frame.aerial = atmosphere.tuning.viewer;
}

/// The pump's frame, before anything is drawn: the GPU context on the first
/// one, the session once it is up, the camera sent and the uploads taken.
fn advance_globe(
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    adapter: Res<RenderAdapter>,
    globe: Res<GlobeRender>,
    frame: Res<GlobeFrame>,
    views: Query<&GlobeView>,
) {
    let Ok(mut globe) = globe.0.lock() else {
        return;
    };
    let globe = &mut *globe;
    let gpu = globe.gpu.get_or_insert_with(|| {
        let (plan, _) = tuile_wgpu::plan_for(globe.compression, &adapter);
        GpuContext::new(device.wgpu_device().clone(), (**queue).clone()).with_imagery_plan(plan)
    });
    let shell = globe.shell.get_or_insert_with(|| {
        tuile_wgpu::prepare(gpu, &tuile_terrain::globe_shell(BARE_GROUND), DVec3::ZERO)
    });
    if let Session::Starting(slot) = &globe.session {
        match slot.take() {
            Some(Ok(started)) => {
                // The device is known: the loader is told how many imagery
                // layers one draw binds, and how the device holds imagery.
                started
                    .layer_budget
                    .set_from_device(gpu.device.limits().max_sampled_textures_per_shader_stage);
                started.plan.set(gpu.imagery_plan());
                globe.session = Session::Running(Box::new(started));
                set_state(&globe.status, SessionState::Running);
            }
            Some(Err(why)) => {
                globe.session = Session::Failed;
                set_state(&globe.status, SessionState::Failed(why));
            }
            None => {}
        }
    }
    let Some(eye) = views.iter().next() else {
        return;
    };
    let origin = eye.pose.position;
    shell.rebase(&gpu.queue, origin);
    let mut wanted: Vec<_> = views
        .iter()
        .map(|v| {
            v.pose
                .view_state(DVec2::new(f64::from(v.viewport.0), f64::from(v.viewport.1)))
                .with_scale(v.scale)
        })
        .collect();
    // Traversed, never drawn: their tiles are asked for now.
    let height = f64::from(eye.viewport.1.max(1));
    wanted.extend(frame.preload.0.iter().map(|p| {
        p.pose
            .view_state(DVec2::new(height * p.aspect.max(1e-6), height))
            .with_scale(eye.scale)
    }));
    match &mut globe.session {
        Session::Running(started) => {
            let deadline = globe.pump.deadline(REFRESH_HZ);
            globe
                .pump
                .advance(&mut started.stream, gpu, &wanted, origin, deadline);
        }
        Session::Starting(_) | Session::Failed => globe.pump.rebase(&gpu.queue, origin),
    }
}

fn set_state(status: &Mutex<TuileStatus>, state: SessionState) {
    if let Ok(mut status) = status.lock() {
        status.session = state;
    }
}

/// The globe's pass, in the camera's colour and depth.
fn draw_globe(
    view: ViewQuery<(
        &ExtractedView,
        &ExtractedCamera,
        &ViewTarget,
        &ViewDepthStencilTexture,
        Option<&GlobeView>,
    )>,
    globe: Res<GlobeRender>,
    frame: Res<GlobeFrame>,
    mut ctx: RenderContext,
) {
    let (extracted, camera, target, depth, globe_view) = view.into_inner();
    if globe_view.is_none() {
        return;
    }
    let Ok(mut globe) = globe.0.lock() else {
        return;
    };
    let Globe {
        gpu,
        pump,
        shell,
        targets,
        status,
        ground,
        surface,
        ..
    } = &mut *globe;
    let (Some(gpu), Some(shell)) = (gpu.as_ref(), shell.as_ref()) else {
        return;
    };
    let depth_texture = depth.texture();
    let layout = TargetLayout {
        colour: target.main_texture_format(),
        depth: depth_texture.format(),
        samples: depth_texture.sample_count(),
        direction: DepthDirection::Reverse,
    };
    let (renderer, pass) = targets.entry(layout).or_insert_with(|| {
        (
            TileRenderer::for_layout(gpu, layout),
            HostPass::new(gpu, layout),
        )
    });

    // The host's camera, exactly as Bevy draws its own meshes with it, moved
    // from Bevy's world (ECEF less the floating origin) onto the pump's
    // render origin in f64 before anything is cast.
    let view_from_world = extracted.world_from_view.to_matrix().inverse();
    let clip_from_ecef = extracted.clip_from_view.as_dmat4()
        * view_from_world.as_dmat4()
        * DMat4::from_translation(-frame.origin.ecef);
    let view_proj = view_proj_from_world(clip_from_ecef, pump.render_origin);
    let ambient = [frame.light.ambient, 0.0, 0.0, 0.0];
    match (frame.sky, globe_view) {
        // The host's atmosphere: the sun where its lights put it, the air
        // before the ground, the sky behind it — tuile-wgpu's, as the viewer
        // and the film draw it.
        (Some((atmosphere, wall)), Some(view)) if atmosphere.enabled => {
            let eye = view.pose.position;
            let sun = tuile_atmosphere::Sun::from_direction(
                atmosphere.lights.at(atmosphere.instant(wall), eye).to_sun,
            );
            let travel = sun.light_travel_direction().as_vec3();
            renderer.set_view(
                &gpu.queue,
                &ViewUniform {
                    view_proj,
                    sun_dir: [travel.x, travel.y, travel.z, 0.0],
                    params: ambient,
                    atmosphere: tuile_atmosphere::AerialPerspective::of_air(
                        eye,
                        pump.render_origin,
                        &sun,
                        &frame.aerial,
                        1.0,
                    ),
                },
            );
            let place = tuile_core::geo::ecef_to_geodetic(eye);
            let (width, height) = view.viewport;
            renderer.draw_atmosphere(
                gpu,
                &atmosphere,
                &tuile_wgpu::SkyView {
                    eye,
                    render_origin: pump.render_origin,
                    forward: view.pose.direction,
                    up: view.pose.up,
                    fov_y: view.pose.fovy,
                    aspect: f64::from(width.max(1)) / f64::from(height.max(1)),
                    height_px: height,
                    ground_height: surface.height_at(place.lon, place.lat).height,
                },
                wall,
            );
        }
        // The flat light: no sky, the clear colour past the ground.
        (sky, _) => {
            let light = frame.light.travel(pump.render_origin);
            renderer.set_view(
                &gpu.queue,
                &ViewUniform {
                    view_proj,
                    sun_dir: [light.x as f32, light.y as f32, light.z as f32, 0.0],
                    params: ambient,
                    atmosphere: Default::default(),
                },
            );
            if sky.is_some() {
                renderer.set_sky(gpu, None);
            }
        }
    }

    let facts = PumpFacts::of(pump);
    let (drawn, resolution) = pump.resolve(gpu);
    if let Ok(mut status) = status.lock() {
        status.record(&resolution, facts);
    }
    let size = (depth_texture.width(), depth_texture.height());
    let viewport = camera.viewport.clone();
    let encoder = ctx.command_encoder();
    // Bevy clears the camera's attachments on their first use in a frame;
    // the globe is the first, so it asks for them as Bevy's own passes do and
    // opens them once — cleared as the camera says, or kept.
    let colour = target.get_color_attachment();
    let host_depth = depth.get_attachment(wgpu::StoreOp::Store);
    let clears = matches!(colour.ops.load, wgpu::LoadOp::Clear(_))
        || host_depth
            .depth_ops
            .is_some_and(|ops| matches!(ops.load, wgpu::LoadOp::Clear(_)));
    if clears {
        let _ = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("tuile host clear"),
            color_attachments: &[Some(colour.clone())],
            depth_stencil_attachment: Some(host_depth.clone()),
            ..Default::default()
        });
    }
    let renderer = &*renderer;
    pass.encode(
        gpu,
        encoder,
        HostFrame {
            colour: colour.view,
            resolve: colour.resolve_target,
            depth: host_depth.view,
            size,
            clear: None,
        },
        |pass| {
            if let Some(viewport) = &viewport {
                pass.set_viewport(
                    viewport.physical_position.x as f32,
                    viewport.physical_position.y as f32,
                    viewport.physical_size.x as f32,
                    viewport.physical_size.y as f32,
                    viewport.depth.start,
                    viewport.depth.end,
                );
            }
            renderer.paint(
                pass,
                std::iter::once(shell).chain(drawn.backdrop.iter().copied()),
                drawn.exact.iter().copied(),
                drawn.fallback.iter().copied(),
                None,
                false,
            );
        },
    );
    // What was just drawn, for the host's ground queries: a new snapshot
    // only when it changed.
    let drawn: std::collections::HashSet<u64> = pump.drawn_tiles().iter().map(|t| t.0).collect();
    let next = surface.with(
        pump.resident_contents().map(|(t, level, content)| {
            // The globe's tiles are terrain tiles: their extent is their
            // grid rectangle, and nothing has to be read off the vertices.
            let (z, x, y) = t.terrain_coord();
            let r = tuile_terrain::GeographicTilingScheme::default()
                .tile_rect(tuile_terrain::TileCoord::new(z, x, y));
            tuile_core::surface::Resident {
                key: t.0,
                level,
                content,
                rect: Some([r.west, r.south, r.east, r.north]),
            }
        }),
        &drawn,
    );
    if next.version() != surface.version() {
        ground.publish(next.clone());
        *surface = next;
    }
}
