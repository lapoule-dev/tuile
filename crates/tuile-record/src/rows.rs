// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! One row a frame: what the recording says of it, and what its picture
//! measures.

use tuile_tape::scan::{picture_stats, PictureStats};
use tuile_tape::session::{Picture, Recorded};

/// A frame of a recorded session, measured. The sidecar's columns, one
/// field each (see [`crate::sidecar`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Row {
    /// The frame, as the tape counts from 0.
    pub frame: u64,
    /// The wall instant it was drawn at, the scene instant it was lit for,
    /// the clock's rate and `instant - wall`. NaN where the recording has
    /// no context for the frame.
    pub wall: f64,
    pub instant: f64,
    pub rate: f64,
    pub offset: f64,
    /// The frame, device pixels, and device pixels a point.
    pub viewport: [u32; 2],
    pub scale: f64,
    /// The origin the GPU's positions were relative to, ECEF metres, and
    /// how far it moved since the last frame (NaN for the first, or
    /// without context).
    pub render_origin: [f64; 3],
    pub origin_step: f64,
    /// The camera, ECEF metres and unit vectors; the vertical field, radians.
    pub position: [f64; 3],
    pub direction: [f64; 3],
    pub up: [f64; 3],
    pub fovy: f64,
    /// The eye as a person reads it: degrees, metres above the ellipsoid,
    /// degrees clockwise from north, degrees below the horizon.
    pub lon: f64,
    pub lat: f64,
    pub altitude: f64,
    pub heading: f64,
    pub pitch: f64,
    /// The atmosphere's state ([`tuile_tape::session::SkyState`]); NaN or
    /// `None` without one.
    pub exposure: Option<f64>,
    pub white_balance: [f64; 3],
    pub ambient: [f64; 3],
    pub illuminance: [f64; 3],
    pub to_sun: [f64; 3],
    pub light_updates: Option<u64>,
    pub view_updates: Option<u64>,
    /// The input events before the frame, `; `-separated.
    pub input: String,
    /// Whether the frame has a picture, and its measures (zero when not).
    pub pictured: bool,
    pub picture: [u32; 2],
    pub luma: f64,
    pub sky_luma: f64,
    pub ground_luma: f64,
    pub sky_pixels: u64,
    pub ground_pixels: u64,
    /// Mean absolute change of each pixel's luma since the last picture
    /// (0–1); NaN for the first, or across a change of size.
    pub delta: f64,
    /// The change of the sky's and the ground's mean luma since the last
    /// picture; NaN for the first.
    pub sky_delta: f64,
    pub ground_delta: f64,
    pub stars: u64,
    pub black: u64,
    pub black_ground: u64,
}

impl Row {
    /// What the recording says of `frame`, before its picture is measured.
    pub fn of(frame: &Recorded) -> Self {
        let c = &frame.camera;
        let camera = tuile_camera::GlobeCamera {
            position: glam::DVec3::from_array(c.position),
            direction: glam::DVec3::from_array(c.direction),
            up: glam::DVec3::from_array(c.up),
            fovy: c.fovy,
        };
        let eye = tuile_core::geo::ecef_to_geodetic(camera.position);
        let nan3 = [f64::NAN; 3];
        let mut row = Self {
            frame: frame.index,
            wall: f64::NAN,
            instant: f64::NAN,
            rate: f64::NAN,
            offset: f64::NAN,
            scale: f64::NAN,
            render_origin: [f64::NAN; 3],
            origin_step: f64::NAN,
            position: c.position,
            direction: c.direction,
            up: c.up,
            fovy: c.fovy,
            lon: eye.lon.to_degrees(),
            lat: eye.lat.to_degrees(),
            altitude: eye.height,
            heading: camera.heading().to_degrees().rem_euclid(360.0),
            pitch: camera.pitch().to_degrees(),
            white_balance: nan3,
            ambient: nan3,
            illuminance: nan3,
            to_sun: nan3,
            input: frame.input.join("; "),
            delta: f64::NAN,
            sky_delta: f64::NAN,
            ground_delta: f64::NAN,
            ..Self::default()
        };
        if let Some(x) = frame.context {
            row.wall = x.wall;
            row.instant = x.instant;
            row.rate = x.rate;
            row.offset = x.offset;
            row.viewport = x.viewport;
            row.scale = x.scale;
            row.render_origin = x.render_origin;
        }
        if let Some(s) = frame.sky {
            row.exposure = s.exposure;
            row.white_balance = s.white_balance;
            row.ambient = s.ambient;
            row.illuminance = s.illuminance;
            row.to_sun = s.to_sun;
            row.light_updates = Some(s.light_updates);
            row.view_updates = Some(s.view_updates);
        }
        row
    }

    /// The camera, as the tape has it.
    pub fn camera(&self) -> tuile_tape::Frame {
        tuile_tape::Frame {
            position: self.position,
            direction: self.direction,
            up: self.up,
            fovy: self.fovy,
        }
    }
}

/// Measures the pictures of a session, frame by frame, keeping the last
/// picture's luma for the frame-to-frame change.
#[derive(Default)]
pub struct Measure {
    last: Option<(Vec<f32>, (u32, u32), PictureStats)>,
}

impl Measure {
    /// Fills `row`'s picture columns from `rgba` of `size`, drawn under
    /// `row`'s camera.
    pub fn picture(&mut self, row: &mut Row, rgba: &[u8], size: (u32, u32)) {
        let stats = picture_stats(rgba, size.0, size.1, &row.camera());
        let luma: Vec<f32> = rgba
            .chunks_exact(4)
            .map(|p| {
                (0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]))
                    / 255.0
            })
            .collect();
        row.pictured = true;
        row.picture = [size.0, size.1];
        row.luma = stats.luma;
        row.sky_luma = stats.sky_luma;
        row.ground_luma = stats.ground_luma;
        row.sky_pixels = stats.sky_pixels;
        row.ground_pixels = stats.ground_pixels;
        row.stars = stats.stars;
        row.black = stats.black;
        row.black_ground = stats.black_ground;
        if let Some((last, last_size, last_stats)) = &self.last {
            if *last_size == size {
                let sum: f64 = luma
                    .iter()
                    .zip(last)
                    .map(|(a, b)| f64::from((a - b).abs()))
                    .sum();
                row.delta = sum / luma.len().max(1) as f64;
            }
            row.sky_delta = stats.sky_luma - last_stats.sky_luma;
            row.ground_delta = stats.ground_luma - last_stats.ground_luma;
        }
        self.last = Some((luma, size, stats));
    }
}

/// Every frame of the session, each with its picture's measures where it
/// has one; `each` is handed every picture decoded, in order, for an
/// encoder.
pub fn measure(
    path: &std::path::Path,
    mut each: impl FnMut(&Row, &[u8], (u32, u32)) -> Result<(), String>,
) -> Result<Vec<Row>, String> {
    let frames = tuile_tape::session::read(path).map_err(|e| e.to_string())?;
    let mut rows: Vec<Row> = frames.iter().map(Row::of).collect();
    for i in 1..rows.len() {
        let (a, b) = (rows[i - 1].render_origin, rows[i].render_origin);
        rows[i].origin_step =
            ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt();
    }
    let mut measure = Measure::default();
    tuile_tape::session::for_each_picture(path, |picture: Picture| {
        let Some(row) = rows.get_mut(picture.index as usize) else {
            return Err(tuile_tape::TapeError::NotAPath(format!(
                "a picture of frame {}, past the camera's {} frames",
                picture.index,
                frames.len()
            )));
        };
        let (rgba, size) = picture.rgba()?;
        measure.picture(row, &rgba, size);
        each(row, &rgba, size).map_err(tuile_tape::TapeError::NotAPath)
    })
    .map_err(|e| e.to_string())?;
    Ok(rows)
}
