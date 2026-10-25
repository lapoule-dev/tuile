// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! GPU in the loop: the globe in a Bevy application with no window, drawn
//! into an image and read back.
//!
//! What must hold in a host's frame that no frame of tuile's own tests:
//!
//! - **no black ground**: the clear colour is black here, so any pixel the
//!   ground leaves uncovered is black — from the first frame, before the
//!   session is up, and once it has settled;
//! - **one depth**: a cube of the host's above the ground is seen, a cube
//!   buried under it is not — the host's meshes, in Bevy's own pipelines,
//!   depth-tested against the ground the globe's pass wrote;
//! - **no seam**: no sample of the backdrop between two tiles;
//! - in every target a Bevy camera renders into: with and without MSAA, in
//!   8-bit sRGB and in HDR.
//!
//! What it does not cover: a host that draws *before* the globe (a depth
//! prepass), WebGL2, and a window's surface.

mod support;

use bevy::prelude::*;
use bevy::render::view::Msaa;
use support::{Headless, Target, LAT, LON};
use tuile_bevy::{local_frame, GeoAnchor, SessionState, TuileCamera};

const ALTITUDE: f64 = 3_000.0;
const FOVY_DEG: f64 = 35.0;

/// How long a frame may take to settle, wall seconds: a bound, not a wait.
/// A software rasterizer (lavapipe, on CI) draws a debug build at a frame or two a
/// second, and a tile's stitch slides over twelve frames: 240 s is what
/// it needs, and a GPU settles in seconds.
const SETTLE_S: f64 = 240.0;

/// Straight down over the map, north up the frame.
fn camera() -> TuileCamera {
    TuileCamera::looking(LON, LAT, ALTITUDE, 0.0, 90.0, FOVY_DEG)
}

/// Where a point `east`, `north` metres from the camera's nadir and `height`
/// metres up lands in a `size`-pixel frame.
fn pixel(east: f64, north: f64, height: f64, size: u32) -> (u32, u32) {
    let half = f64::from(size) / 2.0;
    let reach = (ALTITUDE - height) * (FOVY_DEG.to_radians() / 2.0).tan();
    (
        (half + east / reach * half) as u32,
        (half - north / reach * half) as u32,
    )
}

const METRES_PER_DEG_LON: f64 = 111_319.5;
const METRES_PER_DEG_LAT: f64 = 110_574.3;

/// A cube of the host's, `side` metres, stood up at `east`/`north` of the
/// nadir with its centre `height` metres up, in an unlit colour.
fn cube(app: &mut Headless, east: f64, north: f64, height: f64, side: f32, colour: Color) {
    let at = GeoAnchor::geodetic(
        LON + east / METRES_PER_DEG_LON,
        LAT + north / METRES_PER_DEG_LAT,
        height,
    );
    let world = app.world();
    let mesh = world
        .resource_mut::<Assets<Mesh>>()
        .add(Cuboid::new(side, side, side));
    let material = world
        .resource_mut::<Assets<StandardMaterial>>()
        .add(StandardMaterial {
            base_color: colour,
            unlit: true,
            ..default()
        });
    world.spawn((
        Mesh3d(mesh),
        MeshMaterial3d(material),
        at,
        Transform::from_rotation(local_frame(at.0)),
    ));
}

fn is_red(p: [u8; 3]) -> bool {
    p[0] > 200 && p[1] < 60 && p[2] < 60
}

fn is_ground(p: [u8; 3]) -> bool {
    p[1] > p[0] && p[1] > p[2] && p[1] > 60
}

fn targets() -> [Target; 4] {
    let t = |msaa, hdr| Target {
        msaa,
        hdr,
        size: 256,
    };
    [
        t(Msaa::Sample4, false),
        t(Msaa::Off, false),
        t(Msaa::Sample4, true),
        t(Msaa::Off, true),
    ]
}

fn out_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

/// The ground is there from the first frame: the backdrop covers the planet
/// before the session has answered anything.
#[test]
fn the_first_frame_is_not_black() {
    if !support::has_gpu() {
        eprintln!("no GPU: skipped");
        return;
    }
    for target in targets() {
        let mut app = Headless::new(support::plugin(), camera(), target);
        app.update();
        let picture = app.picture();
        let black = picture.black_pixels(8);
        picture.save(&out_dir().join(format!(
            "first-frame-{:?}-hdr{}.png",
            target.msaa, target.hdr
        )));
        assert_eq!(
            black, 0,
            "{target:?}: {black} black pixels in the first frames"
        );
    }
}

/// Settled, the frame is all ground, the host's cube above it is seen and
/// the one buried under it is not.
#[test]
fn the_host_shares_the_grounds_depth_and_no_ground_is_black() {
    if !support::has_gpu() {
        eprintln!("no GPU: skipped");
        return;
    }
    for target in targets() {
        let mut app = Headless::new(support::plugin(), camera(), target);
        // Above the ground, west of the nadir: in front of it.
        cube(
            &mut app,
            -445.0,
            0.0,
            400.0,
            300.0,
            Color::srgb(1.0, 0.0, 0.0),
        );
        // Under the hill's flank, east of the nadir: its top 150 m below
        // the ground, hidden by it.
        cube(
            &mut app,
            445.0,
            -220.0,
            -300.0,
            300.0,
            Color::srgb(0.0, 0.0, 1.0),
        );
        let status = app.settle(SETTLE_S);
        assert_eq!(status.session, SessionState::Running, "{status:?}");
        assert!(status.is_settled(), "{target:?}: never settled: {status:?}");
        let picture = app.picture();
        picture.save(&out_dir().join(format!("settled-{:?}-hdr{}.png", target.msaa, target.hdr)));

        let black = picture.black_pixels(8);
        assert_eq!(black, 0, "{target:?}: {black} black pixels");
        let (rx, ry) = pixel(-445.0, 0.0, 550.0, picture.width);
        assert!(
            is_red(picture.at(rx, ry)),
            "{target:?}: the cube in front of the ground is not seen at ({rx}, {ry}): {:?}",
            picture.at(rx, ry)
        );
        let (bx, by) = pixel(445.0, -220.0, 0.0, picture.width);
        assert!(
            is_ground(picture.at(bx, by)),
            "{target:?}: the buried cube shows through the ground at ({bx}, {by}): {:?}",
            picture.at(bx, by)
        );
        // And the ground it is buried under is ground everywhere around it.
        let blue = picture
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[2] > 150 && p[0] < 80 && p[1] < 80)
            .count();
        assert_eq!(blue, 0, "{target:?}: {blue} pixels of the buried cube");
        // Nor any of the backdrop between two tiles: the greens hold no
        // more blue than 90, the backdrop 134, and a sample of it blended
        // into an edge pixel shows as a bluish hairline.
        let backdrop = picture
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[2] > 100 && p[0] < 200)
            .count();
        assert_eq!(
            backdrop, 0,
            "{target:?}: {backdrop} pixels of backdrop between tiles"
        );
    }
}

/// Settled, the ground the host reads is the ground drawn: heights from the
/// drawn tiles at their level, the relief's to its sampling, stable while
/// nothing changes; and the ray through the middle of the frame meets it
/// under the camera.
#[test]
fn the_host_reads_the_ground_it_sees() {
    if !support::has_gpu() {
        eprintln!("no GPU: skipped");
        return;
    }
    use support::Hill;
    use tuile_bevy::{LonLat, TuileGround};
    use tuile_terrain::Heights;
    let target = Target {
        msaa: Msaa::Off,
        hdr: false,
        size: 256,
    };
    let mut app = Headless::new(support::plugin(), camera(), target);
    let status = app.settle(SETTLE_S);
    assert!(status.is_settled(), "never settled: {status:?}");
    // A frame more, so the snapshot is the settled draw's.
    app.update();
    let ground = app.world().resource::<TuileGround>().clone();
    // 2 500 places over the frame's ground.
    let places: Vec<LonLat> = (0..50)
        .flat_map(|i| {
            (0..50).map(move |j| {
                LonLat::new(
                    LON - 0.007 + 0.014 * f64::from(i) / 49.0,
                    LAT - 0.007 + 0.014 * f64::from(j) / 49.0,
                )
            })
        })
        .collect();
    let started = std::time::Instant::now();
    let heights = ground.ground_heights(&places);
    let took = started.elapsed();
    let mut worst = 0.0f64;
    for (place, h) in places.iter().zip(&heights) {
        assert!(h.drawn, "{place:?} is on screen: {h:?}");
        assert!(h.level.is_some());
        worst = worst.max((h.height - Hill.height(place.lon, place.lat)).abs());
    }
    assert!(worst < 2.0, "{worst} m from the relief");
    eprintln!(
        "2 500 heights in {took:?}, {} tiles, worst {worst:.3} m off the relief",
        ground.tiles()
    );
    // Nothing changed: the same answers, the same generations.
    app.update();
    let again = ground.ground_heights(&places);
    assert_eq!(heights, again);

    // The middle of the frame, straight down: under the camera.
    let world = app.world();
    let mut cameras = world.query::<(&Camera, &GlobalTransform)>();
    let (camera, transform) = cameras.single(world).expect("one camera");
    let origin = *world.resource::<tuile_bevy::FloatingOrigin>();
    let hit = ground
        .pick_viewport(camera, transform, &origin, Vec2::splat(128.0))
        .expect("the ground under the camera");
    assert!(
        (hit.lon - LON).abs() < 1e-4 && (hit.lat - LAT).abs() < 1e-4,
        "{hit:?}"
    );
    let under = ground.height_at(LonLat::new(hit.lon, hit.lat));
    assert!(
        (hit.height - under.height).abs() < 0.05,
        "{hit:?} vs {under:?}"
    );
}

/// With the host's atmosphere on, the sky is drawn over what was space and
/// the ground is still ground; the sun the host reads back is the one it
/// set; turned off at a later frame, the sky goes.
#[test]
fn the_hosts_atmosphere_draws_a_sky_and_says_where_the_sun_is() {
    if !support::has_gpu() {
        eprintln!("no GPU: skipped");
        return;
    }
    use tuile_bevy::{Lights, TuileAtmosphere};
    use tuile_core::geo::{enu_frame, Geodetic};
    let target = Target {
        msaa: Msaa::Off,
        hdr: false,
        size: 256,
    };
    // Towards the horizon, from 3 km: sky in the top of the frame, ground
    // in the bottom.
    let horizon = TuileCamera::looking(LON, LAT, ALTITUDE, 0.0, 8.0, FOVY_DEG);
    let mut app = Headless::new(support::plugin(), horizon, target);
    // The sun high in the north-east, as the host sets it.
    let enu = enu_frame(Geodetic {
        lon: LON.to_radians(),
        lat: LAT.to_radians(),
        height: 0.0,
    });
    let to_sun = (enu.col(2) * 2.0 + enu.col(0) + enu.col(1)).normalize();
    app.world().insert_resource(
        TuileAtmosphere::default()
            .enabled(true)
            .with_lights(Lights::fixed(to_sun, None)),
    );
    let status = app.settle(SETTLE_S);
    assert!(status.is_settled(), "never settled: {status:?}");
    let picture = app.picture();
    picture.save(&out_dir().join("atmosphere-on.png"));
    let read = app.world().resource::<TuileAtmosphere>().to_sun();
    assert!((read - to_sun).length() < 1e-9, "{read} vs {to_sun}");
    let row = |p: &support::Picture, y: u32| -> [f64; 3] {
        let mut sum = [0.0; 3];
        for x in 0..p.width {
            let c = p.at(x, y);
            for k in 0..3 {
                sum[k] += f64::from(c[k]) / f64::from(p.width);
            }
        }
        sum
    };
    let sky = row(&picture, 4);
    assert!(
        sky[2] > 60.0 && sky[2] > sky[0],
        "a sky at the top of the frame: {sky:?}"
    );
    let black = picture.black_pixels(8);
    assert_eq!(black, 0, "{black} black pixels with the atmosphere on");

    // Off again: the clear colour where the sky was.
    app.world().resource_mut::<TuileAtmosphere>().enabled = false;
    for _ in 0..3 {
        app.update();
    }
    let picture = app.picture();
    picture.save(&out_dir().join("atmosphere-off.png"));
    let top = row(&picture, 4);
    assert!(
        top.iter().all(|c| *c < 8.0),
        "no sky once it is off: {top:?}"
    );
}
