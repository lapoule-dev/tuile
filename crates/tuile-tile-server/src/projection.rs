// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Projections: a scene's slice of the store, as local archives.
//!
//! A bake reads the same ground hundreds of times, frame after frame. Reading
//! it from the bucket costs several round trips per tile; reading it from a
//! local file costs nothing. So before the first frame, every remote zone the
//! scene can see is **projected**: each of the zone's archives is fetched in
//! **one request**, the tiles the scene can use are kept, and one local
//! PMTiles archive per zone is written. The `top` zone — the coarse tiles
//! every flight shares — is filtered like any other.
//!
//! One whole-object request per archive rather than coalesced byte ranges: a
//! zone is a few megabytes, and the latency of each request is what a
//! projection pays for, not the bytes. For the same reason a zone the scene
//! reaches is projected whole — `top` included, which holds the coarse
//! pyramid of the whole globe that every bake pins as its floor. The footprint
//! chooses zones; it does not cut inside them.
//!
//! Except in a **targeted** layer ([`Footprint::with_targeted_layer`]): there
//! a cell zone's archives are listed from their directories alone (the header
//! and root in one request, the leaves in one more), and only the tiles the
//! footprint keeps are fetched, by byte ranges — neighbours on the ground are
//! neighbours in an archive, so nearby ranges are fetched together. Made for
//! terrain: the 900 frames of one measured film chunk read a few hundred
//! terrain tiles below `top`, out of 17 500 (380 MB) in the zones their
//! cameras reach. `top` stays whole there too: every bake pins it.
//!
//! # Which tiles a scene can use
//!
//! A tile of level `L` is drawn only while the camera is close enough for its
//! error to matter and far enough not to need its children: within a few
//! tile widths. A tile is kept when, for some camera position, its distance to
//! the eye — ground distance, and the eye's height above the highest ground —
//! is at most `tile_factor` times its width, and within the horizon. A tile
//! the projection left out is not an error: the store falls back to the
//! bucket, then the source, and counts it, which is how the factor is tuned.

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};

use crate::archive;
use crate::grid::Grid;
use crate::layer::{Layer, Zone};
use crate::manifest;
use crate::store::{download, TileStore};
use crate::StoreError;

/// Mean Earth radius, for great-circle distances and the horizon.
const EARTH_RADIUS_M: f64 = 6_371_008.8;
/// Metres per degree of latitude.
const M_PER_DEG: f64 = EARTH_RADIUS_M * std::f64::consts::PI / 180.0;
/// The highest ground an eye's height is measured against: its height above
/// the terrain is at least `height − this`.
pub const HIGHEST_GROUND_M: f64 = 4_800.0;
/// A [`Footprint::tile_factor`] for a caller that does not size its own. A
/// bake derives its terrain's from its screen-space error and picture height
/// (`tuile_bake::tiles::terrain_tile_factor`).
pub const DEFAULT_TILE_FACTOR: f64 = 12.0;
/// Camera positions closer than this to one already kept add nothing.
const EYE_SPACING_M: f64 = 500.0;
/// How much nearer to a tile a camera left out may be than the one kept in
/// its place: within [`EYE_SPACING_M`] across and as much in height. Added
/// to every reach, so thinning the cameras never loses a tile.
const EYE_SLACK_M: f64 = EYE_SPACING_M * std::f64::consts::SQRT_2;
/// Zones projected at once, unless `TUILE_PROJECTION_ZONES` says.
const ZONES_IN_FLIGHT: usize = 16;
/// Byte ranges of one targeted zone fetched at once, unless
/// `TUILE_PROJECTION_RANGES` says.
const RANGES_IN_FLIGHT: usize = 8;

/// [`ZONES_IN_FLIGHT`], or what the environment says, read once.
fn zones_in_flight() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    crate::knob::count(&N, "TUILE_PROJECTION_ZONES", ZONES_IN_FLIGHT)
}

/// [`RANGES_IN_FLIGHT`], or what the environment says, read once.
fn ranges_in_flight() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    crate::knob::count(&N, "TUILE_PROJECTION_RANGES", RANGES_IN_FLIGHT)
}
/// Two ranges of an archive closer than this are fetched as one: a gap this
/// size costs less than a request.
pub const COALESCE_GAP: u64 = 64 << 10;
/// A merged range grows no longer than this.
pub const MAX_RANGE: u64 = 4 << 20;

/// One camera position: degrees, and metres above the ellipsoid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Eye {
    pub lon: f64,
    pub lat: f64,
    pub height: f64,
}

/// Where a scene's cameras are, and which tiles they can use.
#[derive(Debug, Clone)]
pub struct Footprint {
    eyes: Vec<Eye>,
    /// A tile is kept within this many of its widths from an eye.
    pub tile_factor: f64,
    /// A different factor for some layers (by name; a layer's absence
    /// sibling follows its layer). Imagery needs more than terrain: a drape
    /// composes imagery tiles several levels finer than the terrain tile it
    /// covers, so at a given distance the imagery in use is much smaller.
    layer_factors: Vec<(String, f64)>,
    /// Layers whose cell zones are cut tile by tile, fetched by ranges.
    targeted: Vec<String>,
    /// Layers projected by the register's index rather than their
    /// archives (by name; a layer's absence sibling follows its layer).
    by_index: Vec<String>,
    /// The layers projected, when not every layer of the store.
    only: Option<Vec<String>>,
}

/// A tile's extent in degrees: west, south, east, north.
pub fn bounds(grid: Grid, level: u8, x: u32, y: u32) -> [f64; 4] {
    match grid {
        Grid::WebMercator => {
            let n = f64::from(1u32 << level.min(31));
            let lon = |x: f64| x / n * 360.0 - 180.0;
            let lat = |y: f64| {
                (std::f64::consts::PI * (1.0 - 2.0 * y / n))
                    .sinh()
                    .atan()
                    .to_degrees()
            };
            [
                lon(f64::from(x)),
                lat(f64::from(y) + 1.0),
                lon(f64::from(x) + 1.0),
                lat(f64::from(y)),
            ]
        }
        // Geographic tiles count from the south-west, 2 × 1 at level 0.
        Grid::Geographic => {
            let size = 180.0 / f64::from(1u32 << level.min(30));
            let (w, s) = (-180.0 + f64::from(x) * size, -90.0 + f64::from(y) * size);
            [w, s, w + size, s + size]
        }
    }
}

/// A tile's width as a footprint measures it, metres: the larger of its two
/// sides, across at its middle latitude.
pub fn width_m(grid: Grid, level: u8, x: u32, y: u32) -> f64 {
    let [w, s, e, n] = bounds(grid, level, x, y);
    let mid_lat = ((s + n) / 2.0).to_radians();
    ((e - w) * M_PER_DEG * mid_lat.cos()).max((n - s) * M_PER_DEG)
}

/// Great-circle distance, metres.
fn distance(a_lon: f64, a_lat: f64, b_lon: f64, b_lat: f64) -> f64 {
    let (p1, p2) = (a_lat.to_radians(), b_lat.to_radians());
    let dp = p2 - p1;
    let dl = (b_lon - a_lon).to_radians();
    let h = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * h.sqrt().min(1.0).asin()
}

impl Footprint {
    /// From camera positions, thinned so that nearby ones count once.
    pub fn from_eyes(eyes: impl IntoIterator<Item = Eye>, tile_factor: f64) -> Self {
        let mut kept: Vec<Eye> = Vec::new();
        for e in eyes {
            let near = kept.iter().any(|k| {
                (k.height - e.height).abs() < EYE_SPACING_M
                    && distance(k.lon, k.lat, e.lon, e.lat) < EYE_SPACING_M
            });
            if !near {
                kept.push(e);
            }
        }
        Self {
            eyes: kept,
            tile_factor,
            layer_factors: Vec::new(),
            targeted: Vec::new(),
            by_index: Vec::new(),
            only: None,
        }
    }

    /// Projects these layers (and their absence siblings) and no other: the
    /// layers a consumer reads, not every layer the store's catalog holds.
    pub fn with_only_layers<S: Into<String>>(mut self, names: impl IntoIterator<Item = S>) -> Self {
        self.only = Some(names.into_iter().map(Into::into).collect());
        self
    }

    /// Whether the layer `name` is projected at all.
    pub fn projects(&self, layer: &str) -> bool {
        let base = layer
            .strip_suffix(crate::catalog::ABSENT_SUFFIX)
            .unwrap_or(layer);
        self.only
            .as_ref()
            .is_none_or(|only| only.iter().any(|n| n == base))
    }

    /// Projects the layer `name` (and its absence sibling) by index: which
    /// tiles each zone holds and a stamp of each, read from the store's
    /// register ([`TileStore::with_index_reader`]), and not a byte of the
    /// tiles — for a consumer that names tiles and draws none. A zone the
    /// register does not know whole, or a store without one, is projected
    /// from its archives, as before.
    pub fn with_layer_by_index(mut self, name: impl Into<String>) -> Self {
        self.by_index.push(name.into());
        self
    }

    /// Whether a layer is projected by its indexes.
    pub fn by_index(&self, layer: &str) -> bool {
        let base = layer
            .strip_suffix(crate::catalog::ABSENT_SUFFIX)
            .unwrap_or(layer);
        self.by_index.iter().any(|n| n == base)
    }

    /// Cuts the cell zones of the layer `name` tile by tile: only the tiles
    /// this footprint keeps are fetched, by byte ranges. Its `top` zone and
    /// its absence sibling stay whole.
    pub fn with_targeted_layer(mut self, name: impl Into<String>) -> Self {
        self.targeted.push(name.into());
        self
    }

    /// Whether the layer `name`'s cell zones are cut tile by tile.
    pub fn targets(&self, layer: &str) -> bool {
        self.targeted.iter().any(|t| t == layer)
    }

    /// Uses `factor` for the layer `name` (and its absence sibling).
    pub fn with_layer_factor(mut self, name: impl Into<String>, factor: f64) -> Self {
        self.layer_factors.push((name.into(), factor));
        self
    }

    /// The factor a layer is filtered with.
    pub fn factor_for(&self, layer: &str) -> f64 {
        let base = layer
            .strip_suffix(crate::catalog::ABSENT_SUFFIX)
            .unwrap_or(layer);
        self.layer_factors
            .iter()
            .find(|(n, _)| n == base)
            .map_or(self.tile_factor, |(_, f)| *f)
    }

    pub fn eyes(&self) -> &[Eye] {
        &self.eyes
    }

    /// Whether some eye can use this tile, at the default factor.
    pub fn keeps(&self, grid: Grid, level: u8, x: u32, y: u32) -> bool {
        self.keeps_with(self.tile_factor, grid, level, x, y)
    }

    /// Whether some eye can use this tile, within `factor` of its widths.
    pub fn keeps_with(&self, factor: f64, grid: Grid, level: u8, x: u32, y: u32) -> bool {
        let [w, s, e, n] = bounds(grid, level, x, y);
        let reach = factor * width_m(grid, level, x, y) + EYE_SLACK_M;
        self.eyes.iter().any(|eye| {
            // Nearest point of the tile to the eye's ground position.
            let lon = eye.lon.clamp(w, e);
            let lat = eye.lat.clamp(s, n);
            let ground = distance(eye.lon, eye.lat, lon, lat);
            let above = (eye.height - HIGHEST_GROUND_M).max(0.0);
            let horizon = (2.0 * EARTH_RADIUS_M * eye.height.max(0.0) + eye.height.powi(2)).sqrt()
                + (2.0 * EARTH_RADIUS_M * HIGHEST_GROUND_M).sqrt();
            ground <= horizon && (ground * ground + above * above).sqrt() <= reach
        })
    }

    /// Whether some tile of the zone could be kept: the zone's own extent
    /// within the horizon of an eye. In a targeted layer, the zone's own tile
    /// kept: every other tile of the zone lies within it and is smaller, so
    /// none is kept if it is not.
    fn reaches(&self, layer: &Layer, zone: Zone) -> bool {
        let Zone::Cell { x, y } = zone else {
            return true;
        };
        if self.targets(&layer.name) {
            return self.keeps_with(
                self.factor_for(&layer.name),
                layer.grid,
                layer.zone_level,
                x,
                y,
            );
        }
        let [w, s, e, n] = bounds(layer.grid, layer.zone_level, x, y);
        self.eyes.iter().any(|eye| {
            let ground = distance(eye.lon, eye.lat, eye.lon.clamp(w, e), eye.lat.clamp(s, n));
            let horizon = (2.0 * EARTH_RADIUS_M * eye.height.max(0.0) + eye.height.powi(2)).sqrt()
                + (2.0 * EARTH_RADIUS_M * HIGHEST_GROUND_M).sqrt();
            ground <= horizon
        })
    }
}

/// What a projection did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectionReport {
    /// Remote zones considered, and projected (kept at least one tile).
    pub zones_seen: usize,
    pub zones_projected: usize,
    /// Tiles listed by the remote archives, and kept.
    pub tiles_listed: u64,
    pub tiles_kept: u64,
    /// Bytes fetched, and requests made: one per archive of a zone taken
    /// whole; for a targeted zone, its directories' and its ranges'.
    pub bytes: u64,
    pub requests: u64,
    pub millis: u64,
    /// What the zones seen weigh whole: what a projection that took them
    /// whole would have fetched.
    pub whole_bytes: u64,
    /// The same, layer by layer (`top` apart).
    pub layers: BTreeMap<String, LayerProjection>,
    /// Zones known by index alone, and the tiles their index names.
    pub zones_indexed: usize,
    pub tiles_indexed: u64,
    /// Zones known by the register ([`crate::IndexReader`]) alone — not one
    /// index read, nothing listed — the tiles it gave, in how many batches,
    /// and how long reading it took.
    pub zones_registered: usize,
    pub tiles_registered: u64,
    pub register_batches: u64,
    pub register_millis: u64,
    /// Layers whose zones were listed from the bucket, and from the register.
    pub layers_listed: usize,
    pub layers_from_register: usize,
    /// Zones whose projection was already made from the same archives, by
    /// this bake's process or an earlier one: nothing downloaded for them.
    pub zones_reused: usize,
}

/// What a projection did of one layer: its cell zones, and its `top`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayerProjection {
    pub targeted: bool,
    pub zones_seen: usize,
    pub zones_projected: usize,
    pub tiles_listed: u64,
    pub tiles_kept: u64,
    pub bytes: u64,
    pub requests: u64,
    pub whole_bytes: u64,
    /// Zones known by their indexes alone, and the tiles they name.
    pub zones_indexed: usize,
    pub tiles_indexed: u64,
    /// Of which known by the register alone, and the tiles it gave.
    pub zones_registered: usize,
    pub tiles_registered: u64,
    /// Of which the `top` zone's.
    pub top_tiles: u64,
    pub top_bytes: u64,
    pub top_requests: u64,
}

impl ProjectionReport {
    /// Adds one zone's report, under its layer.
    fn add(&mut self, layer: &str, zone: Zone, targeted: bool, r: &ProjectionReport) {
        self.zones_seen += r.zones_seen;
        self.zones_projected += r.zones_projected;
        self.tiles_listed += r.tiles_listed;
        self.tiles_kept += r.tiles_kept;
        self.bytes += r.bytes;
        self.requests += r.requests;
        self.whole_bytes += r.whole_bytes;
        self.zones_indexed += r.zones_indexed;
        self.tiles_indexed += r.tiles_indexed;
        self.zones_registered += r.zones_registered;
        self.tiles_registered += r.tiles_registered;
        self.register_batches += r.register_batches;
        self.register_millis += r.register_millis;
        self.zones_reused += r.zones_reused;
        let l = self.layers.entry(layer.to_string()).or_default();
        l.targeted |= targeted;
        l.zones_seen += r.zones_seen;
        l.zones_projected += r.zones_projected;
        l.tiles_listed += r.tiles_listed;
        l.tiles_kept += r.tiles_kept;
        l.bytes += r.bytes;
        l.requests += r.requests;
        l.whole_bytes += r.whole_bytes;
        l.zones_indexed += r.zones_indexed;
        l.tiles_indexed += r.tiles_indexed;
        l.zones_registered += r.zones_registered;
        l.tiles_registered += r.tiles_registered;
        if zone == Zone::Top {
            l.top_tiles += r.tiles_kept;
            l.top_bytes += r.bytes;
            l.top_requests += r.requests;
        }
    }
}

/// Byte ranges, `(start, length)`, merged where they touch or lie within
/// [`COALESCE_GAP`] of each other, no merged range longer than [`MAX_RANGE`]
/// unless one tile is.
pub fn coalesce(mut spans: Vec<(u64, u64)>) -> Vec<Range<u64>> {
    spans.sort_unstable();
    spans.dedup();
    let mut out: Vec<Range<u64>> = Vec::new();
    for (start, length) in spans {
        let end = start + length;
        match out.last_mut() {
            Some(last) if start <= last.end + COALESCE_GAP && end - last.start <= MAX_RANGE => {
                last.end = last.end.max(end);
            }
            _ => out.push(start..end),
        }
    }
    out
}

/// What a zone holds and where, read from its archives' directories alone
/// ([`TileStore::zone_listing`]).
#[derive(Debug, Clone, Default)]
pub struct ZoneListing {
    /// The zone's live archives' keys, oldest first.
    pub archives: Vec<String>,
    /// The newest live archive's epoch.
    pub epoch: String,
    /// Each tile id the zone holds: the archive (an index into `archives`)
    /// that gives it, the newest that holds it, and its bytes there.
    pub tiles: BTreeMap<u64, (usize, Range<u64>)>,
    /// Tile entries listed, every archive counted.
    pub listed: u64,
    /// Requests made and bytes read to list it.
    pub requests: u64,
    pub bytes: u64,
    /// What the live archives weigh, whole.
    pub whole_bytes: u64,
}

/// Sorted ranges, those that overlap or touch made one.
pub fn merge_ranges(
    sorted: Vec<std::ops::RangeInclusive<u64>>,
) -> Vec<std::ops::RangeInclusive<u64>> {
    let mut out: Vec<std::ops::RangeInclusive<u64>> = Vec::with_capacity(sorted.len());
    for r in sorted {
        match out.last_mut() {
            Some(last) if *r.start() <= last.end().saturating_add(1) => {
                let end = (*last.end()).max(*r.end());
                *last = *last.start()..=end;
            }
            _ => out.push(r),
        }
    }
    out
}

/// Where a zone's projection lives under `dir`.
pub fn projection_path(dir: &FsPath, layer: &Layer, zone: Zone) -> PathBuf {
    dir.join(format!("{}.pmtiles", layer.zone_prefix(zone)))
}

/// Where the archives a projection was made from are named, beside it: the
/// keys of the zone's live archives, one a line, in the manifest's order.
/// Archives are never rewritten, so the same keys are the same bytes: a
/// projection made from them is good for any bake, of any process that
/// shares the directory, until the zone's manifest names other archives.
pub fn projection_source_path(path: &FsPath) -> PathBuf {
    path.with_extension("pmtiles.source")
}

fn source_of(keys: &[String]) -> String {
    keys.join("\n")
}

/// Tile entries of archive listings kept in memory, unless
/// `TUILE_LISTING_CACHE_TILES` says: some 24 bytes each.
const LISTING_CACHE_TILES: usize = 2_000_000;

pub(crate) fn listing_cache_tiles() -> u64 {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    crate::knob::count(&N, "TUILE_LISTING_CACHE_TILES", LISTING_CACHE_TILES) as u64
}

/// The ids a projection holds, from its directories alone.
async fn ids_of(
    reader: &Arc<archive::LocalReader>,
) -> Result<std::collections::HashSet<u64>, StoreError> {
    let mut ids = std::collections::HashSet::new();
    let mut entries = reader.clone().entries();
    while let Some(entry) = entries.try_next().await? {
        ids.extend(entry.iter_coords().map(|tid| tid.value()));
    }
    Ok(ids)
}

/// Every tile of a projection, read back.
async fn tiles_of(reader: &Arc<archive::LocalReader>) -> Result<BTreeMap<u64, Bytes>, StoreError> {
    let mut tiles = BTreeMap::new();
    let mut entries = reader.clone().entries();
    while let Some(entry) = entries.try_next().await? {
        for tid in entry.iter_coords() {
            if let Some(bytes) = reader.get_tile(tid).await? {
                tiles.insert(tid.value(), bytes);
            }
        }
    }
    Ok(tiles)
}

impl TileStore {
    /// Projects every remote zone the footprint reaches into `dir`, and reads
    /// through those projections from now on.
    ///
    /// The zones of a layer come from the register when the store has one
    /// that knows the layer ([`TileStore::with_index_reader`]), from a
    /// listing of the bucket otherwise; a layer the footprint does not
    /// project is not listed at all. A zone of a layer projected by index
    /// that the register knows whole is known from it alone, its tiles
    /// streamed by ranges of ids: no index read. Whatever the register
    /// cannot answer goes the way it went without one.
    pub async fn project(
        &self,
        footprint: &Footprint,
        dir: &FsPath,
    ) -> Result<ProjectionReport, StoreError> {
        let began = Instant::now();
        let mut total = ProjectionReport::default();
        let mut zones: Vec<(String, Zone)> = Vec::new();
        let mut names: Vec<String> = self.layers().map(|l| l.name.clone()).collect();
        names.sort();
        for name in &names {
            if !footprint.projects(name) {
                continue;
            }
            // Kept by an absence store: read with their layer, never listed.
            if self.absences_first() && name.ends_with(crate::catalog::ABSENT_SUFFIX) {
                continue;
            }
            let layer = self.layer(name)?;
            let (listed, registered) = self.zones_for_projection(layer).await?;
            if registered {
                total.layers_from_register += 1;
            } else {
                total.layers_listed += 1;
            }
            let mut from_register = Vec::new();
            for (zone, indexed) in listed {
                if !footprint.reaches(layer, zone) {
                    continue;
                }
                if indexed && footprint.by_index(name) {
                    from_register.push(zone);
                } else {
                    zones.push((name.clone(), zone));
                }
            }
            if from_register.is_empty() {
                continue;
            }
            match self.index_from_register(layer, &from_register).await {
                Ok(reports) => {
                    for (zone, r) in reports {
                        total.add(name, zone, false, &r);
                    }
                }
                Err(error) => {
                    // Read as without a register: slower, and as right.
                    tracing::warn!(layer = %name, %error, zones = from_register.len(), "REGISTER-READ-FAILED: the zones are read by their indexes");
                    metrics::counter!("tuile_tiles_register_failures_total", "layer" => name.clone(), "what" => "tiles").increment(1);
                    zones.extend(from_register.into_iter().map(|z| (name.clone(), z)));
                }
            }
        }
        let results: Vec<(String, Zone, bool, Result<ProjectionReport, StoreError>)> =
            futures_util::stream::iter(zones)
                .map(|(layer, zone)| async move {
                    let targeted = footprint.targets(&layer) && zone != Zone::Top;
                    metrics::counter!("tuile_tiles_zones_projected_total", "layer" => layer.clone(), "by" => "archives").increment(1);
                    let r = if targeted {
                        self.project_zone_targeted(footprint, dir, &layer, zone)
                            .await
                    } else {
                        self.project_zone(dir, &layer, zone).await.map(|mut r| {
                            r.whole_bytes = r.bytes;
                            r
                        })
                    };
                    (layer, zone, targeted, r)
                })
                .buffer_unordered(zones_in_flight())
                .collect()
                .await;
        for (layer, zone, targeted, r) in results {
            let r = r?;
            tracing::debug!(
                layer,
                zone = ?zone,
                targeted,
                tiles_listed = r.tiles_listed,
                tiles = r.tiles_kept,
                bytes = r.bytes,
                requests = r.requests,
                whole_bytes = r.whole_bytes,
                "TILES-PROJECTED-ZONE"
            );
            total.add(&layer, zone, targeted, &r);
        }
        total.millis = began.elapsed().as_millis() as u64;
        Ok(total)
    }

    /// A layer's zones, and whether they come from the register: each with
    /// whether the register knows its tiles whole. Listed from the bucket
    /// when there is no register, it does not know the layer, or it failed.
    async fn zones_for_projection(
        &self,
        layer: &Layer,
    ) -> Result<(Vec<(Zone, bool)>, bool), StoreError> {
        if let Some(reader) = self.index_reader() {
            let began = Instant::now();
            match reader.zones(&layer.name).await {
                Ok(Some(known)) => {
                    tracing::info!(
                        layer = %layer.name,
                        zones = known.len(),
                        indexed = known.iter().filter(|z| z.indexed).count(),
                        seconds = began.elapsed().as_secs_f64(),
                        "REGISTER-ZONES"
                    );
                    return Ok((
                        known.into_iter().map(|z| (z.zone, z.indexed)).collect(),
                        true,
                    ));
                }
                Ok(None) => {
                    tracing::info!(layer = %layer.name, "REGISTER-LAYER-UNKNOWN: the bucket is listed");
                }
                Err(error) => {
                    tracing::warn!(layer = %layer.name, %error, "REGISTER-ZONES-FAILED: the bucket is listed");
                    metrics::counter!("tuile_tiles_register_failures_total", "layer" => layer.name.clone(), "what" => "zones").increment(1);
                }
            }
        }
        let began = Instant::now();
        let listed = self.zones_of(layer).await?;
        metrics::histogram!("tuile_tiles_zone_listing_seconds", "layer" => layer.name.clone())
            .record(began.elapsed().as_secs_f64());
        Ok((listed.into_iter().map(|z| (z, false)).collect(), false))
    }

    /// Knows `zones` of `layer` by the register alone: their tiles streamed
    /// by ranges of ids, in batches, and each zone attached as if its
    /// archives' indexes had been read. A tile whose winning copy is in an
    /// expired archive is not held, as the store would not read it.
    async fn index_from_register(
        &self,
        layer: &Layer,
        zones: &[Zone],
    ) -> Result<Vec<(Zone, ProjectionReport)>, String> {
        let Some(reader) = self.index_reader() else {
            return Err("no register".into());
        };
        let began = Instant::now();
        let now = self.now();
        let mut ranges: Vec<std::ops::RangeInclusive<u64>> = zones
            .iter()
            .flat_map(|z| layer.zone_id_ranges(*z))
            .collect();
        ranges.sort_by_key(|r| *r.start());
        let ranges = merge_ranges(ranges);
        let mut tiles: HashMap<Zone, HashMap<u64, crate::index::Stamp>> =
            zones.iter().map(|z| (*z, HashMap::new())).collect();
        // The layer's absences, when an absence store keeps them: the same
        // rows, flagged, for the absence sibling's index.
        let absent_layer = format!("{}{}", layer.name, crate::catalog::ABSENT_SUFFIX);
        let absences_here = self.absences_first() && self.layer(&absent_layer).is_ok();
        let mut absent: HashMap<Zone, HashMap<u64, crate::index::Stamp>> = if absences_here {
            zones.iter().map(|z| (*z, HashMap::new())).collect()
        } else {
            HashMap::new()
        };
        let mut expired: HashMap<String, bool> = HashMap::new();
        let (mut rows, mut batches, mut skipped) = (0u64, 0u64, 0u64);
        let mut stream = reader.tiles(&layer.name, &ranges);
        while let Some(batch) = stream.next().await {
            let batch = batch?;
            batches += 1;
            rows += batch.len() as u64;
            for t in batch {
                if t.absent {
                    if !absences_here || t.expires_at.is_some_and(|at| at <= now) {
                        skipped += 1;
                        continue;
                    }
                    let zone = layer
                        .grid
                        .from_id(t.id)
                        .and_then(|(level, x, y)| layer.zone_of(level, x, y).ok());
                    if let Some(held) = zone.and_then(|z| absent.get_mut(&z)) {
                        held.insert(t.id, t.stamp);
                    }
                    continue;
                }
                let gone = match expired.get(&t.epoch) {
                    Some(gone) => *gone,
                    None => {
                        let gone = layer.is_expired(&t.epoch, now);
                        expired.insert(t.epoch.clone(), gone);
                        gone
                    }
                };
                if gone {
                    skipped += 1;
                    continue;
                }
                let Some(zone) = layer
                    .grid
                    .from_id(t.id)
                    .and_then(|(level, x, y)| layer.zone_of(level, x, y).ok())
                else {
                    continue;
                };
                if let Some(held) = tiles.get_mut(&zone) {
                    held.insert(t.id, t.stamp);
                }
            }
        }
        drop(stream);
        let millis = began.elapsed().as_millis() as u64;
        metrics::counter!("tuile_tiles_register_rows_total", "layer" => layer.name.clone())
            .increment(rows);
        metrics::histogram!("tuile_tiles_register_read_seconds", "layer" => layer.name.clone())
            .record(began.elapsed().as_secs_f64());
        tracing::info!(
            layer = %layer.name,
            zones = zones.len(),
            ranges = ranges.len(),
            rows,
            batches,
            expired = skipped,
            seconds = millis as f64 / 1e3,
            "REGISTER-TILES"
        );
        if absences_here {
            let absent_layer = self
                .layer(&absent_layer)
                .map_err(|e| e.to_string())?
                .clone();
            for (zone, held) in absent {
                self.attach_index(
                    &absent_layer,
                    zone,
                    Arc::new(crate::index::ZoneIndex::from_tiles(held)),
                );
            }
        }
        let mut out = Vec::with_capacity(zones.len());
        for (i, (zone, held)) in tiles.into_iter().enumerate() {
            let n = held.len() as u64;
            self.attach_index(
                layer,
                zone,
                Arc::new(crate::index::ZoneIndex::from_tiles(held)),
            );
            metrics::counter!("tuile_tiles_zones_projected_total", "layer" => layer.name.clone(), "by" => "register").increment(1);
            out.push((
                zone,
                ProjectionReport {
                    zones_seen: 1,
                    zones_indexed: 1,
                    tiles_indexed: n,
                    tiles_listed: n,
                    zones_registered: 1,
                    tiles_registered: n,
                    // Said once, on the first zone, so that the sum is the
                    // layer's.
                    register_batches: if i == 0 { batches } else { 0 },
                    register_millis: if i == 0 { millis } else { 0 },
                    ..Default::default()
                },
            ));
        }
        Ok(out)
    }

    /// An archive's listing: from memory when this store listed it before
    /// (`false`), from its directories otherwise (`true`). `None`: the
    /// archive is gone.
    async fn listing_of(
        &self,
        key: &str,
    ) -> Result<Option<(Arc<archive::Listing>, bool)>, StoreError> {
        if let Some(held) = self
            .listings
            .lock()
            .ok()
            .and_then(|mut l| l.get(&key.to_string()))
        {
            return Ok(Some((held, false)));
        }
        let Some(listing) = archive::list_remote(self.object_store().as_ref(), key).await? else {
            return Ok(None);
        };
        let listing = Arc::new(listing);
        if let Ok(mut l) = self.listings.lock() {
            l.insert(
                key.to_string(),
                listing.clone(),
                listing.tiles.len() as u64 + 1,
            );
        }
        Ok(Some((listing, true)))
    }

    /// Held while the projection at `path` is being made or checked: a second
    /// bake of this process that wants the same zone waits for the first, and
    /// then finds it made.
    async fn projection_lock(&self, path: &FsPath) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self
            .projection_locks
            .lock()
            .map(|mut l| l.entry(path.to_path_buf()).or_default().clone())
            .unwrap_or_default();
        lock.lock_owned().await
    }

    /// The projection at `path`, if it was made from exactly the archives
    /// `source` names; touched, so that the eviction of the least recently
    /// used keeps it.
    async fn projection_made_from(
        &self,
        path: &FsPath,
        source: &str,
    ) -> Option<Arc<archive::LocalReader>> {
        let said = std::fs::read_to_string(projection_source_path(path)).ok()?;
        if said != source {
            return None;
        }
        let reader = archive::open_local(path).await.ok()?;
        let now = std::time::SystemTime::now();
        for p in [path.to_path_buf(), projection_source_path(path)] {
            if let Ok(f) = std::fs::File::options().write(true).open(&p) {
                let _ = f.set_modified(now);
            }
        }
        metrics::counter!("tuile_tiles_projections_reused_total").increment(1);
        Some(Arc::new(reader))
    }

    /// Where every tile of a zone lies, newest archive first: each live
    /// archive listed from its directories, and each tile id given to the
    /// newest archive that holds it, as a reader of the zone would see it.
    /// The archives' keys come with it, in the order of the manifest.
    pub async fn zone_listing(&self, layer: &Layer, zone: Zone) -> Result<ZoneListing, StoreError> {
        let now = self.now();
        let m = manifest::read(self.object_store().as_ref(), &layer.zone_prefix(zone))
            .await?
            .manifest;
        let archives: Vec<_> = m
            .archives
            .iter()
            .filter(|a| !layer.is_expired(&a.epoch, now))
            .cloned()
            .collect();
        let listed: Vec<Option<(Arc<archive::Listing>, bool)>> =
            futures_util::stream::iter(&archives)
                .map(|a| self.listing_of(&a.key))
                .buffered(ranges_in_flight())
                .try_collect()
                .await?;
        let mut out = ZoneListing {
            whole_bytes: archives.iter().map(|a| a.bytes).sum(),
            archives: archives.iter().map(|a| a.key.clone()).collect(),
            epoch: archives.last().map(|a| a.epoch.clone()).unwrap_or_default(),
            ..Default::default()
        };
        for (i, listing) in listed.into_iter().enumerate().rev() {
            // Vanished under us (expired, compacted): nothing of it to take.
            let Some((listing, fetched)) = listing else {
                continue;
            };
            if fetched {
                out.requests += listing.requests;
                out.bytes += listing.bytes;
            }
            out.listed += listing.tiles.len() as u64;
            for (id, range) in &listing.tiles {
                out.tiles.entry(*id).or_insert((i, range.clone()));
            }
        }
        Ok(out)
    }

    /// A zone of a targeted layer: listed from its archives' directories,
    /// and only the tiles the footprint keeps fetched, by coalesced ranges.
    async fn project_zone_targeted(
        &self,
        footprint: &Footprint,
        dir: &FsPath,
        layer_name: &str,
        zone: Zone,
    ) -> Result<ProjectionReport, StoreError> {
        let layer = self.layer(layer_name)?.clone();
        let factor = footprint.factor_for(layer_name);
        let path = projection_path(dir, &layer, zone);
        // Before the listing: a second bake that wants this zone waits, then
        // lists it from memory and finds its tiles projected.
        let _made = self.projection_lock(&path).await;
        let listing = self.zone_listing(&layer, zone).await?;
        let mut report = ProjectionReport {
            zones_seen: 1,
            tiles_listed: listing.listed,
            requests: listing.requests,
            bytes: listing.bytes,
            whole_bytes: listing.whole_bytes,
            ..Default::default()
        };
        let kept: Vec<(u64, usize, Range<u64>)> = listing
            .tiles
            .into_iter()
            .filter(|(id, _)| {
                layer.grid.from_id(*id).is_some_and(|(level, x, y)| {
                    footprint.keeps_with(factor, layer.grid, level, x, y)
                })
            })
            .map(|(id, (archive, range))| (id, archive, range))
            .collect();
        if kept.is_empty() {
            return Ok(report);
        }
        let source = source_of(&listing.archives);
        // Made before from the same archives: what it holds is not fetched
        // again; it is enough if it holds every tile kept now.
        let mut tiles: BTreeMap<u64, Bytes> = BTreeMap::new();
        if let Some(reader) = self.projection_made_from(&path, &source).await {
            let held = ids_of(&reader).await?;
            if kept.iter().all(|(id, _, _)| held.contains(id)) {
                report.tiles_kept = kept.len() as u64;
                report.zones_projected = 1;
                report.zones_reused = 1;
                self.attach_projection_reader(&layer, zone, reader);
                return Ok(report);
            }
            tiles = tiles_of(&reader).await?;
        }
        let kept: Vec<(u64, usize, Range<u64>)> = kept
            .into_iter()
            .filter(|(id, _, _)| !tiles.contains_key(id))
            .collect();

        // Archive by archive, the kept tiles' ranges merged where they lie
        // close, then fetched a few at once.
        let mut spans: BTreeMap<usize, Vec<(u64, u64)>> = BTreeMap::new();
        for (_, archive, range) in &kept {
            spans
                .entry(*archive)
                .or_default()
                .push((range.start, range.end - range.start));
        }
        let fetches: Vec<(usize, Range<u64>)> = spans
            .into_iter()
            .flat_map(|(archive, spans)| coalesce(spans).into_iter().map(move |r| (archive, r)))
            .collect();
        let keys = &listing.archives;
        let fetched: Vec<(usize, Range<u64>, Bytes)> = futures_util::stream::iter(fetches)
            .map(|(archive, range)| async move {
                let path = object_store::path::Path::from(keys[archive].as_str());
                match object_store::ObjectStoreExt::get_range(
                    self.object_store().as_ref(),
                    &path,
                    range.clone(),
                )
                .await
                {
                    Ok(bytes) => Ok(Some((archive, range, bytes))),
                    // Gone since it was listed (expired, merged): its tiles
                    // are left to the bucket, which knows where they went.
                    Err(object_store::Error::NotFound { .. }) => Ok(None),
                    Err(e) => Err(StoreError::from(e)),
                }
            })
            .buffer_unordered(ranges_in_flight())
            .try_filter_map(|f| async move { Ok(f) })
            .try_collect()
            .await?;
        let mut blobs: HashMap<usize, Vec<(Range<u64>, Bytes)>> = HashMap::new();
        for (archive, range, bytes) in fetched {
            report.requests += 1;
            report.bytes += bytes.len() as u64;
            blobs.entry(archive).or_default().push((range, bytes));
        }
        for (id, archive, range) in kept {
            let blob = blobs.get(&archive).and_then(|b| {
                b.iter()
                    .find(|(r, _)| r.start <= range.start && range.end <= r.end)
            });
            let Some((r, bytes)) = blob else { continue };
            let at = (range.start - r.start) as usize;
            let len = (range.end - range.start) as usize;
            if at + len > bytes.len() {
                return Err(StoreError::Corrupt(format!(
                    "{}: tile {id} runs past the end of the archive",
                    keys[archive]
                )));
            }
            tiles.insert(id, bytes.slice(at..at + len));
        }
        if tiles.is_empty() {
            return Ok(report);
        }
        report.tiles_kept = tiles.len() as u64;
        report.zones_projected = 1;
        self.persist_projection(dir, &layer, zone, &listing.epoch, tiles, &source)
            .await?;
        Ok(report)
    }

    /// Writes `tiles` as the zone's projection under `dir`, and reads through
    /// it from now on.
    async fn persist_projection(
        &self,
        dir: &FsPath,
        layer: &Layer,
        zone: Zone,
        epoch: &str,
        tiles: BTreeMap<u64, Bytes>,
        source: &str,
    ) -> Result<(), StoreError> {
        let path = projection_path(dir, layer, zone);
        let near = path.parent().unwrap_or(dir).to_path_buf();
        std::fs::create_dir_all(&near)?;
        // No longer said to be made from anything until it is in place.
        let said = projection_source_path(&path);
        let _ = std::fs::remove_file(&said);
        let zooms = archive::zoom_range(tiles.keys());
        let (l, epoch, at) = (layer.clone(), epoch.to_string(), near.clone());
        // Written beside where it goes: a rename does not leave its file
        // system, and the projections may have a volume of their own.
        let file = tokio::task::spawn_blocking(move || {
            archive::write_in(&at, &l, &epoch, zooms, tiles.into_iter().map(Ok))
        })
        .await
        .map_err(|e| StoreError::Corrupt(format!("projection writer: {e}")))??
        .0;
        file.persist(&path).map_err(|e| StoreError::Io(e.error))?;
        let mut note = tempfile::NamedTempFile::new_in(&near)?;
        std::io::Write::write_all(&mut note, source.as_bytes())?;
        note.persist(&said).map_err(|e| StoreError::Io(e.error))?;
        self.attach_projection(layer, zone, &path).await
    }

    async fn project_zone(
        &self,
        dir: &FsPath,
        layer_name: &str,
        zone: Zone,
    ) -> Result<ProjectionReport, StoreError> {
        let layer = self.layer(layer_name)?.clone();
        let mut report = ProjectionReport {
            zones_seen: 1,
            ..Default::default()
        };
        let _made = self
            .projection_lock(&projection_path(dir, &layer, zone))
            .await;
        let now = self.now();
        let m = manifest::read(self.object_store().as_ref(), &layer.zone_prefix(zone))
            .await?
            .manifest;
        let archives: Vec<_> = m
            .archives
            .iter()
            .filter(|a| !layer.is_expired(&a.epoch, now))
            .cloned()
            .collect();
        // Said whole: a projection of only some tiles of the same archives
        // is not this one.
        let source = format!(
            "whole\n{}",
            source_of(&archives.iter().map(|a| a.key.clone()).collect::<Vec<_>>())
        );
        if let Some(reader) = self
            .projection_made_from(&projection_path(dir, &layer, zone), &source)
            .await
        {
            report.zones_projected = 1;
            report.zones_reused = 1;
            self.attach_projection_reader(&layer, zone, reader);
            return Ok(report);
        }

        // Newest first: the first archive to hold a tile wins it. Each archive
        // comes down in one request, then is read locally.
        // The archives come down beside the projections, on their volume.
        std::fs::create_dir_all(dir)?;
        let scratch = tempfile::tempdir_in(dir)?;
        let mut tiles: BTreeMap<u64, Bytes> = BTreeMap::new();
        for (i, a) in archives.iter().enumerate().rev() {
            let local = scratch.path().join(format!("{i}.pmtiles"));
            match download(self.object_store().as_ref(), &a.key, &local).await {
                Ok(()) => {}
                // Vanished under us (expired, compacted): nothing to project.
                Err(StoreError::ObjectStore(object_store::Error::NotFound { .. })) => continue,
                Err(e) => return Err(e),
            }
            report.requests += 1;
            report.bytes += std::fs::metadata(&local)?.len();
            let reader = Arc::new(archive::open_local(&local).await?);
            let mut entries = reader.clone().entries();
            while let Some(entry) = entries.try_next().await? {
                for tid in entry.iter_coords() {
                    let id = tid.value();
                    report.tiles_listed += 1;
                    if tiles.contains_key(&id) {
                        continue;
                    }
                    // A zone the scene reaches is kept whole: its archive came
                    // down in one request anyway, and a tile left out is a
                    // round trip later. That includes `top`: a bake pins a
                    // coarse pyramid of the WHOLE globe as its floor, and
                    // filtering `top` by distance sent 21 800 of its reads back
                    // to the bucket while saving no transfer at all.
                    if let Some(bytes) = reader.get_tile(tid).await? {
                        tiles.insert(id, bytes);
                    }
                }
            }
        }
        if tiles.is_empty() {
            return Ok(report);
        }
        report.tiles_kept = tiles.len() as u64;
        report.zones_projected = 1;
        let epoch = archives.last().map(|a| a.epoch.clone()).unwrap_or_default();
        self.persist_projection(dir, &layer, zone, &epoch, tiles, &source)
            .await?;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_mercator_bounds_are_the_usual_ones() {
        let [w, s, e, n] = bounds(Grid::WebMercator, 0, 0, 0);
        assert_eq!((w, e), (-180.0, 180.0));
        assert!((n - 85.051_128).abs() < 1e-5 && (s + 85.051_128).abs() < 1e-5);
        let [w, _, e, _] = bounds(Grid::WebMercator, 1, 1, 0);
        assert_eq!((w, e), (0.0, 180.0));
    }

    #[test]
    fn geographic_bounds_count_from_the_south_west() {
        assert_eq!(bounds(Grid::Geographic, 0, 1, 0), [0.0, -90.0, 180.0, 90.0]);
        assert_eq!(
            bounds(Grid::Geographic, 1, 0, 1),
            [-180.0, 0.0, -90.0, 90.0]
        );
    }

    #[test]
    fn nearby_ranges_are_fetched_together_and_far_ones_apart() {
        let r = coalesce(vec![(0, 10), (10, 10), (100, 5), (10_000_000, 10)]);
        assert_eq!(r, vec![0..105, 10_000_000..10_000_010]);
        let far = coalesce(vec![(0, 10), (COALESCE_GAP + 11, 10)]);
        assert_eq!(far.len(), 2);
        // Identical tiles share one range: fetched once.
        assert_eq!(coalesce(vec![(5, 10), (5, 10)]), vec![5..15]);
        // No merged range outgrows the cap.
        let long = coalesce((0..64).map(|i| (i * (1 << 20), 1 << 20)).collect());
        assert!(
            long.iter().all(|r| r.end - r.start <= MAX_RANGE),
            "{long:?}"
        );
        assert_eq!(long.len(), 16);
    }

    #[test]
    fn a_camera_thinned_away_loses_no_tile() {
        // Two eyes 400 m apart at the same height: the second is dropped.
        let a = Eye {
            lon: 0.6,
            lat: 42.8,
            height: 5_000.0,
        };
        let b = Eye {
            lon: 0.6 + 400.0 / (M_PER_DEG * 42.8f64.to_radians().cos()),
            ..a
        };
        let both = Footprint::from_eyes([a, b], 2.0);
        assert_eq!(both.eyes().len(), 1, "the nearer eye is thinned away");
        // A z16 tile (~300 m) about 1 km east of b: within two widths and
        // the slack of b, beyond them of a.
        let n = f64::from(1u32 << 16);
        let lat = 42.8f64.to_radians();
        let lon = b.lon + 1_000.0 / (M_PER_DEG * lat.cos());
        let x = ((lon + 180.0) / 360.0 * n) as u32;
        let y =
            ((1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n) as u32;
        let only_b = Footprint::from_eyes([b], 2.0);
        assert!(only_b.keeps(Grid::WebMercator, 16, x, y), "b keeps it");
        assert!(
            both.keeps(Grid::WebMercator, 16, x, y),
            "and so does a footprint that dropped b"
        );
    }

    #[test]
    fn fine_tiles_are_kept_only_near_the_eye() {
        // An eye 1 km above the Pyrenees near Luchon.
        let fp = Footprint::from_eyes(
            [Eye {
                lon: 0.6,
                lat: 42.8,
                height: 5_800.0,
            }],
            DEFAULT_TILE_FACTOR,
        );
        let tile = |z: u8, lon: f64, lat: f64| {
            let n = f64::from(1u32 << z);
            let x = ((lon + 180.0) / 360.0 * n) as u32;
            let y = ((1.0
                - (lat.to_radians().tan() + 1.0 / lat.to_radians().cos()).ln()
                    / std::f64::consts::PI)
                / 2.0
                * n) as u32;
            (x, y)
        };
        let (x, y) = tile(17, 0.6, 42.8);
        assert!(
            fp.keeps(Grid::WebMercator, 17, x, y),
            "the tile under the eye"
        );
        let (x, y) = tile(17, 1.6, 42.8);
        assert!(
            !fp.keeps(Grid::WebMercator, 17, x, y),
            "a z17 tile 80 km away"
        );
        let (x, y) = tile(10, 1.6, 42.8);
        assert!(
            fp.keeps(Grid::WebMercator, 10, x, y),
            "a z10 tile 80 km away"
        );
        assert!(
            fp.keeps(Grid::WebMercator, 0, 0, 0),
            "the whole world at z0"
        );
    }

    #[test]
    fn a_layer_can_have_its_own_factor_and_its_absences_follow_it() {
        let fp = Footprint::from_eyes(
            [Eye {
                lon: 0.6,
                lat: 42.8,
                height: 5_800.0,
            }],
            12.0,
        )
        .with_layer_factor("imagery", 96.0);
        assert_eq!(fp.factor_for("terrain"), 12.0);
        assert_eq!(fp.factor_for("imagery"), 96.0);
        assert_eq!(fp.factor_for("imagery.absent"), 96.0);
        // A z17 tile ~20 km away: out at 12 widths (~2.4 km each), in at 96.
        let n = f64::from(1u32 << 17);
        let lat = 42.8f64.to_radians();
        let x = ((0.85 + 180.0) / 360.0 * n) as u32;
        let y =
            ((1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n) as u32;
        assert!(!fp.keeps_with(fp.factor_for("terrain"), Grid::WebMercator, 17, x, y));
        assert!(fp.keeps_with(fp.factor_for("imagery"), Grid::WebMercator, 17, x, y));
    }

    #[test]
    fn a_high_eye_does_not_keep_the_finest_tiles_under_it() {
        let fp = Footprint::from_eyes(
            [Eye {
                lon: 0.6,
                lat: 42.8,
                height: 50_000.0,
            }],
            DEFAULT_TILE_FACTOR,
        );
        let n = f64::from(1u32 << 19);
        let x = ((0.6 + 180.0) / 360.0 * n) as u32;
        let lat = 42.8f64.to_radians();
        let y =
            ((1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n) as u32;
        assert!(
            !fp.keeps(Grid::WebMercator, 19, x, y),
            "a z19 tile is 45 km below the eye"
        );
    }
}
