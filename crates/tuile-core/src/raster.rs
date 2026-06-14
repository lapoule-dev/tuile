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

use crate::content::{DecodedTexture, DecodedTileContent};
use crate::fetch::{FetchError, TileFetcher};
use crate::geo::{ecef_to_geodetic, Geodetic, WGS84_A};
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
    /// Shallowest level the source actually serves. 0 for most schemes; some
    /// providers (Bing: empty quadkey at level 0 is invalid) start at 1, so
    /// draping must never pick a tile shallower than this.
    pub minimum_level: u32,
    pub maximum_level: u32,
}

impl TilingScheme {
    pub fn web_mercator() -> Self {
        Self {
            projection: Projection::WebMercator,
            root_tiles_x: 1,
            root_tiles_y: 1,
            tile_size: 256,
            minimum_level: 0,
            maximum_level: 19,
        }
    }

    pub fn geographic() -> Self {
        Self {
            projection: Projection::Geographic,
            root_tiles_x: 2,
            root_tiles_y: 1,
            tile_size: 256,
            minimum_level: 0,
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

    /// The deepest single imagery tile that fully contains `rect`. Used for
    /// simple single-texture draping: one imagery tile covers a whole
    /// geometry tile (coarser than per-tile multi-texturing, but no atlas).
    pub fn containing_tile(&self, rect: &GeoRect) -> ImageryCoord {
        let mut best = ImageryCoord {
            level: 0,
            x: 0,
            y: 0,
        };
        // Level 0 may itself have several root tiles; start from the level
        // where the rect first fits in one tile.
        for level in 0..=self.maximum_level {
            let tiles = self.tiles_in_rectangle(rect, level);
            if tiles.len() == 1 {
                best = tiles[0];
            } else {
                break;
            }
        }
        if best.level < self.minimum_level {
            // The natural containing level is shallower than this source
            // serves (e.g. a coarse geographic geometry tile straddles two
            // Web-Mercator tiles in latitude, so it only "fits" at level 0,
            // which Bing has no tile for). Drop to the minimum level and take
            // the tile covering the rect's centre; the geometry tile may
            // overflow it slightly and `uvs_for_positions` clamps the overflow.
            best = self.tile_at_center(rect, self.minimum_level);
        }
        best
    }

    /// The tile at `level` whose extent contains the rectangle's centre.
    fn tile_at_center(&self, rect: &GeoRect, level: u32) -> ImageryCoord {
        let (nx, ny) = self.tiles_at(level);
        let (u, v) = self.projection.to_normalized(Geodetic {
            lon: 0.5 * (rect.west + rect.east),
            lat: 0.5 * (rect.south + rect.north),
            height: 0.0,
        });
        ImageryCoord {
            level,
            x: ((u * nx as f64) as u64).min(nx - 1),
            y: ((v * ny as f64) as u64).min(ny - 1),
        }
    }

    /// Matched-LOD mosaic of every imagery tile covering `rect`: the level is
    /// chosen so the rectangle is sampled by ~`target_texels` texels across
    /// (clamped to the served range and coarsened until at most `max_tiles`
    /// tiles), and ALL overlapping tiles are returned. A geometry tile
    /// straddling an imagery-tile boundary (the prime meridian is one at every
    /// Web-Mercator level) is thus fully covered — no coarse single-tile
    /// fallback, no seam. This is how Cesium's `ImageryLayer` drapes terrain.
    pub fn mosaic_for_rectangle(
        &self,
        rect: &GeoRect,
        target_texels: f64,
        max_tiles: u32,
    ) -> ImageryMosaic {
        let level = self.level_for_rectangle(rect, target_texels);
        self.mosaic_at_level(rect, level, max_tiles)
    }

    /// The imagery level whose texel spacing best matches `texel_spacing`
    /// meters at `lat` (radians) — Cesium's `getLevelWithMaximumTexelSpacing`.
    /// Pass a terrain tile's geometric error to tie imagery detail to terrain
    /// detail (the imagery is as sharp as the geometry it drapes, no sharper).
    pub fn level_for_texel_spacing(&self, texel_spacing: f64, lat: f64) -> u32 {
        use std::f64::consts::TAU;
        let lat_factor = match self.projection {
            Projection::WebMercator => lat.cos(),
            Projection::Geographic => 1.0,
        };
        let level_zero_spacing =
            WGS84_A * TAU * lat_factor / (self.tile_size as f64 * self.root_tiles_x as f64);
        let ratio = level_zero_spacing / texel_spacing.max(1.0e-3);
        let level = ratio.log2().round().max(0.0) as u32;
        level.clamp(self.minimum_level, self.maximum_level)
    }

    /// The mosaic covering `rect` at exactly `level` (clamped to the served
    /// range), coarsened a step at a time until it fits in `max_tiles`.
    pub fn mosaic_at_level(&self, rect: &GeoRect, level: u32, max_tiles: u32) -> ImageryMosaic {
        let mut level = level.clamp(self.minimum_level, self.maximum_level);
        loop {
            let tiles = self.tiles_in_rectangle(rect, level);
            let x0 = tiles.iter().map(|t| t.x).min().unwrap_or(0);
            let x1 = tiles.iter().map(|t| t.x).max().unwrap_or(0);
            let y0 = tiles.iter().map(|t| t.y).min().unwrap_or(0);
            let y1 = tiles.iter().map(|t| t.y).max().unwrap_or(0);
            let cols = (x1 - x0 + 1) as u32;
            let rows = (y1 - y0 + 1) as u32;
            if cols * rows <= max_tiles || level <= self.minimum_level {
                return ImageryMosaic {
                    level,
                    x0,
                    y0,
                    cols,
                    rows,
                };
            }
            level -= 1; // too many tiles for one drape — coarsen a step
        }
    }

    /// Normalized `(x0, y0, x1, y1)` extent a mosaic covers.
    pub fn mosaic_extent(&self, m: &ImageryMosaic) -> (f64, f64, f64, f64) {
        let (nx, ny) = self.tiles_at(m.level);
        (
            m.x0 as f64 / nx as f64,
            m.y0 as f64 / ny as f64,
            (m.x0 + u64::from(m.cols)) as f64 / nx as f64,
            (m.y0 + u64::from(m.rows)) as f64 / ny as f64,
        )
    }

    /// UVs of ECEF-rebased positions within an arbitrary normalized extent
    /// (a mosaic's union), v growing southward; clamped to `[0,1]`.
    pub fn uvs_in_extent(
        &self,
        positions: &[[f32; 3]],
        origin: DVec3,
        ext: (f64, f64, f64, f64),
    ) -> Vec<[f32; 2]> {
        let (x0, y0, x1, y1) = ext;
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

/// A rectangular block of imagery tiles at one level, covering a geometry
/// tile's extent — the matched-LOD mosaic from [`TilingScheme::mosaic_for_rectangle`].
#[derive(Debug, Clone)]
pub struct ImageryMosaic {
    pub level: u32,
    pub x0: u64,
    pub y0: u64,
    pub cols: u32,
    pub rows: u32,
}

impl ImageryMosaic {
    /// The tiles, row-major NW→SE — the order [`stitch_mosaic`] expects.
    pub fn tiles(&self) -> Vec<ImageryCoord> {
        let mut out = Vec::with_capacity((self.cols * self.rows) as usize);
        for j in 0..self.rows {
            for i in 0..self.cols {
                out.push(ImageryCoord {
                    level: self.level,
                    x: self.x0 + u64::from(i),
                    y: self.y0 + u64::from(j),
                });
            }
        }
        out
    }

    pub fn tile_count(&self) -> u32 {
        self.cols * self.rows
    }
}

/// Stitches mosaic tiles (row-major NW→SE, each ≤ `tile_size`²) into one RGBA8
/// texture laid out as a `cols × rows` grid. Tiles smaller than `tile_size`
/// are copied into the top-left of their cell.
pub fn stitch_mosaic(
    tiles: &[DecodedTexture],
    cols: u32,
    rows: u32,
    tile_size: u32,
) -> DecodedTexture {
    let ts = tile_size as usize;
    let w = cols as usize * ts;
    let h = rows as usize * ts;
    let mut rgba8 = vec![0u8; w * h * 4];
    for (idx, tex) in tiles.iter().enumerate() {
        let ci = (idx % cols as usize) * ts;
        let cj = (idx / cols as usize) * ts;
        let tw = (tex.width as usize).min(ts);
        let th = (tex.height as usize).min(ts);
        for row in 0..th {
            let src = row * tex.width as usize * 4;
            let dst = ((cj + row) * w + ci) * 4;
            rgba8[dst..dst + tw * 4].copy_from_slice(&tex.rgba8[src..src + tw * 4]);
        }
    }
    DecodedTexture {
        width: w as u32,
        height: h as u32,
        rgba8,
    }
}

/// Resamples an imagery texture from its source projection (covering
/// `src_extent` in normalized projection coords) into a **geographic**-spaced
/// texture over `rect` (radians) — Cesium's `reprojectToGeographic`, on the
/// CPU. Draping the result with linear lon/lat UVs ([`uvs_geographic`]) is then
/// correct on geographic terrain: no Web-Mercator twist, and latitudes past
/// the projection's limit clamp to its edge row (a smooth polar cap, no
/// pinwheel of stretched meridian wedges).
pub fn reproject_to_geographic(
    src: &DecodedTexture,
    src_extent: (f64, f64, f64, f64),
    rect: &GeoRect,
    projection: Projection,
) -> DecodedTexture {
    let (w, h) = (src.width.max(1), src.height.max(1));
    let (x0, y0, x1, y1) = src_extent;
    let sw = (x1 - x0).max(1e-15);
    let sh = (y1 - y0).max(1e-15);
    let denom_x = (w - 1).max(1) as f64;
    let denom_y = (h - 1).max(1) as f64;
    let mut rgba8 = vec![0u8; (w * h * 4) as usize];
    for row in 0..h {
        let lat = rect.north - (row as f64 / denom_y) * (rect.north - rect.south);
        for col in 0..w {
            let lon = rect.west + (col as f64 / denom_x) * (rect.east - rect.west);
            let (mx, my) = projection.to_normalized(Geodetic {
                lon,
                lat,
                height: 0.0,
            });
            let su = ((mx - x0) / sw).clamp(0.0, 1.0);
            let sv = ((my - y0) / sh).clamp(0.0, 1.0);
            let px = bilinear(src, su, sv);
            let di = ((row * w + col) * 4) as usize;
            rgba8[di..di + 4].copy_from_slice(&px);
        }
    }
    DecodedTexture {
        width: w,
        height: h,
        rgba8,
    }
}

fn bilinear(t: &DecodedTexture, u: f64, v: f64) -> [u8; 4] {
    let fx = u * (t.width.saturating_sub(1)) as f64;
    let fy = v * (t.height.saturating_sub(1)) as f64;
    let x0 = fx.floor() as u32;
    let y0 = fy.floor() as u32;
    let x1 = (x0 + 1).min(t.width - 1);
    let y1 = (y0 + 1).min(t.height - 1);
    let tx = (fx - x0 as f64) as f32;
    let ty = (fy - y0 as f64) as f32;
    let texel = |x: u32, y: u32| {
        let i = ((y * t.width + x) * 4) as usize;
        [t.rgba8[i], t.rgba8[i + 1], t.rgba8[i + 2], t.rgba8[i + 3]]
    };
    let lerp = |a: u8, b: u8, f: f32| (a as f32 + (b as f32 - a as f32) * f) as u8;
    let mix = |a: [u8; 4], b: [u8; 4], f: f32| {
        [
            lerp(a[0], b[0], f),
            lerp(a[1], b[1], f),
            lerp(a[2], b[2], f),
            lerp(a[3], b[3], f),
        ]
    };
    let top = mix(texel(x0, y0), texel(x1, y0), tx);
    let bottom = mix(texel(x0, y1), texel(x1, y1), tx);
    mix(top, bottom, ty)
}

/// Per-vertex UVs mapping ECEF positions **linearly** into a geographic
/// rectangle (radians) — for draping a [`reproject_to_geographic`] texture.
/// `v` grows southward (north = 0).
pub fn uvs_geographic(positions: &[[f32; 3]], origin: DVec3, rect: &GeoRect) -> Vec<[f32; 2]> {
    let dw = (rect.east - rect.west).max(1e-12);
    let dh = (rect.north - rect.south).max(1e-12);
    positions
        .iter()
        .map(|p| {
            let ecef = origin + DVec3::new(f64::from(p[0]), f64::from(p[1]), f64::from(p[2]));
            let g = ecef_to_geodetic(ecef);
            [
                (((g.lon - rect.west) / dw).clamp(0.0, 1.0)) as f32,
                (((rect.north - g.lat) / dh).clamp(0.0, 1.0)) as f32,
            ]
        })
        .collect()
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

/// Computes the single-tile overlay attachment for a decoded geometry tile:
/// the deepest imagery tile covering its geographic extent, plus the per-mesh
/// uv sets. `rect` is the geometry tile's geographic extent.
pub fn single_tile_attachment(
    content: &DecodedTileContent,
    rect: &GeoRect,
    scheme: &TilingScheme,
) -> OverlayAttachment {
    let imagery = scheme.containing_tile(rect);
    let uvs = content
        .meshes
        .iter()
        .map(|m| scheme.uvs_for_positions(&m.positions, content.local_origin_ecef, imagery))
        .collect();
    OverlayAttachment { imagery, uvs }
}

/// Drapes a single imagery texture onto a decoded geometry tile, in place:
/// adds the texture, writes per-mesh uvs from `attachment`, and points each
/// mesh's `base_color_texture` at it. The existing PBR pipeline then renders
/// the geometry textured with the imagery (base color × imagery). For terrain
/// (neutral base color) this is simply the imagery.
pub fn drape_single(
    content: &mut DecodedTileContent,
    attachment: &OverlayAttachment,
    texture: DecodedTexture,
) {
    let index = content.textures.len();
    content.textures.push(texture);
    for (mesh, uvs) in content.meshes.iter_mut().zip(&attachment.uvs) {
        mesh.uvs = Some(uvs.clone());
        mesh.material.base_color_texture = Some(index);
    }
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

/// Upsamples one quadrant of a parent imagery texture back to a full tile —
/// the quality fallback for imagery a provider lacks at a given zoom. `qx`/`qy`
/// select the quadrant (0/1); `qy = 0` is the north (top) half. Uses `image`'s
/// Catmull-Rom (bicubic) filter, so the magnified result stays smooth rather
/// than blocky.
pub fn upsample_quadrant(src: &DecodedTexture, qx: u32, qy: u32) -> DecodedTexture {
    use image::{imageops, RgbaImage};
    let Some(img) = RgbaImage::from_raw(src.width, src.height, src.rgba8.clone()) else {
        return src.clone();
    };
    let (hw, hh) = (src.width / 2, src.height / 2);
    let quadrant = imageops::crop_imm(&img, qx * hw, qy * hh, hw, hh).to_image();
    let full = imageops::resize(&quadrant, src.width, src.height, imageops::FilterType::CatmullRom);
    DecodedTexture {
        width: src.width,
        height: src.height,
        rgba8: full.into_raw(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{DecodedMesh, MaterialDesc};
    use crate::geo::geodetic_to_ecef;
    use glam::Mat4;

    #[test]
    fn containing_tile_is_deepest_single_cover() {
        let wm = TilingScheme::web_mercator();
        // A tiny rect near the equator/prime meridian: the deepest tile that
        // still contains it should be reasonably deep, and unique.
        let rect = GeoRect {
            west: 0.001,
            south: 0.001,
            east: 0.0011,
            north: 0.0011,
        };
        let tile = wm.containing_tile(&rect);
        // The rect must lie inside the chosen tile's extent.
        let (x0, y0, x1, y1) = wm.tile_extent(tile);
        let nw = wm.projection.to_normalized(Geodetic {
            lon: rect.west,
            lat: rect.north,
            height: 0.0,
        });
        let se = wm.projection.to_normalized(Geodetic {
            lon: rect.east,
            lat: rect.south,
            height: 0.0,
        });
        assert!(x0 <= nw.0 && se.0 <= x1 && y0 <= nw.1 && se.1 <= y1);
        // One level deeper would split the rect across tiles.
        assert!(wm.tiles_in_rectangle(&rect, tile.level + 1).len() > 1);
    }

    #[test]
    fn drape_single_attaches_texture_and_uvs() {
        // A flat geometry tile near (lon 0.01, lat 0.01).
        let center = geodetic_to_ecef(Geodetic {
            lon: 0.01,
            lat: 0.01,
            height: 0.0,
        });
        let p = |lon: f64, lat: f64| {
            let e = geodetic_to_ecef(Geodetic {
                lon,
                lat,
                height: 0.0,
            }) - center;
            [e.x as f32, e.y as f32, e.z as f32]
        };
        let mut content = DecodedTileContent {
            meshes: vec![DecodedMesh {
                positions: vec![p(0.009, 0.011), p(0.011, 0.011), p(0.009, 0.009)],
                normals: None,
                uvs: None,
                indices: vec![0, 1, 2],
                material: MaterialDesc::default(),
            }],
            textures: Vec::new(),
            local_origin_ecef: center,
            transform_local: Mat4::IDENTITY,
        };
        let rect = GeoRect {
            west: 0.009,
            south: 0.009,
            east: 0.011,
            north: 0.011,
        };
        let wm = TilingScheme::web_mercator();
        let attachment = single_tile_attachment(&content, &rect, &wm);
        assert_eq!(attachment.uvs.len(), 1);
        assert_eq!(attachment.uvs[0].len(), 3);
        // All uvs inside [0,1] (the geometry fits in the chosen imagery tile).
        for uv in &attachment.uvs[0] {
            assert!((0.0..=1.0).contains(&uv[0]) && (0.0..=1.0).contains(&uv[1]));
        }

        let tex = DecodedTexture {
            width: 1,
            height: 1,
            rgba8: vec![1, 2, 3, 255],
        };
        drape_single(&mut content, &attachment, tex);
        assert_eq!(content.textures.len(), 1);
        assert_eq!(content.meshes[0].material.base_color_texture, Some(0));
        assert!(content.meshes[0].uvs.is_some());
    }

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
