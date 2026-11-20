// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

pub mod budget;
pub mod camera;
pub mod ground;
mod render;
mod session;
pub mod sources;
mod status;
#[cfg(target_arch = "wasm32")]
pub mod web;

use std::sync::Arc;

use bevy::prelude::*;
use bevy::transform::TransformSystems;

pub use budget::TuileBudget;
pub use camera::{
    local_frame, FloatingOrigin, GeoAnchor, PreloadView, PreloadViews, Priority, TuileCamera,
};
pub use ground::{GroundHeight, GroundHit, LonLat, TuileGround};
pub use render::BARE_GROUND;
pub use sources::{GlobeSources, ImagerySpec, Substitute, TerrainSpec, XyzTemplate};
pub use status::{SessionState, TuileLight, TuileStatus};
pub use tuile_camera::GlobeCamera;
pub use tuile_core::texels::ImageryCompression;
pub use tuile_telemetry::Config as TelemetryConfig;
pub use tuile_wgpu::compression_named;

/// The globe, in a Bevy application: see the crate.
#[derive(Clone)]
pub struct TuilePlugin {
    pub sources: GlobeSources,
    pub budget: TuileBudget,
    /// How imagery is held on the GPU: the host's choice, RGBA8 unless it
    /// asks. Resolved against the device; a format the device cannot sample
    /// falls back to RGBA8.
    pub compression: ImageryCompression,
    /// In a browser, the module worker script the decode workers are made
    /// from (`web` (wasm32 only)); `None` decodes on the page's thread. Ignored
    /// elsewhere, where decoding has threads of its own.
    pub decode_workers: Option<String>,
    /// Where the globe's performance numbers go — metrics over OTLP, and on
    /// a desktop CPU profiles — if anywhere: `None`, the default, sends
    /// nothing. See `tuile_telemetry`; `TelemetryConfig::from_vars` reads
    /// the standard variables.
    pub telemetry: Option<TelemetryConfig>,
}

impl TuilePlugin {
    /// The globe made of `sources`, on the platform's default budget, with
    /// imagery uncompressed.
    pub fn new(sources: GlobeSources) -> Self {
        Self {
            sources,
            budget: TuileBudget::default(),
            compression: ImageryCompression::Off,
            decode_workers: None,
            telemetry: None,
        }
    }

    /// Sends the globe's performance numbers where `config` says: nothing
    /// without an endpoint. `tuile_telemetry::set_metrics` and
    /// `set_profiling` switch them at run time.
    #[must_use]
    pub fn with_telemetry(mut self, config: TelemetryConfig) -> Self {
        self.telemetry = Some(config);
        self
    }

    /// In a browser, decode in workers made from the module worker script at
    /// `script`, relative to the page (`web` (wasm32 only)).
    #[must_use]
    pub fn with_decode_workers(mut self, script: impl Into<String>) -> Self {
        self.decode_workers = Some(script.into());
        self
    }

    #[must_use]
    pub fn with_budget(mut self, budget: TuileBudget) -> Self {
        self.budget = budget;
        self
    }

    #[must_use]
    pub fn with_compression(mut self, compression: ImageryCompression) -> Self {
        self.compression = compression;
        self
    }
}

impl Plugin for TuilePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FloatingOrigin>()
            .init_resource::<PreloadViews>()
            .init_resource::<TuileLight>()
            .init_resource::<TuileStatus>()
            .init_resource::<status::SharedStatus>()
            .add_systems(First, camera::clear_preload_views)
            .add_systems(First, status::publish_status)
            .add_systems(
                PostUpdate,
                camera::place_in_world.before(TransformSystems::Propagate),
            );
    }

    /// The session starts here, once the renderer has said what its adapter
    /// can sample.
    fn finish(&self, app: &mut App) {
        let caps = app
            .world()
            .get_resource::<bevy::render::renderer::RenderAdapter>()
            .map(|adapter| tuile_wgpu::texture_caps(adapter))
            .unwrap_or_default();
        let shared = app.world().resource::<status::SharedStatus>().clone();
        let ground = ground::SharedSurface::default();
        app.insert_resource(TuileGround {
            surface: ground.clone(),
        });
        let plan = session::Plan {
            sources: self.sources.clone(),
            budget: self.budget.clone(),
            compression: self.compression,
            caps,
            decode_workers: self.decode_workers.clone(),
        };
        let gpu = app
            .world()
            .get_resource::<bevy::render::renderer::RenderAdapter>()
            .map(|adapter| {
                let info = adapter.get_info();
                (info.name.clone(), info.backend.to_string())
            });
        let slot = start(app, plan, &shared, self.telemetry.as_ref());
        if self.telemetry.is_some() {
            if let Some((name, backend)) = gpu {
                tuile_telemetry::describe_gpu(&name, &backend);
            }
            tuile_telemetry::ready();
            app.add_systems(Last, note_the_camera);
        }
        render::add_to(
            app,
            render::GlobeRender::new(slot, self.compression, Arc::clone(&shared.0), ground),
        );
    }
}

/// Starts the session on a runtime of the plugin's own, kept as long as the
/// application.
#[cfg(not(target_arch = "wasm32"))]
fn start(
    app: &mut App,
    plan: session::Plan,
    shared: &status::SharedStatus,
    telemetry: Option<&TelemetryConfig>,
) -> session::SessionSlot {
    match session::Runtime::new(plan.budget.runtime_workers) {
        Ok(runtime) => {
            if let (Some(config), Some(handle)) = (telemetry, runtime.handle()) {
                let registry = tuile_telemetry::start_native(&handle, config, telemetry_identity());
                // The push's recorder alone — a Bevy application keeps no
                // endpoint of the globe's — and the engine's numbers copied
                // into it.
                if let Some(registry) = registry {
                    tuile_metrics::serve_with(&handle, false, Some(Box::new(registry)));
                }
            }
            let slot = runtime
                .handle()
                .map(|handle| session::launch(&handle, plan))
                .unwrap_or_default();
            app.insert_resource(RuntimeKeeper(Arc::new(runtime)));
            slot
        }
        Err(why) => {
            if let Ok(mut status) = shared.0.lock() {
                status.session = SessionState::Failed(why);
            }
            session::SessionSlot::default()
        }
    }
}

/// Starts the session as a future on the page's thread.
#[cfg(target_arch = "wasm32")]
fn start(
    _app: &mut App,
    plan: session::Plan,
    _shared: &status::SharedStatus,
    telemetry: Option<&TelemetryConfig>,
) -> session::SessionSlot {
    if let Some(config) = telemetry {
        tuile_telemetry::start_web(config, telemetry_identity());
    }
    if let Some(script) = &plan.decode_workers {
        web::start_decode_workers(script, None);
    }
    session::launch(plan)
}

/// The camera's pose for the telemetry's next push, once a second: the
/// host's [`TuileCamera`], its height over the ground drawn under it. The
/// camera's alone — never the device's location.
fn note_the_camera(
    time: Res<Time>,
    mut last: Local<Option<f64>>,
    cameras: Query<&TuileCamera>,
    ground: Option<Res<TuileGround>>,
) {
    let now = time.elapsed_secs_f64();
    if last.is_some_and(|t| now - t < 1.0) {
        return;
    }
    if !(tuile_telemetry::offered().metrics && tuile_telemetry::metrics_on()) {
        return;
    }
    let Some(camera) = cameras.iter().next() else {
        return;
    };
    *last = Some(now);
    let g = camera.pose.geodetic();
    let under = ground
        .map(|ground| {
            ground.ground_heights(&[LonLat::new(g.lon.to_degrees(), g.lat.to_degrees())])[0].height
        })
        .unwrap_or(0.0);
    tuile_telemetry::set_camera(tuile_telemetry::CameraPose::of(
        &camera.pose,
        g.height - under,
    ));
}

/// Who sends: the plugin, on this kind of machine.
fn telemetry_identity() -> tuile_telemetry::Identity<'static> {
    tuile_telemetry::Identity {
        service_name: "tuile-bevy",
        version: env!("CARGO_PKG_VERSION"),
        commit: tuile_telemetry::build::REVISION,
        ref_name: tuile_telemetry::build::REF_NAME,
        device_class: if cfg!(target_arch = "wasm32") {
            "browser"
        } else if cfg!(any(target_os = "ios", target_os = "android")) {
            "handheld"
        } else {
            "desktop"
        },
        realm: if cfg!(target_arch = "wasm32") {
            "page"
        } else {
            "native"
        },
    }
}

/// Keeps the plugin's runtime alive for the application's life.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Resource, Clone)]
struct RuntimeKeeper(#[allow(dead_code)] Arc<session::Runtime>);
