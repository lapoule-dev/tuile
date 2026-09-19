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

/// A number a job can override without a rebuild.
///
/// Same shape as `tuile-hydra`'s: unset or unparseable keeps the default, so a
/// typo degrades to the built-in rather than to zero.
fn env_knob<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<T>().ok())
        .unwrap_or(default)
}

/// The camera's near/far for one frame, from its height over the ellipsoid.
///
/// Near at 5% of altitude keeps the ratio to `far` around 1e5–1e6, where an
/// f32 depth buffer still separates overlapping tile surfaces; the floor stops
/// a ground-hugging frame from clipping its own foreground, the ceiling stops
/// a geostationary one from clipping the globe. Far is the distance to the
/// visible limb from geostationary, once, for every frame — it costs nothing
/// and never truncates a horizon.
fn clipping_range(position: [f64; 3]) -> (f64, f64) {
    // Height above the ELLIPSOID, not above a sphere of the equatorial radius.
    //
    // The constant that stood here came with a comment saying the
    // sphere/ellipsoid difference was "absorbed by the clamp below". It was
    // absorbed the way a fuse absorbs a short circuit: at 42.5° N the
    // ellipsoid's radius is 9.7 km smaller than the equatorial one, so the
    // subtraction is 9.7 km short and goes negative for any camera below that.
    // The clamp then returned 10 m for a camera 5 km up, and near came out at
    // 0.5 m against a far of 60 000 km — a ratio of 1.2e8, which is precisely
    // the depth-buffer collapse the note below is about.
    let altitude = tuile_core::geo::ecef_to_geodetic(glam::DVec3::from_array(position))
        .height
        .max(10.0);
    // Both ends are knobs, because both have been suspected of the black band
    // and neither could be tested without regenerating a manifest and
    // rebuilding an image. A number nobody can vary is a number nobody can
    // rule out.
    let fraction = env_knob("TUILE_CLIP_NEAR_FRACTION", 0.05);
    let near = (fraction * altitude).clamp(0.5, 100_000.0);
    (near, env_knob("TUILE_CLIP_FAR", 60_000_000.0))
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
    // Per-frame clipping, computed from altitude, because a fixed wide range
    // is how the first gate render broke: (0.05, 1e10) is a near/far ratio of
    // 2e11, an f32 depth buffer collapses, and everywhere two tile surfaces
    // overlap — every LOD boundary — the loser z-fights through as black
    // confetti, worse with distance (error grows as z²). Near tracks the
    // camera's height over the ellipsoid; far covers the visible limb from
    // any altitude up to geostationary.
    writeln!(out, "        float2 clippingRange.timeSamples = {{")?;
    for (i, frame) in frames.iter().enumerate() {
        let (near, far) = clipping_range(frame.position);
        writeln!(out, "            {}: ({near}, {far}),", i + 1)?;
    }
    writeln!(out, "        }}")?;
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
    // Lighting is stage-side composition, exactly like any other look
    // decision. The dome matters beyond taste: tile skirts are vertical
    // walls, and under a renderer's lone camera light they shade to black
    // cracks along every tile boundary (measured). An ambient dome lights
    // them from everywhere; the distant light keeps relief readable.
    writeln!(out, "    def DomeLight \"Sky\"")?;
    writeln!(out, "    {{")?;
    writeln!(out, "        float inputs:intensity = 0.8")?;
    writeln!(
        out,
        "        color3f inputs:color = (0.9, 0.95, 1.0)"
    )?;
    writeln!(out, "    }}")?;
    writeln!(out)?;
    writeln!(out, "    def DistantLight \"Sun\"")?;
    writeln!(out, "    {{")?;
    writeln!(out, "        float inputs:intensity = 2.5")?;
    writeln!(
        out,
        "        color3f inputs:color = (1.0, 0.98, 0.92)"
    )?;
    writeln!(
        out,
        "        float inputs:angle = 0.53"
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
    // Un ATTRIBUT, pas une relation.
    //
    // C'était `rel primvars:tuile:cameras = </World/ShotCam>`, et le
    // procédural ne l'a jamais vu. hdGp passe ses arguments par les primvars
    // de la prim, et un primvar EST un attribut : une relation préfixée
    // `primvars:` n'en est pas une, et USD n'a pas de type d'attribut pour un
    // chemin. Le manifeste nommait donc la caméra dans une forme que le
    // lecteur ne lit pas.
    //
    // Ce que ça coûtait, précisément : `_ResolveCamera` descendait ses quatre
    // échelons sans rien trouver et retombait sur une vue fixe au-dessus de
    // l'origine de rendu — c'est-à-dire au centre de l'orbite. Mesuré le
    // 16 septembre 2026 sur un pack de 1440 frames : « no baked frame answers
    // this camera: the nearest is frame 1108, 8000.000 m away », 8000 m étant
    // le rayon de l'orbite au millimètre près. Toutes les poses étaient
    // équidistantes parce que la caméra était au centre de leur cercle.
    writeln!(
        out,
        "        string primvars:tuile:cameras = \"/World/ShotCam\""
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

    /// A camera five kilometres up gets a near plane sized for five
    /// kilometres, not for ten metres.
    ///
    /// It did not. The altitude came from `|position| − equatorial radius`,
    /// which at 42.5° N is 9.7 km short and therefore **negative** for this
    /// camera; the `.max(10.0)` turned that into ten metres, and near into
    /// 0.5 m against a far of 60 000 km. A near/far ratio of 1.2e8 is the
    /// depth-buffer collapse this function exists to avoid — it was producing
    /// the very thing it was written to prevent, and the clamp made it look
    /// deliberate.
    #[test]
    fn the_near_plane_follows_the_height_above_the_ellipsoid() {
        // 5 km over the Pyrenees, where the two radii differ most sharply.
        let camera = tuile_core::geo::geodetic_to_ecef(tuile_core::geo::Geodetic {
            lon: 2.17_f64.to_radians(),
            lat: 42.52_f64.to_radians(),
            height: 5_000.0,
        });
        let (near, far) = clipping_range([camera.x, camera.y, camera.z]);
        assert!(
            (near - 250.0).abs() < 1.0,
            "near is {near} m — 5 % of five kilometres is 250"
        );
        assert!(
            far / near < 1.0e6,
            "near/far is {:.0e}, back in the range where an f32 depth buffer \
             collapses and every LOD boundary z-fights",
            far / near
        );

        // And the spherical reading, kept as a measurement: it is how far off
        // the old one was that makes this worth a test.
        let spherical = camera.length() - 6_378_137.0;
        assert!(
            spherical < 0.0,
            "the equatorial radius no longer overshoots here ({spherical:.0} m) \
             — this test is no longer about anything"
        );
    }
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
            for (axis, (&coordinate, &about)) in f.position.iter().zip(&origin).enumerate() {
                let rebased = coordinate - about;
                let recovered = rebased + about;
                assert!(
                    (recovered - coordinate).abs() < 1e-6,
                    "axis {axis}: {recovered} vs {coordinate}"
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
            // Test positions sit far inside the ellipsoid: altitude clamps to
            // its floor, near to its own — the degenerate case stays sane.
            "float2 clippingRange.timeSamples = {",
            "1: (0.5, 60000000),",
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
            "string primvars:tuile:cameras = \"/World/ShotCam\"",
            "double3 primvars:tuile:renderOrigin = (150, 0, 0)",
            "int primvars:tuile:terrainAssetId = 0",
            "int primvars:tuile:imageryAssetId = 0",
            "double primvars:tuile:maxSse = 0",
            "double2 primvars:tuile:viewportPx = (1920, 1440)",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    /// La caméra est nommée par un ATTRIBUT, jamais par une relation.
    ///
    /// hdGp passe ses arguments par les primvars de la prim, et un primvar est
    /// un attribut. `rel primvars:tuile:cameras` était un relationship portant
    /// le préfixe d'un primvar : USD l'accepte à l'écriture, le procédural ne
    /// le voit pas, et personne ne se plaint.
    ///
    /// Ce que ça coûtait : la résolution de caméra descendait ses quatre
    /// échelons sans rien trouver et retombait sur une vue fixe au-dessus de
    /// l'origine de rendu — le centre de l'orbite. Le 16 septembre 2026, un
    /// pack de 1440 frames a répondu « no baked frame answers this camera: the
    /// nearest is frame 1108, 8000.000 m away » : 8000 m est le rayon de
    /// l'orbite, et 1108 un tirage arbitraire parmi 1440 poses toutes
    /// équidistantes du centre.
    #[test]
    fn the_camera_is_named_by_an_attribute_not_a_relationship() {
        let frames = [
            looking_down_x([200.0, 0.0, 0.0]),
            looking_down_x([100.0, 0.0, 0.0]),
        ];
        let mut out = Vec::new();
        write_manifest(&frames, &ManifestConfig::default(), &mut out)
            .expect("writing");
        let usda = String::from_utf8(out).expect("utf-8");
        let line = usda
            .lines()
            .find(|l| l.contains("tuile:cameras"))
            .expect("le manifeste ne nomme aucune caméra");
        assert!(
            !line.trim_start().starts_with("rel "),
            "la caméra est déclarée par une relation, que hdGp ne lira pas: {line}"
        );
        assert!(
            line.contains("string ") || line.contains("token "),
            "la caméra doit être un attribut textuel: {line}"
        );
        assert!(line.contains("/World/ShotCam"), "{line}");
    }

    /// An empty tape is a usage error worth naming, not an empty file that a
    /// renderer opens and shows nothing about.
    #[test]
    fn an_empty_tape_is_refused() {
        let mut out = Vec::new();
        assert!(write_manifest(&[], &ManifestConfig::default(), &mut out).is_err());
    }
}
