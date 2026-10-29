// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The stars the viewer shows, all the way round: the viewer's own frame,
//! headless, at one instant and one size, turned through every heading at
//! a few pitches and places. For each view, the point sources it shows
//! (pixels over their ring of neighbours by 4 code values, the on-screen
//! controls left out, in the top `--rows` share of the picture).
//!
//! The scan is written as MCAP (`<out>/scan.mcap`): per view, the camera's
//! pose (`/camera`, tuile-tape's), the scene's clock (`/clock`), the frame
//! itself (`/frame`, `foxglove.CompressedImage`) and the count (`/stars`,
//! with the view asked for). The table of counts alone is Parquet
//! (`<out>/scan.parquet`), each view beside the exposure the viewer's sky
//! meters for it (its own rays and calibration, in stops).
//!
//! ```text
//! TUILE_ION_TOKEN=… cargo run --release -p tuile-wgpu-viewer --example stars_scan -- \
//!     --at 2026-06-21T23:30:00Z --width 2880 --height 1620 --scale 2 --out /tmp/scan
//! ```

#[allow(dead_code)]
#[path = "../headless/catalog.rs"]
mod catalog;

use std::path::PathBuf;
use std::sync::Arc;

use arrow_array::{Float64Array, RecordBatch, UInt32Array};
use arrow_schema::{DataType, Field, Schema};
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};

use tuile_atmosphere::clock::parse_rfc3339;
use tuile_viewer::control::{Action, Headless, Start, View};

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// Point sources: pixels whose brightest channel is the greatest of their
/// 3×3 and over the brightest of their ring at distance 3 by `over`.
fn points(rgba: &[u8], (w, h): (u32, u32), over: i32, skip_right: u32, rows: u32) -> usize {
    let at = |x: u32, y: u32| -> i32 {
        let i = ((y * w + x) * 4) as usize;
        i32::from(rgba[i].max(rgba[i + 1]).max(rgba[i + 2]))
    };
    let mut n = 0;
    for y in 3..rows.min(h) - 3 {
        for x in 3..w.saturating_sub(skip_right + 3) {
            let v = at(x, y);
            let mut top = true;
            for dy in 0..3 {
                for dx in 0..3 {
                    if at(x + dx - 1, y + dy - 1) > v {
                        top = false;
                    }
                }
            }
            if !top {
                continue;
            }
            let mut ring = 0;
            for k in 0..7 {
                ring = ring
                    .max(at(x + k - 3, y - 3))
                    .max(at(x + k - 3, y + 3))
                    .max(at(x - 3, y + k - 3))
                    .max(at(x + 3, y + k - 3));
            }
            if v >= ring + over {
                n += 1;
            }
        }
    }
    n
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let number = |name: &str, or: f64| flag(&args, name).map_or(or, |v| v.parse().expect(name));
    let instants: Vec<f64> = flag(&args, "--at")
        .unwrap_or("2026-06-21T23:30:00Z")
        .split(',')
        .map(|t| parse_rfc3339(t).expect("--at"))
        .collect();
    let at = instants[0];
    let size = (
        number("--width", 2880.0) as u32,
        number("--height", 1620.0) as u32,
    );
    let scale = number("--scale", 2.0);
    let out = PathBuf::from(flag(&args, "--out").unwrap_or("scan"));
    let step = number("--step", 15.0);
    let pitches: Vec<f64> = flag(&args, "--pitches")
        .unwrap_or("0,-30,-60")
        .split(',')
        .map(|p| p.parse().expect("--pitches"))
        .collect();
    // The Pic, and two other places: the plain north of it, and far away.
    // lon:lat:altitude[:ground], the ground's height over the ellipsoid
    // under the eye for the meter (100 m under the eye by default).
    let places: Vec<(f64, f64, f64, f64)> = flag(&args, "--places")
        .unwrap_or("0.1418:42.9365:3100:2877,0.10:43.30:400,2.35:48.85:1200")
        .split(',')
        .map(|p| {
            let v: Vec<f64> = p.split(':').map(|x| x.parse().expect("--places")).collect();
            (v[0], v[1], v[2], v.get(3).copied().unwrap_or(v[2] - 100.0))
        })
        .collect();
    std::fs::create_dir_all(&out).expect("out");
    let proxy = catalog::Proxy::serve().expect("proxy");
    let dir = tempfile::Builder::new()
        .prefix("tuile-scan-")
        .tempdir()
        .expect("tmp");
    let first = places[0];
    let mut run = Headless::start(Start {
        view: View {
            lon: first.0,
            lat: first.1,
            altitude: first.2,
            heading: 0.0,
            pitch: 0.0,
        },
        size,
        scale,
        lit_at: at,
        ..Start::new(
            proxy.host(dir.path().join("http")),
            dir.path().join("tiles"),
        )
    })
    .expect("start");
    let skip_right = (size.0 as f64 * 0.12) as u32;
    let rows = (f64::from(size.1) * number("--rows", 1.0)) as u32;
    let mut tape = tuile_tape::Tape::recording(out.join("scan.mcap")).expect("scan.mcap");
    let mut table: Vec<[f64; 7]> = Vec::new();
    let mut counts: Vec<u32> = Vec::new();
    for (t, &instant) in instants.iter().enumerate() {
        run.act(&Action::Time { instant, rate: 0.0 });
        for (p, place) in places.iter().enumerate() {
            for &pitch in &pitches {
                let mut heading = 0.0;
                while heading < 360.0 - 1e-6 {
                    run.act(&Action::Go(View {
                        lon: place.0,
                        lat: place.1,
                        altitude: place.2,
                        heading,
                        pitch,
                    }));
                    // A view asked for is drawn a frame or two later: the
                    // picture is read once it is.
                    run.steps(6);
                    run.steps(6);
                    let (pixels, (w, h)) = run.picture(0).expect("a picture");
                    let n = points(pixels, (w, h), 4, skip_right, rows);
                    println!("{t} {p} pitch {pitch} heading {heading}: {n}");
                    // The pose asked for: heading from north, pitch below
                    // the horizon, the viewer's 45° field.
                    let g = Geodetic {
                        lon: place.0.to_radians(),
                        lat: place.1.to_radians(),
                        height: place.2,
                    };
                    let enu = enu_frame(g);
                    let (hd, pt) = (heading.to_radians(), -pitch.to_radians());
                    let level = enu.x_axis * hd.sin() + enu.y_axis * hd.cos();
                    let forward = level * pt.cos() + enu.z_axis * pt.sin();
                    let up = (enu.z_axis - forward * enu.z_axis.dot(forward)).normalize();
                    tape.push(tuile_tape::Frame {
                        position: geodetic_to_ecef(g).to_array(),
                        direction: forward.to_array(),
                        up: up.to_array(),
                        fovy: 45f64.to_radians(),
                    });
                    tape.push_json(
                        "/clock",
                        "tuile.SceneClock",
                        CLOCK_SCHEMA,
                        &format!(r#"{{"instant":{instant},"rate":0}}"#),
                    );
                    tape.push_json(
                        "/stars",
                        "tuile.StarCount",
                        STARS_SCHEMA,
                        &format!(
                            r#"{{"points":{n},"lon":{},"lat":{},"altitude":{},"heading":{heading},"pitch":{pitch}}}"#,
                            place.0, place.1, place.2
                        ),
                    );
                    tape.push_image(pixels, w, h);
                    let ev = metered_ev(
                        g,
                        place.3,
                        forward,
                        up,
                        instant,
                        f64::from(w) / f64::from(h),
                    );
                    table.push([instant, place.0, place.1, place.2, heading, pitch, ev]);
                    counts.push(n as u32);
                    if flag(&args, "--save").is_some() {
                        let path = out.join(format!("t{t}-p{p}-pitch{pitch}-h{heading:03}.png"));
                        image::save_buffer(&path, pixels, w, h, image::ExtendedColorType::Rgba8)
                            .expect("save");
                    }
                    heading += step;
                }
            }
        }
    }
    run.finish().expect("finish");
    tape.finish().expect("scan.mcap");
    write_table(&out.join("scan.parquet"), &table, &counts);
    println!("{}", out.join("scan.mcap").display());
}

/// The exposure the viewer's sky meters for this view, in stops
/// (`log2` of `SkyLook::exposure_scale`): its own metering rays over the
/// frame (`hillaire::metering`) and its own calibration — as `sky.rs`
/// works it out, from the place's ground.
fn metered_ev(
    eye: Geodetic,
    ground_height: f64,
    forward: glam::DVec3,
    up: glam::DVec3,
    instant: f64,
    aspect: f64,
) -> f64 {
    use tuile_atmosphere::hillaire::{metering, Moment};
    let air = tuile_atmosphere::LutAtmosphere::today();
    let look = tuile_atmosphere::SkyLook::default();
    let ground = Geodetic {
        height: ground_height,
        ..eye
    };
    let right = forward.cross(up).normalize();
    let up = right.cross(forward);
    let half = (45f64.to_radians() * 0.5).tan();
    let metered = metering(
        geodetic_to_ecef(eye),
        forward,
        right * half * aspect,
        up * half,
        &tuile_atmosphere::AtmosphereFrame::at(ground),
        ground.height,
    );
    let light = air.light(
        &air,
        ground,
        Moment {
            to_sun_ecef: tuile_atmosphere::Sun::at_unix_seconds(instant).direction_ecef,
            moon: tuile_atmosphere::Moon::seen_from(instant, eye)
                .map(|m| tuile_atmosphere::Moonlight::of(&m)),
            compensation_ev: 0.0,
        },
        &metered,
        look.calibration,
    );
    f64::from(look.exposure_scale(&light)).log2()
}

const CLOCK_SCHEMA: &str = r#"{"type":"object","properties":{"instant":{"type":"number","description":"Unix seconds UTC"},"rate":{"type":"number"}}}"#;

const STARS_SCHEMA: &str = r#"{"type":"object","properties":{"points":{"type":"integer","description":"point sources over their ring by 4 code values"},"lon":{"type":"number"},"lat":{"type":"number"},"altitude":{"type":"number","description":"metres over the ellipsoid"},"heading":{"type":"number","description":"degrees from north"},"pitch":{"type":"number","description":"degrees below the horizon"}}}"#;

/// The counts as a Parquet table: one row a view.
fn write_table(path: &std::path::Path, rows: &[[f64; 7]], counts: &[u32]) {
    let names = [
        "instant_unix_s",
        "lon_deg",
        "lat_deg",
        "altitude_m",
        "heading_deg",
        "pitch_deg",
        "metered_exposure_ev",
    ];
    let mut fields: Vec<Field> = names
        .iter()
        .map(|n| Field::new(*n, DataType::Float64, false))
        .collect();
    fields.push(Field::new("points", DataType::UInt32, false));
    let schema = Arc::new(Schema::new(fields));
    let mut columns: Vec<Arc<dyn arrow_array::Array>> = (0..7)
        .map(|c| Arc::new(Float64Array::from_iter_values(rows.iter().map(|r| r[c]))) as _)
        .collect();
    columns.push(Arc::new(UInt32Array::from_iter_values(
        counts.iter().copied(),
    )));
    let batch = RecordBatch::try_new(schema.clone(), columns).expect("batch");
    let file = std::fs::File::create(path).expect("scan.parquet");
    let mut writer = parquet::arrow::ArrowWriter::try_new(file, schema, None).expect("writer");
    writer.write(&batch).expect("write");
    writer.close().expect("close");
}
