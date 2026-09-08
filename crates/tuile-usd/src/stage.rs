// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Writes the manifest stage: an animated `UsdGeomCamera` plus one `Globe` prim.
//!
//! Plain `.usda` text on purpose. The manifest is a camera path and a dozen
//! primvars — pulling a USD library into the Rust side to emit that would buy
//! nothing and cost a native dependency on every machine that only ever
//! *writes* manifests. The text form is also the reviewable form: a diff of two
//! manifests reads like what changed.
//!
//! # The render origin
//!
//! Everything here is rebased. An ECEF camera sits ~6.4e6 m from the origin,
//! and both Blender (f32 object transforms) and Storm (f32 on the GPU) would
//! quantise that into metre-scale jitter. The manifest therefore carries
//! `primvars:tuile:renderOrigin`, every camera time sample is written relative
//! to it, and the procedural both adds it back (to recover the true ECEF view
//! for traversal) and subtracts it from every tile's origin (to place
//! geometry). One subtraction, one addition, all in f64.

use std::io::{self, Write};

use tuile_tape::Frame;

/// What the manifest states beyond the camera path.
#[derive(Debug, Clone)]
pub struct ManifestConfig {
    pub fps: f64,
    /// The render resolution the SSE is computed against. Written into the
    /// manifest as `primvars:tuile:viewportPx` — the fallback the procedural's
    /// camera reader uses when no render product states a resolution.
    pub viewport_px: (u32, u32),
    /// ion asset ids, with the ABI's encoding: `0` means the default
    /// (Cesium World Terrain / Bing Aerial), negative imagery disables it.
    pub terrain_asset_id: i64,
    pub imagery_asset_id: i64,
    /// Refine while a tile's screen-space error exceeds this (pixels).
    /// `0.0` lets the traversal default stand.
    pub max_sse: f64,
}

impl Default for ManifestConfig {
    fn default() -> Self {
        Self {
            fps: 24.0,
            viewport_px: (1920, 1440),
            terrain_asset_id: 0,
            imagery_asset_id: 0,
            max_sse: 0.0,
        }
    }
}

/// The origin every position in the manifest is written relative to.
///
/// The centroid of the camera path: nothing about it needs to be on the
/// ground, it needs to be *near the numbers* so that what is written — and
/// later narrowed to f32 by a renderer — is small. Per-trajectory, never
/// per-frame: a moving origin would smuggle the camera's motion into every
/// tile's transform.
pub fn render_origin(frames: &[Frame]) -> [f64; 3] {
    if frames.is_empty() {
        return [0.0; 3];
    }
    let mut sum = [0.0f64; 3];
    for f in frames {
        for (s, p) in sum.iter_mut().zip(f.position) {
            *s += p;
        }
    }
    let n = frames.len() as f64;
    [sum[0] / n, sum[1] / n, sum[2] / n]
}

/// The camera-to-world matrix USD expects, rebased.
///
/// USD cameras look down **-Z** with **+Y** up in their own space, and a
/// `matrix4d` in a `.usda` is row-major with row vectors — the rows *are* the
/// camera's basis in world space. Built from the tape's frame directly rather
/// than through any view-matrix helper: those produce world-to-camera, and
/// inverting one numerically is how a basis stops being orthonormal.
fn camera_rows(frame: &Frame, origin: [f64; 3]) -> [[f64; 4]; 4] {
    let norm = |v: [f64; 3]| {
        let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-300);
        [v[0] / n, v[1] / n, v[2] / n]
    };
    let cross = |a: [f64; 3], b: [f64; 3]| {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    };
    let f = norm(frame.direction);
    let z = [-f[0], -f[1], -f[2]];
    let x = norm(cross(frame.up, z));
    let y = cross(z, x);
    let t = [
        frame.position[0] - origin[0],
        frame.position[1] - origin[1],
        frame.position[2] - origin[2],
    ];
    [
        [x[0], x[1], x[2], 0.0],
        [y[0], y[1], y[2], 0.0],
        [z[0], z[1], z[2], 0.0],
        [t[0], t[1], t[2], 1.0],
    ]
}

/// A `matrix4d` literal. `{}` on f64 is Rust's shortest round-trip formatting,
/// the same guarantee the tape leans on: what is written re-reads bit-exact.
/// `+ 0.0` folds IEEE `-0.0` into `0.0` (their sum is `+0.0`) so a basis built
/// from cross products never prints a `-0`.
fn matrix_literal(rows: &[[f64; 4]; 4]) -> String {
    let row = |r: &[f64; 4]| {
        format!(
            "({}, {}, {}, {})",
            r[0] + 0.0,
            r[1] + 0.0,
            r[2] + 0.0,
            r[3] + 0.0
        )
    };
    format!(
        "( {}, {}, {}, {} )",
        row(&rows[0]),
        row(&rows[1]),
        row(&rows[2]),
        row(&rows[3])
    )
}

/// The focal length (mm) that reproduces a vertical field of view on a 24 mm
/// aperture. The aperture is a free choice — only the ratio reaches the fovy —
/// and 24 is the full-frame convention every DCC displays sensibly.
const VERTICAL_APERTURE: f64 = 24.0;

fn focal_length(fovy_rad: f64) -> f64 {
    VERTICAL_APERTURE / 2.0 / (fovy_rad / 2.0).tan()
}

/// Writes the whole manifest.
///
/// One time sample per tape frame, timecodes `1..=n`, so `--frames 1:n` on any
/// driver addresses the same instants the tape held.
pub fn write_manifest(
    frames: &[Frame],
    config: &ManifestConfig,
    out: &mut impl Write,
) -> io::Result<()> {
    if frames.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a manifest needs at least one camera frame",
        ));
    }
    let origin = render_origin(frames);
    let aspect = f64::from(config.viewport_px.0) / f64::from(config.viewport_px.1);
    let fovy_constant = frames
        .iter()
        .all(|f| f.fovy.to_bits() == frames[0].fovy.to_bits());

    writeln!(out, "#usda 1.0")?;
    writeln!(out, "(")?;
    writeln!(out, "    defaultPrim = \"World\"")?;
    writeln!(out, "    metersPerUnit = 1")?;
    writeln!(out, "    upAxis = \"Z\"")?;
    writeln!(out, "    startTimeCode = 1")?;
    writeln!(out, "    endTimeCode = {}", frames.len())?;
    writeln!(out, "    timeCodesPerSecond = {}", config.fps)?;
    writeln!(out, ")")?;
    writeln!(out)?;
    writeln!(out, "def Xform \"World\"")?;
    writeln!(out, "{{")?;
    writeln!(out, "    def Camera \"ShotCam\"")?;
    writeln!(out, "    {{")?;
    writeln!(
        out,
        "        float2 clippingRange = (0.05, 10000000000)"
    )?;
    writeln!(
        out,
        "        float horizontalAperture = {}",
        VERTICAL_APERTURE * aspect
    )?;
    writeln!(
        out,
        "        float verticalAperture = {VERTICAL_APERTURE}"
    )?;
    if fovy_constant {
        writeln!(
            out,
            "        float focalLength = {}",
            focal_length(frames[0].fovy)
        )?;
    } else {
        writeln!(out, "        float focalLength.timeSamples = {{")?;
        for (i, frame) in frames.iter().enumerate() {
            writeln!(out, "            {}: {},", i + 1, focal_length(frame.fovy))?;
        }
        writeln!(out, "        }}")?;
    }
    writeln!(out, "        matrix4d xformOp:transform.timeSamples = {{")?;
    for (i, frame) in frames.iter().enumerate() {
        writeln!(
            out,
            "            {}: {},",
            i + 1,
            matrix_literal(&camera_rows(frame, origin))
        )?;
    }
    writeln!(out, "        }}")?;
    writeln!(
        out,
        "        uniform token[] xformOpOrder = [\"xformOp:transform\"]"
    )?;
    writeln!(out, "    }}")?;
    writeln!(out)?;
    writeln!(out, "    def GenerativeProcedural \"Globe\" (")?;
    writeln!(
        out,
        "        prepend apiSchemas = [\"HydraGenerativeProceduralAPI\"]"
    )?;
    writeln!(out, "    )")?;
    writeln!(out, "    {{")?;
    writeln!(
        out,
        "        token primvars:hdGp:proceduralType = \"tuileGlobe\""
    )?;
    // The API schema's fallback for this is not delivered on every host
    // (proven on the Blender fork, 2026-09-08); authored explicitly, always.
    writeln!(
        out,
        "        token proceduralSystem = \"hydraGenerativeProcedural\""
    )?;
    writeln!(
        out,
        "        rel primvars:tuile:cameras = </World/ShotCam>"
    )?;
    writeln!(
        out,
        "        double3 primvars:tuile:renderOrigin = ({}, {}, {})",
        origin[0], origin[1], origin[2]
    )?;
    writeln!(
        out,
        "        int primvars:tuile:terrainAssetId = {}",
        config.terrain_asset_id
    )?;
    writeln!(
        out,
        "        int primvars:tuile:imageryAssetId = {}",
        config.imagery_asset_id
    )?;
    writeln!(out, "        double primvars:tuile:maxSse = {}", config.max_sse)?;
    writeln!(
        out,
        "        double2 primvars:tuile:viewportPx = ({}, {})",
        config.viewport_px.0, config.viewport_px.1
    )?;
    writeln!(out, "    }}")?;
    writeln!(out, "}}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn looking_down_x(position: [f64; 3]) -> Frame {
        Frame {
            position,
            direction: [-1.0, 0.0, 0.0],
            up: [0.0, 0.0, 1.0],
            fovy: std::f64::consts::FRAC_PI_2,
        }
    }

    /// The whole anti-jitter contract in one assertion: what the manifest
    /// writes (position − origin) plus what the procedural adds back (origin)
    /// recovers the true ECEF position to well under a millimetre at globe
    /// scale — the narrowing to f32 happens downstream of both, on numbers
    /// this rebasing made small.
    #[test]
    fn rebasing_round_trips_at_ecef_scale() {
        let frames = [
            looking_down_x([6_378_137.0, 1_234.567_890_123, -42.25]),
            looking_down_x([6_378_337.5, 1_240.0, -40.0]),
        ];
        let origin = render_origin(&frames);
        for f in &frames {
            for axis in 0..3 {
                let rebased = f.position[axis] - origin[axis];
                let recovered = rebased + origin[axis];
                assert!(
                    (recovered - f.position[axis]).abs() < 1e-6,
                    "axis {axis}: {} vs {}",
                    recovered,
                    f.position[axis]
                );
            }
        }
    }

    /// The camera basis must be orthonormal and point the USD way: -Z is the
    /// look direction, +Y the screen up, rows are the basis.
    #[test]
    fn the_camera_matrix_is_an_orthonormal_usd_basis() {
        let frame = looking_down_x([100.0, 0.0, 0.0]);
        let rows = camera_rows(&frame, [0.0; 3]);
        // -Z row is the look direction.
        assert_eq!(&rows[2][..3], &[1.0, 0.0, 0.0]);
        // +Y row is up.
        assert_eq!(&rows[1][..3], &[0.0, 0.0, 1.0]);
        // X completes the right-handed set: looking down -X with +Z up,
        // screen-right is +Y (direction × up, checked by hand).
        assert_eq!(&rows[0][..2], &[0.0, 1.0]);
        assert_eq!(rows[0][2] + 0.0, 0.0);
        // Translation carries the rebased position, w column is affine.
        assert_eq!(rows[3], [100.0, 0.0, 0.0, 1.0]);
        assert_eq!([rows[0][3], rows[1][3], rows[2][3]], [0.0; 3]);
    }

    /// A 90° vertical field of view on a 24mm aperture is a 12mm lens —
    /// checkable by hand, and the constant the C++ reader inverts.
    #[test]
    fn focal_length_inverts_the_fovy() {
        assert!((focal_length(std::f64::consts::FRAC_PI_2) - 12.0).abs() < 1e-12);
    }

    /// The golden manifest: two frames, every load-bearing line pinned. This
    /// is the contract with the C++ reader and with Blender's importer — a
    /// drift here silently unresolves the whole pipeline.
    #[test]
    fn the_manifest_is_stable() {
        let frames = [
            looking_down_x([200.0, 0.0, 0.0]),
            looking_down_x([100.0, 0.0, 0.0]),
        ];
        let mut out = Vec::new();
        write_manifest(&frames, &ManifestConfig::default(), &mut out)
            .expect("writing");
        let text = String::from_utf8(out).expect("utf-8");

        for needle in [
            "#usda 1.0",
            "defaultPrim = \"World\"",
            "endTimeCode = 2",
            "timeCodesPerSecond = 24",
            "def Camera \"ShotCam\"",
            "float focalLength = 12",
            "float horizontalAperture = 32",
            "matrix4d xformOp:transform.timeSamples = {",
            // Origin is the centroid (150,0,0); frame 1 rebases to +50.
            "1: ( (0, 1, 0, 0), (0, 0, 1, 0), (1, 0, 0, 0), (50, 0, 0, 1) ),",
            "2: ( (0, 1, 0, 0), (0, 0, 1, 0), (1, 0, 0, 0), (-50, 0, 0, 1) ),",
            "uniform token[] xformOpOrder = [\"xformOp:transform\"]",
            "def GenerativeProcedural \"Globe\" (",
            "prepend apiSchemas = [\"HydraGenerativeProceduralAPI\"]",
            "token primvars:hdGp:proceduralType = \"tuileGlobe\"",
            "token proceduralSystem = \"hydraGenerativeProcedural\"",
            "rel primvars:tuile:cameras = </World/ShotCam>",
            "double3 primvars:tuile:renderOrigin = (150, 0, 0)",
            "int primvars:tuile:terrainAssetId = 0",
            "int primvars:tuile:imageryAssetId = 0",
            "double primvars:tuile:maxSse = 0",
            "double2 primvars:tuile:viewportPx = (1920, 1440)",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    /// An empty tape is a usage error worth naming, not an empty file that a
    /// renderer opens and shows nothing about.
    #[test]
    fn an_empty_tape_is_refused() {
        let mut out = Vec::new();
        assert!(write_manifest(&[], &ManifestConfig::default(), &mut out).is_err());
    }
}
