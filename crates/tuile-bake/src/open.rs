// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Open sources: a globe baked with no account and no token.
//!
//! What a view from 70 or 300 kilometres needs is the whole planet imaged,
//! not a relief no camera that high can read. These are its two halves:
//!
//! - **[`ELLIPSOID`]**: terrain that is the WGS84 ellipsoid itself, every
//!   tile a regular grid at height zero, made here. Its curvature is the
//!   only relief there is to see from orbit.
//! - **[`BLUE_MARBLE`]**: NASA's *Blue Marble: Next Generation* (land
//!   surface, shallow water and shaded-relief bathymetry, cloud-free; a
//!   monthly composite, here the one GIBS serves by default), from NASA's
//!   Global Imagery Browse Services in Web Mercator, levels 0 to 8 (about
//!   600 m a pixel at the equator). **Public domain**: NASA imagery carries
//!   no copyright; GIBS asks to be credited — "Imagery: NASA Earth
//!   Observatory, Blue Marble: Next Generation, served by NASA EOSDIS GIBS".
//!
//! They are asked for by asset number like any other source, in a range no
//! ion asset uses, so a pack, a tile cache and a tile store name them as
//! they name every source.

use std::sync::Arc;

use bytes::Bytes;
use tuile_core::fetch::{Fetched, TileFetcher};
use tuile_core::geo::{geodetic_to_ecef, Geodetic, WGS84_A};
use tuile_core::raster::{ImageryCoord, ImageryProvider, RasterError, TilingScheme};
use tuile_terrain::decode::{encode, Header};
use tuile_terrain::{
    AvailabilityRange, GeographicTilingScheme, LayerJson, TerrainSource, TerrainSourceError,
    TileCoord,
};

use crate::globe::Sources;
use image::ImageEncoder as _;

/// Where a bake's ground comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ground {
    /// Cesium ion: its terrain and imagery assets (World Terrain and Bing
    /// Aerial unless the bake names others). Needs a token.
    #[default]
    Ion,
    /// The open Earth: the ellipsoid draped with NASA's Blue Marble. No
    /// token.
    OpenEarth,
    /// No ground at all: every frame baked through the same stages, its
    /// selection empty — a sky over the atmosphere's own smooth planet,
    /// what a sky is checked by. Only under a physical atmosphere.
    Empty,
}

impl Ground {
    /// From a command line's word: `ion`, `open-earth` or `empty`.
    pub fn parse(word: &str) -> Result<Self, String> {
        match word {
            "ion" => Ok(Self::Ion),
            "open-earth" => Ok(Self::OpenEarth),
            "empty" => Ok(Self::Empty),
            _ => Err(format!("ground is ion, open-earth or empty, not {word}")),
        }
    }

    /// Whether its sources need an ion token.
    pub fn needs_token(self) -> bool {
        self == Self::Ion
    }
}

// The open sources keep asset numbers of their own, in a range no ion asset
// uses: a tile cache and a tile store name every source's tiles by them.
// They are this module's; a bake chooses a [`Ground`].
/// The terrain that is the ellipsoid.
pub(crate) const ELLIPSOID: i64 = 1_000_000_001;
/// The terrain of no tile at all.
pub(crate) const NOTHING: i64 = 1_000_000_002;
/// NASA Blue Marble: Next Generation, its August 2004 composite, through
/// GIBS.
pub(crate) const BLUE_MARBLE: i64 = 1_000_000_101;

/// The deepest level of the ellipsoid's tiles: past it a tile would be
/// flatter than any camera that wants it can tell.
const ELLIPSOID_LEVELS: u32 = 14;
/// Spacings a side of an ellipsoid tile: enough that its curve is smooth
/// from orbit at level 0.
const ELLIPSOID_STEPS: usize = 32;

/// The ellipsoid as terrain.
pub struct Ellipsoid;

impl Ellipsoid {
    /// Its `layer.json`: quantized mesh, TMS, geographic, every tile there.
    pub fn layer() -> LayerJson {
        let available = (0..=ELLIPSOID_LEVELS)
            .map(|level| {
                vec![AvailabilityRange {
                    start_x: 0,
                    start_y: 0,
                    end_x: (2u64 << level) - 1,
                    end_y: (1u64 << level) - 1,
                }]
            })
            .collect();
        LayerJson {
            format: "quantized-mesh-1.0".into(),
            scheme: Some("tms".into()),
            projection: Some("EPSG:4326".into()),
            tiles: vec!["{z}/{x}/{y}".into()],
            version: Some("1".into()),
            minzoom: 0,
            maxzoom: ELLIPSOID_LEVELS,
            metadata_availability: None,
            extensions: Vec::new(),
            available,
            attribution: None,
        }
    }

    /// Tile `coord`'s quantized mesh: a grid on the ellipsoid at height 0.
    pub fn tile(coord: TileCoord) -> Vec<u8> {
        let rect = GeographicTilingScheme::default().tile_rect(coord);
        let n = ELLIPSOID_STEPS + 1;
        let (mut u, mut v) = (Vec::with_capacity(n * n), Vec::with_capacity(n * n));
        for j in 0..n {
            for i in 0..n {
                u.push(i as f64 / ELLIPSOID_STEPS as f64);
                v.push(j as f64 / ELLIPSOID_STEPS as f64);
            }
        }
        let mut indices = Vec::with_capacity(ELLIPSOID_STEPS * ELLIPSOID_STEPS * 6);
        for j in 0..ELLIPSOID_STEPS {
            for i in 0..ELLIPSOID_STEPS {
                let a = (j * n + i) as u32;
                let (b, c, d) = (a + 1, a + n as u32, a + n as u32 + 1);
                indices.extend_from_slice(&[a, b, c, c, b, d]);
            }
        }
        let (lon, lat) = rect.center();
        let centre = geodetic_to_ecef(Geodetic {
            lon,
            lat,
            height: 0.0,
        });
        // A sphere that holds the tile: its corners and edges' middles.
        let mut radius = 0.0f64;
        for (fu, fv) in [
            (0.0, 0.0),
            (1.0, 0.0),
            (0.0, 1.0),
            (1.0, 1.0),
            (0.5, 0.0),
            (0.5, 1.0),
            (0.0, 0.5),
            (1.0, 0.5),
        ] {
            let p = geodetic_to_ecef(Geodetic {
                lon: rect.west + fu * rect.width(),
                lat: rect.south + fv * rect.height(),
                height: 0.0,
            });
            radius = radius.max(p.distance(centre));
        }
        let line = |f: &dyn Fn(usize) -> usize| (0..n).map(|k| f(k) as u32).collect::<Vec<u32>>();
        let header = Header {
            center: centre.to_array(),
            min_height: 0.0,
            max_height: 0.0,
            bounding_sphere_center: centre.to_array(),
            bounding_sphere_radius: radius.max(1.0),
            horizon_occlusion: (centre / WGS84_A).to_array(),
        };
        let edges = [
            line(&|k| k * n),
            line(&|k| k),
            line(&|k| k * n + ELLIPSOID_STEPS),
            line(&|k| (n - 1) * n + k),
        ];
        // Vertices renumbered in the order the triangles first name them:
        // the high-water-mark coding of the indices requires it.
        let mut order = vec![u32::MAX; n * n];
        let mut next = 0u32;
        for i in &mut indices {
            let slot = &mut order[*i as usize];
            if *slot == u32::MAX {
                *slot = next;
                next += 1;
            }
            *i = *slot;
        }
        let mut uu = vec![0.0; n * n];
        let mut vv = vec![0.0; n * n];
        for (old, &new) in order.iter().enumerate() {
            uu[new as usize] = u[old];
            vv[new as usize] = v[old];
        }
        let edges = edges.map(|e| e.into_iter().map(|i| order[i as usize]).collect());
        let heights = vec![0.0; n * n];
        encode(&header, &uu, &vv, &heights, &indices, None, &edges)
    }
}

/// Terrain of no ground: the two root tiles, each a mesh of no triangle,
/// and nothing under them.
pub struct Nothing;

impl Nothing {
    pub fn layer() -> LayerJson {
        LayerJson {
            available: vec![vec![AvailabilityRange {
                start_x: 0,
                start_y: 0,
                end_x: 1,
                end_y: 0,
            }]],
            maxzoom: 0,
            ..Ellipsoid::layer()
        }
    }

    /// A root tile's mesh: its header, and no vertex.
    pub fn tile(coord: TileCoord) -> Vec<u8> {
        let rect = GeographicTilingScheme::default().tile_rect(coord);
        let (lon, lat) = rect.center();
        let centre = geodetic_to_ecef(Geodetic {
            lon,
            lat,
            height: 0.0,
        });
        let header = Header {
            center: centre.to_array(),
            min_height: 0.0,
            max_height: 0.0,
            bounding_sphere_center: centre.to_array(),
            bounding_sphere_radius: WGS84_A,
            horizon_occlusion: (centre / WGS84_A).to_array(),
        };
        encode(
            &header,
            &[],
            &[],
            &[],
            &[],
            None,
            &[Vec::new(), Vec::new(), Vec::new(), Vec::new()],
        )
    }
}

#[async_trait::async_trait]
impl TerrainSource for Nothing {
    async fn fetch_tile(&self, coord: TileCoord) -> Result<Fetched<Vec<u8>>, TerrainSourceError> {
        Ok(Fetched::undated(Self::tile(coord)))
    }
}

#[async_trait::async_trait]
impl TerrainSource for Ellipsoid {
    async fn fetch_tile(&self, coord: TileCoord) -> Result<Fetched<Vec<u8>>, TerrainSourceError> {
        Ok(Fetched::undated(Self::tile(coord)))
    }
}

/// A layer of NASA's Global Imagery Browse Services, in Web Mercator.
pub struct Gibs<F> {
    fetcher: Arc<F>,
    layer: &'static str,
    /// The composite's date, pinned: another date is other imagery, under
    /// another asset number ([`BLUE_MARBLE`]).
    date: &'static str,
    extension: &'static str,
    maximum_level: u32,
}

impl<F: TileFetcher> Gibs<F> {
    /// Blue Marble: Next Generation.
    pub fn blue_marble(fetcher: Arc<F>) -> Self {
        Self {
            fetcher,
            layer: "BlueMarble_NextGeneration",
            date: "2004-08-01",
            extension: "jpeg",
            maximum_level: 8,
        }
    }

    /// The tile's address: GIBS's REST form, its date pinned, row from the
    /// north.
    pub fn url(&self, coord: ImageryCoord) -> String {
        format!(
            "https://gibs.earthdata.nasa.gov/wmts/epsg3857/best/{}/default/{}/GoogleMapsCompatible_Level{}/{}/{}/{}.{}",
            self.layer, self.date, self.maximum_level, coord.level, coord.y, coord.x, self.extension
        )
    }
}

#[async_trait::async_trait]
impl<F: TileFetcher + 'static> ImageryProvider for Gibs<F> {
    fn tiling_scheme(&self) -> TilingScheme {
        TilingScheme {
            maximum_level: self.maximum_level,
            ..TilingScheme::web_mercator()
        }
    }

    async fn fetch_tile_bytes(&self, coord: ImageryCoord) -> Result<Fetched<Bytes>, RasterError> {
        let url = self
            .url(coord)
            .parse()
            .map_err(|e: url::ParseError| RasterError::Image(e.to_string()))?;
        Ok(self.fetcher.fetch_cacheable(&url).await?)
    }
}

/// The Earth at night: NASA's *Black Marble* (VIIRS Day/Night Band, 2016
/// composite), from GIBS on its geographic grid at level 3 — ten by five
/// tiles of 512, 5 120 by 2 560 texels, about 8 km a texel. **Public
/// domain** (NASA Earth Observatory); credit "Imagery: NASA Earth
/// Observatory, Black Marble 2016, served by NASA EOSDIS GIBS".
pub struct NightLights {
    pub width: u32,
    pub height: u32,
    /// One luminance a texel, row 0 at the north pole, column 0 at the
    /// antimeridian.
    pub luma: Vec<u8>,
}

impl NightLights {
    /// As a grey PNG: what the run store keeps.
    pub fn to_png(&self) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(
                &self.luma,
                self.width,
                self.height,
                image::ExtendedColorType::L8,
            )
            .map_err(|e| e.to_string())?;
        Ok(out)
    }

    /// From the PNG [`Self::to_png`] wrote.
    pub fn from_png(png: &[u8]) -> Result<Self, String> {
        let grey = image::load_from_memory(png)
            .map_err(|e| e.to_string())?
            .to_luma8();
        Ok(Self {
            width: grey.width(),
            height: grey.height(),
            luma: grey.into_raw(),
        })
    }
}

/// Where the run store keeps a night-lights mosaic: by its content, so that
/// a render names exactly the lights it was made with, and a film's name
/// changes with them.
pub fn night_lights_key(png: &[u8]) -> String {
    // The content names it; the source (VIIRS_Black_Marble, 2016-01-01,
    // geographic level 3) is pinned in `night_lights`.
    format!(
        "assets/night-lights/{}.png",
        tuile_pack::scene_digest(&[png])
    )
}

/// The night-lights mosaic in the run store `store`: fetched from GIBS
/// once, under `cache_dir`, and kept by its content. Returns its key.
pub async fn publish_night_lights(
    store: &dyn crate::NightLightsStore,
    cache_dir: Option<std::path::PathBuf>,
) -> Result<String, String> {
    let png = night_lights(cache_dir).await?.to_png()?;
    let key = night_lights_key(&png);
    store.keep(&key, &png).await?;
    Ok(key)
}

/// The tiles across and down of the night-lights mosaic, and their side.
const NIGHT_TILES: (u32, u32) = (10, 5);
const NIGHT_TILE: u32 = 512;

/// The night lights, fetched — through the cache under `cache_dir` — and
/// put together.
pub async fn night_lights(cache_dir: Option<std::path::PathBuf>) -> Result<NightLights, String> {
    let transport = tuile_native_fetchers::TransportConfig {
        retry: tuile_native_fetchers::RetryConfig::patient(),
        ..match &cache_dir {
            Some(dir) => tuile_native_fetchers::TransportConfig::at(dir),
            None => tuile_native_fetchers::TransportConfig::at(
                tuile_native_fetchers::default_cache_dir(),
            ),
        }
    };
    let http = tuile_native_fetchers::NativeHttp::with_transport(transport)
        .await
        .map_err(|e| e.to_string())?;
    let (across, down) = NIGHT_TILES;
    let (width, height) = (across * NIGHT_TILE, down * NIGHT_TILE);
    let mut luma = vec![0u8; (width * height) as usize];
    for row in 0..down {
        for col in 0..across {
            let url = format!(
                "https://gibs.earthdata.nasa.gov/wmts/epsg4326/best/VIIRS_Black_Marble/default/2016-01-01/500m/3/{row}/{col}.png"
            );
            let url: url::Url = url.parse().map_err(|e: url::ParseError| e.to_string())?;
            let bytes = http
                .fetch_cacheable(&url)
                .await
                .map_err(|e| format!("{url}: {e}"))?
                .value;
            let tile = image::load_from_memory(&bytes)
                .map_err(|e| format!("{url}: {e}"))?
                .to_luma8();
            for (y, line) in tile.rows().enumerate().take(NIGHT_TILE as usize) {
                let at = ((row * NIGHT_TILE + y as u32) * width + col * NIGHT_TILE) as usize;
                for (x, p) in line.enumerate().take(NIGHT_TILE as usize) {
                    luma[at + x] = p.0[0];
                }
            }
        }
    }
    Ok(NightLights {
        width,
        height,
        luma,
    })
}

/// The tile store's layers of the sources `ground` reads, declared with
/// their licence and credit: their tiles are kept as any source's. For ion,
/// `ion` names the terrain and imagery assets the bake reads; only the
/// default two are known well enough to declare, the others are left to a
/// store that already describes them. A layer a store already has is never
/// redeclared ([`crate::tiles::Tiles::from_env_with_layers`]).
pub fn store_layers(ground: Ground, ion: (i64, Option<i64>)) -> Vec<tuile_tile_server::LayerDef> {
    use tuile_tile_server::{Grid, LayerDef};
    let terrain = |asset: i64| LayerDef {
        name: crate::source_namespace(asset),
        grid: Grid::Geographic,
        tile_type: "other".into(),
        tile_compression: "none".into(),
        zone_level: 9,
        expiry_days: None,
        content_type: "application/vnd.quantized-mesh".into(),
        index: false,
        licence: Some("CC0-1.0 (made by tuile: the WGS84 ellipsoid)".into()),
        attribution: None,
    };
    match ground {
        Ground::Ion => {
            let proprietary = || Some("Proprietary: the source's terms of service".into());
            let mut layers = Vec::new();
            if ion.0 == crate::CESIUM_WORLD_TERRAIN {
                layers.push(LayerDef {
                    licence: proprietary(),
                    content_type: "application/vnd.quantized-mesh".into(),
                    ..terrain(ion.0)
                });
            }
            if ion.1 == Some(crate::BING_AERIAL) {
                layers.push(LayerDef {
                    name: crate::source_namespace(crate::BING_AERIAL),
                    grid: Grid::WebMercator,
                    tile_type: "jpeg".into(),
                    tile_compression: "none".into(),
                    zone_level: 10,
                    expiry_days: None,
                    content_type: "image/jpeg".into(),
                    index: false,
                    licence: proprietary(),
                    // Credited per view, as the source's session lists it.
                    attribution: None,
                });
            }
            layers
        }
        Ground::Empty => vec![terrain(NOTHING)],
        Ground::OpenEarth => vec![
            terrain(ELLIPSOID),
            LayerDef {
                name: crate::source_namespace(BLUE_MARBLE),
                grid: Grid::WebMercator,
                tile_type: "jpeg".into(),
                tile_compression: "none".into(),
                zone_level: 10,
                expiry_days: None,
                content_type: "image/jpeg".into(),
                index: false,
                licence: Some("Public domain (NASA)".into()),
                attribution: Some(
                    "Imagery: NASA Earth Observatory, Blue Marble: Next Generation, served by NASA EOSDIS GIBS"
                        .into(),
                ),
            },
        ],
    }
}

/// The open sources, answering by asset number.
pub struct OpenSources {
    cache_dir: Option<std::path::PathBuf>,
}

impl OpenSources {
    pub fn new(cache_dir: Option<std::path::PathBuf>) -> Self {
        Self { cache_dir }
    }
}

#[async_trait::async_trait]
impl Sources for OpenSources {
    async fn terrain(&self, asset: i64) -> Result<(LayerJson, Arc<dyn TerrainSource>), String> {
        match asset {
            ELLIPSOID => Ok((Ellipsoid::layer(), Arc::new(Ellipsoid))),
            NOTHING => Ok((Nothing::layer(), Arc::new(Nothing))),
            other => Err(format!("terrain asset {other} is not an open source")),
        }
    }

    async fn imagery(&self, asset: i64) -> Result<Arc<dyn ImageryProvider>, String> {
        match asset {
            BLUE_MARBLE => {
                let transport = tuile_native_fetchers::TransportConfig {
                    retry: tuile_native_fetchers::RetryConfig::patient(),
                    ..match &self.cache_dir {
                        Some(dir) => tuile_native_fetchers::TransportConfig::at(dir),
                        None => tuile_native_fetchers::TransportConfig::at(
                            tuile_native_fetchers::default_cache_dir(),
                        ),
                    }
                };
                let http = tuile_native_fetchers::NativeHttp::with_transport(transport)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(Arc::new(Gibs::blue_marble(Arc::new(http))))
            }
            other => Err(format!("imagery asset {other} is not an open source")),
        }
    }
}

/// Where a point of an ellipsoid tile is, for tests.
#[cfg(test)]
fn on_ellipsoid(p: glam::DVec3) -> f64 {
    tuile_core::geo::ecef_to_geodetic(p).height
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ions_default_assets_are_declared_and_no_other() {
        let names = |ion| -> Vec<String> {
            store_layers(Ground::Ion, ion)
                .into_iter()
                .map(|l| {
                    assert!(l.licence.is_some(), "{} has no licence", l.name);
                    l.name
                })
                .collect()
        };
        assert_eq!(
            names((crate::CESIUM_WORLD_TERRAIN, Some(crate::BING_AERIAL))),
            ["asset-1", "asset-2"]
        );
        assert!(names((7, Some(9))).is_empty());
        assert_eq!(names((crate::CESIUM_WORLD_TERRAIN, None)), ["asset-1"]);
    }

    #[test]
    fn an_ellipsoid_tile_lies_on_the_ellipsoid_and_closes_on_its_neighbours() {
        let layer = Ellipsoid::layer();
        for coord in [
            TileCoord::new(0, 0, 0),
            TileCoord::new(6, 64, 50),
            TileCoord::new(14, 16_390, 12_000),
        ] {
            assert!(layer.is_available(coord));
            let mesh = tuile_terrain::decode::decode(&Ellipsoid::tile(coord)).expect("decodes");
            assert_eq!(mesh.u.len(), (ELLIPSOID_STEPS + 1).pow(2));
            assert!(mesh.height.iter().all(|h| *h == 0.0));
            assert!(mesh.header.min_height == 0.0 && mesh.header.max_height == 0.0);
            let centre = glam::DVec3::from_array(mesh.header.center);
            assert!(on_ellipsoid(centre).abs() < 1e-3);
            assert!(mesh.edges.iter().all(|e| e.len() == ELLIPSOID_STEPS + 1));
        }
        assert!(!layer.is_available(TileCoord::new(ELLIPSOID_LEVELS + 1, 0, 0)));
        assert_eq!(Ground::parse("open-earth"), Ok(Ground::OpenEarth));
        assert!(Ground::Ion.needs_token() && !Ground::Empty.needs_token());
    }

    #[test]
    fn a_blue_marble_tile_is_asked_for_by_row_from_the_north() {
        struct Never;
        #[async_trait::async_trait]
        impl TileFetcher for Never {
            async fn fetch(&self, _: &url::Url) -> Result<Bytes, tuile_core::fetch::FetchError> {
                unreachable!()
            }
        }
        let gibs = Gibs::blue_marble(Arc::new(Never));
        assert_eq!(
            gibs.url(ImageryCoord { level: 3, x: 4, y: 2 }),
            "https://gibs.earthdata.nasa.gov/wmts/epsg3857/best/BlueMarble_NextGeneration/default/2004-08-01/GoogleMapsCompatible_Level8/3/2/4.jpeg"
        );
        assert_eq!(gibs.tiling_scheme().maximum_level, 8);
    }
}
