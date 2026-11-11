// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The memory bench's flight, shared by the test binaries that count its
//! bytes (`bake_memory`) and that say where they were allocated
//! (`bake_memory_sites`). See `tests/bake_memory.rs`.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Instant;

use crate::support::{measured, LAT, LON};
use glam::DVec3;
use tuile_bake::settle::{read_each, settle_each, Drawn};
use tuile_bake::{pose_view, Finish, GlobeConfig, GroundDrawer, Session, SourcesHandle};
use tuile_core::fetch::Fetched;
use tuile_core::raster::{ImageryCoord, ImageryProvider, RasterError, TilingScheme};
use tuile_film::ground::{Ground, Prepare};
use tuile_film::smoothing::{Smooth, DEFAULT_BOUND_M};
use tuile_terrain::{LayerJson, TerrainSource, TerrainSourceError, TileCoord};

pub fn knob<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// The finest terrain the made-up source has: a real source's, about.
const DEEPEST: u32 = 16;

struct Relief {
    pub steps: usize,
}

#[async_trait::async_trait]
impl TerrainSource for Relief {
    async fn fetch_tile(&self, coord: TileCoord) -> Result<Fetched<Vec<u8>>, TerrainSourceError> {
        if coord.level > DEEPEST {
            return Err(TerrainSourceError::no_such_tile(format!("{coord:?}")));
        }
        Ok(Fetched::undated(with_normals(&measured(coord, self.steps))))
    }
}

/// A terrain tile's bytes as a real source serves them: the relief, its
/// four edges (so the ground hangs skirts from them) and the oct-encoded
/// normals extension a real source sends — the vertices numbered in the
/// order the triangles first use them, as the format codes its indices.
pub fn with_normals(mesh: &tuile_terrain::QuantizedMesh) -> Vec<u8> {
    let mut order: Vec<u32> = Vec::new();
    let mut renumbered = vec![u32::MAX; mesh.vertex_count()];
    for &i in &mesh.indices {
        if renumbered[i as usize] == u32::MAX {
            renumbered[i as usize] = order.len() as u32;
            order.push(i);
        }
    }
    let pick = |values: &[f64]| -> Vec<f64> { order.iter().map(|&i| values[i as usize]).collect() };
    // Up, leaned by the relief's own slope across the tile: a normal a
    // vertex of its own, as a real tile's are.
    let normals: Vec<[f32; 3]> = order
        .iter()
        .map(|&i| {
            let (u, v) = (mesh.u[i as usize], mesh.v[i as usize]);
            let lean = 0.2 * (mesh.height[i as usize] - 0.5);
            let n = DVec3::new(0.3 + lean * u, -0.2 + lean * v, 1.0).normalize();
            [n.x as f32, n.y as f32, n.z as f32]
        })
        .collect();
    let indices: Vec<u32> = mesh
        .indices
        .iter()
        .map(|&i| renumbered[i as usize])
        .collect();
    let edges = mesh
        .edges
        .clone()
        .map(|edge| edge.iter().map(|&i| renumbered[i as usize]).collect());
    tuile_terrain::decode::encode(
        &mesh.header,
        &pick(&mesh.u),
        &pick(&mesh.v),
        &pick(&mesh.height),
        &indices,
        Some(&normals),
        &edges,
    )
}

/// Imagery the farm's store describes without its bytes: a stamp of a
/// 256 × 256 picture, never a pixel — what a bake by reference is given.
struct Described;

#[async_trait::async_trait]
impl ImageryProvider for Described {
    fn tiling_scheme(&self) -> TilingScheme {
        TilingScheme::web_mercator()
    }

    async fn fetch_tile_bytes(
        &self,
        coord: ImageryCoord,
    ) -> Result<Fetched<bytes::Bytes>, RasterError> {
        Err(RasterError::Image(format!(
            "{coord:?}: described, never fetched"
        )))
    }

    async fn fetch_tile_stamp(&self, coord: ImageryCoord) -> tuile_core::storage::Stamp {
        tuile_core::storage::Stamp::Present(tuile_core::storage::ValueStamp {
            digest: u64::from(coord.level) << 48 ^ coord.x << 24 ^ coord.y,
            width: 256,
            height: 256,
        })
    }
}

pub struct MadeUp {
    pub steps: usize,
}

#[async_trait::async_trait]
impl tuile_bake::Sources for MadeUp {
    async fn terrain(&self, _asset: i64) -> Result<(LayerJson, Arc<dyn TerrainSource>), String> {
        let available: Vec<String> = (0..=DEEPEST)
            .map(|level| {
                format!(
                    r#"[{{"startX":0,"startY":0,"endX":{},"endY":{}}}]"#,
                    (2u64 << level) - 1,
                    (1u64 << level) - 1
                )
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
        Ok((layer, Arc::new(Relief { steps: self.steps })))
    }

    async fn imagery(&self, _asset: i64) -> Result<Arc<dyn ImageryProvider>, String> {
        Ok(Arc::new(Described))
    }
}

/// A high follow's camera at frame `i`: 600 m over the ground, looking
/// ahead and down at the athlete, flying north 40 m a frame — a film's
/// time compressed as an export's is.
pub fn pose(i: u32) -> tuile_tape::Frame {
    let height: f64 = knob("BAKE_MEMORY_EYE_M", 700.0);
    let pitch: f64 = knob("BAKE_MEMORY_PITCH", 0.45);
    let lat = LAT + f64::from(i) * 0.00036;
    let (p, l) = (lat.to_radians(), LON.to_radians());
    let up = DVec3::new(p.cos() * l.cos(), p.cos() * l.sin(), p.sin());
    let north = DVec3::new(-p.sin() * l.cos(), -p.sin() * l.sin(), p.cos());
    let eye = tuile_core::geo::geodetic_to_ecef(tuile_core::geo::Geodetic {
        lon: l,
        lat: p,
        height,
    });
    let direction = (north - up * pitch).normalize();
    let camera_up = direction.cross(north.cross(up)).normalize() * -1.0;
    tuile_tape::Frame {
        position: eye.to_array(),
        direction: direction.to_array(),
        up: camera_up.to_array(),
        fovy: std::f64::consts::FRAC_PI_4,
    }
}

pub const DRAWN: Drawn = Drawn {
    how: Prepare {
        skirts: true,
        smooth: Smooth::Geometry {
            bound: DEFAULT_BOUND_M,
        },
    },
    stitch: true,
};

/// What a frame reads off its ground: its height under the eye.
pub fn under(i: u32, ground: &Ground) -> Option<u64> {
    let lat = (LAT + f64::from(i) * 0.00036).to_radians();
    ground.height_at(LON.to_radians(), lat).map(f64::to_bits)
}

/// One row: where the process stood after a frame of a stage.
#[derive(Default, Clone)]
struct Row {
    stage: &'static str,
    frame: u32,
    millis: u64,
    heap_live: u64,
    heap_peak: u64,
    ledger_bytes: u64,
    ledger_imagery_bytes: u64,
    selected_tiles: u64,
    kept_tiles: u64,
    kept_bytes: u64,
    prepared_tiles: u64,
    prepared_bytes: u64,
    settled_tiles: u64,
    settled_bytes: u64,
    reads_pending: u64,
    reads_pending_bytes: u64,
}

fn row(
    stage: &'static str,
    frame: u32,
    session: Option<&Session>,
    began: Instant,
    live: fn() -> usize,
    peak: fn() -> usize,
) -> Row {
    let m = tuile_core::metrics::metrics();
    let ground = session.and_then(Session::ground_memory).unwrap_or_default();
    Row {
        stage,
        frame,
        millis: began.elapsed().as_millis() as u64,
        heap_live: live() as u64,
        heap_peak: peak() as u64,
        ledger_bytes: m.resident_bytes.get(),
        ledger_imagery_bytes: m.imagery_bytes.get(),
        selected_tiles: m.tiles_selected.get(),
        kept_tiles: ground.kept_tiles as u64,
        kept_bytes: ground.kept_bytes as u64,
        prepared_tiles: ground.prepared_tiles as u64,
        prepared_bytes: ground.prepared_bytes as u64,
        settled_tiles: m.settled_tiles.get(),
        settled_bytes: m.settled_bytes.get(),
        reads_pending: m.ground_reads_pending.get(),
        reads_pending_bytes: m.ground_reads_pending_bytes.get(),
    }
}

fn write_parquet(rows: &[Row], path: &std::path::Path) {
    use arrow_array::{ArrayRef, RecordBatch, StringArray, UInt32Array, UInt64Array};
    let u64s = |f: fn(&Row) -> u64| -> ArrayRef {
        Arc::new(UInt64Array::from(rows.iter().map(f).collect::<Vec<_>>()))
    };
    let columns: Vec<(&str, ArrayRef)> = vec![
        (
            "stage",
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.stage).collect::<Vec<_>>(),
            )),
        ),
        (
            "frame",
            Arc::new(UInt32Array::from(
                rows.iter().map(|r| r.frame).collect::<Vec<_>>(),
            )),
        ),
        ("millis", u64s(|r| r.millis)),
        ("heap_live", u64s(|r| r.heap_live)),
        ("heap_peak", u64s(|r| r.heap_peak)),
        ("ledger_bytes", u64s(|r| r.ledger_bytes)),
        ("ledger_imagery_bytes", u64s(|r| r.ledger_imagery_bytes)),
        ("selected_tiles", u64s(|r| r.selected_tiles)),
        ("kept_tiles", u64s(|r| r.kept_tiles)),
        ("kept_bytes", u64s(|r| r.kept_bytes)),
        ("prepared_tiles", u64s(|r| r.prepared_tiles)),
        ("prepared_bytes", u64s(|r| r.prepared_bytes)),
        ("settled_tiles", u64s(|r| r.settled_tiles)),
        ("settled_bytes", u64s(|r| r.settled_bytes)),
        ("reads_pending", u64s(|r| r.reads_pending)),
        ("reads_pending_bytes", u64s(|r| r.reads_pending_bytes)),
    ];
    let batch = RecordBatch::try_from_iter(columns).expect("a batch");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = std::fs::File::create(path).expect("the table's file");
    let mut writer =
        parquet::arrow::ArrowWriter::try_new(file, batch.schema(), None).expect("a writer");
    writer.write(&batch).expect("written");
    writer.close().expect("closed");
}

fn gib(b: u64) -> f64 {
    b as f64 / f64::from(1u32 << 30)
}

/// Flies the stages and writes the table; `live` and `peak` read the
/// test binary's allocator.
pub fn run(live: fn() -> usize, peak: fn() -> usize) {
    let frames: u32 = knob("BAKE_MEMORY_FRAMES", 96);
    let viewport = (
        knob("BAKE_MEMORY_WIDTH", 3840.0),
        knob("BAKE_MEMORY_HEIGHT", 2160.0),
    );
    let sse: f64 = knob("BAKE_MEMORY_SSE", 3.0);
    let steps: usize = knob("BAKE_MEMORY_STEPS", 32);
    let keep_gib: f64 = knob("BAKE_MEMORY_KEEP_GIB", 1.0);
    let out = std::env::var("BAKE_MEMORY_OUT")
        .unwrap_or_else(|_| "target/bake-memory.parquet".to_string());

    let mut config = GlobeConfig::new("");
    config.sources = Some(SourcesHandle(Arc::new(MadeUp { steps })));
    config.session.traversal.maximum_screen_space_error = sse;
    config.session.finish = Finish::Identity;
    config.session.release_beyond = Some((keep_gib * f64::from(1u32 << 30)) as u64);
    let began = Instant::now();
    let before = live();
    let mut session = Session::globe(config).expect("a globe");
    session.keep_ground();
    let mut drawer = GroundDrawer::new();
    let mut rows: Vec<Row> = vec![row("open", 0, Some(&session), began, live, peak)];

    // 1. The survey: every frame of the window settled once, its ground
    //    read, its selection kept — a smoothed film's window.
    let numbers: Vec<u32> = (0..frames).collect();
    let views = |i: u32, _: &Session| vec![pose_view(&pose(i), viewport)];
    let sampled = std::sync::Mutex::new(Vec::new());
    let settled = settle_each(
        &mut session,
        &mut drawer,
        DRAWN,
        &numbers,
        views,
        |i, ground: &Arc<Ground>| {
            let value = under(i, ground);
            if let Ok(mut s) = sampled.lock() {
                s.push(row("survey", i, None, began, live, peak));
            }
            value
        },
        |_| true,
    )
    .expect("settled");
    let mut survey = sampled.into_inner().expect("rows");
    survey.sort_by_key(|r| r.frame);
    rows.extend(survey);
    rows.push(row(
        "survey-done",
        frames,
        Some(&session),
        began,
        live,
        peak,
    ));

    // 2. The window's grounds, built again from the selections kept.
    let kept: Vec<(u32, &tuile_bake::Selected)> = settled
        .iter()
        .filter_map(|s| Some((s.number, s.selected.as_ref()?)))
        .collect();
    let sampled = std::sync::Mutex::new(Vec::new());
    let _ = read_each(&mut drawer, DRAWN, &kept, |i, ground| {
        let value = under(i, ground);
        if let Ok(mut s) = sampled.lock() {
            s.push(row("hud", i, None, began, live, peak));
        }
        value
    });
    let mut hud = sampled.into_inner().expect("rows");
    hud.sort_by_key(|r| r.frame);
    rows.extend(hud);
    drop(kept);
    drop(settled);
    rows.push(row("hud-done", frames, Some(&session), began, live, peak));

    // 3. The bake's own frames, one pose a convergence.
    for i in 0..frames {
        session.frame_for_pose(&pose(i), viewport).expect("a frame");
        rows.push(row("bake", i, Some(&session), began, live, peak));
    }
    drop(session);
    drop(drawer);
    rows.push(row("closed", frames, None, began, live, peak));

    let peak = rows.iter().map(|r| r.heap_peak).max().unwrap_or(0);
    let stage_peak = |stage: &str| {
        rows.iter()
            .filter(|r| r.stage == stage)
            .map(|r| r.heap_live)
            .max()
            .unwrap_or(0)
    };
    let most = |f: fn(&Row) -> u64| rows.iter().map(f).max().unwrap_or(0);
    eprintln!(
        "BENCH {{\"frames\":{frames},\"viewport\":[{},{}],\"sse\":{sse},\"steps\":{steps},\"keep_gib\":{keep_gib},\
\"heap_before_gib\":{:.3},\"heap_peak_gib\":{:.3},\"survey_live_max_gib\":{:.3},\"hud_live_max_gib\":{:.3},\"bake_live_max_gib\":{:.3},\
\"closed_live_gib\":{:.3},\"ledger_max_gib\":{:.3},\"ledger_imagery_max_gib\":{:.3},\"selected_max\":{},\"kept_tiles_max\":{},\"kept_max_gib\":{:.3},\
\"prepared_max_gib\":{:.3},\"settled_tiles_max\":{},\"settled_max_gib\":{:.3},\"reads_pending_max\":{},\"reads_pending_max_gib\":{:.3},\"seconds\":{:.1}}}",
        viewport.0,
        viewport.1,
        gib(before as u64),
        gib(peak),
        gib(stage_peak("survey")),
        gib(stage_peak("hud")),
        gib(stage_peak("bake")),
        gib(rows.last().map_or(0, |r| r.heap_live)),
        gib(most(|r| r.ledger_bytes)),
        gib(most(|r| r.ledger_imagery_bytes)),
        most(|r| r.selected_tiles),
        most(|r| r.kept_tiles),
        gib(most(|r| r.kept_bytes)),
        gib(most(|r| r.prepared_bytes)),
        most(|r| r.settled_tiles),
        gib(most(|r| r.settled_bytes)),
        most(|r| r.reads_pending),
        gib(most(|r| r.reads_pending_bytes)),
        began.elapsed().as_secs_f64(),
    );
    write_parquet(&rows, std::path::Path::new(&out));
    eprintln!("table: {out} ({} rows)", rows.len());
}
