// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **Where the source has no tile at a level, the nearest ancestor that has
//! one is the ground there: drawn, and answering heights.**
//!
//! The case is a real one. Over the Cerdagne plateau (lon 2.084482°, lat
//! 42.549562°) the world terrain stops at level 13: tile 13/8286/6032 is
//! served, and every tile of level 14 and deeper under it answers "no such
//! tile" (HTTP 404), while its neighbours go deeper. A film's camera flew
//! six kilometres over that ground on 31 October 2026 (track 1576223), and a
//! bake has to read the height under its eye there to keep it clear.
//!
//! A made-up source stands in for the world terrain with the same shape —
//! data down to level 15 everywhere but under that level-13 tile, where it
//! stops — so the test needs no network and no token. Two ways the gap can
//! be told:
//!
//! - honestly: the availability (`layer.json`) never lists the deeper
//!   tiles, so they are never asked for;
//! - not: the availability lists them, the source answers "no such tile"
//!   when asked — a tile a register recorded absent.
//!
//! Either way the traversal still divides there (the imagery wants smaller
//! tiles), each deeper tile is cut out of the level-13 one, and the ground
//! a camera is kept over answers under the eye, at the height of the level-13
//! surface: no hole, and nothing that jumps where the data stops.

#[path = "../../tuile-terrain/tests/support/mod.rs"]
mod support;

use std::sync::Arc;

use glam::DVec3;
use support::{encoded, measured, relief};
use tuile_bake::settle::{settle_each, Drawn};
use tuile_bake::{Finish, GlobeConfig, GroundDrawer, Session, SourcesHandle};
use tuile_core::fetch::Fetched;
use tuile_core::geo::{ecef_to_geodetic, enu_frame};
use tuile_core::raster::{ImageryCoord, ImageryProvider, RasterError, TilingScheme};
use tuile_core::traversal::ViewStateParams;
use tuile_film::ground::{Ground, Prepare};
use tuile_film::smoothing::{Smooth, DEFAULT_BOUND_M};
use tuile_terrain::{
    GeographicTilingScheme, LayerJson, TerrainSource, TerrainSourceError, TileCoord,
};

/// The deepest level the source has, where it has data at all.
const DEEPEST: u32 = 15;
/// The tile the source stops at: nothing of it is served deeper.
const CEILING: TileCoord = TileCoord {
    level: 13,
    x: 8286,
    y: 6032,
};
/// The picture of the film: portrait, as it was shot.
const VIEWPORT: (f64, f64) = (720.0, 1280.0);
/// The farm's screen-space error.
const SSE: f64 = 6.0;
/// Relief steps a side of each tile.
const STEPS: usize = 16;

/// The film's cameras over the gap, as its plan recorded them (ECEF eye,
/// direction): frames 1584 to 1590 of export 20261031T191556Z-4ce0. The
/// eye of 1585 is the place the failed exports named.
#[allow(clippy::type_complexity)]
const CAMERAS: &[(u32, [f64; 3], [f64; 3])] = &[
    (
        1584,
        [4707334.378853762, 171310.939557121, 4294901.021955708],
        [
            -0.990384136196439,
            -0.1329171454655097,
            -0.03837049923749056,
        ],
    ),
    (
        1585,
        [4707323.928718875, 171333.09244310835, 4294908.127615107],
        [
            -0.9907373737368349,
            -0.12965133755721067,
            -0.040373096867414845,
        ],
    ),
    (
        1586,
        [4707313.876986647, 171354.60092901342, 4294914.943219002],
        [
            -0.9909766376934415,
            -0.1270771446086151,
            -0.04262279746711932,
        ],
    ),
    (
        1587,
        [4707303.904395097, 171375.97923722336, 4294921.6731582675],
        [
            -0.9910956771064962,
            -0.12523405059021117,
            -0.045230425529553996,
        ],
    ),
    (
        1588,
        [4707293.898381703, 171397.03225134494, 4294928.438310687],
        [
            -0.9911155709806779,
            -0.12398731341993896,
            -0.04813596441903589,
        ],
    ),
    (
        1589,
        [4707283.884906751, 171417.65189507251, 4294935.266439508],
        [
            -0.9910213794765372,
            -0.12343771348897706,
            -0.05137855884544366,
        ],
    ),
    (
        1590,
        [4707274.079206214, 171438.4508762807, 4294941.932543332],
        [
            -0.9907943918302003,
            -0.12375168429527772,
            -0.05488163401271197,
        ],
    ),
];

/// Whether `c` lies under the ceiling tile, deeper than it.
fn under_the_ceiling(c: TileCoord) -> bool {
    c.level > CEILING.level && {
        let shift = c.level - CEILING.level;
        c.x >> shift == CEILING.x && c.y >> shift == CEILING.y
    }
}

/// The relief, down to [`DEEPEST`], with nothing under the ceiling tile.
struct Relief;

#[async_trait::async_trait]
impl TerrainSource for Relief {
    async fn fetch_tile(&self, coord: TileCoord) -> Result<Fetched<Vec<u8>>, TerrainSourceError> {
        if coord.level > DEEPEST || under_the_ceiling(coord) {
            return Err(TerrainSourceError::no_such_tile(format!("{coord:?}")));
        }
        Ok(Fetched::undated(encoded(&measured(coord, STEPS))))
    }
}

/// A 4 × 4 PNG of one colour, for every imagery tile.
struct OnePicture;

const PICTURE: [u8; 73] = [
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 4, 0, 0, 0, 4, 8, 2, 0,
    0, 0, 38, 147, 9, 41, 0, 0, 0, 16, 73, 68, 65, 84, 120, 156, 99, 168, 104, 202, 131, 35, 6,
    226, 56, 0, 73, 242, 22, 129, 231, 235, 56, 46, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

#[async_trait::async_trait]
impl ImageryProvider for OnePicture {
    fn tiling_scheme(&self) -> TilingScheme {
        TilingScheme::web_mercator()
    }

    async fn fetch_tile_bytes(
        &self,
        _coord: ImageryCoord,
    ) -> Result<Fetched<bytes::Bytes>, RasterError> {
        Ok(Fetched::undated(bytes::Bytes::from_static(&PICTURE)))
    }
}

/// How the availability tells the gap.
#[derive(Clone, Copy, Debug)]
enum Told {
    /// The deeper tiles are not listed.
    Honestly,
    /// They are listed, and the source answers that it has none.
    Not,
}

/// One availability range, as `layer.json` writes it.
fn range(x0: u64, y0: u64, x1: u64, y1: u64) -> String {
    format!(r#"{{"startX":{x0},"startY":{y0},"endX":{x1},"endY":{y1}}}"#)
}

struct MadeUp(Told);

#[async_trait::async_trait]
impl tuile_bake::Sources for MadeUp {
    async fn terrain(&self, _asset: i64) -> Result<(LayerJson, Arc<dyn TerrainSource>), String> {
        let scheme = GeographicTilingScheme::default();
        let available: Vec<String> = (0..=DEEPEST)
            .map(|level| {
                let (w, h) = (scheme.tiles_x(level) - 1, scheme.tiles_y(level) - 1);
                let ranges = match self.0 {
                    Told::Honestly if level > CEILING.level => {
                        // Everything but the ceiling tile's descendants: the
                        // strips west, east, south and north of them.
                        let shift = level - CEILING.level;
                        let (x0, y0) = (CEILING.x << shift, CEILING.y << shift);
                        let (x1, y1) = (
                            ((CEILING.x + 1) << shift) - 1,
                            ((CEILING.y + 1) << shift) - 1,
                        );
                        [
                            range(0, 0, x0 - 1, h),
                            range(x1 + 1, 0, w, h),
                            range(x0, 0, x1, y0 - 1),
                            range(x0, y1 + 1, x1, h),
                        ]
                        .join(",")
                    }
                    _ => range(0, 0, w, h),
                };
                format!("[{ranges}]")
            })
            .collect();
        let layer = LayerJson::from_slice(
            format!(
                r#"{{"tilejson":"2.1.0","format":"quantized-mesh-1.0","scheme":"tms",
                    "projection":"EPSG:4326","tiles":["{{z}}/{{x}}/{{y}}.terrain"],
                    "bounds":[-180,-90,180,90],"available":[{}]}}"#,
                available.join(",")
            )
            .as_bytes(),
        )
        .map_err(|e| e.to_string())?;
        Ok((layer, Arc::new(Relief)))
    }

    async fn imagery(&self, _asset: i64) -> Result<Arc<dyn ImageryProvider>, String> {
        Ok(Arc::new(OnePicture))
    }
}

fn session(told: Told) -> Session {
    let mut config = GlobeConfig::new("");
    config.sources = Some(SourcesHandle(Arc::new(MadeUp(told))));
    config.session.traversal.maximum_screen_space_error = SSE;
    config.session.finish = Finish::Identity;
    config.session.bake_max_size = 128;
    let mut session = Session::globe(config).expect("a globe");
    session.keep_ground();
    session
}

/// The ground as the render service draws it.
const DRAWN: Drawn = Drawn {
    how: Prepare {
        skirts: true,
        smooth: Smooth::Geometry {
            bound: DEFAULT_BOUND_M,
        },
    },
    stitch: true,
};

fn frame(position: DVec3, direction: DVec3, up: DVec3) -> tuile_tape::Frame {
    tuile_tape::Frame {
        position: position.to_array(),
        direction: direction.to_array(),
        up: up.to_array(),
        fovy: std::f64::consts::FRAC_PI_4,
    }
}

/// The views camera `i` is settled on, as a bake settles one: its own pose,
/// and straight down from its eye.
fn views(i: u32, _: &Session) -> Vec<ViewStateParams> {
    let (_, eye, direction) = CAMERAS[i as usize];
    let eye = DVec3::from_array(eye);
    let enu = enu_frame(ecef_to_geodetic(eye));
    let direction = DVec3::from_array(direction);
    let up = direction.cross(enu.z_axis).normalize().cross(direction);
    vec![
        tuile_bake::pose_view(&frame(eye, direction, up), VIEWPORT),
        tuile_bake::pose_view(&frame(eye, -enu.z_axis, enu.y_axis), VIEWPORT),
    ]
}

/// The heights the ceiling tile's own surface spans: what its descendants,
/// cut out of it, can answer.
fn ceiling_span() -> (f64, f64) {
    let rect = GeographicTilingScheme::default().tile_rect(CEILING);
    let mut span = (f64::INFINITY, f64::NEG_INFINITY);
    for j in 0..=STEPS {
        for i in 0..=STEPS {
            let (u, v) = (i as f64 / STEPS as f64, j as f64 / STEPS as f64);
            let h = relief(rect.west + u * rect.width(), rect.south + v * rect.height());
            span = (span.0.min(h), span.1.max(h));
        }
    }
    span
}

/// Settles every camera and reads the height under its eye.
fn under_the_eyes(told: Told) -> Vec<(u32, Option<f64>)> {
    let mut session = session(told);
    let numbers: Vec<u32> = (0..CAMERAS.len() as u32).collect();
    settle_each(
        &mut session,
        &mut GroundDrawer::new(),
        DRAWN,
        &numbers,
        views,
        |i, ground: &Arc<Ground>| {
            let (frame, eye, _) = CAMERAS[i as usize];
            let at = ecef_to_geodetic(DVec3::from_array(eye));
            (frame, ground.height_at(at.lon, at.lat))
        },
    )
    .expect("settled")
    .into_iter()
    .map(|s| s.value)
    .collect()
}

fn the_ceiling_answers(told: Told) {
    // Every eye is over the ceiling tile.
    let scheme = GeographicTilingScheme::default();
    let rect = scheme.tile_rect(CEILING);
    for (n, eye, _) in CAMERAS {
        let at = ecef_to_geodetic(DVec3::from_array(*eye));
        assert!(
            rect.west <= at.lon
                && at.lon <= rect.east
                && rect.south <= at.lat
                && at.lat <= rect.north,
            "frame {n}: its eye is not over {CEILING:?}"
        );
    }
    let (low, high) = ceiling_span();
    for (n, height) in under_the_eyes(told) {
        assert!(
            height.is_some(),
            "{told:?}: frame {n}: no ground under the eye, where the level-13 tile is"
        );
        let height = height.unwrap_or_default();
        // The level-13 surface, smoothed within its bound: nothing else is
        // there to answer.
        assert!(
            height >= low - f64::from(DEFAULT_BOUND_M) && height <= high + f64::from(DEFAULT_BOUND_M),
            "{told:?}: frame {n}: {height:.1} m, outside the level-13 surface's {low:.1}..{high:.1} m"
        );
    }
}

#[test]
fn a_level_the_availability_does_not_list_is_drawn_from_the_ancestor_that_has_one() {
    the_ceiling_answers(Told::Honestly);
}

#[test]
fn a_tile_the_source_says_it_does_not_have_is_drawn_from_the_ancestor_that_has_one() {
    the_ceiling_answers(Told::Not);
}
