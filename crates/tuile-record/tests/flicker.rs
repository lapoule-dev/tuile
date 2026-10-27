// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A session recorded with a flicker in its pictures — an exposure step
//! and the stars going out — exported and diagnosed: the sidecar carries
//! the recording's poses and clocks to the bit, the flicker's frame is
//! flagged and no other, and the film holds every picture.

use tuile_tape::session::{FrameContext, SkyState};
use tuile_tape::{Frame, Tape};

const SIZE: (u32, u32) = (64, 48);
const FRAMES: u64 = 40;
/// From this frame on, every light is a step brighter.
const STEP: u64 = 20;
/// From this frame on, there are no stars.
const DARK: u64 = 30;

/// Looking along the horizon from 2 km over the equator, drifting a metre
/// a frame: the lower part of the picture is ground.
fn camera(n: u64) -> Frame {
    Frame {
        position: [6_378_137.0 + 2_000.0, n as f64 * 1.000_000_1, 0.1 / 3.0],
        direction: [0.0, 1.0, 0.0],
        up: [1.0, 0.0, 0.0],
        fovy: 45f64.to_radians(),
    }
}

fn context(n: u64) -> FrameContext {
    let wall = 1_782_043_200.0 + n as f64 / 30.0 + 1e-7;
    FrameContext {
        wall,
        instant: wall + 3_600.0 / 7.0,
        rate: 0.0,
        offset: 3_600.0 / 7.0,
        viewport: [SIZE.0 * 2, SIZE.1 * 2],
        scale: 2.0,
        render_origin: camera(n).position,
    }
}

/// A dark sky with stars over grey ground, `gain` brighter.
fn picture(n: u64) -> Vec<u8> {
    let gain = if n >= STEP { 1.4 } else { 1.0 };
    let (w, h) = SIZE;
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let sky = y < h / 2;
            let star = sky && n < DARK && x % 8 == 3 && y % 6 == 2;
            let v: f64 = if star {
                230.0
            } else if sky {
                40.0
            } else {
                110.0
            };
            let v = (v * gain).min(255.0) as u8;
            rgba.extend_from_slice(&[v, v, v, 255]);
        }
    }
    rgba
}

fn png(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    image::ImageEncoder::write_image(
        image::codecs::png::PngEncoder::new(&mut out),
        rgba,
        SIZE.0,
        SIZE.1,
        image::ExtendedColorType::Rgba8,
    )
    .expect("encodes");
    out
}

fn record(path: &std::path::Path) {
    let mut tape = Tape::recording(path).expect("records");
    for n in 0..FRAMES {
        tape.push(camera(n));
        tape.push_context(&context(n));
        tape.push_sky(&SkyState {
            light_updates: n / 10,
            ..SkyState::default()
        });
        // A picture lands a frame late, as one read back from the GPU does.
        if n > 0 {
            tape.push_picture(n - 1, "png", &png(&picture(n - 1)));
        }
    }
    tape.push_picture(FRAMES - 1, "png", &png(&picture(FRAMES - 1)));
    tape.finish().expect("closes");
}

#[test]
fn a_flicker_in_the_pictures_is_flagged_on_its_frame() {
    let dir = tempfile::tempdir().expect("temp dir");
    let session = dir.path().join("session.mcap");
    let parquet = dir.path().join("session.parquet");
    let mp4 = dir.path().join("session.mp4");
    record(&session);
    let exported = tuile_record::export(
        &session,
        Some(&mp4),
        Some(&parquet),
        tuile_record::film::Codec::Av1,
        0,
        true,
    )
    .expect("exports");
    assert_eq!(exported.filmed, FRAMES as u32);
    assert!(std::fs::metadata(&mp4).expect("a film").len() > 0);

    // The sidecar is the recording's, to the bit.
    let rows = tuile_record::sidecar::read(&parquet).expect("reads");
    assert_eq!(rows.len(), FRAMES as usize);
    for (n, row) in rows.iter().enumerate() {
        let n = n as u64;
        assert_eq!(row.frame, n);
        let (c, x) = (camera(n), context(n));
        for (a, b) in row.position.iter().zip(c.position) {
            assert_eq!(a.to_bits(), b.to_bits(), "frame {n}");
        }
        assert_eq!(row.direction, c.direction);
        assert_eq!(row.fovy.to_bits(), c.fovy.to_bits());
        assert_eq!(row.wall.to_bits(), x.wall.to_bits(), "frame {n}");
        assert_eq!(row.instant.to_bits(), x.instant.to_bits(), "frame {n}");
        assert_eq!(row.viewport, x.viewport);
        assert_eq!(row.light_updates, Some(n / 10));
        assert!(row.pictured);
        assert_eq!(row.picture, [SIZE.0, SIZE.1]);
        assert_eq!(row.stars > 0, n < DARK, "frame {n}: {} stars", row.stars);
    }
    // The step, and the stars going out, each on its own frame.
    let flagged = tuile_record::flag(
        &rows,
        &tuile_record::Bounds {
            min_pixels: 100,
            min_stars: 5,
            ..tuile_record::Bounds::default()
        },
    );
    let frames: Vec<u64> = flagged.iter().map(|f| rows[f.row].frame).collect();
    assert_eq!(frames, vec![STEP, DARK], "{flagged:?}");
    assert!(flagged[0]
        .why
        .iter()
        .any(|w| matches!(w, tuile_record::Why::Sky(d) if *d > 0.03)));
    assert!(flagged[1]
        .why
        .iter()
        .any(|w| matches!(w, tuile_record::Why::Stars(_, 0))));
    // The link back holds the instant, the clock held.
    let link = tuile_record::link(&rows[STEP as usize]);
    assert!(link.starts_with("tuile://goto?lon="), "{link}");
    assert!(
        link.contains("&time=2026-") && link.ends_with("&timeRate=0"),
        "{link}"
    );

    // The plots are drawn.
    let (rgba, (w, h)) = tuile_record::plots::grid(&rows, &[STEP as usize]);
    assert_eq!(rgba.len(), (w * h * 4) as usize);
    assert!(
        rgba.as_chunks::<4>()
            .0
            .iter()
            .any(|p| p[..3] == [220, 30, 30]),
        "a red dot"
    );
}

/// The film's tape of a recording flies the same cameras, frame for frame,
/// under a clock through the recorded instants.
#[test]
fn the_film_tape_flies_the_recorded_poses_at_their_instants() {
    let dir = tempfile::tempdir().expect("temp dir");
    let session = dir.path().join("session.mcap");
    let film = dir.path().join("film.mcap");
    record(&session);
    let made = tuile_record::film_tape(&session, &film).expect("writes");
    assert_eq!(made.frames, FRAMES);
    assert!(made.worst_instant < 1e-6, "{made:?}");
    let clock = tuile_tape::Clock::of_tape(&film)
        .expect("reads")
        .expect("a clock");
    assert_eq!(Some(clock), made.clock);
    let mut tape = Tape::replaying(&film).expect("replays");
    for n in 0..FRAMES {
        assert_eq!(tape.next_frame(), Some(camera(n)), "frame {n}");
        assert!((clock.instant(n as u32 + 1) - context(n).instant).abs() < 1e-6);
    }
    assert_eq!(tape.next_frame(), None);
}

/// A step the camera explains — a turn between the two pictures — is not a
/// flicker; the same step with the camera held is.
#[test]
fn a_step_the_camera_explains_is_not_flagged() {
    let held = tuile_record::Row {
        pictured: true,
        sky_luma: 0.2,
        sky_pixels: 50_000,
        altitude: 3_000.0,
        direction: [0.0, 1.0, 0.0],
        up: [1.0, 0.0, 0.0],
        ..tuile_record::Row::default()
    };
    let stepped = tuile_record::Row {
        frame: 1,
        sky_luma: 0.3,
        ..held.clone()
    };
    let bounds = tuile_record::Bounds::default();
    assert_eq!(
        tuile_record::flag(&[held.clone(), stepped.clone()], &bounds).len(),
        1
    );
    let turned = tuile_record::Row {
        direction: [0.0, 1.0, 0.1],
        ..stepped.clone()
    };
    assert!(tuile_record::flag(&[held.clone(), turned], &bounds).is_empty());
    let late = tuile_record::Row {
        frame: 9,
        ..stepped
    };
    assert!(tuile_record::flag(&[held, late], &bounds).is_empty());
}
