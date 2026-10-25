// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! tuile's globe in a Bevy application: the application drives the camera,
//! and draws meshes of its own in the same depth as the ground.
//!
//! An orbit camera of the kind a strategy game has: it looks at a point on
//! the ground from a distance, tilted. Left-drag pans the point, right-drag
//! turns and tilts, the wheel zooms from 30 km down to 50 m. Pillars of the
//! application's own stand on the drawn ground (`TuileGround::ground_heights`,
//! one batch a frame), and a marker follows the cursor on it
//! (`TuileGround::pick_viewport`).
//!
//! ```text
//! cargo run --release -p tuile-heightfield-serve        # terminal 1
//! cargo run --release -p tuile-bevy-viewer              # terminal 2
//! ```
//!
//! Flags (every one optional):
//!
//! ```text
//! --terrain-url URL          a layer.json        (default: heightfield-serve's)
//! --imagery-template T       {z}/{x}/{y}         (default: heightfield-serve's)
//! --imagery-scheme S         geographic | webmercator   (default: geographic)
//! --imagery-max-level N      the template's deepest level (default: 15)
//! --lon D --lat D            what the camera looks at    (default: 10, 0)
//! --range M                  how far from it, metres     (default: 12000)
//! --heading D --pitch D      degrees; pitch below the horizon (default: 20, 40)
//! --msaa N                   1, 2, 4 or 8                (default: 4)
//! --hdr                      render in HDR
//! --compression C            off | auto | bc7 | astc | uastc (default: off)
//! --gpu-mib N                the globe's GPU budget
//! --screenshot PATH          once the globe has settled, save the window and quit
//! --no-atmosphere            the flat light and no sky (the atmosphere is on)
//! --time T                   now | sunset | sunrise | an RFC 3339 instant (default: now)
//! --time-offset D            added to it: -3h30m, +45m, 90s
//! --time-rate R              scene seconds a wall second (default: 1)
//! --day S                    a world of its own: a day of S clock seconds
//! ```
//!
//! In a browser (`web/build.sh`, then `web/check.py` for the headless run)
//! the same options are the page's query —
//! `?lon=10&range=3000&hdr` — and the page's `window.tuileStatus` says how
//! the globe stands, for a test driving it.

use bevy::camera::Hdr;
use bevy::input::mouse::{AccumulatedMouseMotion, AccumulatedMouseScroll, MouseScrollUnit};
use bevy::prelude::*;
use bevy::render::view::screenshot::{save_to_disk, Screenshot};
use bevy::render::view::Msaa;
use glam::DVec3;
use tuile_bevy::{
    local_frame, FloatingOrigin, GeoAnchor, GlobeCamera, GlobeSources, ImageryCompression, Lights,
    LonLat, TuileAtmosphere, TuileBudget, TuileCamera, TuileGround, TuilePlugin, TuileStatus,
    XyzTemplate,
};
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};

const DEFAULT_TERRAIN: &str = "http://127.0.0.1:8092/terrain/layer.json";
const DEFAULT_IMAGERY: &str = "http://127.0.0.1:8092/imagery/{z}/{x}/{y}.png";
const FOVY_DEG: f64 = 35.0;
const MIN_RANGE: f64 = 50.0;
const MAX_RANGE: f64 = 30_000.0;

/// What the command line said.
#[derive(Resource, Debug, Clone)]
struct Options {
    terrain: url::Url,
    imagery: XyzTemplate,
    view: Orbit,
    msaa: Msaa,
    hdr: bool,
    compression: ImageryCompression,
    gpu_mib: Option<u64>,
    screenshot: Option<std::path::PathBuf>,
    /// The sky and its sun, or the flat light.
    atmosphere: TuileAtmosphere,
}

fn number(flag: &str, value: Option<String>) -> Result<f64, String> {
    value
        .ok_or(format!("{flag} needs a value"))?
        .parse()
        .map_err(|e| format!("{flag}: {e}"))
}

/// The command line's words — or, on a page, the query's: `?a=1&b` reads
/// as `--a 1 --b`.
fn words() -> Vec<String> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::env::args().skip(1).collect()
    }
    #[cfg(target_arch = "wasm32")]
    {
        let search = web_sys::window()
            .and_then(|w| w.location().search().ok())
            .unwrap_or_default();
        let mut words = Vec::new();
        for pair in search.trim_start_matches('?').split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            // The page's own, not the application's.
            if key.is_empty() || key == "backend" {
                continue;
            }
            words.push(format!("--{key}"));
            if !value.is_empty() {
                words.push(
                    js_sys::decode_uri_component(value)
                        .ok()
                        .and_then(|v| v.as_string())
                        .unwrap_or_else(|| value.to_owned()),
                );
            }
        }
        words
    }
}

fn options() -> Result<Options, String> {
    let mut terrain = DEFAULT_TERRAIN.to_owned();
    let mut template = DEFAULT_IMAGERY.to_owned();
    let mut geographic = true;
    let mut max_level = Some(15);
    let mut view = Orbit::default();
    // WebGL2 can neither read a multisampled depth nor shade per sample: a
    // host whose depth has no stencil draws single-sampled there.
    let mut msaa = if cfg!(feature = "webgl2") {
        Msaa::Off
    } else {
        Msaa::Sample4
    };
    let mut hdr = false;
    let mut compression = ImageryCompression::Off;
    let mut gpu_mib = None;
    let mut screenshot = None;
    let mut sky = true;
    let (mut time, mut offset, mut rate) = (None, None, None);
    let mut day = None;
    let mut args = words().into_iter();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--terrain-url" => terrain = args.next().ok_or("--terrain-url needs a URL")?,
            "--imagery-template" => {
                template = args.next().ok_or("--imagery-template needs a template")?;
            }
            "--imagery-scheme" => {
                geographic = match args.next().as_deref() {
                    Some("geographic") => true,
                    Some("webmercator") => false,
                    other => return Err(format!("--imagery-scheme: {other:?}")),
                };
            }
            "--imagery-max-level" => max_level = Some(number(&flag, args.next())? as u32),
            "--lon" => view.lon = number(&flag, args.next())?,
            "--lat" => view.lat = number(&flag, args.next())?,
            "--range" => view.range = number(&flag, args.next())?.clamp(MIN_RANGE, MAX_RANGE),
            "--heading" => view.heading = number(&flag, args.next())?,
            "--pitch" => view.pitch = number(&flag, args.next())?.clamp(5.0, 90.0),
            "--msaa" => {
                msaa = match number(&flag, args.next())? as u32 {
                    1 => Msaa::Off,
                    2 => Msaa::Sample2,
                    4 => Msaa::Sample4,
                    8 => Msaa::Sample8,
                    n => return Err(format!("--msaa {n}: 1, 2, 4 or 8")),
                };
            }
            "--hdr" => hdr = true,
            "--no-atmosphere" => sky = false,
            "--time" => {
                time = Some(
                    args.next()
                        .ok_or("--time needs now, sunset, sunrise or an instant")?,
                )
            }
            "--time-offset" => offset = Some(args.next().ok_or("--time-offset needs a duration")?),
            "--time-rate" => rate = Some(args.next().ok_or("--time-rate needs a number")?),
            "--day" => day = Some(number(&flag, args.next())?),
            "--compression" => {
                compression = tuile_bevy::compression_named(
                    &args.next().ok_or("--compression needs a value")?,
                )?;
            }
            "--gpu-mib" => gpu_mib = Some(number(&flag, args.next())? as u64),
            "--screenshot" => {
                screenshot = Some(args.next().ok_or("--screenshot needs a path")?.into());
            }
            "--help" | "-h" => {
                return Err(
                    "the flags are listed at the head of examples/bevy-viewer/src/main.rs".into(),
                )
            }
            other => return Err(format!("unknown flag {other:?}")),
        }
    }
    let terrain = url::Url::parse(&terrain).map_err(|e| format!("--terrain-url: {e}"))?;
    let mut imagery = if geographic {
        XyzTemplate::geographic(template)
    } else {
        XyzTemplate::web_mercator(template)
    };
    imagery.max_level = max_level;
    imagery.check()?;
    let clock = tuile_bevy::atmosphere::ClockSettings::parse(
        time.as_deref(),
        offset.as_deref(),
        rate.as_deref(),
    )?;
    let mut atmosphere = TuileAtmosphere::earth(clock).enabled(sky);
    if let Some(day) = day {
        atmosphere = atmosphere.with_lights(Lights::orbit(day.max(1.0)));
    }
    Ok(Options {
        terrain,
        imagery,
        view,
        msaa,
        hdr,
        compression,
        gpu_mib,
        screenshot,
        atmosphere,
    })
}

fn main() {
    #[cfg(target_arch = "wasm32")]
    {
        // The decode workers instantiate this very module, and the bindings
        // call `main` there too.
        if tuile_bevy::web::is_decode_worker() {
            return;
        }
    }
    let options = match options() {
        Ok(options) => options,
        Err(why) => {
            eprintln!("tuile-bevy-viewer: {why}");
            std::process::exit(2);
        }
    };
    let mut budget = TuileBudget::default();
    if let Some(mib) = options.gpu_mib {
        budget = budget.with_gpu_bytes(mib * 1024 * 1024);
    }
    let globe = TuilePlugin::new(GlobeSources::addresses(
        options.terrain.clone(),
        options.imagery.clone(),
    ))
    .with_budget(budget)
    .with_compression(options.compression)
    // In a browser, the decode workers this page ships (`web/`), loading the
    // same build as the page.
    .with_decode_workers(if cfg!(feature = "webgl2") {
        "./decode-worker.js?pkg=webgl2"
    } else {
        "./decode-worker.js?pkg=webgpu"
    });
    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .set(bevy::log::LogPlugin {
                // The engine's determinism trace (`tuile_det`), one line per
                // tile decision: a file's business, never a console's.
                filter: format!("{},tuile_det=off", bevy::log::DEFAULT_FILTER),
                ..default()
            })
            .set(WindowPlugin {
                primary_window: Some(Window {
                    title: "tuile-bevy-viewer".into(),
                    resolution: (1280, 720).into(),
                    canvas: Some("#bevy".into()),
                    fit_canvas_to_parent: true,
                    ..default()
                }),
                ..default()
            }),
    )
    .add_plugins(globe)
    .insert_resource(options.view)
    .insert_resource(options.atmosphere.clone())
    .insert_resource(options)
    .add_systems(Startup, setup)
    .add_systems(
        Update,
        (
            steer,
            aim,
            stand_on_the_ground,
            follow_the_cursor,
            follow_the_sun,
            report,
            screenshot_when_settled,
        )
            .chain(),
    );
    #[cfg(target_arch = "wasm32")]
    app.add_systems(Update, publish);
    app.run();
}

/// The orbit camera's state: what it looks at, from how far, from where.
#[derive(Resource, Debug, Clone, Copy)]
struct Orbit {
    lon: f64,
    lat: f64,
    range: f64,
    heading: f64,
    pitch: f64,
}

impl Default for Orbit {
    fn default() -> Self {
        Self {
            lon: 10.0,
            lat: 0.0,
            range: 12_000.0,
            heading: 20.0,
            pitch: 40.0,
        }
    }
}

impl Orbit {
    /// The eye, looking at the point from `range` metres, `heading` clockwise
    /// from north and `pitch` below the horizon.
    fn pose(&self) -> GlobeCamera {
        let target = Geodetic {
            lon: self.lon.to_radians(),
            lat: self.lat.to_radians(),
            height: 0.0,
        };
        let at = geodetic_to_ecef(target);
        let enu = enu_frame(target);
        let (east, north, up) = (enu.col(0), enu.col(1), enu.col(2));
        let (h, p) = (self.heading.to_radians(), self.pitch.to_radians());
        let horizontal = north * h.cos() + east * h.sin();
        let direction = horizontal * p.cos() - up * p.sin();
        GlobeCamera::look_at(at - direction * self.range, at, up, FOVY_DEG.to_radians())
    }
}

fn setup(
    mut commands: Commands,
    options: Res<Options>,
    orbit: Res<Orbit>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let mut camera = commands.spawn((
        Camera3d::default(),
        Camera {
            clear_color: ClearColorConfig::Custom(Color::BLACK),
            ..default()
        },
        options.msaa,
        Projection::Perspective(PerspectiveProjection {
            far: 1.0e7,
            ..default()
        }),
        TuileCamera { pose: orbit.pose() },
    ));
    if options.hdr {
        camera.insert(Hdr);
    }
    // The application's own light, for its own meshes, kept on tuile's sun
    // (`follow_the_sun`): the pillars are lit as the ground is.
    commands.spawn((
        DirectionalLight {
            illuminance: 8_000.0,
            ..default()
        },
        Transform::from_rotation(Quat::from_rotation_x(-1.0)),
        Sunlight,
    ));
    // Pillars of the application's own, 300 m tall, stood on the drawn
    // ground every frame (`stand_on_the_ground`): they follow it as it
    // refines.
    let pillar = meshes.add(Cuboid::new(60.0, PILLAR_M as f32, 60.0));
    let colours = [
        Color::srgb(0.85, 0.25, 0.2),
        Color::srgb(0.95, 0.75, 0.2),
        Color::srgb(0.25, 0.45, 0.9),
    ];
    for i in -2i32..=2 {
        for j in -2..=2 {
            let place = LonLat::new(
                orbit.lon + f64::from(i) * 0.012,
                orbit.lat + f64::from(j) * 0.012,
            );
            let at = GeoAnchor::geodetic(place.lon, place.lat, 0.0);
            let colour = colours[(i + j).rem_euclid(3) as usize];
            commands.spawn((
                Mesh3d(pillar.clone()),
                MeshMaterial3d(materials.add(StandardMaterial {
                    base_color: colour,
                    ..default()
                })),
                at,
                OnTheGround {
                    place,
                    lift: PILLAR_M / 2.0,
                },
                Transform::from_rotation(local_frame(at.0)),
            ));
        }
    }
    // The marker under the cursor: where a ray through the pixel meets the
    // drawn ground (`follow_the_cursor`).
    commands.spawn((
        Mesh3d(meshes.add(Sphere::new(1.0))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(1.0, 0.1, 0.8),
            unlit: true,
            ..default()
        })),
        GeoAnchor(DVec3::ZERO),
        Transform::default(),
        Visibility::Hidden,
        Cursor,
    ));
}

/// The application's sun.
#[derive(Component)]
struct Sunlight;

/// The application's light along tuile's sun, as bright as the sun is high
/// over the camera — out at night, when only an ambient term is left.
fn follow_the_sun(
    atmosphere: Res<TuileAtmosphere>,
    cameras: Query<&TuileCamera>,
    mut lights: Query<(&mut Transform, &mut DirectionalLight), With<Sunlight>>,
) {
    let Some(eye) = cameras.iter().next().map(|c| c.pose.position) else {
        return;
    };
    let to_sun = if atmosphere.enabled {
        atmosphere.to_sun()
    } else {
        // The flat light's sun: straight over the camera.
        eye.normalize()
    };
    let up = eye.normalize();
    let elevation = to_sun.dot(up).clamp(-1.0, 1.0);
    for (mut transform, mut light) in &mut lights {
        *transform = Transform::default().looking_to((-to_sun).as_vec3(), up.as_vec3());
        light.illuminance = (10_000.0 * elevation.max(0.0).sqrt()) as f32;
    }
}

/// A pillar's height, metres.
const PILLAR_M: f64 = 300.0;

/// An entity stood on the drawn ground at `place`, its centre `lift`
/// metres above it.
#[derive(Component)]
struct OnTheGround {
    place: LonLat,
    lift: f64,
}

/// The marker under the cursor.
#[derive(Component)]
struct Cursor;

/// Every grounded entity, at the drawn ground's height under it: one batch
/// a frame.
fn stand_on_the_ground(
    ground: Res<TuileGround>,
    mut placed: Query<(&OnTheGround, &mut GeoAnchor)>,
) {
    let places: Vec<LonLat> = placed.iter().map(|(g, _)| g.place).collect();
    let heights = ground.ground_heights(&places);
    for ((on, mut anchor), h) in placed.iter_mut().zip(heights) {
        let at = GeoAnchor::geodetic(on.place.lon, on.place.lat, h.height + on.lift);
        if *anchor != at {
            *anchor = at;
        }
    }
}

/// The marker, where the ray under the cursor meets the drawn ground (the
/// window's middle without a cursor), sized
/// to stay visible from any range.
fn follow_the_cursor(
    ground: Res<TuileGround>,
    origin: Res<FloatingOrigin>,
    windows: Query<&Window>,
    cameras: Query<(&Camera, &GlobalTransform), With<TuileCamera>>,
    mut marker: Query<(&mut GeoAnchor, &mut Transform, &mut Visibility), With<Cursor>>,
) {
    let Ok((mut anchor, mut transform, mut visibility)) = marker.single_mut() else {
        return;
    };
    let hit = (|| {
        // The cursor, or the middle of the window when there is none (a
        // touch screen, a window not under the pointer).
        let window = windows.iter().next()?;
        let cursor = window
            .cursor_position()
            .unwrap_or(Vec2::new(window.width(), window.height()) / 2.0);
        let (camera, camera_transform) = cameras.iter().next()?;
        ground.pick_viewport(camera, camera_transform, &origin, cursor)
    })();
    match hit {
        Some(hit) => {
            anchor.0 = hit.ecef;
            transform.scale = Vec3::splat((hit.distance * 0.006).max(1.0) as f32);
            *visibility = Visibility::Visible;
        }
        None => *visibility = Visibility::Hidden,
    }
}

/// The orbit controller: drags and the wheel move the orbit.
fn steer(
    buttons: Res<ButtonInput<MouseButton>>,
    motion: Res<AccumulatedMouseMotion>,
    scroll: Res<AccumulatedMouseScroll>,
    mut orbit: ResMut<Orbit>,
) {
    let delta = motion.delta;
    if buttons.pressed(MouseButton::Left) && delta != Vec2::ZERO {
        // Pan: the point moves under the cursor by about the ground a pixel
        // covers at this range.
        let metres_per_pixel = orbit.range * 0.0015;
        let h = orbit.heading.to_radians();
        let (right, forward) = (-f64::from(delta.x), f64::from(delta.y));
        let east = (right * h.cos() + forward * h.sin()) * metres_per_pixel;
        let north = (-right * h.sin() + forward * h.cos()) * metres_per_pixel;
        orbit.lon += east / (111_319.5 * orbit.lat.to_radians().cos().max(0.01));
        orbit.lat = (orbit.lat + north / 110_574.3).clamp(-85.0, 85.0);
    }
    if buttons.pressed(MouseButton::Right) && delta != Vec2::ZERO {
        orbit.heading = (orbit.heading + f64::from(delta.x) * 0.3).rem_euclid(360.0);
        orbit.pitch = (orbit.pitch + f64::from(delta.y) * 0.2).clamp(5.0, 90.0);
    }
    let lines = match scroll.unit {
        MouseScrollUnit::Line => f64::from(scroll.delta.y),
        MouseScrollUnit::Pixel => f64::from(scroll.delta.y) / 40.0,
    };
    if lines != 0.0 {
        orbit.range = (orbit.range * (1.0 - 0.12 * lines)).clamp(MIN_RANGE, MAX_RANGE);
    }
}

/// The orbit, handed to the globe's camera.
fn aim(orbit: Res<Orbit>, mut cameras: Query<&mut TuileCamera>) {
    if !orbit.is_changed() {
        return;
    }
    for mut camera in &mut cameras {
        camera.pose = orbit.pose();
    }
}

/// A line when the globe's state changes.
fn report(status: Res<TuileStatus>, mut last: Local<Option<(bool, usize)>>) {
    let now = (status.is_settled(), status.lost);
    if *last != Some(now) {
        info!(
            "globe: {:?}, {} exact, {} coarser, {} lost, {:.0} MiB, settled {}",
            status.session,
            status.exact,
            status.coarser,
            status.lost,
            status.gpu_bytes as f64 / 1_048_576.0,
            status.is_settled()
        );
        *last = Some(now);
    }
}

/// The globe's state where a page's test can read it: `window.tuileStatus`.
#[cfg(target_arch = "wasm32")]
fn publish(status: Res<TuileStatus>) {
    if !status.is_changed() {
        return;
    }
    let line = format!(
        "{:?} settled={} exact={} coarser={} lost={} mib={:.0}",
        status.session,
        status.is_settled(),
        status.exact,
        status.coarser,
        status.lost,
        status.gpu_bytes as f64 / 1_048_576.0
    );
    let _ = js_sys::Reflect::set(&js_sys::global(), &"tuileStatus".into(), &line.into());
}

/// With `--screenshot`, the window once the globe has settled, then quit.
fn screenshot_when_settled(
    mut commands: Commands,
    options: Res<Options>,
    status: Res<TuileStatus>,
    mut state: Local<(u32, u32)>,
    mut exit: MessageWriter<AppExit>,
) {
    let Some(path) = &options.screenshot else {
        return;
    };
    let (settled, after) = &mut *state;
    if *after > 0 {
        *after += 1;
        if *after > 30 {
            exit.write(AppExit::Success);
        }
        return;
    }
    *settled = if status.is_settled() { *settled + 1 } else { 0 };
    if *settled > 30 {
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path.clone()));
        *after = 1;
    }
}
