// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A synthetic globe, offline, and a headless Bevy application that draws it
//! into an image of its own.

#![allow(dead_code)]

use std::io::Cursor;
use std::sync::{Arc, Mutex, OnceLock};

use bevy::app::SubApps;
use bevy::asset::RenderAssetUsages;
use bevy::camera::{Hdr, RenderTarget};
use bevy::prelude::*;
use bevy::render::render_resource::{PollType, TextureFormat};
use bevy::render::renderer::RenderDevice;
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured};
use bevy::render::view::{Msaa, Tonemapping};
use bevy::render::RenderPlugin;
use bevy::window::ExitCondition;
use tuile_bevy::{GlobeSources, ImagerySpec, TerrainSpec, TuileBudget, TuilePlugin, TuileStatus};
use tuile_core::fetch::Fetched;
use tuile_core::raster::{ImageryCoord, ImageryProvider, RasterError, TilingScheme};
use tuile_terrain::{GeoRect, HeightfieldSource, Heights};

/// Where the synthetic map is: on the equator, off any level-0 edge.
pub const LON: f64 = 10.0;
pub const LAT: f64 = 0.0;

/// A smooth hill, 80 m high, a kilometre across, on a 6 km square of flat
/// ground; the ellipsoid around it.
pub struct Hill;

impl Heights for Hill {
    fn height(&self, lon_deg: f64, lat_deg: f64) -> f64 {
        let east = (lon_deg - LON - 0.004) * 111_319.5;
        let north = (lat_deg - LAT - 0.002) * 110_574.3;
        80.0 * (-(east * east + north * north) / (2.0 * 500.0 * 500.0)).exp()
    }

    fn footprint(&self) -> GeoRect {
        GeoRect::from_degrees(LON - 0.03, LAT - 0.03, LON + 0.03, LAT + 0.03)
    }
}

/// The colours the imagery's tiles alternate between, by `x + y` parity:
/// two greens, neither near black, neither near the host's red or blue.
pub const GREENS: [[u8; 3]; 2] = [[70, 150, 60], [110, 180, 90]];

/// Imagery on the geographic grid, every tile one flat green.
pub struct Greens;

fn png(colour: [u8; 3]) -> bytes::Bytes {
    let image = image::RgbaImage::from_pixel(
        256,
        256,
        image::Rgba([colour[0], colour[1], colour[2], 255]),
    );
    let mut out = Cursor::new(Vec::new());
    image
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("png");
    bytes::Bytes::from(out.into_inner())
}

#[async_trait::async_trait]
impl ImageryProvider for Greens {
    fn tiling_scheme(&self) -> TilingScheme {
        let mut scheme = TilingScheme::geographic();
        scheme.maximum_level = 15;
        scheme
    }

    async fn fetch_tile_bytes(
        &self,
        coord: ImageryCoord,
    ) -> Result<Fetched<bytes::Bytes>, RasterError> {
        static TILES: OnceLock<[bytes::Bytes; 2]> = OnceLock::new();
        let tiles = TILES.get_or_init(|| [png(GREENS[0]), png(GREENS[1])]);
        Ok(Fetched::undated(
            tiles[((coord.x + coord.y) % 2) as usize].clone(),
        ))
    }
}

/// The hill under the greens.
pub fn sources() -> GlobeSources {
    let terrain = HeightfieldSource::new(Hill, 14);
    GlobeSources {
        terrain: TerrainSpec::Source {
            layer: terrain.layer(),
            source: Arc::new(terrain),
        },
        imagery: ImagerySpec::Provider(Arc::new(Greens)),
        substitute: None,
    }
}

/// Whether this machine has a GPU at all: CI without one skips.
pub fn has_gpu() -> bool {
    pollster::block_on(tuile_wgpu::GpuContext::headless()).is_ok()
}

/// How the host's camera renders.
#[derive(Debug, Clone, Copy)]
pub struct Target {
    pub msaa: Msaa,
    pub hdr: bool,
    pub size: u32,
}

/// A Bevy application with no window, its update loop pumped by hand, its
/// one camera drawing into an image.
pub struct Headless {
    apps: SubApps,
    pub target: Handle<Image>,
    pub camera: Entity,
}

impl Headless {
    pub fn new(plugin: TuilePlugin, camera: tuile_bevy::TuileCamera, target: Target) -> Self {
        let mut app = App::new();
        app.add_plugins(
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: None,
                    exit_condition: ExitCondition::DontExit,
                    ..default()
                })
                .set(RenderPlugin {
                    synchronous_pipeline_compilation: true,
                    ..default()
                })
                .disable::<bevy::winit::WinitPlugin>()
                .disable::<bevy::log::LogPlugin>()
                .disable::<bevy::render::pipelined_rendering::PipelinedRenderingPlugin>(),
        )
        .add_plugins(plugin);
        app.finish();
        app.cleanup();
        let mut apps = std::mem::take(app.sub_apps_mut());
        let world = apps.main.world_mut();
        let mut image = Image::new_target_texture(
            target.size,
            target.size,
            TextureFormat::Rgba8UnormSrgb,
            None,
        );
        image.asset_usage = RenderAssetUsages::RENDER_WORLD;
        let handle = world.resource_mut::<Assets<Image>>().add(image);
        let mut entity = world.spawn((
            Camera3d::default(),
            Camera {
                clear_color: ClearColorConfig::Custom(Color::BLACK),
                ..default()
            },
            RenderTarget::from(handle.clone()),
            target.msaa,
            Tonemapping::None,
            Projection::Perspective(PerspectiveProjection {
                far: 1.0e7,
                ..default()
            }),
            Transform::default(),
            camera,
        ));
        if target.hdr {
            entity.insert(Hdr);
        }
        let camera = entity.id();
        Self {
            apps,
            target: handle,
            camera,
        }
    }

    pub fn world(&mut self) -> &mut World {
        self.apps.main.world_mut()
    }

    /// One frame, waited for on the GPU.
    pub fn update(&mut self) {
        self.apps.update();
        self.apps
            .main
            .world()
            .resource::<RenderDevice>()
            .wgpu_device()
            .poll(PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .expect("poll");
    }

    pub fn status(&self) -> TuileStatus {
        self.apps.main.world().resource::<TuileStatus>().clone()
    }

    /// Frames until the globe has settled — or `seconds` have gone, which is
    /// itself the finding.
    pub fn settle(&mut self, seconds: f64) -> TuileStatus {
        let start = std::time::Instant::now();
        let mut settled_for = 0;
        while start.elapsed().as_secs_f64() < seconds {
            self.update();
            if self.status().is_settled() {
                settled_for += 1;
                if settled_for > 3 {
                    break;
                }
            } else {
                settled_for = 0;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        self.status()
    }

    /// The camera's picture, as it is now: RGBA8, sRGB, rows top to bottom.
    pub fn picture(&mut self) -> Picture {
        self.try_picture().expect("the screenshot never arrived")
    }

    fn try_picture(&mut self) -> Option<Picture> {
        let taken: Arc<Mutex<Option<Image>>> = Arc::default();
        let out = Arc::clone(&taken);
        let target = self.target.clone();
        self.world().spawn(Screenshot::image(target)).observe(
            move |captured: On<ScreenshotCaptured>| {
                if let Ok(mut slot) = out.lock() {
                    *slot = Some(captured.image.clone());
                }
            },
        );
        for _ in 0..10 {
            self.update();
            if let Some(image) = taken.lock().ok().and_then(|mut i| i.take()) {
                let size = image.size();
                return Some(Picture {
                    width: size.x,
                    height: size.y,
                    rgba: image.data?,
                });
            }
        }
        None
    }
}

/// A frame read back.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Picture {
    pub fn at(&self, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * self.width + x) * 4) as usize;
        [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2]]
    }

    /// Pixels no brighter than `level` in every channel.
    pub fn black_pixels(&self, level: u8) -> usize {
        self.rgba
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] <= level && p[1] <= level && p[2] <= level)
            .count()
    }

    pub fn save(&self, path: &std::path::Path) {
        image::RgbaImage::from_raw(self.width, self.height, self.rgba.clone())
            .expect("size")
            .save(path)
            .expect("saved");
    }
}

/// The plugin over the synthetic globe, on a small budget.
pub fn plugin() -> TuilePlugin {
    TuilePlugin::new(sources()).with_budget(TuileBudget::handheld())
}
