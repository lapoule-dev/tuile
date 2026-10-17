// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **A terrain projected tile by tile bakes the pack a whole one does.**
//!
//! A made-up globe — a relief measured by each terrain tile, one picture for
//! every imagery tile — behind a tile store in memory. A first bake fills the
//! store from the sources. Then the same frames are baked twice from the
//! store: once with every reached zone projected whole, as before; once with
//! the terrain's zones cut to the tiles its footprint keeps, at the factor
//! the bake's own screen-space error and picture height give, and the
//! imagery known by its archives' indexes alone. The two packs
//! are the same bytes, and the second read nothing from the bucket: every
//! tile it asked for was in its projections.

#[path = "../../tuile-terrain/tests/support/mod.rs"]
mod support;

use std::sync::Arc;

use glam::DVec3;
use support::{encoded, measured, tile_at, LAT, LON};
use tuile_bake::bake::{bake_frames, BakeFrames, PackContent, PackedMirror, Poses, SceneName};
use tuile_bake::tiles::{terrain_tile_factor, Tiles};
use tuile_bake::{exact_traversal, source_namespace, Finish, GlobeConfig, Session, SourcesHandle};
use tuile_core::fetch::Fetched;
use tuile_core::raster::{ImageryCoord, ImageryProvider, RasterError, TilingScheme};
use tuile_terrain::{LayerJson, TerrainSource, TerrainSourceError, TileCoord};
use tuile_tile_server::{Eye, Footprint, Grid, LayerDef, StoreConfig, TileStore};

/// The finest terrain the made-up source has.
const DEEPEST: u32 = 14;
/// The picture.
const VIEWPORT: (f64, f64) = (480.0, 640.0);
const SSE: f64 = 6.0;
const FRAMES: u32 = 3;

/// The relief, served as quantized mesh at every level down to [`DEEPEST`].
struct Relief;

#[async_trait::async_trait]
impl TerrainSource for Relief {
    async fn fetch_tile(&self, coord: TileCoord) -> Result<Fetched<Vec<u8>>, TerrainSourceError> {
        if coord.level > DEEPEST {
            return Err(TerrainSourceError::no_such_tile(format!("{coord:?}")));
        }
        Ok(Fetched::undated(encoded(&measured(coord, 4))))
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

struct MadeUp;

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
        Ok((layer, Arc::new(Relief)))
    }

    async fn imagery(&self, _asset: i64) -> Result<Arc<dyn ImageryProvider>, String> {
        Ok(Arc::new(OnePicture))
    }
}

/// A camera flying north over the relief, a kilometre up, looking ahead
/// and down.
fn poses() -> Vec<tuile_tape::Frame> {
    (0..FRAMES)
        .map(|i| {
            let lat = LAT + f64::from(i) * 0.002;
            let (p, l) = (lat.to_radians(), LON.to_radians());
            let up = DVec3::new(p.cos() * l.cos(), p.cos() * l.sin(), p.sin());
            let north = DVec3::new(-p.sin() * l.cos(), -p.sin() * l.sin(), p.cos());
            let eye = tuile_core::geo::geodetic_to_ecef(tuile_core::geo::Geodetic {
                lon: l,
                lat: p,
                height: 1_300.0,
            });
            let direction = (north - up * 0.6).normalize();
            let camera_up = direction.cross(north.cross(up)).normalize() * -1.0;
            tuile_tape::Frame {
                position: eye.to_array(),
                direction: direction.to_array(),
                up: camera_up.to_array(),
                fovy: std::f64::consts::FRAC_PI_4,
            }
        })
        .collect()
}

/// The terrain's layer and the imagery's, as the bake names them.
fn layers() -> (String, String) {
    let config = GlobeConfig::new("");
    (
        source_namespace(config.terrain_asset_id),
        source_namespace(config.imagery_asset_id.expect("an imagery")),
    )
}

/// A bucket in memory, its catalog naming the two layers.
fn bucket() -> Arc<dyn object_store::ObjectStore> {
    let objects: Arc<dyn object_store::ObjectStore> =
        Arc::new(object_store::memory::InMemory::new());
    let (terrain, imagery) = layers();
    let catalog = tuile_tile_server::Catalog {
        layers: vec![
            LayerDef {
                name: terrain,
                grid: Grid::Geographic,
                tile_type: "other".into(),
                tile_compression: "none".into(),
                zone_level: 9,
                expiry_days: None,
                content_type: "application/vnd.quantized-mesh".into(),
                index: false,
                licence: None,
                attribution: None,
            },
            LayerDef {
                name: imagery,
                grid: Grid::WebMercator,
                tile_type: "png".into(),
                tile_compression: "none".into(),
                zone_level: 10,
                expiry_days: None,
                content_type: "image/png".into(),
                // Indexed: the targeted bake knows its imagery by the
                // archives' indexes, as a bake by reference does.
                index: true,
                licence: None,
                attribution: None,
            },
        ],
    };
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(tuile_tile_server::catalog::write(
            objects.as_ref(),
            &catalog,
        ))
        .expect("catalog");
    objects
}

/// Terrain the bake will not read, put in the store beside what it will:
/// every tile of levels 12 to 14 over a block of 13 × 13 level-12 tiles
/// around the scene — the source's own bytes, as a wider flight would have
/// left them.
fn neighbourhood(objects: Arc<dyn object_store::ObjectStore>) {
    let (terrain, _) = layers();
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let store = TileStore::open(objects, StoreConfig::default())
            .await
            .expect("store");
        let centre = tile_at(12, LON, LAT);
        for dx in 0..13u64 {
            for dy in 0..13u64 {
                let (x, y) = (centre.x + dx - 6, centre.y + dy - 6);
                for depth in 0..=2u32 {
                    for i in 0..(1u64 << depth) {
                        for j in 0..(1u64 << depth) {
                            let coord =
                                TileCoord::new(12 + depth, (x << depth) + i, (y << depth) + j);
                            store
                                .put(
                                    &terrain,
                                    coord.level as u8,
                                    coord.x as u32,
                                    coord.y as u32,
                                    bytes::Bytes::from(encoded(&measured(coord, 4))),
                                )
                                .await
                                .expect("put");
                        }
                    }
                }
            }
        }
        store.flush_all().await.expect("flush");
    });
}

/// Bakes the frames through `tiles` into a pack of references at `out`.
fn bake(tiles: &Tiles, out: &std::path::Path) {
    let mut config = GlobeConfig::new("");
    config.sources = Some(SourcesHandle(Arc::new(MadeUp)));
    config.session.traversal.maximum_screen_space_error = SSE;
    config.session.finish = Finish::Identity;
    config.tile_cache = Some(tiles.cache());
    let packed = PackedMirror::new();
    config.held_drape = Some(packed.held_drape());
    let (terrain_layer, imagery_layer) = layers();
    let content = PackContent {
        content: tuile_pack::Content::References,
        terrain_layer,
        imagery_layer,
    };
    let poses = poses();
    let mut session = Session::globe(config).expect("a globe");
    bake_frames(
        &mut session,
        &BakeFrames {
            first: 1,
            last: FRAMES,
            viewport: VIEWPORT,
            out,
            scene: SceneName::Given("made-up".into()),
            culling: "full",
            content: &content,
            packed: &packed,
        },
        &mut Poses(&poses),
    )
    .expect("a bake");
}

fn eyes() -> Vec<Eye> {
    poses()
        .iter()
        .map(|p| {
            let g = tuile_core::geo::ecef_to_geodetic(DVec3::from_array(p.position));
            Eye {
                lon: g.lon.to_degrees(),
                lat: g.lat.to_degrees(),
                height: g.height,
            }
        })
        .collect()
}

#[test]
fn a_terrain_projected_tile_by_tile_bakes_the_pack_a_whole_one_does() {
    // One fetch at a time. With the bake's usual sixty-four in flight, two
    // bakes of the same frames from the same projections do not always
    // select the same far, coarse tiles — the order tiles arrive in decides
    // (measured: three runs of four, a level-2 tile against four level-3
    // ones). That variation is the bake's own and the same whatever the
    // projection; one fetch at a time takes it away, so that a difference
    // left is the projection's. The only test in this binary: nothing else
    // reads the environment.
    std::env::set_var("TUILE_FETCHES", "1");
    let objects = bucket();
    let dir = tempfile::tempdir().expect("dir");

    // The store filled from the sources, by a first bake, and around it.
    neighbourhood(objects.clone());
    let warm = Tiles::over(objects.clone()).expect("store");
    bake(&warm, &dir.path().join("warm.tuilepack"));
    warm.flush().expect("flush");

    // Every reached zone whole: what a projection did until now.
    let whole = Tiles::over(objects.clone()).expect("store");
    let report = whole
        .project_footprint(
            &Footprint::from_eyes(eyes(), 12.0),
            &dir.path().join("whole"),
        )
        .expect("project");
    let (terrain, _) = layers();
    assert!(!report.layers[&terrain].targeted, "{report:?}");
    let a = dir.path().join("whole.tuilepack");
    bake(&whole, &a);

    // The imagery's register, rebuilt from the bucket: what a host keeps.
    let register = Arc::new(tuile_tile_server::MemoryRegister::new());
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(async {
            let store = TileStore::open(objects.clone(), StoreConfig::default()).await?;
            for layer in store.layers().filter(|l| l.index) {
                for zone in store.zones_of(layer).await? {
                    let read = store.zone_tiles(&layer.name, zone, 4).await?;
                    register.rebuild_zone(layer, zone, &read.manifest, &read.archives);
                }
            }
            Ok::<_, tuile_tile_server::StoreError>(())
        })
        .expect("register");

    // The terrain cut to what this bake can read.
    // Set up as a host does: on the store itself, before it is shared.
    let targeted = Tiles::over(objects)
        .expect("store")
        .with_store(|store| store.set_index_reader(register))
        .expect("reader");
    let traversal = {
        let mut config = GlobeConfig::new("");
        config.session.traversal.maximum_screen_space_error = SSE;
        exact_traversal(config.session.traversal)
    };
    let factor = terrain_tile_factor(&traversal, VIEWPORT.1, std::f64::consts::FRAC_PI_4);
    let (_, imagery) = layers();
    let footprint = Footprint::from_eyes(eyes(), factor)
        .with_targeted_layer(&terrain)
        .with_layer_by_index(&imagery);
    let report = targeted
        .project_footprint(&footprint, &dir.path().join("targeted"))
        .expect("project");
    let cut = &report.layers[&terrain];
    assert!(cut.targeted, "{report:?}");
    let indexed = &report.layers[&imagery];
    assert!(
        indexed.zones_registered > 0 && indexed.bytes == 0,
        "the imagery known by the register: {indexed:?}"
    );
    assert!(
        cut.tiles_kept < cut.tiles_listed,
        "the terrain was cut: {cut:?}"
    );
    let b = dir.path().join("targeted.tuilepack");
    bake(&targeted, &b);

    let (wa, wb) = (whole.stats(), targeted.stats());
    assert!(wa.projection > 0, "{wa:?}");
    assert_eq!(
        (wb.projection_misses, wb.remote, wb.absent),
        (0, 0, 0),
        "every tile the bake read was projected: {wb:?}"
    );
    assert!(wb.projection > 0, "{wb:?}");
    assert!(
        std::fs::read(&a).expect("pack") == std::fs::read(&b).expect("pack"),
        "the two packs differ"
    );
}
