// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A pack by reference over imagery projected by the store's register: not
//! one imagery archive read, and the same pack, byte for byte, as over
//! imagery projected from its archives — and as straight from the source.
//!
//! The tiles are loaded by the globe's own loader through the engine's own
//! store-backed cache, finished by identity and written by the pack writer:
//! everything a bake does to a tile once it is selected. What is not here is
//! the selection — a traversal over a synthetic terrain — which reads terrain
//! and no imagery pixel.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use object_store::ObjectStore;
use tuile_core::fetch::{FetchError, Fetched};
use tuile_core::raster::{CachedImagery, ImageryCoord, ImageryProvider, RasterError, TilingScheme};
use tuile_core::source::{Loaded, TileId};
use tuile_planetary::{globe_with_provenance, GlobeOptions};
use tuile_terrain::{GeographicTilingScheme, TerrainSource, TerrainSourceError, TileCoord};
use tuile_tile_server::{
    catalog, Compression, Eye, Footprint, Grid, Layer, StoreConfig, StoreContent, TileStore,
    TileType,
};

use crate::bake::packed_tile;
use crate::session::{finish_tile_as, Finish, Frame, TextureMemo};

const LAYER: &str = "imagery";
const BAKE_SIZE: u32 = 2048;
/// Terrain tiles a frame "selects": a three-by-three patch at this level.
const TERRAIN_LEVEL: u32 = 12;
const PATCH: (u64, u64) = (4189, 2967);

/// Flat ground, one quad per tile, centred where the tile is.
struct Flat;

#[async_trait::async_trait]
impl TerrainSource for Flat {
    async fn fetch_tile(&self, c: TileCoord) -> Result<Fetched<Vec<u8>>, TerrainSourceError> {
        let r = GeographicTilingScheme::default().tile_rect(c);
        let centre = tuile_core::geo::geodetic_to_ecef(tuile_core::geo::Geodetic {
            lon: (r.west + r.east) / 2.0,
            lat: (r.south + r.north) / 2.0,
            height: 500.0,
        })
        .to_array();
        let header = tuile_terrain::Header {
            center: centre,
            min_height: 400.0,
            max_height: 600.0,
            bounding_sphere_center: centre,
            bounding_sphere_radius: 1.0e5,
            horizon_occlusion: [0.0; 3],
        };
        Ok(Fetched::undated(tuile_terrain::decode::encode(
            &header,
            &[0.0, 1.0, 0.0, 1.0],
            &[0.0, 0.0, 1.0, 1.0],
            &[0.0, 0.25, 0.5, 1.0],
            &[0, 1, 2, 2, 1, 3],
            None,
            &[vec![0, 2], vec![0, 1], vec![1, 3], vec![2, 3]],
        )))
    }
}

/// The imagery's origin: a PNG per tile, and none where `absent` says.
/// Counts what it is asked.
struct Origin(AtomicU64);

fn absent(c: ImageryCoord) -> bool {
    // A few of the finest tiles the drapes ask for: their parents stand in.
    c.level >= 13 && (c.x + c.y).is_multiple_of(7)
}

fn png_of(c: ImageryCoord) -> Bytes {
    let mut img = image::RgbaImage::new(256, 256);
    for (i, p) in img.pixels_mut().enumerate() {
        let v = (c.x.wrapping_mul(31) ^ c.y.wrapping_mul(17)) as u32 ^ c.level ^ (i as u32 >> 6);
        *p = image::Rgba([v as u8, (v >> 3) as u8, 90, 255]);
    }
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .expect("png");
    Bytes::from(out)
}

#[async_trait::async_trait]
impl ImageryProvider for Origin {
    fn tiling_scheme(&self) -> TilingScheme {
        TilingScheme::web_mercator()
    }

    async fn fetch_tile_bytes(&self, c: ImageryCoord) -> Result<Fetched<Bytes>, RasterError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        if absent(c) {
            return Err(RasterError::Fetch(FetchError::NotFound(
                url::Url::parse(&format!(
                    "https://origin.invalid/{}/{}/{}",
                    c.level, c.x, c.y
                ))
                .expect("url"),
            )));
        }
        Ok(Fetched::undated(png_of(c)))
    }
}

fn layers() -> Vec<Layer> {
    let layer = Layer {
        name: LAYER.into(),
        grid: Grid::WebMercator,
        tile_type: TileType::Png,
        tile_compression: Compression::None,
        zone_level: 10,
        expiry: None,
        content_type: "image/png".into(),
        index: true,
    };
    vec![catalog::absence_of(&layer), layer]
}

/// An object store that counts the reads of archives.
#[derive(Debug)]
struct Counting {
    inner: Arc<dyn ObjectStore>,
    archive_gets: AtomicU64,
}

impl std::fmt::Display for Counting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Counting({})", self.inner)
    }
}

type OsPath = object_store::path::Path;
type OsStream<T> = futures_util::stream::BoxStream<'static, object_store::Result<T>>;

impl Counting {
    fn note(&self, at: &OsPath) {
        if at.as_ref().ends_with(".pmtiles") {
            self.archive_gets.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[async_trait::async_trait]
impl ObjectStore for Counting {
    async fn put_opts(
        &self,
        location: &OsPath,
        payload: object_store::PutPayload,
        opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }
    async fn put_multipart_opts(
        &self,
        location: &OsPath,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }
    async fn get_opts(
        &self,
        location: &OsPath,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.note(location);
        self.inner.get_opts(location, options).await
    }
    async fn get_ranges(
        &self,
        location: &OsPath,
        ranges: &[std::ops::Range<u64>],
    ) -> object_store::Result<Vec<Bytes>> {
        self.note(location);
        self.inner.get_ranges(location, ranges).await
    }
    fn delete_stream(&self, locations: OsStream<OsPath>) -> OsStream<OsPath> {
        self.inner.delete_stream(locations)
    }
    fn list(&self, prefix: Option<&OsPath>) -> OsStream<object_store::ObjectMeta> {
        self.inner.list(prefix)
    }
    fn list_with_offset(
        &self,
        prefix: Option<&OsPath>,
        offset: &OsPath,
    ) -> OsStream<object_store::ObjectMeta> {
        self.inner.list_with_offset(prefix, offset)
    }
    async fn list_with_delimiter(
        &self,
        prefix: Option<&OsPath>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }
    async fn copy_opts(
        &self,
        from: &OsPath,
        to: &OsPath,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// The patch of terrain tiles every pass loads.
fn patch() -> Vec<TileId> {
    let mut out = Vec::new();
    for dx in 0..3 {
        for dy in 0..3 {
            out.push(TileId::from_terrain(
                TERRAIN_LEVEL,
                PATCH.0 + dx,
                PATCH.1 + dy,
            ));
        }
    }
    out
}

/// `mesh` wearing its own `drape`, as a session dresses a tile drawn exactly.
fn dressed(
    mesh: &tuile_core::DecodedTileContent,
    tile: TileId,
    drape: tuile_core::content::DrapeContent,
) -> tuile_core::DecodedTileContent {
    use tuile_core::budget::{DrapeChoice, DrawItem, ResourceId, Role, UvTransform};
    let resource = ResourceId::drape(tile, drape.layer, drape.level);
    let item = DrawItem {
        mesh: ResourceId::Mesh(tile),
        role: Role::Exact,
        drapes: [DrapeChoice {
            resource,
            layer: drape.layer,
            level: drape.level,
            uv: UvTransform::IDENTITY,
        }]
        .into_iter()
        .collect(),
        serves: [tile].into_iter().collect(),
    };
    let drapes = std::collections::HashMap::from([(resource, Arc::new(drape))]);
    tuile_core::drive::dress(mesh, &item, &drapes)
}

/// Loads the patch through `imagery`, finishes every tile by identity and
/// writes the pack of references. Returns its bytes.
async fn pack_of<I: ImageryProvider + 'static>(imagery: I, by_stamp: bool) -> Vec<u8> {
    let layer = tuile_terrain::LayerJson::from_slice(
        br#"{"format":"quantized-mesh-1.0","tiles":["{z}/{x}/{y}.terrain"],"maxzoom":16}"#,
    )
    .expect("layer.json");
    let (_tree, loader, _heights, provenance) = globe_with_provenance(
        Flat,
        imagery,
        layer,
        GlobeOptions {
            deterministic_floor: true,
            imagery_by_stamp: by_stamp,
            ..GlobeOptions::default()
        },
        tuile_core::offload::threaded(),
    );
    let memo = Arc::new(TextureMemo::default());
    let mut tiles = Vec::new();
    for id in patch() {
        let Loaded::Content(mesh) = loader.load(id).await.expect("load") else {
            unreachable!("terrain loads content");
        };
        // The tile's own drape, at the imagery level of its terrain's: what
        // the budget asks of a tile seen at its terrain's density.
        let layer = tuile_core::budget::LayerId(0);
        let drape = loader
            .load_drape(id, layer, TERRAIN_LEVEL)
            .await
            .expect("drape");
        let content = dressed(&mesh, id, drape);
        tiles.push(Arc::new(finish_tile_as(
            &memo,
            "t",
            id,
            content,
            BAKE_SIZE,
            Some(&provenance),
            Finish::Identity,
        )));
    }
    let frame = Frame::with_memo("t", tiles, memo);
    let dir = tempfile::tempdir().expect("tempdir");
    let mut writer =
        tuile_pack::PackWriter::new("scene".to_string(), [0.0; 3], dir.path().join("blob.part"))
            .expect("writer")
            .with_content(tuile_pack::Content::References, "terrain", LAYER);
    let mut open = writer.begin_frame(
        1,
        tuile_pack::BakedView {
            position: [1.0, 2.0, 3.0],
            direction: [0.0, 0.0, -1.0],
            up: [0.0, 1.0, 0.0],
            viewport_px: [540.0, 960.0],
            fovy_rad: std::f64::consts::FRAC_PI_4,
        },
    );
    for (index, tile) in frame.tiles.iter().enumerate() {
        open.push(
            packed_tile(&frame, index, tile, tuile_pack::Content::References)
                .expect("a tile for the pack"),
        );
    }
    open.end();
    let path = dir.path().join("p.tuilepack");
    writer.finish_to(&path).expect("pack");
    std::fs::read(&path).expect("read")
}

fn footprint() -> Footprint {
    let r = GeographicTilingScheme::default().tile_rect(TileCoord::new(
        TERRAIN_LEVEL,
        PATCH.0,
        PATCH.1,
    ));
    Footprint::from_eyes(
        [Eye {
            lon: r.west.to_degrees(),
            lat: r.south.to_degrees(),
            height: 5_000.0,
        }],
        12.0,
    )
}

#[test]
fn a_pack_over_indexed_imagery_reads_no_imagery_archive_and_is_the_same_pack() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let objects: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let cfg = StoreConfig {
            flush_bytes: usize::MAX,
            max_buffered_bytes: usize::MAX,
            ..StoreConfig::default()
        };

        // From the origin, through the store: the tiles are stored, the
        // absences remembered, every archive told to the register as it is
        // published.
        let origin = Arc::new(Origin(AtomicU64::new(0)));
        let register = Arc::new(tuile_tile_server::MemoryRegister::new());
        let warm = Arc::new(
            TileStore::new(objects.clone(), layers(), cfg.clone())
                .with_ledger(register.clone())
                .with_absences(register.clone()),
        );
        let from_origin = pack_of(
            CachedImagery::new(
                tuile_planetary::Held(origin.clone() as Arc<dyn ImageryProvider>),
                Arc::new(StoreContent::new(warm.clone())),
                LAYER,
            ),
            false,
        )
        .await;
        warm.flush_all().await.expect("flush");
        let asked = origin.0.load(Ordering::SeqCst);
        assert!(asked > 10, "the drapes asked the origin {asked} tiles");

        // Projected by the register, named by stamp.
        let counting = Arc::new(Counting {
            inner: objects.clone(),
            archive_gets: AtomicU64::new(0),
        });
        let by_index = Arc::new(
            TileStore::new(counting.clone(), layers(), cfg.clone())
                .with_index_reader(register.clone())
                .with_absences(register.clone()),
        );
        let dir = tempfile::tempdir().expect("dir");
        let report = by_index
            .project(&footprint().with_layer_by_index(LAYER), dir.path())
            .await
            .expect("project");
        assert!(
            report.zones_indexed >= 3,
            "cells, top and absences: {report:?}"
        );
        assert_eq!(report.zones_projected, 0, "{report:?}");
        let named = pack_of(
            CachedImagery::new(
                tuile_planetary::Held(origin.clone() as Arc<dyn ImageryProvider>),
                Arc::new(StoreContent::new(by_index.clone())),
                LAYER,
            ),
            true,
        )
        .await;
        assert_eq!(
            counting.archive_gets.load(Ordering::SeqCst),
            0,
            "not one imagery archive read"
        );
        assert_eq!(
            origin.0.load(Ordering::SeqCst),
            asked,
            "nor the origin asked"
        );

        // Projected from the archives and read by bytes: the bake as it was.
        let whole =
            Arc::new(TileStore::new(objects.clone(), layers(), cfg).with_absences(register));
        let dir = tempfile::tempdir().expect("dir");
        let report = whole
            .project(&footprint(), dir.path())
            .await
            .expect("project");
        assert!(report.zones_projected >= 3, "{report:?}");
        let from_archives = pack_of(
            CachedImagery::new(
                tuile_planetary::Held(origin.clone() as Arc<dyn ImageryProvider>),
                Arc::new(StoreContent::new(whole.clone())),
                LAYER,
            ),
            false,
        )
        .await;
        assert_eq!(origin.0.load(Ordering::SeqCst), asked);

        let pack = tuile_pack::Pack::open(&named).expect("open");
        let imagery = pack.imagery_tiles();
        assert!(
            imagery.len() > 10,
            "the pack names imagery: {}",
            imagery.len()
        );
        assert!(
            imagery.keys().all(|&(l, x, y)| !absent(ImageryCoord {
                level: u32::from(l),
                x: u64::from(x),
                y: u64::from(y)
            })),
            "an absent tile is never named: its parent stands in"
        );
        assert!(
            imagery.keys().any(|&(l, _, _)| l == 12) && imagery.keys().any(|&(l, _, _)| l == 13),
            "parents stood in, and fine tiles were named: {:?}",
            imagery
                .keys()
                .map(|k| k.0)
                .collect::<std::collections::BTreeSet<_>>()
        );
        assert_eq!(named, from_archives, "the same pack as from the archives");
        assert_eq!(named, from_origin, "the same pack as from the origin");
    });
}

/// The digest an index records is the one a pack's reference carries.
#[test]
fn the_index_digest_is_the_provenance_digest() {
    for bytes in [
        &b""[..],
        b"a",
        b"foobar",
        &png_of(ImageryCoord {
            level: 3,
            x: 1,
            y: 2,
        }),
    ] {
        assert_eq!(
            tuile_tile_server::index::digest(bytes),
            tuile_planetary::digest(bytes)
        );
    }
}
