// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! GPU in the loop: the physical atmosphere holds still while only the
//! camera moves.
//!
//! One renderer over a few hundred frames, as a viewer draws them: a zoom
//! out from 3 km to 300 km over the Pic du Midi, then a pan at constant
//! height, the clock stopped an hour before sunset. Nothing in the sky
//! changes but the eye, so the picture may drift — what the camera sees
//! changes — but it may not step: no frame may be much brighter or darker
//! than the one before it, in the sky or on the ground.
//!
//! The ground is plain terrain of one grey, its tiles rebased onto the eye
//! every frame, as the viewer does. With `TUILE_FLICKER_TRACE` set to a
//! directory, every frame's exposure, grade, sky harmonic, light and
//! sky-view counts and both lumas go to `trace.csv` there, and every
//! frame's picture to `frame-NNNN.png`.

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
    }
}

fn clip_from_world(eye: &Eye, clearance: f64) -> DMat4 {
    let near = (clearance * 0.25).clamp(0.5, 1.0e6);
    let far = eye.at.length() + 2.0e7;
    glam::dcamera::rh::proj::directx::perspective(
        FOV_Y,
        f64::from(WIDTH) / f64::from(HEIGHT),
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
    ambient: f32,
    lights: u64,
    views: u64,
    sky: f64,
    ground: f64,
}

fn luma(rgba: &[u8], rows: std::ops::Range<u32>) -> f64 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in rows {
        for x in 0..WIDTH {
            let i = ((y * WIDTH + x) * 4) as usize;
            let [r, g, b] = [rgba[i], rgba[i + 1], rgba[i + 2]].map(f64::from);
            sum += 0.2126 * r + 0.7152 * g + 0.0722 * b;
            n += 1.0;
        }
    }
    sum / n
}

/// Draws `eyes` one after the other through one renderer at `FPS`, its
/// clock stopped an hour before the Pic's sunset, and traces each frame.
fn fly(gpu: &GpuContext, eyes: &[Eye], trace: Option<&std::path::Path>) -> Vec<Traced> {
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let mut renderer = TileRenderer::new(gpu, format);
    let tiles = ground(gpu, eyes[0].at, 3, 8, 6);
    let start = 1_782_043_200.0; // 2026-06-21 12:00 UTC
    let sunset = tuile_atmosphere::Sun::next_sunset(
        start,
        Geodetic {
            lon: PIC_LON.to_radians(),
            lat: PIC_LAT.to_radians(),
            height: 0.0,
        },
    )
    .expect("the sun sets over the Pyrenees in June");
    let instant = sunset - 3_600.0;
    let targets = FrameTargets::new(gpu, format, (WIDTH, HEIGHT));
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("flicker"),
        size: wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
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
    for (n, eye) in eyes.iter().enumerate() {
        let wall = n as f64 / FPS;
        let height = tuile_core::geo::ecef_to_geodetic(eye.at).height;
        let clearance = (height - TERRAIN).max(1.0);
        // The viewer's render origin is its camera, every frame.
        let origin = eye.at;
        for tile in &tiles {
            tile.rebase(&gpu.queue, origin);
        }
        let sun = tuile_atmosphere::Sun::at_unix_seconds(instant);
        let travel = sun.light_travel_direction().as_vec3();
        renderer.set_view(
            &gpu.queue,
            &ViewUniform {
                view_proj: view_proj_from_world(clip_from_world(eye, clearance), origin),
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
            fov_y: FOV_Y,
            aspect: f64::from(WIDTH) / f64::from(HEIGHT),
            height_px: HEIGHT,
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
        let pixels = readback.take(gpu, (WIDTH, HEIGHT)).expect("read back");
        let u = renderer.sky_uniform().expect("the sky is on");
        let (lights, views) = renderer.sky_updates();
        let traced = Traced {
            height,
            exposure: renderer.sky_exposure().expect("the sky is on"),
            grade: [u.grade[0], u.grade[1], u.grade[2]],
            ambient: u.ambient[1],
            lights,
            views,
            // The top eighth is sky all the way; the bottom quarter ground.
            sky: luma(&pixels, 0..HEIGHT / 8),
            ground: luma(&pixels, HEIGHT * 3 / 4..HEIGHT),
        };
        if let Some(dir) = trace {
            image::RgbaImage::from_raw(WIDTH, HEIGHT, pixels)
                .expect("image")
                .save(dir.join(format!("frame-{n:04}.png")))
                .expect("save frame");
        }
        out.push(traced);
    }
    if let Some(dir) = trace {
        let mut csv = String::from(
            "frame,wall,height,exposure,wb_r,wb_g,wb_b,ambient_g,lights,views,sky_luma,ground_luma\n",
        );
        for (n, t) in out.iter().enumerate() {
            csv += &format!(
                "{n},{:.4},{:.1},{:.6},{:.6},{:.6},{:.6},{:.6},{},{},{:.3},{:.3}\n",
                n as f64 / FPS,
                t.height,
                t.exposure,
                t.grade[0],
                t.grade[1],
                t.grade[2],
                t.ambient,
                t.lights,
                t.views,
                t.sky,
                t.ground
            );
        }
        std::fs::write(dir.join("trace.csv"), csv).expect("write trace");
    }
    out
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
    let traced = fly(&gpu, &eyes, trace.as_deref());
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
