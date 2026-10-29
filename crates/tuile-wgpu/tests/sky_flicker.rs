// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! GPU in the loop: the physical atmosphere is a continuous function of
//! the frame — its eye, its view, its clock — and nothing else.
//!
//! One renderer over hundreds of frames, as a viewer draws them:
//!
//! - a zoom out from 3 km to 300 km over the Pic du Midi and a pan, the
//!   clock stopped: no frame may be much brighter or darker than the one
//!   before it, in the sky or on the ground;
//! - climbs from 10 km to 36 000 km and from 400 km to 100 000 km at the
//!   horizon, a zoom out straight down over the same span, a slow pan at
//!   geostationary height: the limb is in every frame, and never jumps;
//! - the camera still under a clock run thirty and six hundred times fast
//!   across a sunset: the sun is the clock's every frame, and the light
//!   turns smoothly.
//!
//! The ground is plain terrain of one grey, its tiles rebased onto the eye
//! every frame, as the viewer does. With each test's `TUILE_*_TRACE` set to
//! a directory, every frame's camera, exposure, white balance, sky
//! harmonic, light and sky-view counts, lumas, limb and picture go to an
//! MCAP tape there, `trace.mcap`, one channel a signal.

use glam::{DMat4, DVec3};
use tuile_core::content::DecodedTileContent;
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};
use tuile_terrain::{fill_content, GeographicTilingScheme, TileCoord};
use tuile_wgpu::{
    draw_to_texture, prepare, view_proj_from_world, FrameOnTexture, FrameTargets, GpuContext,
    PreparedTile, Readback, SkyView, TileRenderer, ViewUniform,
};

const WIDTH: u32 = 384;
const HEIGHT: u32 = 256;
const FOV_Y: f64 = 50.0 * std::f64::consts::PI / 180.0;
/// The limb's pictures: larger, for a band a few pixels thick.
const LIMB_WIDTH: u32 = 768;
const LIMB_HEIGHT: u32 = 512;
const GROUND: f32 = 0.08;
const PIC_LON: f64 = 0.1418;
const PIC_LAT: f64 = 42.9365;
/// The terrain's height around the Pic, metres.
const TERRAIN: f64 = 600.0;
/// The viewer's frame rate: the wall advances by one over it each frame.
const FPS: f64 = 60.0;

fn gpu() -> Option<GpuContext> {
    match pollster::block_on(GpuContext::headless()) {
        Ok(gpu) => Some(gpu),
        Err(e) => {
            assert!(
                std::env::var("TUILE_REQUIRE_GPU").is_err(),
                "TUILE_REQUIRE_GPU is set and there is no usable adapter: {e}"
            );
            eprintln!("skipping GPU test: no usable adapter ({e})");
            None
        }
    }
}

fn grey(mut content: DecodedTileContent) -> DecodedTileContent {
    for mesh in &mut content.meshes {
        mesh.material.base_color_factor = [GROUND, GROUND, GROUND, 1.0];
        mesh.material.base_color_texture = None;
    }
    content.textures.clear();
    content
}

/// Every tile of `level` over the Earth, and those of `fine` within
/// `reach` tiles of the Pic at the terrain's height.
fn ground(gpu: &GpuContext, origin: DVec3, level: u32, fine: u32, reach: i64) -> Vec<PreparedTile> {
    let scheme = GeographicTilingScheme::default();
    let mut out = Vec::new();
    for y in 0..(1u32 << level) {
        for x in 0..(2u32 << level) {
            let rect = scheme.tile_rect(TileCoord::new(level, u64::from(x), u64::from(y)));
            out.push(prepare(
                gpu,
                &grey(fill_content(&rect, [Some(0.0); 4], 0.0)),
                origin,
            ));
        }
    }
    let columns = 2i64 << fine;
    let rows = 1i64 << fine;
    let x0 = ((PIC_LON + 180.0) / 360.0 * columns as f64).floor() as i64;
    let y0 = ((90.0 - PIC_LAT) / 180.0 * rows as f64).floor() as i64;
    for dy in -reach..=reach {
        for dx in -reach..=reach {
            let (x, y) = ((x0 + dx).rem_euclid(columns), y0 + dy);
            if !(0..rows).contains(&y) {
                continue;
            }
            let rect = scheme.tile_rect(TileCoord::new(fine, x as u64, y as u64));
            out.push(prepare(
                gpu,
                &grey(fill_content(&rect, [Some(TERRAIN); 4], TERRAIN)),
                origin,
            ));
        }
    }
    out
}

#[derive(Clone, Copy)]
struct Eye {
    at: DVec3,
    forward: DVec3,
    up: DVec3,
    fov_y: f64,
}

/// An eye over a place, heading from north and pitch from the horizon,
/// degrees.
fn eye_at(lon: f64, lat: f64, height: f64, heading: f64, pitch: f64) -> Eye {
    let g = Geodetic {
        lon: lon.to_radians(),
        lat: lat.to_radians(),
        height,
    };
    let enu = enu_frame(g);
    let (h, p) = (heading.to_radians(), pitch.to_radians());
    let level = enu.x_axis * h.sin() + enu.y_axis * h.cos();
    let forward = level * p.cos() + enu.z_axis * p.sin();
    let up = (enu.z_axis - forward * enu.z_axis.dot(forward)).normalize();
    Eye {
        at: geodetic_to_ecef(g),
        forward,
        up,
        fov_y: FOV_Y,
    }
}

fn clip_from_world(eye: &Eye, clearance: f64, size: (u32, u32)) -> DMat4 {
    let near = (clearance * 0.25).clamp(0.5, 1.0e6);
    let far = eye.at.length() + 2.0e7;
    glam::dcamera::rh::proj::directx::perspective(
        eye.fov_y,
        f64::from(size.0) / f64::from(size.1),
        near,
        far,
    ) * glam::dcamera::rh::view::look_at_mat4(eye.at, eye.at + eye.forward, eye.up)
}

/// The camera path: a zoom out from 3 km to 300 km over the Pic in
/// `zoom` frames, then a pan east at 300 km in `pan` frames — heading
/// west, 15° down, so the horizon and the sky stay in the picture.
fn path(zoom: usize, pan: usize) -> Vec<Eye> {
    let mut out = Vec::new();
    for i in 0..zoom {
        let t = i as f64 / (zoom - 1) as f64;
        let height = 3_000.0 * 100f64.powf(t);
        out.push(eye_at(PIC_LON, PIC_LAT, height, 270.0, -15.0));
    }
    for i in 1..=pan {
        let lon = PIC_LON + 3.0 * i as f64 / pan as f64;
        out.push(eye_at(lon, PIC_LAT, 300_000.0, 270.0, -15.0));
    }
    out
}

/// One frame of the trace.
#[derive(Debug, Clone, Copy)]
struct Traced {
    height: f64,
    exposure: f32,
    grade: [f32; 3],
    lights: u64,
    views: u64,
    sky: f64,
    ground: f64,
    /// The brightest the limb is: the largest luma of the central columns
    /// over the horizon, which is the picture's middle row.
    limb: f64,
    /// How much light the limb holds: over the central columns, the luma
    /// over the horizon above each column's darkest, summed down the column
    /// — levels times pixels, which a band one pixel thick keeps however
    /// it falls on the pixels.
    limb_energy: f64,
    /// Looking down: the brightest of the ring just outside the globe's
    /// disc, and whether the disc is small enough for the ring to be in
    /// the picture.
    ring: f64,
    ring_in_view: bool,
    /// The sun the frame was lit by.
    to_sun: DVec3,
}

fn luma_at(rgba: &[u8], width: u32, x: u32, y: u32) -> f64 {
    let i = ((y * width + x) * 4) as usize;
    let [r, g, b] = [rgba[i], rgba[i + 1], rgba[i + 2]].map(f64::from);
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

fn luma(rgba: &[u8], width: u32, rows: std::ops::Range<u32>) -> f64 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in rows {
        for x in 0..width {
            sum += luma_at(rgba, width, x, y);
            n += 1.0;
        }
    }
    sum / n
}

/// The instant an hour before the Pic's sunset on midsummer's day 2026.
fn before_sunset() -> f64 {
    let start = 1_782_043_200.0; // 2026-06-21 12:00 UTC
    tuile_atmosphere::Sun::next_sunset(
        start,
        Geodetic {
            lon: PIC_LON.to_radians(),
            lat: PIC_LAT.to_radians(),
            height: 0.0,
        },
    )
    .expect("the sun sets over the Pyrenees in June")
        - 3_600.0
}

/// Draws `eyes` one after the other through one renderer at `FPS`, its
/// clock stopped at `instant`, over `tiles` at `size`, and traces each
/// frame — into `trace`'s `trace.mcap`, if given: the camera, one channel a
/// signal, and the pictures.
fn fly(
    gpu: &GpuContext,
    tiles: &[PreparedTile],
    eyes: &[Eye],
    instant: f64,
    size: (u32, u32),
    trace: Option<&std::path::Path>,
) -> Vec<Traced> {
    fly_clock(gpu, tiles, eyes, |_| instant, size, trace)
}

/// [`fly`] under a clock: frame `n` at the wall's `n / FPS` is lit at
/// `clock(wall)`.
fn fly_clock(
    gpu: &GpuContext,
    tiles: &[PreparedTile],
    eyes: &[Eye],
    clock: impl Fn(f64) -> f64,
    size: (u32, u32),
    trace: Option<&std::path::Path>,
) -> Vec<Traced> {
    let (width, height_px) = size;
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let mut renderer = TileRenderer::new(gpu, format);
    let targets = FrameTargets::new(gpu, format, size);
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("flicker"),
        size: wgpu::Extent3d {
            width,
            height: height_px,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let mut out = Vec::new();
    let mut tape = trace.map(|dir| {
        tuile_tape::Tape::recording(dir.join("trace.mcap")).expect("a trace can be written")
    });
    // Every picture of a short flight; one in five of a long one.
    let image_every = if eyes.len() > 600 { 5 } else { 1 };
    for (n, eye) in eyes.iter().enumerate() {
        let wall = n as f64 / FPS;
        let instant = clock(wall);
        let height = tuile_core::geo::ecef_to_geodetic(eye.at).height;
        let clearance = (height - TERRAIN).max(1.0);
        // The viewer's render origin is its camera, every frame.
        let origin = eye.at;
        for tile in tiles {
            tile.rebase(&gpu.queue, origin);
        }
        let sun = tuile_atmosphere::Sun::at_unix_seconds(instant);
        let travel = sun.light_travel_direction().as_vec3();
        renderer.set_view(
            &gpu.queue,
            &ViewUniform {
                view_proj: view_proj_from_world(clip_from_world(eye, clearance, size), origin),
                sun_dir: [travel.x, travel.y, travel.z, 0.0],
                params: [0.25, 0.0, 0.0, 0.0],
                atmosphere: tuile_atmosphere::AerialPerspective::new(eye.at, origin, &sun, 0.0),
            },
        );
        let view = SkyView {
            eye: eye.at,
            render_origin: origin,
            forward: eye.forward,
            up: eye.up,
            fov_y: eye.fov_y,
            aspect: f64::from(width) / f64::from(height_px),
            height_px,
            ground_height: TERRAIN,
        };
        renderer.set_sky(gpu, Some(&view.at(instant, wall)));
        let mut readback = Readback::default();
        draw_to_texture(
            FrameOnTexture {
                gpu,
                texture: &texture,
                targets: &targets,
                clear: wgpu::Color::BLACK,
                capture: Some(&mut readback),
            },
            |pass| {
                renderer.paint(
                    pass,
                    std::iter::empty(),
                    tiles.iter(),
                    std::iter::empty(),
                    None,
                    false,
                )
            },
            |_, _| {},
        );
        let pixels = readback.take(gpu, size).expect("read back");
        let light = renderer.read_sky_light(gpu).expect("the sky is on");
        let u = light.uniform;
        let (lights, views) = renderer.sky_updates();
        let mut limb = 0.0f64;
        let mut limb_energy = 0.0f64;
        let columns = width * 9 / 20..width * 11 / 20;
        for x in columns.clone() {
            let column: Vec<f64> = (0..=height_px / 2)
                .map(|y| luma_at(&pixels, width, x, y))
                .collect();
            let darkest = column.iter().copied().fold(f64::INFINITY, f64::min);
            limb = column.iter().copied().fold(limb, f64::max);
            limb_energy += column.iter().map(|l| l - darkest).sum::<f64>();
        }
        limb_energy /= columns.len() as f64;
        // The globe's disc seen from the eye, in pixels from the middle.
        let r = tuile_core::geo::WGS84_A;
        let disc = (r / eye.at.length()).asin().tan() / (eye.fov_y / 2.0).tan()
            * f64::from(height_px)
            / 2.0;
        let ring_in_view = disc * 1.05 < f64::from(height_px) / 2.0;
        let mut ring = 0.0f64;
        if ring_in_view {
            let (cx, cy) = (f64::from(width) / 2.0, f64::from(height_px) / 2.0);
            for k in 0..720 {
                let a = f64::from(k) / 720.0 * std::f64::consts::TAU;
                for step in 0..12 {
                    let rr = disc * (1.0 + 0.007 * f64::from(step));
                    let (x, y) = (cx + rr * a.cos(), cy + rr * a.sin());
                    if x >= 0.0 && y >= 0.0 && x < f64::from(width) && y < f64::from(height_px) {
                        ring = ring.max(luma_at(&pixels, width, x as u32, y as u32));
                    }
                }
            }
        }
        let s = u.sun;
        let traced = Traced {
            height,
            exposure: light.exposure,
            grade: [u.grade[0], u.grade[1], u.grade[2]],
            lights,
            views,
            // The top eighth is sky all the way; the bottom quarter ground.
            sky: luma(&pixels, width, 0..height_px / 8),
            ground: luma(&pixels, width, height_px * 3 / 4..height_px),
            limb,
            limb_energy,
            ring,
            ring_in_view,
            to_sun: DVec3::new(f64::from(s[0]), f64::from(s[1]), f64::from(s[2])),
        };
        if let Some(tape) = tape.as_mut() {
            record(tape, eye, &traced, &light);
            if n % image_every == 0 {
                tape.push_image(&pixels, width, height_px);
            }
        }
        out.push(traced);
    }
    if let Some(tape) = tape {
        tape.finish().expect("the trace is written");
    }
    out
}

/// One frame's signals on the tape, one channel each, on the frame whose
/// camera is pushed here.
fn record(tape: &mut tuile_tape::Tape, eye: &Eye, t: &Traced, light: &tuile_wgpu::SkyLightRead) {
    tape.push(tuile_tape::Frame {
        position: eye.at.to_array(),
        direction: eye.forward.to_array(),
        up: eye.up.to_array(),
        fovy: eye.fov_y,
    });
    let number = r#"{"type":"object","additionalProperties":{"type":"number"}}"#;
    let json = |pairs: &[(&str, f64)]| {
        let body: Vec<String> = pairs.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect();
        format!("{{{}}}", body.join(","))
    };
    tape.push_json(
        "/atmosphere/exposure",
        "tuile.Exposure",
        number,
        &json(&[
            ("exposure", f64::from(t.exposure)),
            ("ev100", f64::from(light.ev100)),
        ]),
    );
    tape.push_json(
        "/atmosphere/white_balance",
        "tuile.WhiteBalance",
        number,
        &json(&[
            ("r", f64::from(t.grade[0])),
            ("g", f64::from(t.grade[1])),
            ("b", f64::from(t.grade[2])),
        ]),
    );
    tape.push_json(
        "/atmosphere/harmonic",
        "tuile.SkyHarmonic",
        number,
        &json(&[
            ("ambient_r", f64::from(light.uniform.ambient[0])),
            ("ambient_g", f64::from(light.uniform.ambient[1])),
            ("ambient_b", f64::from(light.uniform.ambient[2])),
        ]),
    );
    tape.push_json(
        "/atmosphere/events",
        "tuile.LightEvents",
        number,
        &json(&[("lights", t.lights as f64), ("sky_views", t.views as f64)]),
    );
    tape.push_json(
        "/atmosphere/sun",
        "tuile.Sun",
        number,
        &json(&[("x", t.to_sun.x), ("y", t.to_sun.y), ("z", t.to_sun.z)]),
    );
    tape.push_json(
        "/luminance",
        "tuile.Luminance",
        number,
        &json(&[("height", t.height), ("sky", t.sky), ("ground", t.ground)]),
    );
    tape.push_json(
        "/limb",
        "tuile.Limb",
        number,
        &json(&[
            ("drawn", f64::from(u8::from(t.limb_energy >= 60.0))),
            ("luminance", t.limb),
            ("energy", t.limb_energy),
            ("ring", t.ring),
            ("ring_in_view", f64::from(u8::from(t.ring_in_view))),
        ]),
    );
}

/// The largest step, in 8-bit luma, from one frame to the next.
fn worst_step(trace: &[Traced], of: impl Fn(&Traced) -> f64) -> (usize, f64) {
    trace
        .windows(2)
        .enumerate()
        .map(|(n, w)| (n + 1, (of(&w[1]) - of(&w[0])).abs()))
        .fold((0, 0.0), |a, b| if b.1 > a.1 { b } else { a })
}

/// A zoom out and a pan under a stopped clock: the sky and the ground
/// drift with what the camera sees, but no frame steps from the one
/// before it by more than two levels of eight-bit luma — a step the eye
/// sees as a blink when it comes four times a second.
#[test]
fn a_moving_camera_under_a_stopped_clock_does_not_blink() {
    let Some(gpu) = gpu() else { return };
    let trace = std::env::var_os("TUILE_FLICKER_TRACE").map(std::path::PathBuf::from);
    if let Some(dir) = &trace {
        std::fs::create_dir_all(dir).expect("trace dir");
    }
    let eyes = path(240, 120);
    let tiles = ground(&gpu, eyes[0].at, 3, 8, 6);
    let traced = fly(
        &gpu,
        &tiles,
        &eyes,
        before_sunset(),
        (WIDTH, HEIGHT),
        trace.as_deref(),
    );
    let (sky_at, sky_step) = worst_step(&traced, |t| t.sky);
    let (ground_at, ground_step) = worst_step(&traced, |t| t.ground);
    let last = traced.last().expect("frames");
    eprintln!(
        "{} lights, {} sky-views; worst step: sky {sky_step:.2} at frame {sky_at}, ground {ground_step:.2} at frame {ground_at}",
        last.lights, last.views
    );
    const BOUND: f64 = 2.0;
    assert!(
        sky_step <= BOUND,
        "the sky blinks: {sky_step:.2} levels of luma at frame {sky_at} ({:?} → {:?})",
        traced[sky_at - 1],
        traced[sky_at]
    );
    assert!(
        ground_step <= BOUND,
        "the ground blinks: {ground_step:.2} levels of luma at frame {ground_at} ({:?} → {:?})",
        traced[ground_at - 1],
        traced[ground_at]
    );
}

/// Over Europe, where the sun sets at the instant: lon, lat, degrees.
const EUROPE: (f64, f64) = (5.0, 46.0);

/// A camera over Europe at `height`, looking north at the horizon — along
/// the terminator, so that the limb holds a lit side, a twilight band and
/// a dark side — `fov_y` radians.
fn at_the_horizon(lon: f64, height: f64, fov_y: f64) -> Eye {
    let r = tuile_core::geo::WGS84_A;
    let dip = (r / (r + height)).acos().to_degrees();
    let mut eye = eye_at(lon, EUROPE.1, height, 0.0, -dip);
    eye.fov_y = fov_y;
    eye
}

/// A climb from `from` to `to` metres over Europe in `frames` frames, the
/// height growing by one ratio a frame, looking at the horizon, the field
/// narrowing with the Earth's disc from 40° to 20°, so that the limb stays
/// a band of the picture.
fn climb(frames: usize, from: f64, to: f64) -> Vec<Eye> {
    (0..frames)
        .map(|i| {
            let t = i as f64 / (frames - 1) as f64;
            let height = from * (to / from).powf(t);
            at_the_horizon(EUROPE.0, height, (40.0 * (1.0 - 0.5 * t)).to_radians())
        })
        .collect()
}

/// A zoom out as a wheel makes it: straight down over Europe from `from` to
/// `to` metres, the field the viewer's 50°.
fn zoom_out(frames: usize, from: f64, to: f64) -> Vec<Eye> {
    (0..frames)
        .map(|i| {
            let t = i as f64 / (frames - 1) as f64;
            let height = from * (to / from).powf(t);
            eye_at(EUROPE.0, EUROPE.1, height, 0.0, -89.999)
        })
        .collect()
}

fn sunset_over_europe() -> f64 {
    let (lon, lat) = EUROPE;
    tuile_atmosphere::Sun::next_sunset(
        1_782_043_200.0,
        Geodetic {
            lon: lon.to_radians(),
            lat: lat.to_radians(),
            height: 0.0,
        },
    )
    .expect("the sun sets over Europe in June")
}

fn trace_dir(var: &str) -> Option<std::path::PathBuf> {
    let dir = std::env::var_os(var).map(std::path::PathBuf::from)?;
    std::fs::create_dir_all(&dir).expect("trace dir");
    Some(dir)
}

/// What the limb asks of every frame of a flight: there — at least the
/// light of a line a pixel thick `floor` levels over space — and
/// continuous: its light changes by at most `step` of itself from one
/// frame to the next.
fn the_limb_holds(name: &str, traced: &[Traced], floor: f64, step: f64) {
    for (n, t) in traced.iter().enumerate() {
        assert!(
            t.limb_energy >= floor,
            "{name}: the limb is gone at frame {n}, {:.0} m: it holds {:.1} levels·pixels",
            t.height,
            t.limb_energy
        );
    }
    let (at, worst) = traced
        .windows(2)
        .enumerate()
        .map(|(n, w)| (n + 1, (w[1].limb_energy / w[0].limb_energy).ln().abs()))
        .fold((0, 0.0), |a, b| if b.1 > a.1 { b } else { a });
    let (exposure_at, exposure_step) = worst_step(traced, |t| f64::from(t.exposure).log2());
    eprintln!(
        "{name}: faintest limb {:.1} levels·pixels; worst limb step {:.1} % at frame {at}; worst exposure step {exposure_step:.4} EV at frame {exposure_at}",
        traced.iter().map(|t| t.limb_energy).fold(f64::INFINITY, f64::min),
        100.0 * (worst.exp() - 1.0)
    );
    assert!(
        worst <= step.ln_1p(),
        "{name}: the limb's light changes by {:.1} % at frame {at} ({:.0} m → {:.0} m)",
        100.0 * (worst.exp() - 1.0),
        traced[at - 1].height,
        traced[at].height
    );
}

/// From 10 km to geostationary height at sunset, the limb — the thin shell
/// of lit air around the globe — stays in the picture: never darker than
/// a band the eye picks out against space, and never jumping from one
/// frame to the next.
#[test]
fn the_limb_stays_from_ten_kilometres_to_geostationary_height() {
    let Some(gpu) = gpu() else { return };
    let trace = trace_dir("TUILE_LIMB_TRACE");
    let eyes = climb(181, 10_000.0, 36_000_000.0);
    let tiles = ground(&gpu, eyes[0].at, 3, 8, 6);
    let traced = fly(
        &gpu,
        &tiles,
        &eyes,
        sunset_over_europe(),
        (LIMB_WIDTH, LIMB_HEIGHT),
        trace.as_deref(),
    );
    // A frame is 4.7 % higher than the one before: the limb may brighten
    // or dim by some levels from one to the next, never blink.
    the_limb_holds("climb", &traced, 60.0, 0.25);
}

/// A slow climb from 400 km to 100 000 km — a thousand frames, a third of
/// a percent higher each — at the horizon, and a slow zoom out straight
/// down over the same span, as a wheel makes it: the limb is drawn in
/// every frame it is in the picture, and its light never jumps.
#[test]
fn the_limb_never_blinks_on_a_slow_way_out() {
    let Some(gpu) = gpu() else { return };
    let size = (WIDTH, HEIGHT);
    let eyes = climb(1_000, 400_000.0, 100_000_000.0);
    let tiles = ground(&gpu, eyes[0].at, 3, 8, 6);
    let traced = fly(
        &gpu,
        &tiles,
        &eyes,
        sunset_over_europe(),
        size,
        trace_dir("TUILE_LIMB_SLOW_TRACE").as_deref(),
    );
    the_limb_holds("slow climb", &traced, 60.0, 0.05);
    let eyes = zoom_out(1_000, 400_000.0, 100_000_000.0);
    let traced = fly(
        &gpu,
        &tiles,
        &eyes,
        sunset_over_europe(),
        size,
        trace_dir("TUILE_ZOOM_TRACE").as_deref(),
    );
    let (exposure_at, exposure_step) = worst_step(&traced, |t| f64::from(t.exposure).log2());
    // Where the ring is in the picture in both frames.
    let (ring_at, ring_step) = traced
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w[0].ring_in_view && w[1].ring_in_view)
        .map(|(n, w)| (n + 1, (w[1].ring - w[0].ring).abs()))
        .fold((0, 0.0), |a, b| if b.1 > a.1 { b } else { a });
    eprintln!(
        "zoom out: worst exposure step {exposure_step:.4} EV at frame {exposure_at}; worst ring step {ring_step:.2} at frame {ring_at}"
    );
    for (n, t) in traced.iter().enumerate() {
        if t.ring_in_view {
            assert!(
                t.ring >= 40.0,
                "zoom out: the limb is gone at frame {n}, {:.0} m: {:.1} levels",
                t.height,
                t.ring
            );
        }
    }
    // A third of a percent further a frame, the globe shrinking under a
    // meter that weighs its night side by its stops: the exposure moves by
    // a few hundredths of a stop a frame, never by a step.
    assert!(
        exposure_step <= 0.05,
        "zoom out: the exposure steps by {exposure_step:.4} EV at frame {exposure_at}"
    );
    assert!(
        ring_step <= 3.0,
        "zoom out: the limb jumps by {ring_step:.2} at frame {ring_at}"
    );
}

/// At geostationary height a pan of a hundredth of a degree a frame —
/// less than any step the light used to wait for — keeps the limb where
/// the globe is: the sky's frame is the frame's own eye's.
#[test]
fn a_slow_pan_far_away_keeps_the_limb_on_the_globe() {
    let Some(gpu) = gpu() else { return };
    let eyes: Vec<Eye> = (0..120)
        .map(|i| at_the_horizon(EUROPE.0 + 0.01 * i as f64, 36_000_000.0, 20f64.to_radians()))
        .collect();
    let tiles = ground(&gpu, eyes[0].at, 3, 8, 6);
    let traced = fly(
        &gpu,
        &tiles,
        &eyes,
        sunset_over_europe(),
        (LIMB_WIDTH, LIMB_HEIGHT),
        trace_dir("TUILE_PAN_TRACE").as_deref(),
    );
    the_limb_holds("pan at 36 000 km", &traced, 60.0, 0.05);
}

/// The camera held still on the Pic du Midi, looking west, the clock run
/// at `rate` across the sunset for `frames` frames at 60 a second.
fn run_the_clock(
    gpu: &GpuContext,
    rate: f64,
    frames: usize,
    trace: Option<&std::path::Path>,
) -> Vec<Traced> {
    let eye = eye_at(PIC_LON, PIC_LAT, 2_877.0, 290.0, -2.0);
    let eyes = vec![eye; frames];
    let tiles = ground(gpu, eye.at, 3, 8, 6);
    let sunset = before_sunset() + 3_600.0;
    // Centred on the sunset.
    let start = sunset - rate * frames as f64 / FPS / 2.0;
    fly_clock(
        gpu,
        &tiles,
        &eyes,
        |wall| start + rate * wall,
        (WIDTH, HEIGHT),
        trace,
    )
}

/// Under a clock run fast — thirty and six hundred times — across the
/// sunset, the camera still: every frame's sun is the clock's, its step
/// from frame to frame what the sun moves in a frame of the clock, and
/// the sky and the ground darken smoothly, never by a step.
#[test]
fn a_fast_clock_turns_the_light_smoothly() {
    let Some(gpu) = gpu() else { return };
    for (rate, frames, var) in [
        (30.0, 1_200, "TUILE_RATE30_TRACE"),
        (600.0, 600, "TUILE_RATE600_TRACE"),
    ] {
        let traced = run_the_clock(&gpu, rate, frames, trace_dir(var).as_deref());
        for (n, w) in traced.windows(2).enumerate() {
            // The sun turns 2π a day about the axis: per frame, `rate / FPS`
            // seconds of it, on a circle of its declination.
            let declination = w[0].to_sun.z.asin();
            let expected = rate / FPS * std::f64::consts::TAU / 86_400.0 * declination.cos();
            let turned = w[0].to_sun.angle_between(w[1].to_sun);
            assert!(
                (turned / expected - 1.0).abs() < 0.05,
                "rate {rate}, frame {}: the sun turned {turned:.3e} rad, a frame of the clock is {expected:.3e}",
                n + 1
            );
        }
        for (what, of) in [
            ("sky", (|t: &Traced| t.sky) as fn(&Traced) -> f64),
            ("ground", |t: &Traced| t.ground),
        ] {
            let steps: Vec<f64> = traced.windows(2).map(|w| of(&w[1]) - of(&w[0])).collect();
            // Smooth: no step much larger than its neighbours'.
            for n in 0..steps.len() {
                let lo = n.saturating_sub(8);
                let hi = (n + 9).min(steps.len());
                let around: f64 = steps[lo..hi]
                    .iter()
                    .enumerate()
                    .filter(|&(i, _)| lo + i != n)
                    .map(|(_, s)| s.abs())
                    .sum::<f64>()
                    / (hi - lo - 1) as f64;
                assert!(
                    steps[n].abs() <= 3.0 * around + 0.6,
                    "rate {rate}: the {what} steps by {:.2} levels at frame {} where its neighbours move by {around:.2}",
                    steps[n],
                    n + 1
                );
            }
            let worst = steps.iter().fold(0.0f64, |a, s| a.max(s.abs()));
            eprintln!("rate {rate}: worst {what} step {worst:.2} levels");
        }
    }
}
