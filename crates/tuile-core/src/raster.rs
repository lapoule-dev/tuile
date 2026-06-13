// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Raster imagery overlays: providers, tiling schemes, and the
//! geometry↔imagery mapping.
//!
//! Imagery is a capability INDEPENDENT of geometry — its own providers, its
//! own 2D quadtree, its own fetches — that crosses geometry at exactly one
//! seam: the [`OverlayAttachment`] computed per decoded geometry tile (which
//! imagery tiles drape it, with which texture coordinates). Imagery tiles
//! are shared N:M between geometry tiles, which is why they are referenced
//! by [`ImageryCoord`] rather than embedded.
//!
//! Everything here is pure except [`ImageryProvider::fetch_tile`], which
//! goes through the same [`TileFetcher`] abstraction as geometry content —
//! so local files, plain HTTP and `tuile-cesium-ion` all work unchanged.

use crate::content::DecodedTexture;
use crate::fetch::{FetchError, TileFetcher};
use crate::geo::{ecef_to_geodetic, Geodetic};
use crate::math::Obb;
use async_trait::async_trait;
use glam::DVec3;
use std::sync::Arc;
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum RasterError {
    #[error("fetch: {0}")]
    Fetch(#[from] FetchError),
    #[error("image decode: {0}")]
    Image(String),
    #[error("invalid imagery url: {0}")]
    InvalidUrl(#[from] url::ParseError),
}

/// Address of an imagery tile in its provider's quadtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageryCoord {
    pub level: u32,
    pub x: u64,
    pub y: u64,
}

/// A geographic rectangle, radians, WGS84. No antimeridian crossing in v1
/// (split rectangles upstream).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeoRect {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
}

impl GeoRect {
    pub fn width(&self) -> f64 {
        (self.east - self.west).max(0.0)
    }
    pub fn height(&self) -> f64 {
        (self.north - self.south).max(0.0)
    }
}

/// Map projection of an imagery layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Projection {
    /// EPSG:3857, the slippy-map standard. One root tile.
    WebMercator,
    /// EPSG:4326 (geographic / equirectangular). Two root tiles side by side.
    Geographic,
}

impl Projection {
    /// Projects to normalized [0,1]² map space; (0,0) is the north-west
    /// corner (image convention: v grows southward).
    pub fn to_normalized(&self, g: Geodetic) -> (f64, f64) {
        use std::f64::consts::{FRAC_PI_4, PI, TAU};
        let x = (g.lon + PI) / TAU;
        let y = match self {
            Projection::WebMercator => {
                // Clamp to the mercator square (±85.051°).
                let lat = g.lat.clamp(-1.484_422, 1.484_422);
                let merc = (FRAC_PI_4 + lat / 2.0).tan().ln();
                0.5 - merc / TAU
            }
            Projection::Geographic => 0.5 - g.lat / PI,
        };
        (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0))
    }
}

/// A quadtree tiling scheme over a projection.
#[derive(Debug, Clone, Copy)]
pub struct TilingScheme {
    pub projection: Projection,
    /// Tiles across at level 0 (1 for WebMercator, 2 for Geographic).
    pub root_tiles_x: u64,
    pub root_tiles_y: u64,
    /// Texture size of one tile, pixels.
    pub tile_size: u32,
    pub maximum_level: u32,
}

impl TilingScheme {
    pub fn web_mercator() -> Self {
        Self {
            projection: Projection::WebMercator,
            root_tiles_x: 1,
            root_tiles_y: 1,
            tile_size: 256,
            maximum_level: 19,
        }
    }

    pub fn geographic() -> Self {
        Self {
            projection: Projection::Geographic,
            root_tiles_x: 2,
            root_tiles_y: 1,
            tile_size: 256,
            maximum_level: 19,
        }
    }

    pub fn tiles_at(&self, level: u32) -> (u64, u64) {
        (self.root_tiles_x << level, self.root_tiles_y << level)
    }

    /// The [0,1]² extent of a tile (x0, y0, x1, y1).
    pub fn tile_extent(&self, c: ImageryCoord) -> (f64, f64, f64, f64) {
        let (nx, ny) = self.tiles_at(c.level);
        let w = 1.0 / nx as f64;
        let h = 1.0 / ny as f64;
        (
            c.x as f64 * w,
            c.y as f64 * h,
            (c.x + 1) as f64 * w,
            (c.y + 1) as f64 * h,
        )
    }

    /// Picks the level where one imagery texel roughly covers
    /// `rect.width / target_texels` of the map — i.e. the rect is sampled by
    /// about `target_texels` texels across. Clamped to `maximum_level`.
    pub fn level_for_rectangle(&self, rect: &GeoRect, target_texels: f64) -> u32 {
        use std::f64::consts::TAU;
        let frac = (rect.width() / TAU).max(1e-12);
        let texels_needed = target_texels.max(1.0);
        // texels across the rect at level L:
        //   frac * root_x * 2^L * tile_size
        let ideal = (texels_needed / (frac * self.root_tiles_x as f64 * self.tile_size as f64))
            .log2()
            .ceil()
            .max(0.0) as u32;
        ideal.min(self.maximum_level)
    }

    /// All tiles of `level` intersecting `rect`.
    pub fn tiles_in_rectangle(&self, rect: &GeoRect, level: u32) -> Vec<ImageryCoord> {
        let nw = self.projection.to_normalized(Geodetic {
            lon: rect.west,
            lat: rect.north,
            height: 0.0,
        });
        let se = self.projection.to_normalized(Geodetic {
            lon: rect.east,
            lat: rect.south,
            height: 0.0,
        });
        let (nx, ny) = self.tiles_at(level);
        let clamp_tile = |v: f64, n: u64| -> u64 { ((v * n as f64) as u64).min(n - 1) };
        let x0 = clamp_tile(nw.0, nx);
        let x1 = clamp_tile(se.0.max(nw.0), nx);
        let y0 = clamp_tile(nw.1, ny);
        let y1 = clamp_tile(se.1.max(nw.1), ny);
        let mut out = Vec::with_capacity(((x1 - x0 + 1) * (y1 - y0 + 1)) as usize);
        for y in y0..=y1 {
            for x in x0..=x1 {
                out.push(ImageryCoord { level, x, y });
            }
        }
        out
    }

    /// Texture coordinates of ECEF-rebased positions within one imagery
    /// tile. (0,0) = tile north-west corner, v grows southward (image
    /// convention); values outside [0,1] mean the vertex falls outside the
    /// tile (clamped).
    pub fn uvs_for_positions(
        &self,
        positions: &[[f32; 3]],
        origin: DVec3,
        tile: ImageryCoord,
    ) -> Vec<[f32; 2]> {
        let (x0, y0, x1, y1) = self.tile_extent(tile);
        let w = (x1 - x0).max(1e-15);
        let h = (y1 - y0).max(1e-15);
        positions
            .iter()
            .map(|p| {
                let ecef = origin + DVec3::new(f64::from(p[0]), f64::from(p[1]), f64::from(p[2]));
                let (x, y) = self.projection.to_normalized(ecef_to_geodetic(ecef));
                [
                    (((x - x0) / w).clamp(0.0, 1.0)) as f32,
                    (((y - y0) / h).clamp(0.0, 1.0)) as f32,
                ]
            })
            .collect()
    }
}

/// Geographic extent of a world-space OBB (corner sampling — fine for tile
/// volumes, which are small relative to the globe; antimeridian-crossing
/// volumes are out of v1 scope).
pub fn rectangle_from_obb(obb: &Obb) -> GeoRect {
    let mut rect = GeoRect {
        west: f64::INFINITY,
        south: f64::INFINITY,
        east: f64::NEG_INFINITY,
        north: f64::NEG_INFINITY,
    };
    for corner in obb.corners() {
        let g = ecef_to_geodetic(corner);
        rect.west = rect.west.min(g.lon);
        rect.east = rect.east.max(g.lon);
        rect.south = rect.south.min(g.lat);
        rect.north = rect.north.max(g.lat);
    }
    rect
}

/// The geometry↔imagery crossing point: which imagery tile drapes a decoded
/// geometry tile, with which texture coordinates. Imagery PIXELS are not
/// here — they are shared N:M and travel as their own protocol content,
/// keyed by [`ImageryCoord`].
#[derive(Debug, Clone)]
pub struct OverlayAttachment {
    pub imagery: ImageryCoord,
    /// One uv set per mesh, parallel to `DecodedTileContent::meshes`.
    pub uvs: Vec<Vec<[f32; 2]>>,
}

/// An imagery source: a tiling scheme plus tile fetching+decoding.
#[async_trait]
pub trait ImageryProvider: Send + Sync {
    fn tiling_scheme(&self) -> TilingScheme;
    async fn fetch_tile(&self, coord: ImageryCoord) -> Result<DecodedTexture, RasterError>;
}

/// URL-template imagery provider: expands `{z}/{x}/{y}` against any
/// [`TileFetcher`] and decodes JPEG/PNG. `tuile-cesium-ion`'s
/// `ImageryEndpoint::tile_url` produces exactly such URLs.
pub struct TemplateProvider<F> {
    template: String,
    fetcher: Arc<F>,
    scheme: TilingScheme,
}

impl<F: TileFetcher> TemplateProvider<F> {
    pub fn new(template: impl Into<String>, fetcher: Arc<F>, scheme: TilingScheme) -> Self {
        Self {
            template: template.into(),
            fetcher,
            scheme,
        }
    }

    fn tile_url(&self, c: ImageryCoord) -> Result<Url, RasterError> {
        Ok(Url::parse(
            &self
                .template
                .replace("{z}", &c.level.to_string())
                .replace("{x}", &c.x.to_string())
                .replace("{y}", &c.y.to_string()),
        )?)
    }
}

#[async_trait]
impl<F: TileFetcher> ImageryProvider for TemplateProvider<F> {
    fn tiling_scheme(&self) -> TilingScheme {
        self.scheme
    }

    async fn fetch_tile(&self, coord: ImageryCoord) -> Result<DecodedTexture, RasterError> {
        let url = self.tile_url(coord)?;
        let bytes = self.fetcher.fetch(&url).await?;
        decode_image(&bytes)
    }
}

/// Decodes JPEG/PNG bytes to tightly packed RGBA8. Blocking, no I/O — same
/// contract as `content::decode`.
pub fn decode_image(bytes: &[u8]) -> Result<DecodedTexture, RasterError> {
    let img = image::load_from_memory(bytes).map_err(|e| RasterError::Image(e.to_string()))?;
    let rgba = img.to_rgba8();
    Ok(DecodedTexture {
        width: rgba.width(),
        height: rgba.height(),
        rgba8: rgba.into_raw(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::geodetic_to_ecef;

    #[test]
    fn normalized_projection_reference_points() {
        let wm = Projection::WebMercator;
        let (x, y) = wm.to_normalized(Geodetic {
            lon: 0.0,
            lat: 0.0,
            height: 0.0,
        });
        assert!((x - 0.5).abs() < 1e-12 && (y - 0.5).abs() < 1e-12);
        // North pole clamps to the top edge of the mercator square.
        let (_, y) = wm.to_normalized(Geodetic {
            lon: 0.0,
            lat: 1.5,
            height: 0.0,
        });
        assert!(y.abs() < 1e-6, "y = {y}");

        let geo = Projection::Geographic;
        let (x, y) = geo.to_normalized(Geodetic {
            lon: -std::f64::consts::PI,
            lat: std::f64::consts::FRAC_PI_2,
            height: 0.0,
        });
        assert!((x - 0.0).abs() < 1e-12 && (y - 0.0).abs() < 1e-12);
    }

    #[test]
    fn tile_counts_and_extents() {
        let wm = TilingScheme::web_mercator();
        assert_eq!(wm.tiles_at(0), (1, 1));
        assert_eq!(wm.tiles_at(3), (8, 8));
        let geo = TilingScheme::geographic();
        assert_eq!(geo.tiles_at(0), (2, 1));

        let (x0, y0, x1, y1) = wm.tile_extent(ImageryCoord {
            level: 1,
            x: 1,
            y: 0,
        });
        assert_eq!((x0, y0, x1, y1), (0.5, 0.0, 1.0, 0.5));
    }

    #[test]
    fn level_selection_is_monotonic_and_clamped() {
        let wm = TilingScheme::web_mercator();
        let wide = GeoRect {
            west: -1.0,
            south: 0.0,
            east: 1.0,
            north: 0.5,
        };
        let narrow = GeoRect {
            west: 0.0,
            south: 0.0,
            east: 1e-4,
            north: 1e-4,
        };
        let lw = wm.level_for_rectangle(&wide, 512.0);
        let ln = wm.level_for_rectangle(&narrow, 512.0);
        assert!(ln > lw, "narrower extent → deeper level ({ln} vs {lw})");
        assert!(ln <= wm.maximum_level);
        let tiny = GeoRect {
            west: 0.0,
            south: 0.0,
            east: 1e-12,
            north: 1e-12,
        };
        assert_eq!(wm.level_for_rectangle(&tiny, 512.0), wm.maximum_level);
    }

    #[test]
    fn tiles_in_rectangle_cover_boundaries() {
        let wm = TilingScheme::web_mercator();
        // A rect straddling the equator and prime meridian at level 1:
        // all four tiles.
        let rect = GeoRect {
            west: -0.1,
            south: -0.1,
            east: 0.1,
            north: 0.1,
        };
        let tiles = wm.tiles_in_rectangle(&rect, 1);
        assert_eq!(tiles.len(), 4);
        // Fully inside one tile.
        let rect = GeoRect {
            west: 0.2,
            south: -0.4,
            east: 0.3,
            north: -0.3,
        };
        let tiles = wm.tiles_in_rectangle(&rect, 1);
        assert_eq!(
            tiles,
            vec![ImageryCoord {
                level: 1,
                x: 1,
                y: 1
            }]
        );
    }

    #[test]
    fn uvs_map_known_points_into_the_tile() {
        let wm = TilingScheme::web_mercator();
        // Level-1 tile (1,1): lon [0, π], lat [-85°, 0].
        let tile = ImageryCoord {
            level: 1,
            x: 1,
            y: 1,
        };
        let origin = DVec3::ZERO;
        let p = |lon: f64, lat: f64| {
            let e = geodetic_to_ecef(Geodetic {
                lon,
                lat,
                height: 0.0,
            });
            [e.x as f32, e.y as f32, e.z as f32]
        };
        // North-west corner of the tile (lon 0, lat 0) → uv (0, 0);
        // mid-longitude on the equator → u 0.5, v 0.
        let uvs = wm.uvs_for_positions(
            &[p(0.0, 0.0), p(std::f64::consts::FRAC_PI_2, 0.0)],
            origin,
            tile,
        );
        assert!(uvs[0][0] < 1e-3 && uvs[0][1] < 1e-3, "{:?}", uvs[0]);
        assert!(
            (uvs[1][0] - 0.5).abs() < 1e-3 && uvs[1][1] < 1e-3,
            "{:?}",
            uvs[1]
        );
    }

    #[test]
    fn rectangle_from_region_obb_matches_region() {
        let region = [-0.0102, 0.7820, -0.0073, 0.7833, 0.0, 500.0];
        let obb = crate::geo::region_to_obb(&region);
        let rect = rectangle_from_obb(&obb);
        // The OBB is conservative: the rect must contain the region with a
        // modest margin.
        assert!(rect.west <= region[0] && rect.east >= region[2]);
        assert!(rect.south <= region[1] && rect.north >= region[3]);
        assert!(rect.width() < (region[2] - region[0]) * 1.5);
    }

    #[test]
    fn template_provider_fetches_and_decodes() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Write a 2x2 PNG (red) at the expected template position.
        std::fs::create_dir_all(dir.path().join("3/5")).expect("mkdir");
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
        img.save(dir.path().join("3/5/7.png")).expect("save");

        let template = format!(
            "{}/{{z}}/{{x}}/{{y}}.png",
            Url::from_directory_path(dir.path())
                .expect("dir url")
                .as_str()
                .trim_end_matches('/')
        );
        let provider = TemplateProvider::new(
            template,
            Arc::new(crate::fetch::FsFetcher),
            TilingScheme::web_mercator(),
        );
        let tex = futures_executor::block_on(provider.fetch_tile(ImageryCoord {
            level: 3,
            x: 5,
            y: 7,
        }))
        .expect("fetch");
        assert_eq!((tex.width, tex.height), (2, 2));
        assert_eq!(&tex.rgba8[0..4], &[255, 0, 0, 255]);

        // A miss is a typed error.
        let err = futures_executor::block_on(provider.fetch_tile(ImageryCoord {
            level: 1,
            x: 0,
            y: 0,
        }));
        assert!(matches!(
            err,
            Err(RasterError::Fetch(FetchError::NotFound(_)))
        ));
    }
}
