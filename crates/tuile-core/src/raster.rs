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
use crate::fetch::{FetchError, Fetched, TileFetcher};
use crate::geo::{ecef_to_geodetic, Geodetic, WGS84_A};
use crate::math::Obb;
use crate::storage::ContentStore;
use async_trait::async_trait;
use bytes::Bytes;
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

/// The latitude Web Mercator stops at, `2·atan(e^π) − π/2` — the one that
/// makes the projected world square.
///
/// Written to full double precision rather than rounded, because it is used in
/// both directions: `to_normalized` clamps to it and `from_normalized` produces
/// it, and a truncated constant makes the pair disagree by a few metres at the
/// top of the map. That difference is invisible in a picture and very visible
/// in a round-trip assertion, which is how it was found.
pub const MERCATOR_MAX_LAT: f64 = 1.484_422_229_745_332_4;

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
                let lat = g.lat.clamp(-MERCATOR_MAX_LAT, MERCATOR_MAX_LAT);
                let merc = (FRAC_PI_4 + lat / 2.0).tan().ln();
                0.5 - merc / TAU
            }
            Projection::Geographic => 0.5 - g.lat / PI,
        };
        (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0))
    }

    /// Inverse of [`Projection::to_normalized`], at height 0.
    ///
    /// Not an exact inverse at the poles, and cannot be: `to_normalized`
    /// *clamps* latitudes past the Mercator limit onto the edge of the square,
    /// which throws the excess away. Round-tripping such a latitude returns the
    /// limit — which is what the clamp meant, and what every consumer here wants.
    pub fn from_normalized(&self, x: f64, y: f64) -> Geodetic {
        use std::f64::consts::{FRAC_PI_2, PI, TAU};
        let lon = x * TAU - PI;
        let lat = match self {
            Projection::WebMercator => 2.0 * ((0.5 - y) * TAU).exp().atan() - FRAC_PI_2,
            Projection::Geographic => (0.5 - y) * PI,
        };
        Geodetic {
            lon,
            lat,
            height: 0.0,
        }
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

    /// The geographic rectangle an imagery tile covers.
    ///
    /// [`TilingScheme::tile_extent`] read back through the projection. The
    /// latitude mapping is nonlinear under Web Mercator but strictly monotone,
    /// so the corners of the normalized extent are the corners of the rectangle
    /// and no sampling in between is needed.
    ///
    /// This is what makes an imagery tile placeable *on its own*, without
    /// reference to whatever geometry happens to be draped in it — which is the
    /// whole point of sharing one reprojected texture between the many geometry
    /// tiles it covers.
    pub fn tile_rect(&self, c: ImageryCoord) -> GeoRect {
        let (x0, y0, x1, y1) = self.tile_extent(c);
        let nw = self.projection.from_normalized(x0, y0);
        let se = self.projection.from_normalized(x1, y1);
        GeoRect {
            west: nw.lon,
            north: nw.lat,
            east: se.lon,
            south: se.lat,
        }
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

/// How many imagery textures may drape one geometry tile.
///
/// A **binding** limit, not a memory one, and the difference is the point of
/// the layered model. It used to be a memory limit: a drape stitched its
/// imagery into one private texture, so allowing sixteen tiles meant a 1024²
/// image per terrain tile — 657 of those over one orbit against a 1.5 GiB
/// budget is what made the second half of a turn spend itself reloading what
/// the first half had paid for. A graded per-level allowance existed only to
/// hold that down.
///
/// Layers are referenced now, so one imagery tile costs the same whether one
/// geometry tile names it or twenty, and the allowance can be flat. What is
/// left to bound is how many textures one draw binds, against the sixteen per
/// shader stage that wgpu's default limits guarantee.
///
/// Twelve, because a tile draped one level finer than its own geometric error
/// straddles up to a 3×3 block, and a budget of eight would coarsen exactly
/// those tiles back a level. That is not a small loss of sharpness: it is
/// *neighbours disagreeing about their level*, which is one of the two reasons
/// adjacent tiles visibly differed in colour. Whatever the budget is, it has to
/// clear 3×3 cleanly, and the room above that absorbs a rectangle that straddles
/// worse than most.
///
/// A tile needing more than this coarsens, which is the remaining reason the
/// ground cannot be made arbitrarily sharper than the mesh under it. The way
/// past that is to refine the geometry quadtree past its data by upsampling the
/// parent mesh — Cesium's answer — not a larger number here.
///
/// The renderer's shader must bind exactly this many slots.
pub const MAX_IMAGERY_LAYERS: u32 = 12;

/// The budget is what one draw binds, so it has to stay inside the per-stage
/// texture limit wgpu's default limits guarantee, with the tile's own base
/// colour alongside it. A build error rather than a test, because a value over
/// the limit does not fail *somewhere* — it fails on the narrowest device that
/// ever runs this, which is nowhere near here.
const _: () = assert!(
    MAX_IMAGERY_LAYERS < 16,
    "an imagery slot per texture unit leaves none for the base colour"
);

/// And it has to admit the ordinary case without coarsening: a geometry tile
/// draped one level finer than its own geometric error straddles up to a 3×3
/// block. Coarsening exactly those tiles is neighbours disagreeing about their
/// level, which is what makes them differ in colour — the symptom this whole
/// model exists to remove.
const _: () = assert!(
    MAX_IMAGERY_LAYERS >= 9,
    "a tile one level finer than its match needs up to 3x3 layers"
);

/// One imagery texture draped over a geometry tile — **referenced, not owned**.
///
/// This is the type that stops us copying. A geometry tile names the imagery
/// tiles that cover it and says where each one lands in its own uv space; the
/// pixels stay in exactly one place, shared by every geometry tile that names
/// the same [`ImageryCoord`]. Two neighbours therefore cannot disagree about
/// colour or filtering the way they do when each resamples its own copy.
///
/// It follows the shape Cesium's `sampleAndBlend` consumes, because that shape
/// is the minimum a fragment shader needs and no less: a coverage rectangle to
/// mask with, and an affine map into the texture.
#[derive(Debug, Clone)]
pub struct ImageryLayer {
    /// Identity. The upload key on the GPU and the cache key on the CPU — two
    /// geometry tiles naming the same coord must resolve to the same texture.
    pub coord: ImageryCoord,
    /// The pixels, reprojected to geographic spacing over the imagery tile's
    /// own rectangle. Shared; never cloned per geometry tile.
    pub texture: Arc<DecodedTexture>,
    /// The part of the geometry tile's uv space this layer covers, as
    /// `[u_min, v_min, u_max, v_max]`, v growing southward.
    ///
    /// A layer finer than the geometry tile covers a sub-rectangle of it and the
    /// others cover the rest; a layer coarser than it covers all of it. Outside
    /// this rectangle the layer contributes nothing — masking on it is what
    /// lets several layers be blended in one pass without bleeding into each
    /// other.
    pub coverage: [f32; 4],
    /// With [`ImageryLayer::scale`]: `texture_uv = tile_uv * scale + translation`.
    pub translation: [f32; 2],
    pub scale: [f32; 2],
}

impl ImageryLayer {
    /// Places an imagery tile within a geometry tile's uv space.
    ///
    /// Both rectangles are geographic, and both uv spaces are linear in lon/lat
    /// over their own rectangle — which is precisely what reprojecting the
    /// imagery to geographic spacing buys ([`reproject_tile_to_geographic`]).
    /// The map between them is therefore affine, and this is it.
    ///
    /// Nothing here assumes the imagery is finer than the geometry: a coarse
    /// tile containing the geometry places by the same formula, with a scale
    /// below 1 and full coverage.
    pub fn placed(
        coord: ImageryCoord,
        texture: Arc<DecodedTexture>,
        tile: &GeoRect,
        imagery: &GeoRect,
    ) -> Self {
        Self::substituted(coord, texture, tile, imagery, imagery)
    }

    /// Places a texture that spans `source` over the ground `covers` asks for.
    ///
    /// The two differ when a provider has no tile at the level wanted and an
    /// ancestor stands in: the pixels span the ancestor's whole rectangle, but
    /// this layer is only responsible for the descendant's share of it — the
    /// siblings covering the rest are placed from the same texture, each masked
    /// to its own quarter. Sampling from the ancestor while masking to the
    /// descendant is how Cesium's `TileImagery` shows a coarse tile under a fine
    /// one, and it is what makes the fallback free: no upsampled copy, no second
    /// generation of filtering, one texture on the GPU however many tiles lean
    /// on it.
    pub fn substituted(
        coord: ImageryCoord,
        texture: Arc<DecodedTexture>,
        tile: &GeoRect,
        source: &GeoRect,
        covers: &GeoRect,
    ) -> Self {
        let (tw, th) = (tile.width().max(1e-15), tile.height().max(1e-15));
        let (sw, sh) = (source.width().max(1e-15), source.height().max(1e-15));
        // texture_u = (lon - source.west) / sw, and lon = tile.west + u * tw.
        // v grows southward on both sides, hence north rather than south.
        let scale = [(tw / sw) as f32, (th / sh) as f32];
        let translation = [
            ((tile.west - source.west) / sw) as f32,
            ((source.north - tile.north) / sh) as f32,
        ];
        let coverage = [
            (((covers.west - tile.west) / tw).clamp(0.0, 1.0)) as f32,
            (((tile.north - covers.north) / th).clamp(0.0, 1.0)) as f32,
            (((covers.east - tile.west) / tw).clamp(0.0, 1.0)) as f32,
            (((tile.north - covers.south) / th).clamp(0.0, 1.0)) as f32,
        ];
        Self {
            coord,
            texture,
            coverage,
            translation,
            scale,
        }
    }

    /// Whether this layer covers any of the geometry tile at all.
    ///
    /// A degenerate coverage rectangle means the two rectangles only touched at
    /// an edge — real, because tile grids are half-open and a geometry tile's
    /// extent is a conservative bound on its vertices. Such a layer costs a
    /// texture binding and contributes no pixels, so it is worth dropping.
    pub fn is_visible(&self) -> bool {
        self.coverage[2] > self.coverage[0] && self.coverage[3] > self.coverage[1]
    }
}

/// The layer table a renderer uploads for one geometry tile: two `vec4` per
/// slot — coverage, then placement — for **every** slot, used or not.
///
/// `[2i]` is `[u_min, v_min, u_max, v_max]` and `[2i + 1]` is
/// `[translation.x, translation.y, scale.x, scale.y]`.
///
/// An unused slot gets an *empty* coverage rectangle rather than a count the
/// shader would have to test. `[1, 1, 0, 0]` fails the mask for every uv in
/// `[0, 1]`, including both corners, so the slot contributes nothing while the
/// shader stays branch-free — which matters because the coverage test sits in
/// the same control flow as a texture sample, and a divergent branch around a
/// sample is exactly what shading languages forbid.
///
/// This lives here rather than in a backend because it is the contract *between*
/// backends: the packing, the sentinel, and the masking rule are the same
/// whether the shader is WGSL, GLSL or a scene-graph material. Only the binding
/// mechanics differ.
pub fn imagery_layer_table(layers: &[ImageryLayer]) -> [[f32; 4]; 2 * MAX_IMAGERY_LAYERS as usize] {
    const EMPTY_COVERAGE: [f32; 4] = [1.0, 1.0, 0.0, 0.0];
    let mut table = [EMPTY_COVERAGE; 2 * MAX_IMAGERY_LAYERS as usize];
    for (slot, layer) in layers.iter().take(MAX_IMAGERY_LAYERS as usize).enumerate() {
        table[2 * slot] = layer.coverage;
        table[2 * slot + 1] = [
            layer.translation[0],
            layer.translation[1],
            layer.scale[0],
            layer.scale[1],
        ];
    }
    table
}

/// A pool of per-imagery-tile resources shared between the geometry tiles that
/// drape them — a backend's uploaded textures, typically.
///
/// The sharing is the point of the layered model, and where it is enforced is
/// here rather than in any one renderer: an imagery tile covering twenty
/// geometry tiles is materialised once.
///
/// Eviction is by reference counting rather than by a budget, because the right
/// answer is already known exactly — a resource is needed for precisely as long
/// as some resident tile references it. The pool therefore holds [`Weak`]
/// handles and hands out [`Arc`]s: when the last tile referencing an entry is
/// dropped, the resource frees itself and the dangling key is swept on the next
/// insert. A byte budget here would only be a worse guess at the same question,
/// and one that could free something still being drawn.
pub struct ImageryPool<T> {
    entries: std::collections::HashMap<ImageryCoord, std::sync::Weak<T>>,
}

impl<T> Default for ImageryPool<T> {
    fn default() -> Self {
        Self {
            entries: std::collections::HashMap::new(),
        }
    }
}

impl<T> ImageryPool<T> {
    /// The resource for `coord`, building it only if no live holder already has
    /// one. `make` runs at most once per coord per lifetime of the resource.
    pub fn get_or_insert(&mut self, coord: ImageryCoord, make: impl FnOnce() -> T) -> Arc<T> {
        if let Some(live) = self.entries.get(&coord).and_then(std::sync::Weak::upgrade) {
            return live;
        }
        let entry = Arc::new(make());
        self.entries.insert(coord, Arc::downgrade(&entry));
        // Dead keys accumulate silently otherwise: nothing runs when the last
        // `Arc` drops. Sweeping in proportion to the map's own growth keeps the
        // cost amortised without needing a schedule of its own.
        if self.entries.len() > 2 * self.live_count() {
            self.entries.retain(|_, w| w.strong_count() > 0);
        }
        entry
    }

    fn live_count(&self) -> usize {
        self.entries
            .values()
            .filter(|w| w.strong_count() > 0)
            .count()
    }

    /// Everything still held by some tile. Counting these is the only honest
    /// measure of what imagery costs — a per-tile total counts a shared resource
    /// once per tile that names it, which is the number the sharing exists to
    /// stop being true.
    pub fn live(&self) -> Vec<Arc<T>> {
        self.entries
            .values()
            .filter_map(std::sync::Weak::upgrade)
            .collect()
    }
}

/// Resamples one imagery tile onto geographic spacing over its **own**
/// rectangle, so it can be placed by an affine map alone.
///
/// This is the per-imagery-tile replacement for reprojecting a stitched mosaic
/// per geometry tile, and it is the reason the result can be shared: the output
/// depends on the imagery tile and nothing else.
///
/// Two cases skip the work entirely, both from Cesium's `_reprojectTexture`:
/// a provider already serving geographic tiles has nothing to remap, and a tile
/// whose rectangle spans less than ~1e-5 radians per texel has a Mercator
/// distortion smaller than one texel across its whole height — resampling it
/// would only cost a generation of filtering.
pub fn reproject_tile_to_geographic(
    src: &DecodedTexture,
    scheme: &TilingScheme,
    coord: ImageryCoord,
) -> Option<DecodedTexture> {
    let rect = scheme.tile_rect(coord);
    if scheme.projection == Projection::Geographic {
        return None;
    }
    // Below this many radians of latitude per texel, the Mercator remap moves
    // nothing by as much as a texel across the whole tile — so the resample
    // would only cost a generation of filtering and buy a picture identical to
    // the one it started from.
    const NEGLIGIBLE_DISTORTION_PER_TEXEL: f64 = 1.0e-5;
    if rect.height() / f64::from(src.height.max(1)) <= NEGLIGIBLE_DISTORTION_PER_TEXEL {
        return None;
    }
    Some(reproject_to_geographic(
        src,
        scheme.tile_extent(coord),
        &rect,
        scheme.projection,
    ))
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
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait ImageryProvider: Send + Sync {
    fn tiling_scheme(&self) -> TilingScheme;

    /// The tile's bytes **as served** — JPEG or PNG, still encoded — with the
    /// lifetime its origin stated.
    ///
    /// This is the primary method because it is what a cache should hold. The
    /// decoded form is an order of magnitude larger (a 20 KiB JPEG becomes
    /// 256 KiB of RGBA), so storing it would spend the disk tier ten times
    /// faster to save a decode that costs microseconds.
    async fn fetch_tile_bytes(&self, coord: ImageryCoord) -> Result<Fetched<Bytes>, RasterError>;

    /// The decoded tile. Provided: every provider decodes the same way.
    async fn fetch_tile(
        &self,
        coord: ImageryCoord,
    ) -> Result<Fetched<DecodedTexture>, RasterError> {
        self.fetch_tile_bytes(coord)
            .await?
            .try_map(|bytes| decode_image(&bytes))
    }
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

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl<F: TileFetcher> ImageryProvider for TemplateProvider<F> {
    fn tiling_scheme(&self) -> TilingScheme {
        self.scheme
    }

    async fn fetch_tile_bytes(&self, coord: ImageryCoord) -> Result<Fetched<Bytes>, RasterError> {
        let url = self.tile_url(coord)?;
        Ok(self.fetcher.fetch_cacheable(&url).await?)
    }
}

/// Wraps an [`ImageryProvider`] with a [`ContentStore`], so a tile served once
/// is not fetched again — across runs, if the store is persistent.
///
/// Caches the tile **as served** (JPEG/PNG), never the decoded RGBA: decoded is
/// an order of magnitude larger, and the decode it would save costs
/// microseconds against a disk read. Entries are kept for the lifetime the
/// origin stated ([`Fetched::ttl`]).
///
/// `namespace` separates providers that number their tiles differently — two
/// sources sharing one store must never read each other's z/x/y.
pub struct CachedImagery<P> {
    inner: P,
    store: Arc<dyn ContentStore>,
    namespace: String,
}

impl<P: ImageryProvider> CachedImagery<P> {
    pub fn new(inner: P, store: Arc<dyn ContentStore>, namespace: impl Into<String>) -> Self {
        Self {
            inner,
            store,
            namespace: namespace.into(),
        }
    }

    fn key(&self, c: ImageryCoord) -> String {
        let ImageryCoord { level, x, y } = c;
        format!("img/{}/{level}/{x}/{y}", self.namespace)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl<P: ImageryProvider> ImageryProvider for CachedImagery<P> {
    fn tiling_scheme(&self) -> TilingScheme {
        self.inner.tiling_scheme()
    }

    async fn fetch_tile_bytes(&self, coord: ImageryCoord) -> Result<Fetched<Bytes>, RasterError> {
        let key = self.key(coord);
        if let Some(bytes) = self.store.get(&key).await {
            // The store already applied the lifetime this was written with;
            // re-stating one here would only be a second, weaker guess.
            return Ok(Fetched::undated(bytes));
        }
        let fetched = self.inner.fetch_tile_bytes(coord).await?;
        self.store
            .put(&key, fetched.value.clone(), fetched.ttl)
            .await;
        Ok(fetched)
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
    let full = imageops::resize(
        &quadrant,
        src.width,
        src.height,
        imageops::FilterType::CatmullRom,
    );
    DecodedTexture {
        width: src.width,
        height: src.height,
        rgba8: full.into_raw(),
    }
}

/// The mip chain of a texture: levels 1.., each half the size of the previous
/// one (WebGPU's `max(1, size >> level)` rule), down to 1×1. The mirror of
/// [`upsample_quadrant`] — same `image` filters, the other direction; `Triangle`
/// over an exact halving is a plain box average.
///
/// Backends generate the chain here, on the CPU, rather than blitting it level
/// by level on the GPU: Apple's Metal driver leaks memory in proportion to the
/// number of **render passes** created (gfx-rs/wgpu#8768, and Dawn has it too),
/// and a GPU chain costs one render pass per level per texture.
///
/// Like `upsample_quadrant`, this averages in the encoded domain rather than in
/// linear light, so the chain darkens very slightly against a filtering GPU
/// sampler's own result. Consistent with the rest of the module.
pub fn mip_chain(src: &DecodedTexture) -> Vec<DecodedTexture> {
    use image::{imageops, ImageBuffer, Rgba};
    let mut levels: Vec<DecodedTexture> = Vec::new();
    loop {
        let prev = levels.last().unwrap_or(src);
        if prev.width <= 1 && prev.height <= 1 {
            return levels;
        }
        let Some(view) =
            ImageBuffer::<Rgba<u8>, _>::from_raw(prev.width, prev.height, &prev.rgba8[..])
        else {
            return levels;
        };
        let width = (prev.width / 2).max(1);
        let height = (prev.height / 2).max(1);
        let next = imageops::resize(&view, width, height, imageops::FilterType::Triangle);
        levels.push(DecodedTexture {
            width,
            height,
            rgba8: next.into_raw(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{DecodedMesh, MaterialDesc};
    use crate::geo::geodetic_to_ecef;
    use glam::Mat4;
    use std::collections::HashMap;
    use std::sync::Mutex;

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
            imagery: Vec::new(),
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

    /// The inverse must actually invert, away from the clamped poles — every
    /// rectangle an imagery tile is placed by is read back through it.
    #[test]
    fn normalized_projection_round_trips() {
        for projection in [Projection::WebMercator, Projection::Geographic] {
            for lon in [-3.0, -0.4, 0.0, 1.2, 3.0] {
                for lat in [-1.4, -0.7, 0.0, 0.3, 1.4] {
                    let g = Geodetic {
                        lon,
                        lat,
                        height: 0.0,
                    };
                    let (x, y) = projection.to_normalized(g);
                    let back = projection.from_normalized(x, y);
                    assert!(
                        (back.lon - lon).abs() < 1e-9 && (back.lat - lat).abs() < 1e-9,
                        "{projection:?} at ({lon}, {lat}) came back ({}, {})",
                        back.lon,
                        back.lat
                    );
                }
            }
        }
    }

    /// A tile's rectangle is its normalized extent read back through the
    /// projection — so projecting the rectangle's corners must return the
    /// extent it came from. Under Web Mercator this is the nonlinear direction,
    /// which is exactly where a wrong inverse would hide.
    #[test]
    fn a_tile_rect_projects_back_to_its_extent() {
        for scheme in [TilingScheme::web_mercator(), TilingScheme::geographic()] {
            for c in [coord(0, 0, 0), coord(3, 5, 2), coord(9, 100, 300)] {
                let (nx, ny) = scheme.tiles_at(c.level);
                if c.x >= nx || c.y >= ny {
                    continue;
                }
                let rect = scheme.tile_rect(c);
                let (x0, y0, x1, y1) = scheme.tile_extent(c);
                let nw = scheme.projection.to_normalized(Geodetic {
                    lon: rect.west,
                    lat: rect.north,
                    height: 0.0,
                });
                let se = scheme.projection.to_normalized(Geodetic {
                    lon: rect.east,
                    lat: rect.south,
                    height: 0.0,
                });
                assert!(
                    (nw.0 - x0).abs() < 1e-9 && (nw.1 - y0).abs() < 1e-9,
                    "{c:?} north-west: {nw:?} vs ({x0}, {y0})"
                );
                assert!(
                    (se.0 - x1).abs() < 1e-9 && (se.1 - y1).abs() < 1e-9,
                    "{c:?} south-east: {se:?} vs ({x1}, {y1})"
                );
                assert!(rect.width() > 0.0 && rect.height() > 0.0);
            }
        }
    }

    fn tex1x1() -> Arc<DecodedTexture> {
        Arc::new(DecodedTexture {
            width: 1,
            height: 1,
            rgba8: vec![0, 0, 0, 255],
        })
    }

    /// The invariant the whole layered model rests on: a point on the ground
    /// has one texture coordinate, and going there through the geometry tile's
    /// uv space must land where the imagery tile's own uv space puts it.
    ///
    /// Checked in both directions of nesting, because they are the two real
    /// cases and they exercise opposite signs: imagery finer than the geometry
    /// (several layers tiling it) and imagery coarser (one ancestor standing in
    /// for a tile still loading).
    #[test]
    fn a_placed_layer_agrees_with_the_imagery_tile_s_own_uvs() {
        let tile = GeoRect {
            west: 0.10,
            south: 0.20,
            east: 0.14,
            north: 0.26,
        };
        let finer = GeoRect {
            west: 0.11,
            south: 0.22,
            east: 0.13,
            north: 0.25,
        };
        let coarser = GeoRect {
            west: 0.00,
            south: 0.10,
            east: 0.40,
            north: 0.60,
        };

        for imagery in [finer, coarser] {
            let layer = ImageryLayer::placed(coord(5, 1, 1), tex1x1(), &tile, &imagery);
            for (lon, lat) in [(0.115, 0.23), (0.125, 0.245), (0.12, 0.225)] {
                let tile_uv = [
                    (lon - tile.west) / tile.width(),
                    (tile.north - lat) / tile.height(),
                ];
                let got = [
                    tile_uv[0] * f64::from(layer.scale[0]) + f64::from(layer.translation[0]),
                    tile_uv[1] * f64::from(layer.scale[1]) + f64::from(layer.translation[1]),
                ];
                let want = [
                    (lon - imagery.west) / imagery.width(),
                    (imagery.north - lat) / imagery.height(),
                ];
                assert!(
                    (got[0] - want[0]).abs() < 1e-6 && (got[1] - want[1]).abs() < 1e-6,
                    "({lon}, {lat}) mapped to {got:?}, the imagery tile says {want:?}"
                );
            }
        }
    }

    /// When a provider has no tile at the level wanted, the four descendants
    /// stand on their parent's pixels — but each must still answer only for its
    /// own quarter, or they blend over each other. The affine map is the
    /// parent's, identically for all four; only the mask differs.
    #[test]
    fn ancestor_substitution_shares_one_texture_across_disjoint_quarters() {
        let tile = GeoRect {
            west: 0.0,
            south: 0.0,
            east: 1.0,
            north: 1.0,
        };
        let parent = tile; // the ancestor happens to span the geometry tile
        let quarters = [
            (
                "north-west",
                GeoRect {
                    west: 0.0,
                    south: 0.5,
                    east: 0.5,
                    north: 1.0,
                },
                [0.0, 0.0, 0.5, 0.5],
            ),
            (
                "north-east",
                GeoRect {
                    west: 0.5,
                    south: 0.5,
                    east: 1.0,
                    north: 1.0,
                },
                [0.5, 0.0, 1.0, 0.5],
            ),
            (
                "south-west",
                GeoRect {
                    west: 0.0,
                    south: 0.0,
                    east: 0.5,
                    north: 0.5,
                },
                [0.0, 0.5, 0.5, 1.0],
            ),
            (
                "south-east",
                GeoRect {
                    west: 0.5,
                    south: 0.0,
                    east: 1.0,
                    north: 0.5,
                },
                [0.5, 0.5, 1.0, 1.0],
            ),
        ];
        let texture = tex1x1();
        for (name, covers, expected) in quarters {
            let layer = ImageryLayer::substituted(
                coord(3, 1, 1),
                Arc::clone(&texture),
                &tile,
                &parent,
                &covers,
            );
            assert_eq!(layer.coverage, expected, "{name}");
            // Every sibling reads the parent the same way — only the mask moves.
            assert_eq!(layer.scale, [1.0, 1.0], "{name}");
            assert_eq!(layer.translation, [0.0, 0.0], "{name}");
            assert!(Arc::ptr_eq(&layer.texture, &texture), "{name} copied");
        }
    }

    /// Coverage is what masks a layer outside its own ground, and it must be
    /// stated in the geometry tile's uv space with v southward. A layer nested
    /// inside the tile covers a strict sub-rectangle; one containing the tile
    /// covers all of it.
    #[test]
    fn coverage_masks_a_finer_layer_and_admits_a_coarser_one() {
        let tile = GeoRect {
            west: 0.0,
            south: 0.0,
            east: 1.0,
            north: 1.0,
        };
        // The north-east quarter of the tile.
        let quarter = GeoRect {
            west: 0.5,
            south: 0.5,
            east: 1.0,
            north: 1.0,
        };
        let layer = ImageryLayer::placed(coord(1, 1, 0), tex1x1(), &tile, &quarter);
        // u from the west edge, v from the NORTH edge: the north-east quarter is
        // the upper half in v, not the lower.
        assert_eq!(layer.coverage, [0.5, 0.0, 1.0, 0.5]);
        assert!(layer.is_visible());

        let containing = GeoRect {
            west: -1.0,
            south: -1.0,
            east: 2.0,
            north: 2.0,
        };
        let ancestor = ImageryLayer::placed(coord(0, 0, 0), tex1x1(), &tile, &containing);
        assert_eq!(ancestor.coverage, [0.0, 0.0, 1.0, 1.0]);
        assert!(ancestor.scale[0] < 1.0 && ancestor.scale[1] < 1.0);
        assert!(ancestor.is_visible());
    }

    /// A layer that only touches the tile along an edge draws nothing, and tile
    /// grids being half-open makes that a routine outcome rather than a corner
    /// case. It still costs a texture binding, so it is worth naming.
    #[test]
    fn a_layer_touching_only_an_edge_is_not_visible() {
        let tile = GeoRect {
            west: 0.0,
            south: 0.0,
            east: 1.0,
            north: 1.0,
        };
        let east_neighbour = GeoRect {
            west: 1.0,
            south: 0.0,
            east: 2.0,
            north: 1.0,
        };
        let layer = ImageryLayer::placed(coord(1, 1, 0), tex1x1(), &tile, &east_neighbour);
        assert!(!layer.is_visible(), "coverage {:?}", layer.coverage);
    }

    /// Reprojection is a resample, so the cheapest correct answer is not to do
    /// it. A geographic provider has nothing to remap at all; a deep Mercator
    /// tile distorts by less than one texel across its own height.
    #[test]
    fn reprojection_is_skipped_where_it_would_change_nothing() {
        let src = DecodedTexture {
            width: 256,
            height: 256,
            rgba8: vec![128; 256 * 256 * 4],
        };
        let geo = TilingScheme::geographic();
        assert!(
            reproject_tile_to_geographic(&src, &geo, coord(4, 3, 2)).is_none(),
            "a geographic provider is already in the target spacing"
        );

        let wm = TilingScheme::web_mercator();
        // A shallow tile spans tens of degrees: the latitude remap is gross.
        assert!(reproject_tile_to_geographic(&src, &wm, coord(2, 1, 1)).is_some());
        // A deep one spans a few metres, well under a texel of distortion.
        let deep = coord(19, 100_000, 100_000);
        assert!(
            reproject_tile_to_geographic(&src, &wm, deep).is_none(),
            "{:?} rad tall over 256 texels",
            wm.tile_rect(deep).height()
        );
    }

    /// Reprojecting a tile must not move its own corners — they are the fixed
    /// points of the remap, and they are what the affine placement relies on.
    #[test]
    fn reprojection_keeps_a_tile_within_its_own_rectangle() {
        // A vertical ramp: row 0 black, last row white. After a latitude remap
        // the ends must still be the ends, whatever happened in between.
        let (w, h) = (16u32, 16u32);
        let mut rgba8 = Vec::with_capacity((w * h * 4) as usize);
        for row in 0..h {
            let v = (row * 255 / (h - 1)) as u8;
            for _ in 0..w {
                rgba8.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let src = DecodedTexture {
            width: w,
            height: h,
            rgba8,
        };
        let wm = TilingScheme::web_mercator();
        let out = reproject_tile_to_geographic(&src, &wm, coord(2, 1, 1)).expect("reprojected");
        assert_eq!((out.width, out.height), (w, h));
        let first = out.rgba8[0];
        let last = out.rgba8[((h - 1) * w * 4) as usize];
        assert!(first < 8, "north edge drifted to {first}");
        assert!(last > 247, "south edge drifted to {last}");
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
    fn mip_chain_halves_down_to_one_texel() {
        let src = DecodedTexture {
            width: 8,
            height: 4,
            rgba8: vec![128; 8 * 4 * 4],
        };
        let chain = mip_chain(&src);
        // 8×4 → 4×2 → 2×1 → 1×1: the WebGPU `max(1, size >> level)` rule, so
        // the chain runs past the shorter side rather than stopping at it.
        let sizes: Vec<(u32, u32)> = chain.iter().map(|t| (t.width, t.height)).collect();
        assert_eq!(sizes, vec![(4, 2), (2, 1), (1, 1)]);
        for level in &chain {
            assert_eq!(
                level.rgba8.len(),
                (level.width * level.height * 4) as usize,
                "each level is tightly packed RGBA8"
            );
        }
    }

    #[test]
    fn mip_chain_of_a_flat_image_keeps_its_colour() {
        // A box filter over a constant image is that constant: this catches a
        // downsample that misreads the row stride or drops the alpha channel.
        let src = DecodedTexture {
            width: 4,
            height: 4,
            rgba8: [40u8, 90, 200, 255].repeat(4 * 4),
        };
        for level in mip_chain(&src) {
            for texel in level.rgba8.chunks_exact(4) {
                assert_eq!(texel, &[40, 90, 200, 255]);
            }
        }
    }

    #[test]
    fn mip_chain_of_a_single_texel_is_empty() {
        let src = DecodedTexture {
            width: 1,
            height: 1,
            rgba8: vec![255; 4],
        };
        assert!(mip_chain(&src).is_empty(), "nothing left to halve");
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

    /// A store shared by the tests: records every write so a test can assert
    /// what was cached, not merely that a second read succeeded.
    #[derive(Default)]
    struct MemStore {
        entries: Mutex<HashMap<String, Bytes>>,
        writes: Mutex<Vec<(String, Option<std::time::Duration>)>>,
    }

    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    impl ContentStore for MemStore {
        async fn get(&self, key: &str) -> Option<Bytes> {
            self.entries.lock().expect("lock").get(key).cloned()
        }
        async fn put(&self, key: &str, value: Bytes, ttl: Option<std::time::Duration>) {
            self.entries
                .lock()
                .expect("lock")
                .insert(key.to_owned(), value);
            self.writes
                .lock()
                .expect("lock")
                .push((key.to_owned(), ttl));
        }
    }

    /// Counts how many times the origin was actually asked.
    struct CountingImagery {
        calls: Mutex<u32>,
        ttl: Option<std::time::Duration>,
    }

    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    impl ImageryProvider for CountingImagery {
        fn tiling_scheme(&self) -> TilingScheme {
            TilingScheme::web_mercator()
        }
        async fn fetch_tile_bytes(&self, _c: ImageryCoord) -> Result<Fetched<Bytes>, RasterError> {
            *self.calls.lock().expect("lock") += 1;
            Ok(Fetched {
                value: Bytes::from_static(b"encoded"),
                ttl: self.ttl,
            })
        }
    }

    fn coord(level: u32, x: u64, y: u64) -> ImageryCoord {
        ImageryCoord { level, x, y }
    }

    #[test]
    fn a_cached_provider_asks_the_origin_once() {
        let store = Arc::new(MemStore::default());
        let ttl = Some(std::time::Duration::from_secs(600));
        let inner = CountingImagery {
            calls: Mutex::new(0),
            ttl,
        };
        let provider = CachedImagery::new(inner, store.clone(), "test");

        let c = coord(3, 4, 5);
        let first = futures_executor::block_on(provider.fetch_tile_bytes(c)).expect("first");
        let second = futures_executor::block_on(provider.fetch_tile_bytes(c)).expect("second");

        assert_eq!(first.value, second.value);
        // The second call is served from the store, not the origin.
        let calls = *provider.inner.calls.lock().expect("lock");
        assert_eq!(calls, 1, "origin asked {calls} times");
    }

    #[test]
    fn a_cached_provider_stores_the_bytes_as_served_with_their_ttl() {
        let store = Arc::new(MemStore::default());
        let ttl = Some(std::time::Duration::from_secs(600));
        let provider = CachedImagery::new(
            CountingImagery {
                calls: Mutex::new(0),
                ttl,
            },
            store.clone(),
            "test",
        );
        futures_executor::block_on(provider.fetch_tile_bytes(coord(3, 4, 5))).expect("fetch");

        let writes = store.writes.lock().expect("lock");
        let (key, written_ttl) = writes.first().expect("one write");
        assert_eq!(*written_ttl, ttl, "the origin's lifetime is forwarded");
        // Encoded as served — never the decoded RGBA, which is far larger.
        let stored = store.entries.lock().expect("lock")[key].clone();
        assert_eq!(stored, Bytes::from_static(b"encoded"));
    }

    #[test]
    fn cached_keys_separate_tiles_and_namespaces() {
        let store = Arc::new(MemStore::default());
        let mk = |ns: &str| {
            CachedImagery::new(
                CountingImagery {
                    calls: Mutex::new(0),
                    ttl: None,
                },
                store.clone(),
                ns,
            )
        };
        let (a, b) = (mk("bing"), mk("osm"));
        assert_ne!(a.key(coord(3, 4, 5)), a.key(coord(3, 4, 6)));
        // Two providers number their tiles differently: never share entries.
        assert_ne!(a.key(coord(3, 4, 5)), b.key(coord(3, 4, 5)));
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
        assert_eq!((tex.value.width, tex.value.height), (2, 2));
        assert_eq!(&tex.value.rgba8[0..4], &[255, 0, 0, 255]);

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
