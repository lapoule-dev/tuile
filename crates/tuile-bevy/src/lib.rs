// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

pub mod atmosphere;
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

pub use atmosphere::{Lights, TuileAtmosphere};
pub use budget::TuileBudget;
pub use camera::{
    local_frame, FloatingOrigin, GeoAnchor, PreloadView, PreloadViews, Priority, TuileCamera,
};
pub use ground::{GroundHeight, GroundHit, LonLat, TuileGround};
pub use render::BARE_GROUND;
pub use sources::{GlobeSources, ImagerySpec, TerrainSpec, XyzTemplate};
pub use status::{SessionState, TuileLight, TuileStatus};
pub use tuile_camera::GlobeCamera;
pub use tuile_core::texels::ImageryCompression;
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
        }
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
            .init_resource::<TuileAtmosphere>()
            .init_resource::<TuileStatus>()
            .init_resource::<status::SharedStatus>()
            .add_systems(First, camera::clear_preload_views)
            .add_systems(First, status::publish_status)
            .add_systems(
                PostUpdate,
                (camera::place_in_world, atmosphere::advance_atmosphere)
                    .chain()
                    .before(TransformSystems::Propagate),
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
        let slot = start(app, plan, &shared);
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
) -> session::SessionSlot {
    match session::Runtime::new(plan.budget.runtime_workers) {
        Ok(runtime) => {
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
) -> session::SessionSlot {
    if let Some(script) = &plan.decode_workers {
        web::start_decode_workers(script, None);
    }
    session::launch(plan)
}

/// Keeps the plugin's runtime alive for the application's life.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Resource, Clone)]
struct RuntimeKeeper(#[allow(dead_code)] Arc<session::Runtime>);
