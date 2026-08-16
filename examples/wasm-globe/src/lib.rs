// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Raw-wasm globe demo — runs **entirely in the browser, with no server**.
//!
//! This wasm module IS the engine: it assembles the ion globe (terrain + Bing
//! imagery, from documents JS fetched), drives the real SSE traversal for a
//! Pyrénées view, decodes the quantized-mesh tiles, drapes the imagery mosaic,
//! and accumulates a [`GeometryReport`]. JavaScript only performs `fetch` —
//! there is no geometry *server*, no WebSocket, no wgpu. The request/provide/
//! step loop mirrors the server protocol, but JS plays the network and this
//! code plays everything else, in one browser thread.
//!
//! Flow (see `www/main.js`): resolve ion terrain endpoint + `layer.json`, the
//! Bing endpoint + metadata; `new Globe(...)`, `set_imagery(...)`; then loop
//! `step()` → fetch the returned terrain + imagery tiles → `provide(...)` /
//! `provide_imagery(...)` → `step()` again, until `done`; finally `report()`.

use std::collections::{HashMap, HashSet};

use glam::DVec2;
use serde::Serialize;
use url::Url;
use wasm_bindgen::prelude::*;

use tuile_bing::BingMetadata;
use tuile_core::content::{DecodedTexture, DecodedTileContent};
use tuile_core::geo::{ecef_to_geodetic, enu_frame, geodetic_to_ecef, Geodetic, WGS84_A};
use tuile_core::raster::{
    self, GeoRect as RasterRect, ImageryCoord, ImageryMosaic, OverlayAttachment, TilingScheme,
};
use tuile_core::report::GeometryReport;
use tuile_core::source::TileId;
use tuile_core::traversal::{traverse, Config, ResidencyView, TraversalOutput, ViewState};
use tuile_terrain::{decode, level_geometric_error, to_decoded, LayerJson, TerrainTree, TileCoord};

/// Max imagery tiles stitched per terrain tile (bounds the demo's fetches).
const IMAGERY_CAP: u32 = 16;

/// A terrain tile JS should fetch, with its fully-built ion URL.
#[derive(Serialize)]
struct Req {
    z: u32,
    x: u32,
    y: u32,
    url: String,
}

/// An imagery tile JS should fetch for a given terrain tile.
#[derive(Serialize)]
struct ImgReq {
    tz: u32,
    tx: u32,
    ty: u32,
    level: u32,
    x: u32,
    y: u32,
    url: String,
}

/// The result of one traversal step, handed back to JS.
#[derive(Serialize)]
struct StepResult {
    done: bool,
    selected: usize,
    resident: usize,
    pending_imagery: usize,
    vertices: usize,
    triangles: usize,
    textures: usize,
    terrain: Vec<Req>,
    imagery: Vec<ImgReq>,
}

/// A terrain tile whose geometry is built but whose imagery mosaic is still
/// arriving.
struct Pending {
    content: DecodedTileContent,
    rect: RasterRect,
    mosaic: ImageryMosaic,
    coords: Vec<ImageryCoord>,
    got: HashMap<ImageryCoord, DecodedTexture>,
}

/// The in-browser globe engine.
#[wasm_bindgen]
pub struct Globe {
    tree: TerrainTree,
    base: Url,
    bearer: String,
    bing: Option<BingMetadata>,
    img_scheme: Option<TilingScheme>,
    config: Config,
    views: Vec<ViewState>,
    residency: ResidencyView,
    out: TraversalOutput,
    /// What the previous step drew — see `Config::loading_descendant_limit`.
    rendered_last: std::collections::HashSet<tuile_core::source::TileId>,
    report: GeometryReport,
    failed: HashSet<TileId>,
    pending: HashMap<TileId, Pending>,
    frame: u64,
}

#[wasm_bindgen]
impl Globe {
    /// Assembles the ion globe from a fetched `layer.json`, its base URL and
    /// the asset access token, and arms a Pyrénées oblique view (mirrors the
    /// `globe-bulk` preset).
    #[wasm_bindgen(constructor)]
    pub fn new(layer_json: &str, base_url: &str, access_token: &str) -> Result<Globe, JsError> {
        console_error_panic_hook::set_once();
        let layer = LayerJson::from_slice(layer_json.as_bytes())
            .map_err(|e| JsError::new(&e.to_string()))?;
        let base = Url::parse(base_url).map_err(|e| JsError::new(&e.to_string()))?;
        let tree = TerrainTree::new(layer);

        let center = geodetic_to_ecef(Geodetic {
            lon: 1f64.to_radians(),
            lat: 42.7f64.to_radians(),
            height: 0.0,
        });
        let f = enu_frame(ecef_to_geodetic(center));
        let (east, north, up) = (f.col(0), f.col(1), f.col(2));
        let eye = center + up * 220_000.0 - north * 320_000.0 + east * 30_000.0;
        let target = center + north * 60_000.0;
        let view = ViewState::perspective(
            eye,
            (target - eye).normalize(),
            up,
            DVec2::new(2048.0, 2048.0),
            40f64.to_radians(),
        );

        Ok(Globe {
            tree,
            base,
            bearer: access_token.to_string(),
            bing: None,
            img_scheme: None,
            config: Config {
                maximum_screen_space_error: 2.0,
                ..Config::default()
            },
            views: vec![view],
            residency: ResidencyView::default(),
            out: TraversalOutput::default(),
            rendered_last: std::collections::HashSet::new(),
            report: GeometryReport::new(),
            failed: HashSet::new(),
            pending: HashMap::new(),
            frame: 0,
        })
    }

    /// Arms Bing imagery from its fetched metadata JSON. Terrain tiles provided
    /// after this will request and drape an imagery mosaic.
    pub fn set_imagery(&mut self, metadata_json: &str) -> Result<(), JsError> {
        let meta = BingMetadata::from_json(metadata_json.as_bytes())
            .map_err(|e| JsError::new(&e.to_string()))?;
        self.img_scheme = Some(meta.tiling_scheme());
        self.bing = Some(meta);
        Ok(())
    }

    /// Folds a fetched terrain tile in: decodes it, grows availability from its
    /// `metadata` extension, builds geometry, marks it resident. With imagery
    /// armed, the tile then awaits its imagery mosaic; otherwise it is finalized
    /// straight away. A decode failure marks the tile failed.
    pub fn provide(&mut self, z: u32, x: u32, y: u32, bytes: &[u8]) {
        let (x, y) = (x as u64, y as u64);
        let tile = TileId::from_terrain(z, x, y);
        let qm = match decode(bytes) {
            Ok(qm) => qm,
            Err(_) => {
                self.failed.insert(tile);
                return;
            }
        };
        if let Some(ranges) = &qm.metadata_available {
            self.tree.availability().add_descendant_ranges(z, ranges);
        }
        let rect = self.tree.scheme().tile_rect(TileCoord::new(z, x, y));
        let content = to_decoded(&qm, &rect, 0.0);
        self.residency.insert(tile);

        match self.img_scheme {
            Some(scheme) => {
                let georect = RasterRect {
                    west: rect.west,
                    south: rect.south,
                    east: rect.east,
                    north: rect.north,
                };
                let ge = level_geometric_error(z, WGS84_A, self.tree.scheme().root_tiles_x);
                let lat = 0.5 * (rect.south + rect.north);
                let level = scheme.level_for_texel_spacing(ge, lat);
                let mosaic = scheme.mosaic_at_level(&georect, level, IMAGERY_CAP);
                let coords = mosaic.tiles();
                self.pending.insert(
                    tile,
                    Pending {
                        content,
                        rect: georect,
                        mosaic,
                        coords,
                        got: HashMap::new(),
                    },
                );
            }
            None => self.report.add(tile, &content),
        }
    }

    /// Folds a fetched imagery tile into its terrain tile's mosaic; drapes once
    /// the mosaic is complete. (Flat args: a wasm-bindgen method JS calls.)
    #[allow(clippy::too_many_arguments)]
    pub fn provide_imagery(
        &mut self,
        tz: u32,
        tx: u32,
        ty: u32,
        level: u32,
        x: u32,
        y: u32,
        bytes: &[u8],
    ) {
        let size = self.img_scheme.map_or(256, |s| s.tile_size);
        let tex = raster::decode_image(bytes).unwrap_or_else(|_| gray_tile(size));
        self.store_imagery(tz, tx, ty, level, x, y, tex);
    }

    /// Folds in an imagery tile a Web Worker already decoded and reprojected.
    ///
    /// Same effect as [`Self::provide_imagery`], minus the work: the pixels
    /// arrive finished, so this call is a move into the mosaic rather than a
    /// decode. `rgba` is tightly packed RGBA8, `width * height * 4` long — the
    /// buffer [`decode_imagery_tile`] produced in the worker, handed over by
    /// `postMessage` as a transferable.
    ///
    /// A length that does not match is dropped for a neutral tile rather than
    /// panicking across the wasm boundary: the mosaic still stitches, and the
    /// tile is merely grey.
    #[allow(clippy::too_many_arguments)]
    pub fn provide_imagery_decoded(
        &mut self,
        tz: u32,
        tx: u32,
        ty: u32,
        level: u32,
        x: u32,
        y: u32,
        rgba: Vec<u8>,
        width: u32,
        height: u32,
    ) {
        let size = self.img_scheme.map_or(256, |s| s.tile_size);
        let expected = (width as usize) * (height as usize) * 4;
        let tex = if rgba.len() == expected && expected > 0 {
            DecodedTexture {
                width,
                height,
                rgba8: rgba,
            }
        } else {
            gray_tile(size)
        };
        self.store_imagery(tz, tx, ty, level, x, y, tex);
    }

    /// Marks an imagery tile missing (JS calls this on a 404); a neutral tile
    /// fills its slot so the mosaic can still stitch.
    pub fn fail_imagery(&mut self, tz: u32, tx: u32, ty: u32, level: u32, x: u32, y: u32) {
        let size = self.img_scheme.map_or(256, |s| s.tile_size);
        self.store_imagery(tz, tx, ty, level, x, y, gray_tile(size));
    }

    /// Re-runs the SSE traversal, returning the next terrain tiles and imagery
    /// tiles to fetch (with URLs) plus running totals. `done` once nothing more
    /// is needed and no imagery is outstanding.
    pub fn step(&mut self) -> Result<JsValue, JsError> {
        self.frame += 1;
        traverse(
            &self.tree,
            &self.residency,
            &self.views,
            &self.config,
            self.frame,
            &self.rendered_last,
            &mut self.out,
        );
        self.rendered_last.clear();
        self.rendered_last
            .extend(self.out.selected.iter().map(|(tile, _)| *tile));
        let terrain: Vec<Req> = self
            .out
            .requests
            .iter()
            .map(|r| r.tile)
            .filter(|t| !self.residency.is_resident(*t) && !self.failed.contains(t))
            .filter_map(|t| {
                let (z, x, y) = t.terrain_coord();
                self.tile_url(TileCoord::new(z, x, y)).map(|url| Req {
                    z,
                    x: x as u32,
                    y: y as u32,
                    url,
                })
            })
            .collect();
        let imagery: Vec<ImgReq> = match &self.bing {
            Some(meta) => self
                .pending
                .iter()
                .flat_map(|(tile, p)| {
                    let (tz, tx, ty) = tile.terrain_coord();
                    p.coords
                        .iter()
                        .filter(move |c| !p.got.contains_key(c))
                        .map(move |c| ImgReq {
                            tz,
                            tx: tx as u32,
                            ty: ty as u32,
                            level: c.level,
                            x: c.x as u32,
                            y: c.y as u32,
                            url: meta.tile_url(*c),
                        })
                })
                .collect(),
            None => Vec::new(),
        };
        let result = StepResult {
            done: terrain.is_empty() && self.pending.is_empty(),
            selected: self.out.selected.len(),
            resident: self.residency.iter().count(),
            pending_imagery: self.pending.len(),
            vertices: self.report.totals.vertices,
            triangles: self.report.totals.triangles,
            textures: self.report.totals.textures,
            terrain,
            imagery,
        };
        serde_wasm_bindgen::to_value(&result).map_err(|e| JsError::new(&e.to_string()))
    }

    /// The accumulated geometry report as pretty JSON.
    pub fn report(&self) -> String {
        self.report.to_json_pretty()
    }

    #[allow(clippy::too_many_arguments)]
    fn store_imagery(
        &mut self,
        tz: u32,
        tx: u32,
        ty: u32,
        level: u32,
        x: u32,
        y: u32,
        tex: DecodedTexture,
    ) {
        let tile = TileId::from_terrain(tz, tx as u64, ty as u64);
        let coord = ImageryCoord {
            level,
            x: x as u64,
            y: y as u64,
        };
        match self.pending.get_mut(&tile) {
            Some(p) => {
                p.got.insert(coord, tex);
            }
            None => return,
        }
        self.finalize_if_ready(tile);
    }

    /// Once a terrain tile's whole mosaic is in, stitches + reprojects it and
    /// drapes it onto the geometry, then files the tile in the report.
    fn finalize_if_ready(&mut self, tile: TileId) {
        let ready = self
            .pending
            .get(&tile)
            .is_some_and(|p| p.coords.iter().all(|c| p.got.contains_key(c)));
        if !ready {
            return;
        }
        let Some(scheme) = self.img_scheme else {
            return;
        };
        let p = self.pending.remove(&tile).expect("ready implies present");
        let texs: Vec<DecodedTexture> = p
            .coords
            .iter()
            .map(|c| {
                p.got
                    .get(c)
                    .cloned()
                    .unwrap_or_else(|| gray_tile(scheme.tile_size))
            })
            .collect();
        let stitched = raster::stitch_mosaic(&texs, p.mosaic.cols, p.mosaic.rows, scheme.tile_size);
        let ext = scheme.mosaic_extent(&p.mosaic);
        let geographic =
            raster::reproject_to_geographic(&stitched, ext, &p.rect, scheme.projection);
        let mut content = p.content;
        let uvs = content
            .meshes
            .iter()
            .map(|m| raster::uvs_geographic(&m.positions, content.local_origin_ecef, &p.rect))
            .collect();
        raster::drape_single(
            &mut content,
            &OverlayAttachment {
                imagery: ImageryCoord {
                    level: p.mosaic.level,
                    x: p.mosaic.x0,
                    y: p.mosaic.y0,
                },
                uvs,
            },
            geographic,
        );
        self.report.add(tile, &content);
    }

    /// Builds the ion terrain tile URL: base + `layer.json` template + the
    /// advertised `extensions`, with the token as a query param (a header would
    /// force a CORS preflight per tile — see www/main.js).
    fn tile_url(&self, coord: TileCoord) -> Option<String> {
        let rel = self.tree.layer().tile_url(coord)?;
        let mut url = self.base.join(&rel).ok()?;
        if let Some(ext) = self.tree.layer().extensions_query() {
            url.query_pairs_mut().append_pair("extensions", &ext);
        }
        url.query_pairs_mut()
            .append_pair("access_token", &self.bearer);
        Some(url.to_string())
    }
}

/// A neutral grey tile, used in place of imagery that failed to load.
fn gray_tile(size: u32) -> DecodedTexture {
    DecodedTexture {
        width: size,
        height: size,
        rgba8: vec![128u8; (size * size * 4) as usize],
    }
}

/// One imagery tile, decoded and reprojected, on its way out of a Web Worker.
///
/// Flat and owned so `postMessage` can hand the pixels over as a transferable
/// rather than copy them: a 256×256 tile is 256 KiB, and a mosaic is several of
/// those per terrain tile.
#[wasm_bindgen]
pub struct DecodedTile {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

#[wasm_bindgen]
impl DecodedTile {
    #[wasm_bindgen(getter)]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[wasm_bindgen(getter)]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The pixels, moved out. Consumes the tile — there is one reader and
    /// copying a quarter-megabyte to be polite would defeat the point.
    #[wasm_bindgen(getter)]
    pub fn rgba(self) -> Vec<u8> {
        self.rgba
    }
}

/// Decodes and reprojects one imagery tile. **The whole point of the worker.**
///
/// Free rather than a method, and stateless but for the tiling scheme, because
/// a Worker runs its own instance of this module and has no `Globe` to call
/// into. Everything it needs arrives as arguments; everything it returns is
/// bytes. That is what lets the work leave the browser's main thread at all —
/// see [`tuile_core::raster::decode_and_reproject`] for why a closure could not.
///
/// `metadata_json` is the same Bing metadata document `Globe::set_imagery`
/// takes, so both sides derive the identical scheme from the identical source
/// rather than agreeing by hand.
#[wasm_bindgen]
pub struct ImageryDecoder {
    scheme: TilingScheme,
}

#[wasm_bindgen]
impl ImageryDecoder {
    #[wasm_bindgen(constructor)]
    pub fn new(metadata_json: &str) -> Result<ImageryDecoder, JsError> {
        let meta = BingMetadata::from_json(metadata_json.as_bytes())
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(ImageryDecoder {
            scheme: meta.tiling_scheme(),
        })
    }

    /// Decode + reproject, the unit of work this worker exists to run.
    pub fn decode(&self, bytes: &[u8], level: u32, x: u32, y: u32) -> Result<DecodedTile, JsError> {
        let coord = ImageryCoord {
            level,
            x: x as u64,
            y: y as u64,
        };
        let tex = raster::decode_and_reproject(bytes, &self.scheme, coord)
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(DecodedTile {
            width: tex.width,
            height: tex.height,
            rgba: tex.rgba8,
        })
    }
}
