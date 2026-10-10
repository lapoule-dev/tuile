// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Camera paths computed after the fact: smoothing, clearances, bounded turns.
//!
//! [`crate::CameraController`] answers gestures as they arrive. This module is
//! for the other kind of camera — one that follows a **subject** along a
//! **route** that is already known in full, sampled at a constant period `dt`
//! (seconds), and that may therefore look as far ahead as behind.
//!
//! Everything here is a plain function over slices. Positions are f64 and are
//! meant to be ECEF (the engine's rule: f64 until rebasing); angles are
//! radians, headings clockwise from north like [`crate::GlobeCamera::heading`].
//! Every duration and every bound is **per second**, never per sample, so the
//! same path sampled at 30 Hz and at 60 Hz comes out the same.
//!
//! # What is here
//!
//! | need | function |
//! |---|---|
//! | a path that no longer shakes, with no delay | [`smooth_path`], [`smooth_series`] |
//! | a clearance that rises early and never undercuts | [`envelope`], [`envelope_margin`] |
//! | a smoothed aim that cannot lose its subject | [`leash`], [`leash_path`] |
//! | a heading that turns within a rate and an acceleration | [`pursue`], [`pursue_step`] |
//! | where a route is and how large | [`route_centre`], [`route_diagonal`], [`bearing`] |
//! | the camera for a subject, a heading, a pitch, a distance | [`follow_camera`] |
//!
//! # The filter is the `biquad` crate's
//!
//! This module designs no filter. It asks [`biquad`] for a second-order
//! low-pass of time constant `τ`, critically damped (`Q = ½`, two equal real
//! poles: it settles without ever overshooting, which a camera must not), and
//! runs it over the series forward, then backward over the result. Two passes
//! in opposite directions cancel each other's delay, and the path neither
//! trails its input nor runs ahead of it. In weights, that is
//!
//! ```text
//! w(k) ∝ (1 + |k|·dt/τ) · exp(−|k|·dt/τ)
//! ```
//!
//! What is written here is what the crate does not do: the two passes and the
//! state each one starts from (see [`smooth_series`]).

use std::collections::VecDeque;
use std::f64::consts::{PI, TAU};

use biquad::{Biquad, Coefficients, DirectForm1, Hertz, Type};
use glam::{DVec2, DVec3};
use tuile_core::geo::{ecef_to_geodetic, enu_frame};

use crate::GlobeCamera;

/// Two equal real poles: the fastest second-order response with no overshoot.
const CRITICALLY_DAMPED: f64 = 0.5;

/// The low-pass of time constant `time_constant` at one sample every `dt`;
/// `None` when there is nothing to smooth.
///
/// A low-pass of time constant τ has its corner at `1/(2πτ)`. A corner at or
/// above half the sampling rate (`τ ≤ dt/π`) is no filter at that cadence, and
/// comes back as `None` like a zero or negative `τ` does.
fn low_pass(dt: f64, time_constant: f64) -> Option<Coefficients<f64>> {
    if !(dt > 0.0 && dt.is_finite() && time_constant > 0.0 && time_constant.is_finite()) {
        return None;
    }
    let sampling = Hertz::from_hz(1.0 / dt).ok()?;
    let corner = Hertz::from_hz(1.0 / (TAU * time_constant)).ok()?;
    Coefficients::from_params(Type::LowPass, sampling, corner, CRITICALLY_DAMPED).ok()
}

/// How many samples `time_constants` of a filter last, rounded up.
fn samples_in(time_constants: f64, dt: f64, time_constant: f64) -> usize {
    (time_constants * time_constant / dt).ceil() as usize
}

/// One pass, in the order `values` come.
///
/// The filter starts at rest **on the first value**: the series is filtered
/// as its distance from that value, so a filter whose state is zero is a
/// filter that has been reading the first value for ever. There is then no
/// run-in at either end, where a subject that stands still really was a point
/// — and an ECEF coordinate of four million metres is not a step from zero.
fn pass(coefficients: Coefficients<f64>, values: impl Iterator<Item = f64>) -> Vec<f64> {
    let mut filter = DirectForm1::<f64>::new(coefficients);
    let mut rest = None;
    values
        .map(|v| {
            let rest = *rest.get_or_insert(v);
            rest + filter.run(v - rest)
        })
        .collect()
}

/// Both passes: forward, then backward over the result.
fn both_ways(coefficients: Coefficients<f64>, values: &[f64]) -> Vec<f64> {
    let forward = pass(coefficients, values.iter().copied());
    let mut back = pass(coefficients, forward.into_iter().rev());
    back.reverse();
    back
}

/// `values`, one every `dt` seconds, low-passed at `time_constant` seconds
/// with no delay.
///
/// # Guarantees
///
/// - **Zero phase.** The weights are symmetric, so a series that changes at a
///   steady rate is returned where it was: no lag, no lead. This holds away
///   from the two ends; within a few time constants of an end the series is
///   pulled toward its first (last) value, which the filter treats as having
///   been held for ever before (after) it.
/// - **No overshoot.** The weights are all positive: the output stays inside
///   the range of the input.
/// - **The level is kept**: the weights sum to one.
/// - **Cadence-independent.** `time_constant` is in seconds; the same signal
///   sampled at 30 Hz and at 60 Hz is smoothed onto the same curve.
///
/// A wobble of period `T` is attenuated by `1 / (1 + (2π·τ/T)²)²` — with
/// `τ = 1.5 s`, a 1.25 s wobble is divided by about 3000.
///
/// A `time_constant` that is zero, negative, not finite, or too short to mean
/// anything at this cadence (`≤ dt/π`) returns the values unchanged, as does a
/// `dt` that is not a positive number.
pub fn smooth_series(values: &[f64], dt: f64, time_constant: f64) -> Vec<f64> {
    match low_pass(dt, time_constant) {
        Some(coefficients) => both_ways(coefficients, values),
        None => values.to_vec(),
    }
}

/// A path, one point every `dt` seconds, low-passed at `time_constant` seconds
/// with no delay.
///
/// Each axis is filtered on its own, which for a linear filter is the same as
/// filtering the vectors: the result does not depend on the frame the points
/// are expressed in. The guarantees are [`smooth_series`]'s, per axis; in
/// particular a subject moving in a straight line at a steady speed keeps its
/// smoothed point exactly on itself.
///
/// Only the returned path is smooth. The points passed in are not touched:
/// smooth where a camera *looks*, and let the subject stay on its route.
pub fn smooth_path(points: &[DVec3], dt: f64, time_constant: f64) -> Vec<DVec3> {
    let Some(coefficients) = low_pass(dt, time_constant) else {
        return points.to_vec();
    };
    let axes: [Vec<f64>; 3] = std::array::from_fn(|axis| {
        let along: Vec<f64> = points.iter().map(|p| p[axis]).collect();
        both_ways(coefficients, &along)
    });
    (0..points.len())
        .map(|i| DVec3::new(axes[0][i], axes[1][i], axes[2][i]))
        .collect()
}

/// How far around a sample its ceiling is taken by [`envelope`], in time
/// constants.
///
/// The smoothed ceiling stands under a sharp peak by the weight the filter
/// gives to what lies beyond the ceiling's reach, and the sample's own value
/// then makes up the difference in one step. Six time constants out that
/// weight is `(2 + 6)·e⁻⁶ / 2`, one percent: thirty metres of clearance arrive
/// with a step of thirty centimetres. At three it would be twelve percent.
pub const ENVELOPE_TIME_CONSTANTS: f64 = 6.0;

/// How far past the samples it answers for a caller working in pieces must
/// read, in time constants, so that what lies beyond no longer weighs.
///
/// A recursive filter remembers everything before it, so a piece cannot
/// compute *exactly* what the whole series would; it computes it to within
/// the weight the filter still gives to what the piece did not read. Fifteen
/// time constants out, that weight is `(2 + 15)·e⁻¹⁵ / 4`, about a millionth.
pub const SETTLE_TIME_CONSTANTS: f64 = 15.0;

/// The number of samples to read on each side of a piece of a series for
/// [`envelope`] over the piece to agree with [`envelope`] over the whole.
///
/// A caller that cannot hold the whole series (or computes it in parallel
/// pieces) passes `values[first − margin ..= last + margin]`, clamped to the
/// series, and keeps the middle. The agreement is to about a millionth of the
/// largest value left unread — see [`SETTLE_TIME_CONSTANTS`]. Zero when
/// [`envelope`] would not smooth at all.
pub fn envelope_margin(dt: f64, time_constant: f64) -> usize {
    match low_pass(dt, time_constant) {
        Some(_) => samples_in(
            ENVELOPE_TIME_CONSTANTS + SETTLE_TIME_CONSTANTS,
            dt,
            time_constant,
        ),
        None => 0,
    }
}

/// The largest value within `reach` samples of each sample.
///
/// The classic monotonic queue: indices whose values can still be a maximum,
/// largest first. Each index enters and leaves once, so the whole is linear.
fn sliding_max(values: &[f64], reach: usize) -> Vec<f64> {
    let mut candidates: VecDeque<usize> = VecDeque::new();
    let mut out = Vec::with_capacity(values.len());
    let mut read = 0;
    for i in 0..values.len() {
        let end = (i + reach + 1).min(values.len());
        while read < end {
            while candidates
                .back()
                .is_some_and(|&j| values[j] <= values[read])
            {
                candidates.pop_back();
            }
            candidates.push_back(read);
            read += 1;
        }
        while candidates.front().is_some_and(|&j| j + reach < i) {
            candidates.pop_front();
        }
        out.push(candidates.front().map_or(values[i], |&j| values[j]));
    }
    out
}

/// A smooth curve that is **never under** `values`: a sliding maximum, then
/// the zero-phase low-pass, then each sample's own value as a floor.
///
/// This is how a clearance is smoothed — a height a camera must keep above
/// the ground, a margin it must keep from an obstacle. Smoothing such a need
/// directly would violate it: an average is under its largest term. So each
/// value is first raised to the **ceiling** of the values within
/// [`ENVELOPE_TIME_CONSTANTS`] time constants of it, and that envelope is what
/// is smoothed: the result rises *before* a peak and comes down after it,
/// instead of jumping when the peak arrives. And because a smoothed envelope
/// can still dip under a sharp peak (by about one percent of it), each sample
/// finally takes the larger of its smoothed value and its own.
///
/// # Guarantee
///
/// `envelope(values, ..)[i] >= values[i]` for every `i`, whatever the values,
/// `dt` and `time_constant` — the last step is a `max`, and no filter is
/// trusted with it. When there is nothing to smooth (see [`smooth_series`])
/// the values are returned unchanged.
pub fn envelope(values: &[f64], dt: f64, time_constant: f64) -> Vec<f64> {
    let Some(coefficients) = low_pass(dt, time_constant) else {
        return values.to_vec();
    };
    let reach = samples_in(ENVELOPE_TIME_CONSTANTS, dt, time_constant);
    let ceiling = sliding_max(values, reach);
    both_ways(coefficients, &ceiling)
        .into_iter()
        .zip(values)
        .map(|(smoothed, own)| smoothed.max(*own))
        .collect()
}

/// `point`, brought back to within `max_distance` of `subject`.
///
/// A smoothed aim no longer sits on its subject — that is the point of
/// smoothing it — but a subject that leaps (a bad position fix) or turns
/// sharply at speed can leave it far behind. The leash bounds how far: inside
/// the ball of radius `max_distance` the point is left alone, and outside it
/// is moved onto the ball along the line to the subject. The subject is free
/// inside the ball and is never pulled back to its centre.
///
/// # Guarantee
///
/// The result is at most `max_distance` from `subject` (to rounding), and is
/// `point` itself when `point` already is. A negative `max_distance` is zero.
pub fn leash(point: DVec3, subject: DVec3, max_distance: f64) -> DVec3 {
    subject + (point - subject).clamp_length_max(max_distance.max(0.0))
}

/// [`leash`], sample by sample: `points[i]` kept within `max_distance` of
/// `subjects[i]`. The result is as long as the shorter of the two.
pub fn leash_path(points: &[DVec3], subjects: &[DVec3], max_distance: f64) -> Vec<DVec3> {
    points
        .iter()
        .zip(subjects)
        .map(|(point, subject)| leash(*point, *subject, max_distance))
        .collect()
}

/// The smallest turn from the angle `from` to the angle `to`, radians, in
/// `[-π, π)`: positive the way angles grow.
pub fn turn_between(from: f64, to: f64) -> f64 {
    (to - from + PI).rem_euclid(TAU) - PI
}

/// A heading and how fast it is turning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Heading {
    /// Radians, **unwrapped**: it runs past 2π rather than jump back, so a
    /// series of them can be differenced.
    pub angle: f64,
    /// Radians per second.
    pub rate: f64,
}

impl Heading {
    /// A heading at `angle`, not turning.
    pub fn at_rest(angle: f64) -> Self {
        Self { angle, rate: 0.0 }
    }
}

/// How fast a heading may turn, and how fast that may change.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TurnLimits {
    /// Radians per second.
    pub max_rate: f64,
    /// Radians per second, per second.
    pub max_accel: f64,
}

/// One step of `dt` seconds of a heading pursuing `target` (radians, any
/// wrap), its rate and the change of its rate both bounded.
///
/// The heading turns toward the target the short way round, as fast as it
/// may — and no faster than it can still stop from: at an angle `e` of the
/// target, a turn that sheds `max_accel` a second comes to rest on it from
/// `√(2·max_accel·e)` (less half a step's worth, since it sheds it one sample
/// at a time). That is the whole rule. Far behind, the heading turns at
/// `max_rate`; near, it eases onto the target instead of crossing it; and a
/// target that swings away faster than the bounds allow is simply followed
/// late.
///
/// # Guarantees
///
/// For a heading that enters within the bounds:
/// `|rate| ≤ max_rate` and `|rate − heading.rate| ≤ max_accel·dt`, exactly.
/// A heading at rest on its target stays there. Negative limits are zero, and
/// a `dt` that is not positive returns the heading unchanged.
pub fn pursue_step(heading: Heading, target: f64, dt: f64, limits: TurnLimits) -> Heading {
    if dt.is_nan() || dt <= 0.0 {
        return heading;
    }
    let (max_rate, max_accel) = (limits.max_rate.max(0.0), limits.max_accel.max(0.0));
    let turn = turn_between(heading.angle, target);
    let half_step = max_accel * dt / 2.0;
    let can_stop_from = (half_step * half_step + 2.0 * max_accel * turn.abs()).sqrt() - half_step;
    let wanted = turn.signum() * can_stop_from.min(max_rate).min(turn.abs() / dt);
    let most = max_accel * dt;
    let rate =
        (heading.rate + (wanted - heading.rate).clamp(-most, most)).clamp(-max_rate, max_rate);
    Heading {
        angle: heading.angle + rate * dt,
        rate,
    }
}

/// The heading that pursues `targets`, one every `dt` seconds, within
/// `limits`: unwrapped radians, one per target.
///
/// It starts on the first target, at rest, and takes one [`pursue_step`] per
/// sample after it. Differenced, the result never turns faster than
/// `max_rate` nor changes its rate faster than `max_accel` — both per second,
/// so the same targets sampled at 30 Hz and at 60 Hz give the same heading at
/// the same instant, to within what the target itself turns over a sample or
/// two (the two differ only in how often they look at it).
pub fn pursue(targets: &[f64], dt: f64, limits: TurnLimits) -> Vec<f64> {
    let Some(&first) = targets.first() else {
        return Vec::new();
    };
    let mut heading = Heading::at_rest(first);
    let mut out = Vec::with_capacity(targets.len());
    out.push(heading.angle);
    for &target in &targets[1..] {
        heading = pursue_step(heading, target, dt, limits);
        out.push(heading.angle);
    }
    out
}

/// The centre of a route: the mean of `positions`, leaving out the samples
/// the subject spends stopped.
///
/// `positions[i]` is where the subject is at `times[i]` (seconds, ascending).
/// The mean is over the samples, so it weighs the route **as it was
/// sampled**: sample at a constant step of time and a stretch the subject
/// lingers on weighs as long as it lasted. A sample counts when the subject
/// reached it at `stopped_below` metres per second or more; a wait at a
/// start line, however long, then does not pull the centre to it. When
/// nothing moves at all, every sample counts.
///
/// Deliberately not the centre of the bounding box, which a single far
/// excursion drags away.
///
/// `None` for an empty route. The result is a mean of ECEF points: for a
/// route tens of kilometres across it lies a few metres under the surface
/// the route is on, which does not matter to a direction to look in.
pub fn route_centre(positions: &[DVec3], times: &[f64], stopped_below: f64) -> Option<DVec3> {
    let mean = |points: &[DVec3]| {
        (!points.is_empty()).then(|| points.iter().sum::<DVec3>() / points.len() as f64)
    };
    let moving: Vec<DVec3> = positions
        .windows(2)
        .zip(times.windows(2))
        .filter(|(p, t)| p[1].distance(p[0]) >= stopped_below * (t[1] - t[0]))
        .map(|(p, _)| p[1])
        .collect();
    mean(&moving).or_else(|| mean(positions))
}

/// How far across a route is on the ground: the diagonal of its bounding box
/// in the local east/north plane at its first point, metres.
///
/// Heights do not count — a climb is not an extent to frame — and the plane
/// is flat, which is right for the routes a single camera frames (the error
/// is the Earth's curvature over the route: a metre at 10 km, a hundred at
/// 100 km). Zero for fewer than two points.
pub fn route_diagonal(positions: &[DVec3]) -> f64 {
    let Some(&origin) = positions.first() else {
        return 0.0;
    };
    let frame = enu_frame(ecef_to_geodetic(origin));
    let (low, high) =
        positions
            .iter()
            .fold((DVec2::INFINITY, DVec2::NEG_INFINITY), |(low, high), p| {
                let at = DVec2::new(
                    (*p - origin).dot(frame.x_axis),
                    (*p - origin).dot(frame.y_axis),
                );
                (low.min(at), high.max(at))
            });
    (high - low).length()
}

/// Closer than this on the ground, metres, a point has no [`bearing`] to
/// another: every direction is as good as the next, and the one the last
/// millimetres would pick is noise.
pub const MIN_BEARING_DISTANCE: f64 = 1.0;

/// The compass bearing from `from` to `to`: radians clockwise from north in
/// `[0, 2π)`, read in the local east/north/up frame at `from`.
///
/// `None` when the two are within [`MIN_BEARING_DISTANCE`] of each other on
/// the ground. A caller following a bearing keeps its last one then — which
/// is what feeding [`pursue_step`] an unchanged target does.
pub fn bearing(from: DVec3, to: DVec3) -> Option<f64> {
    let frame = enu_frame(ecef_to_geodetic(from));
    let toward = to - from;
    let (east, north) = (toward.dot(frame.x_axis), toward.dot(frame.y_axis));
    (east.hypot(north) >= MIN_BEARING_DISTANCE).then(|| east.atan2(north).rem_euclid(TAU))
}

/// The camera that looks at `subject` along `heading`, from `pitch` above it
/// and `distance` away.
///
/// `heading` is the compass heading **of the view** (radians clockwise from
/// north), `pitch` how far below horizontal it looks (`π/2` = straight down),
/// both read in the local frame at the subject — the conventions of
/// [`GlobeCamera::from_geodetic`]. The eye therefore stands *behind* the
/// subject: at heading 0 it is south of it, looking north. `distance` is the
/// slant distance from eye to subject, metres.
///
/// To stand outside a route and look in, pass the [`bearing`] from the
/// subject to the route's centre as the heading (through [`pursue`], so that
/// it turns calmly when the subject passes near the centre).
///
/// The camera's `position` is the eye and it looks exactly at `subject`, with
/// the subject's local up as its up: no roll. Nothing here knows the ground;
/// keeping the eye out of terrain is the caller's, afterwards
/// ([`tuile_core::ground`]).
pub fn follow_camera(
    subject: DVec3,
    heading: f64,
    pitch: f64,
    distance: f64,
    fovy: f64,
) -> GlobeCamera {
    let frame = enu_frame(ecef_to_geodetic(subject));
    let (east, north, up) = (frame.x_axis, frame.y_axis, frame.z_axis);
    let forward = north * heading.cos() + east * heading.sin();
    let eye = subject - forward * (pitch.cos() * distance) + up * (pitch.sin() * distance);
    GlobeCamera::look_at(eye, subject, up, fovy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::geo::{geodetic_to_ecef, Geodetic};

    /// One and a half seconds, sampled sixty times a second.
    const TAU_S: f64 = 1.5;
    const DT: f64 = 1.0 / 60.0;

    /// A spot in the mountains, 1500 m up, and its local frame.
    fn spot() -> (DVec3, glam::DMat3) {
        let g = Geodetic {
            lon: 6.4_f64.to_radians(),
            lat: 45.1_f64.to_radians(),
            height: 1500.0,
        };
        (geodetic_to_ecef(g), enu_frame(g))
    }

    /// The point `east` and `north` metres from the spot, on its local plane.
    fn at(east: f64, north: f64) -> DVec3 {
        let (origin, frame) = spot();
        origin + frame.x_axis * east + frame.y_axis * north
    }

    // ── Smoothing ──────────────────────────────────────────────────────────

    #[test]
    fn nothing_to_smooth_returns_the_series_unchanged() {
        let values = [3.0, -1.0, 4.0, 1.0, -5.0];
        for (dt, tau) in [
            (DT, 0.0),
            (DT, -1.0),
            (DT, f64::NAN),
            (0.0, 1.0),
            (-DT, 1.0),
            (DT, DT / 4.0),
        ] {
            assert_eq!(smooth_series(&values, dt, tau), values, "dt {dt}, τ {tau}");
            assert_eq!(envelope(&values, dt, tau), values, "dt {dt}, τ {tau}");
            assert_eq!(envelope_margin(dt, tau), 0);
        }
        assert!(smooth_series(&[], DT, TAU_S).is_empty());
        assert!(smooth_path(&[], DT, TAU_S).is_empty());
    }

    /// The two passes are the kernel the module's head writes down: an
    /// impulse comes out as `(1 + |k|/τ)·e^(−|k|/τ)`, the same before as
    /// after, positive everywhere, summing to one.
    #[test]
    fn an_impulse_comes_out_as_the_symmetric_kernel() {
        let mut impulse = vec![0.0; 4001];
        impulse[2000] = 1.0;
        let out = smooth_series(&impulse, DT, TAU_S);
        let sum: f64 = out.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6, "it keeps the level: {sum}");
        for k in [0usize, 45, 90, 180, 360] {
            let (before, after) = (out[2000 - k], out[2000 + k]);
            assert!(
                (before - after).abs() < 1e-3 * out[2000],
                "{k} samples: {before} before, {after} after"
            );
            let x = k as f64 * DT / TAU_S;
            let expected = (1.0 + x) * (-x).exp() * out[2000];
            assert!(
                (after - expected).abs() < 0.01 * out[2000],
                "{k} samples out: {after}, expected {expected}"
            );
        }
        assert!(out.iter().all(|v| *v > -1e-12), "it never overshoots");
    }

    /// No phase: a path that moves at a steady pace is left exactly where it
    /// was. One pass alone would put it `2τ` of travel behind.
    #[test]
    fn a_steady_path_is_neither_delayed_nor_advanced() {
        let (origin, _) = spot();
        let velocity = DVec3::new(15.0, -6.0, 3.0);
        let path: Vec<DVec3> = (0..6000)
            .map(|i| origin + velocity * (f64::from(i) * DT))
            .collect();
        let out = smooth_path(&path, DT, TAU_S);
        for i in [2000usize, 3000, 4000] {
            let off = out[i].distance(path[i]);
            assert!(off < 1e-6, "sample {i} is {off} m off");
        }
    }

    /// A path that stands still is left standing from the first sample: the
    /// filter starts at rest on the path, not at the centre of the Earth.
    #[test]
    fn a_path_that_stands_still_is_left_standing_from_the_first_sample() {
        let (here, _) = spot();
        let out = smooth_path(&vec![here; 400], DT, TAU_S);
        assert!(out.iter().all(|p| p.distance(here) < 1e-9));
    }

    /// The largest third difference of a path, as a jerk in m/s³.
    fn worst_jerk(path: &[DVec3], dt: f64) -> f64 {
        path.windows(4)
            .map(|w| (w[3] - 3.0 * w[2] + 3.0 * w[1] - w[0]).length() / (dt * dt * dt))
            .fold(0.0, f64::max)
    }

    /// A subject drifting at 10 m/s while circling at `radius` once every
    /// `period` seconds, sampled every `dt`.
    fn spiral(samples: usize, dt: f64, radius: f64, period: f64) -> Vec<DVec3> {
        (0..samples)
            .map(|i| {
                let t = i as f64 * dt;
                let a = t * TAU / period;
                at(10.0 * t + radius * a.cos(), radius * a.sin())
            })
            .collect()
    }

    /// The case this was written for: a subject circling 60 m wide once every
    /// 1.25 s. Aimed at the subject, a camera is thrown about with it; aimed
    /// at the smoothed path, it is all but still. Theory says the circle is
    /// divided by `(1 + (2π·1.5/1.25)²)²` ≈ 3350; the assertion keeps a wide
    /// margin under it.
    #[test]
    fn a_tight_spiral_loses_its_jerk() {
        let path = spiral(3600, DT, 60.0, 1.25);
        let out = smooth_path(&path, DT, TAU_S);
        // Away from the ends, where the filter is still leaving its rest.
        let (raw, smooth) = (
            worst_jerk(&path[900..2700], DT),
            worst_jerk(&out[900..2700], DT),
        );
        assert!(raw > 7000.0, "the spiral itself jerks at {raw} m/s³");
        assert!(
            smooth * 1000.0 < raw,
            "jerk {raw} → {smooth} m/s³, only ÷{:.0}",
            raw / smooth
        );
        // And the smoothed point is the drift, with the circle gone.
        let off = (900..2700usize)
            .map(|i| out[i].distance(at(10.0 * i as f64 * DT, 0.0)))
            .fold(0.0, f64::max);
        assert!(off < 0.05, "the smoothed aim still circles by {off} m");
    }

    /// `τ` is in seconds: the same path sampled at 30 Hz and at 60 Hz is
    /// smoothed onto the same curve.
    #[test]
    fn smoothing_is_the_same_at_30_and_60_samples_a_second() {
        let fast = smooth_path(&spiral(3600, 1.0 / 60.0, 60.0, 4.0), 1.0 / 60.0, TAU_S);
        let slow = smooth_path(&spiral(1800, 1.0 / 30.0, 60.0, 4.0), 1.0 / 30.0, TAU_S);
        let apart = (450..1350usize)
            .map(|i| slow[i].distance(fast[2 * i]))
            .fold(0.0, f64::max);
        assert!(apart < 0.01, "the two cadences part by {apart} m");
    }

    // ── Envelope ───────────────────────────────────────────────────────────

    /// Hills of different widths and heights, and flat ground between them.
    fn needs(samples: usize) -> Vec<f64> {
        (0..samples)
            .map(|i| {
                let x = i as f64;
                let hill = |at: f64, width: f64, height: f64| {
                    height * (1.0 - ((x - at) / width).powi(2)).max(0.0)
                };
                hill(700.0, 40.0, 120.0)
                    + hill(5200.0, 300.0, 45.0)
                    + hill(8100.0, 3.0, 150.0)
                    + hill(40.0, 60.0, 80.0)
            })
            .collect()
    }

    fn worst_step(values: &[f64]) -> f64 {
        values
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f64::max)
    }

    #[test]
    fn an_envelope_is_never_under_its_input_and_no_longer_steps() {
        let needs = needs(10_000);
        let out = envelope(&needs, DT, TAU_S);
        for (i, (out, need)) in out.iter().zip(&needs).enumerate() {
            assert!(out >= need, "sample {i}: {out} under {need}");
        }
        // It rises before a peak: three seconds ahead of a hill three samples
        // wide, it is already most of the way up.
        assert!(
            needs[8100 - 180] == 0.0 && out[8100 - 180] > 140.0,
            "{}",
            out[8100 - 180]
        );
        // And it is a smoothing: no step when a peak arrives.
        assert!(
            worst_step(&needs) > 50.0 && worst_step(&out) < 2.5,
            "{} → {}",
            worst_step(&needs),
            worst_step(&out)
        );
        // Far from any hill there is next to nothing.
        assert!(out[3000] < 1e-3, "{}", out[3000]);
    }

    /// The floor holds for values of any sign — a clearance below a datum is
    /// still a clearance.
    #[test]
    fn an_envelope_of_negative_values_is_still_above_them() {
        let values: Vec<f64> = needs(4000).iter().map(|v| v - 500.0).collect();
        let out = envelope(&values, DT, TAU_S);
        assert!(out.iter().zip(&values).all(|(out, own)| out >= own));
        assert!((out[3000] + 500.0).abs() < 1e-3, "{}", out[3000]);
    }

    #[test]
    fn the_sliding_maximum_is_the_maximum_of_the_window() {
        let values = needs(2000);
        for reach in [0usize, 1, 7, 90, 5000] {
            let fast = sliding_max(&values, reach);
            for i in (0..values.len()).step_by(37) {
                let around = &values[i.saturating_sub(reach)..(i + reach + 1).min(values.len())];
                let expected = around.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                assert_eq!(fast[i], expected, "reach {reach}, sample {i}");
            }
        }
    }

    /// What the margin buys: a piece that reads its margin agrees with the
    /// whole series to under a thousandth of a unit.
    #[test]
    fn a_piece_with_its_margin_agrees_with_the_whole() {
        let needs = needs(10_000);
        let whole = envelope(&needs, DT, TAU_S);
        let margin = envelope_margin(DT, TAU_S);
        let mut worst = 0.0f64;
        for (first, last) in [
            (0usize, 299usize),
            (300, 1699),
            (1700, 1700),
            (1701, 5099),
            (5100, 8098),
            (8099, 9999),
        ] {
            let (from, to) = (
                first.saturating_sub(margin),
                (last + margin).min(needs.len() - 1),
            );
            let piece = envelope(&needs[from..=to], DT, TAU_S);
            for i in first..=last {
                worst = worst.max((whole[i] - piece[i - from]).abs());
            }
        }
        assert!(worst < 1e-3, "a piece is {worst} from the whole");
        // Twenty-one time constants, in samples.
        assert_eq!(margin, 1890);
    }

    // ── Leash ──────────────────────────────────────────────────────────────

    #[test]
    fn a_leash_holds_a_point_within_reach_and_leaves_a_near_one_alone() {
        let subject = at(0.0, 0.0);
        let near = at(30.0, 40.0);
        assert_eq!(
            leash(near, subject, 50.0 + 1e-6),
            near,
            "inside, it is not touched"
        );
        let far = at(300.0, 400.0);
        let held = leash(far, subject, 50.0);
        assert!(
            (held.distance(subject) - 50.0).abs() < 1e-6,
            "{}",
            held.distance(subject)
        );
        assert!(
            held.distance(near) < 1e-6,
            "brought back along the line to the subject"
        );
        assert_eq!(leash(far, subject, 0.0), subject);
        assert_eq!(leash(far, subject, -3.0), subject);
        assert_eq!(leash(subject, subject, 50.0), subject);
    }

    /// The whole chain: a subject that leaps leaves its smoothed aim behind,
    /// and the leash is what keeps it in the picture.
    #[test]
    fn a_leashed_aim_never_loses_a_subject_that_leaps() {
        // A steady 10 m/s, and a 400 m leap sideways two thirds of the way.
        let subjects: Vec<DVec3> = (0..1800)
            .map(|i| {
                at(
                    10.0 * f64::from(i) * DT,
                    if i >= 1200 { 400.0 } else { 0.0 },
                )
            })
            .collect();
        let aims = smooth_path(&subjects, DT, TAU_S);
        let furthest = |aims: &[DVec3]| {
            aims.iter()
                .zip(&subjects)
                .map(|(a, s)| a.distance(*s))
                .fold(0.0, f64::max)
        };
        assert!(
            furthest(&aims) > 190.0,
            "the leap leaves the aim {} m behind",
            furthest(&aims)
        );
        let held = leash_path(&aims, &subjects, 50.0);
        assert_eq!(held.len(), subjects.len());
        assert!(
            furthest(&held) < 50.0 + 1e-6,
            "leashed, it is still {} m away",
            furthest(&held)
        );
        // Away from the leap the leash does nothing at all.
        assert_eq!(held[600], aims[600]);
    }

    // ── Pursuit ────────────────────────────────────────────────────────────

    const LIMITS: TurnLimits = TurnLimits {
        max_rate: 0.5,
        max_accel: 0.25,
    };

    /// The bearing to a point passed 30 m to the left at 250 m/s: it swings
    /// half a turn in well under a second, far faster than [`LIMITS`].
    fn swinging_target(seconds: f64, dt: f64) -> Vec<f64> {
        let samples = (seconds / dt).round() as usize + 1;
        (0..samples)
            .map(|i| {
                let along = 250.0 * (i as f64 * dt - seconds / 2.0);
                (-along).atan2(30.0).rem_euclid(TAU)
            })
            .collect()
    }

    /// The heading's rate and the change of its rate, per second, at worst.
    fn turning(headings: &[f64], dt: f64) -> (f64, f64) {
        let rates: Vec<f64> = headings.windows(2).map(|w| (w[1] - w[0]) / dt).collect();
        let fastest = rates.iter().fold(0.0f64, |m, r| m.max(r.abs()));
        let hardest = rates
            .windows(2)
            .map(|w| ((w[1] - w[0]) / dt).abs())
            .fold(0.0, f64::max);
        (fastest, hardest)
    }

    #[test]
    fn the_shortest_turn_goes_the_short_way_round() {
        let deg = f64::to_radians;
        assert!((turn_between(deg(350.0), deg(10.0)) - deg(20.0)).abs() < 1e-12);
        assert!((turn_between(deg(10.0), deg(350.0)) + deg(20.0)).abs() < 1e-12);
        assert!((turn_between(deg(10.0), deg(10.0 + 720.0))).abs() < 1e-12);
        assert!(
            (turn_between(0.0, PI) + PI).abs() < 1e-12,
            "half a turn is −π, not +π"
        );
    }

    /// The pivot is not a case: it is what a bounded pursuit does when its
    /// target swings. The heading is left behind, turns as fast as it may and
    /// no faster, and settles back on the target without crossing it.
    #[test]
    fn a_pursuit_turns_within_its_bounds_and_settles_on_its_target() {
        let targets = swinging_target(60.0, DT);
        let headings = pursue(&targets, DT, LIMITS);
        assert_eq!(headings.len(), targets.len());
        let off = |i: usize| turn_between(headings[i], targets[i]).abs();
        let n = headings.len();
        // Long before and long after the swing, the heading is the target.
        assert!(
            (0..n / 3).all(|i| off(i) < 0.01),
            "it holds the target on the way in"
        );
        assert!(
            (5 * n / 6..n).all(|i| off(i) < 0.01),
            "and is back on it on the way out"
        );
        // In between it is left far behind: it does not jump.
        let left_behind = (0..n).map(off).fold(0.0, f64::max);
        assert!(
            left_behind > 2.0,
            "it never trails by more than {left_behind} rad"
        );
        // It turns at its bound and never past it.
        let (fastest, hardest) = turning(&headings, DT);
        assert!(
            fastest > 0.999 * LIMITS.max_rate,
            "it only reaches {fastest} rad/s"
        );
        assert!(
            fastest <= LIMITS.max_rate + 1e-9,
            "it turns at {fastest} rad/s"
        );
        assert!(
            hardest <= LIMITS.max_accel + 1e-9,
            "its turn changes by {hardest} rad/s²"
        );
        // Half a turn, the way the target went, and not a hair past it.
        let turned = headings[n - 1] - headings[0];
        assert!((turned.abs() - PI).abs() < 0.02, "it turned {turned} rad");
        let furthest = headings
            .iter()
            .fold(0.0f64, |m, h| m.max((h - headings[0]).abs()));
        assert!(
            furthest <= turned.abs() + 1e-6,
            "it went {furthest} rad and settled at {turned}"
        );
    }

    #[test]
    fn a_heading_on_its_target_stays_and_one_behind_it_does_not_cross_it() {
        let deg = f64::to_radians;
        let rest = Heading::at_rest(deg(350.0));
        assert_eq!(pursue_step(rest, deg(350.0), DT, LIMITS), rest);
        assert_eq!(
            pursue_step(rest, deg(10.0), 0.0, LIMITS),
            rest,
            "no time, no turn"
        );
        // The short way round: from 350° to 10° is 20° clockwise, not 340° back.
        let mut heading = rest;
        let mut furthest = heading.angle;
        for _ in 0..600 {
            heading = pursue_step(heading, deg(10.0), DT, LIMITS);
            furthest = furthest.max(heading.angle);
        }
        assert!(
            (heading.angle - deg(370.0)).abs() < 1e-4,
            "ten seconds later it is at {}",
            heading.angle.to_degrees()
        );
        assert!(
            furthest < deg(370.0) + 1e-4,
            "it went as far as {} past 370",
            furthest.to_degrees()
        );
        assert!(pursue(&[], DT, LIMITS).is_empty());
    }

    /// The bounds are per second: the same targets sampled at 30 Hz and at
    /// 60 Hz give the same heading at the same instant.
    #[test]
    fn a_pursuit_is_the_same_at_30_and_60_samples_a_second() {
        let slow = pursue(&swinging_target(60.0, 1.0 / 30.0), 1.0 / 30.0, LIMITS);
        let fast = pursue(&swinging_target(60.0, 1.0 / 60.0), 1.0 / 60.0, LIMITS);
        assert_eq!(fast.len(), 2 * slow.len() - 1);
        let apart = slow
            .iter()
            .enumerate()
            .map(|(i, h)| (h - fast[2 * i]).abs())
            .fold(0.0, f64::max);
        assert!(apart < 0.02, "the two cadences part by {apart} rad");
    }

    // ── Route ──────────────────────────────────────────────────────────────

    /// A leg 20 km long flown west to east in `moving` seconds after
    /// `waiting` seconds spent on its first point, one sample a second.
    fn leg(waiting: usize, moving: usize) -> (Vec<DVec3>, Vec<f64>) {
        let samples = waiting + moving + 1;
        let positions = (0..samples)
            .map(|i| {
                at(
                    -10_000.0 + 20_000.0 * i.saturating_sub(waiting) as f64 / moving as f64,
                    0.0,
                )
            })
            .collect();
        (positions, (0..samples).map(|i| i as f64).collect())
    }

    /// The centre is where the subject is over the route, stops left out: an
    /// hour's wait at the start does not pull it there.
    #[test]
    fn a_wait_does_not_weigh_on_the_routes_centre() {
        let (moving, times) = leg(0, 2000);
        let centre = route_centre(&moving, &times, 0.5).expect("a centre");
        assert!(
            centre.distance(at(0.0, 0.0)) < 10.0,
            "the middle of the leg: {}",
            centre.distance(at(0.0, 0.0))
        );
        let (waited, times) = leg(3600, 2000);
        let apart = route_centre(&waited, &times, 0.5)
            .expect("a centre")
            .distance(centre);
        assert!(apart < 10.0, "the wait moved the centre by {apart} m");
        // With no threshold, the wait is two thirds of the mean.
        let dragged = route_centre(&waited, &times, 0.0)
            .expect("a centre")
            .distance(centre);
        assert!(dragged > 6000.0, "{dragged}");
    }

    #[test]
    fn a_route_that_never_moves_is_centred_on_itself() {
        assert_eq!(route_centre(&[], &[], 0.5), None);
        let here = at(12.0, -7.0);
        assert_eq!(route_centre(&[here], &[0.0], 0.5), Some(here));
        let centre = route_centre(&[here; 5], &[0.0, 1.0, 2.0, 3.0, 4.0], 0.5).expect("a centre");
        assert!(centre.distance(here) < 1e-6);
    }

    #[test]
    fn the_diagonal_is_the_ground_extent_whatever_the_climb() {
        assert_eq!(route_diagonal(&[]), 0.0);
        assert_eq!(route_diagonal(&[at(5.0, 5.0)]), 0.0);
        let (_, frame) = spot();
        // 3 km east-west by 4 km north-south, with a 2 km climb on the way.
        // The climb is on the northernmost point, where counting it would show.
        let route = [
            at(0.0, 0.0),
            at(3000.0, 1000.0),
            at(0.0, 4000.0) + frame.z_axis * 2000.0,
            at(1500.0, 0.0),
        ];
        let diagonal = route_diagonal(&route);
        assert!((diagonal - 5000.0).abs() < 1e-6, "{diagonal}");
    }

    #[test]
    fn a_bearing_is_a_compass_bearing() {
        let here = at(0.0, 0.0);
        let deg = |b: Option<f64>| b.expect("a bearing").to_degrees();
        assert!(
            deg(bearing(here, at(0.0, 1000.0))).min(360.0 - deg(bearing(here, at(0.0, 1000.0))))
                < 1e-9,
            "north"
        );
        assert!(
            (deg(bearing(here, at(1000.0, 0.0))) - 90.0).abs() < 1e-9,
            "east"
        );
        assert!(
            (deg(bearing(here, at(0.0, -1000.0))) - 180.0).abs() < 1e-9,
            "south"
        );
        assert!(
            (deg(bearing(here, at(-1000.0, 0.0))) - 270.0).abs() < 1e-9,
            "west"
        );
        assert_eq!(
            bearing(here, at(0.5, 0.5)),
            None,
            "on top of it there is none"
        );
        let (_, frame) = spot();
        assert_eq!(
            bearing(here, here + frame.z_axis * 5000.0),
            None,
            "nor straight above"
        );
    }

    // ── Placement ──────────────────────────────────────────────────────────

    #[test]
    fn the_follow_camera_stands_behind_its_subject_and_looks_at_it() {
        let subject = at(0.0, 0.0);
        let (_, frame) = spot();
        let enu = |p: DVec3| {
            DVec3::new(
                (p - subject).dot(frame.x_axis),
                (p - subject).dot(frame.y_axis),
                (p - subject).dot(frame.z_axis),
            )
        };
        // Heading north, 30° down, 200 m away: south of the subject, 100 up.
        let camera = follow_camera(subject, 0.0, 30f64.to_radians(), 200.0, 0.6);
        let eye = enu(camera.position);
        assert!(
            eye.x.abs() < 1e-6
                && (eye.y + 200.0 * 0.75f64.sqrt()).abs() < 1e-6
                && (eye.z - 100.0).abs() < 1e-6,
            "{eye}"
        );
        // Heading east: the eye is west of it.
        let eye = enu(
            follow_camera(subject, 90f64.to_radians(), 30f64.to_radians(), 200.0, 0.6).position,
        );
        assert!(
            (eye.x + 200.0 * 0.75f64.sqrt()).abs() < 1e-6 && eye.y.abs() < 1e-6,
            "{eye}"
        );

        // And it is the camera the rest of the crate reads the same way.
        let (heading, pitch, distance) = (117f64.to_radians(), 42f64.to_radians(), 3000.0);
        let camera = follow_camera(subject, heading, pitch, distance, 0.6);
        assert!((camera.position.distance(subject) - distance).abs() < 1e-6);
        assert!(
            camera
                .direction
                .dot((subject - camera.position).normalize())
                > 1.0 - 1e-12
        );
        // Read under the eye, 3 km from the subject, the frame has turned by
        // the Earth's curvature over that distance: half a milliradian.
        assert!(
            turn_between(camera.heading(), heading).abs() < 1e-3,
            "{}",
            camera.heading()
        );
        assert!((camera.pitch() - pitch).abs() < 1e-3, "{}", camera.pitch());
        assert_eq!(camera.fovy, 0.6);
        // No roll: the camera's right is horizontal at the subject.
        assert!(camera.right().dot(frame.z_axis).abs() < 1e-12);
    }
}
