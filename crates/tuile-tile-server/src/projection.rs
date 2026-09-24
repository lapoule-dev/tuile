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
//! projection pays for, not the bytes. For the same reason a cell zone the
//! scene reaches is projected whole; only `top` — every scene's coarse tiles,
//! and growing with each — is filtered tile by tile.
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

use std::collections::BTreeMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};
use pmtiles::TileCoord;

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
/// Default for [`Footprint::tile_factor`]. At a screen-space error of 3 on a
/// 2880-pixel-high viewport a tile stops being selected beyond about 6.4 of
/// its widths; twelve leaves a margin the fallback counters confirm.
pub const DEFAULT_TILE_FACTOR: f64 = 12.0;
/// Camera positions closer than this to one already kept add nothing.
const EYE_SPACING_M: f64 = 500.0;
/// Zones projected at once.
const ZONES_IN_FLIGHT: usize = 16;

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
}

/// A tile's extent in degrees: west, south, east, north.
pub fn bounds(grid: Grid, level: u8, x: u32, y: u32) -> [f64; 4] {
    match grid {
        Grid::WebMercator => {
            let n = f64::from(1u32 << level.min(31));
            let lon = |x: f64| x / n * 360.0 - 180.0;
            let lat = |y: f64| (std::f64::consts::PI * (1.0 - 2.0 * y / n)).sinh().atan().to_degrees();
            [lon(f64::from(x)), lat(f64::from(y) + 1.0), lon(f64::from(x) + 1.0), lat(f64::from(y))]
        }
        // Geographic tiles count from the south-west, 2 × 1 at level 0.
        Grid::Geographic => {
            let size = 180.0 / f64::from(1u32 << level.min(30));
            let (w, s) = (-180.0 + f64::from(x) * size, -90.0 + f64::from(y) * size);
            [w, s, w + size, s + size]
        }
    }
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
                (k.height - e.height).abs() < EYE_SPACING_M && distance(k.lon, k.lat, e.lon, e.lat) < EYE_SPACING_M
            });
            if !near {
                kept.push(e);
            }
        }
        Self { eyes: kept, tile_factor, layer_factors: Vec::new() }
    }

    /// Uses `factor` for the layer `name` (and its absence sibling).
    pub fn with_layer_factor(mut self, name: impl Into<String>, factor: f64) -> Self {
        self.layer_factors.push((name.into(), factor));
        self
    }

    /// The factor a layer is filtered with.
    pub fn factor_for(&self, layer: &str) -> f64 {
        let base = layer.strip_suffix(crate::catalog::ABSENT_SUFFIX).unwrap_or(layer);
        self.layer_factors.iter().find(|(n, _)| n == base).map_or(self.tile_factor, |(_, f)| *f)
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
        let mid_lat = ((s + n) / 2.0).to_radians();
        let width = ((e - w) * M_PER_DEG * mid_lat.cos()).max((n - s) * M_PER_DEG);
        let reach = factor * width;
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
    /// within the horizon of an eye.
    fn reaches(&self, layer: &Layer, zone: Zone) -> bool {
        let Zone::Cell { x, y } = zone else { return true };
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
    /// Bytes fetched, and requests made: one per archive.
    pub bytes: u64,
    pub requests: u64,
    pub millis: u64,
}

/// Where a zone's projection lives under `dir`.
pub fn projection_path(dir: &FsPath, layer: &Layer, zone: Zone) -> PathBuf {
    dir.join(format!("{}.pmtiles", layer.zone_prefix(zone)))
}

impl TileStore {
    /// Projects every remote zone the footprint reaches into `dir`, and reads
    /// through those projections from now on.
    pub async fn project(&self, footprint: &Footprint, dir: &FsPath) -> Result<ProjectionReport, StoreError> {
        let began = Instant::now();
        let zones: Vec<(String, Zone)> = self
            .zones()
            .await?
            .into_iter()
            .filter(|(l, z)| self.layer(l).map(|l| footprint.reaches(l, *z)).unwrap_or(false))
            .collect();
        let results: Vec<Result<ProjectionReport, StoreError>> = futures_util::stream::iter(zones)
            .map(|(layer, zone)| async move { self.project_zone(footprint, dir, &layer, zone).await })
            .buffer_unordered(ZONES_IN_FLIGHT)
            .collect()
            .await;
        let mut total = ProjectionReport::default();
        for r in results {
            let r = r?;
            total.zones_seen += r.zones_seen;
            total.zones_projected += r.zones_projected;
            total.tiles_listed += r.tiles_listed;
            total.tiles_kept += r.tiles_kept;
            total.bytes += r.bytes;
            total.requests += r.requests;
        }
        total.millis = began.elapsed().as_millis() as u64;
        Ok(total)
    }

    async fn project_zone(
        &self,
        footprint: &Footprint,
        dir: &FsPath,
        layer_name: &str,
        zone: Zone,
    ) -> Result<ProjectionReport, StoreError> {
        let layer = self.layer(layer_name)?.clone();
        let mut report = ProjectionReport { zones_seen: 1, ..Default::default() };
        let now = self.now();
        let m = manifest::read(self.object_store().as_ref(), &layer.zone_prefix(zone)).await?.manifest;
        let archives: Vec<_> = m.archives.iter().filter(|a| !layer.is_expired(&a.epoch, now)).cloned().collect();

        // Newest first: the first archive to hold a tile wins it. Each archive
        // comes down in one request, then is read locally.
        let factor = footprint.factor_for(&layer.name);
        let scratch = tempfile::tempdir()?;
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
                    // A cell zone the scene reaches is kept whole: its archive
                    // came down in one request anyway, and a tile left out is
                    // a round trip later. Only `top`, shared by every scene
                    // and growing with each, is filtered tile by tile.
                    if zone == Zone::Top {
                        let Some((level, x, y)) = layer.grid.from_archive(TileCoord::from(tid)) else { continue };
                        if !footprint.keeps_with(factor, layer.grid, level, x, y) {
                            continue;
                        }
                    }
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

        let path = projection_path(dir, &layer, zone);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let epoch = archives.last().map(|a| a.epoch.clone()).unwrap_or_default();
        let zooms = archive::zoom_range(tiles.keys());
        let l = layer.clone();
        let file = tokio::task::spawn_blocking(move || archive::write(&l, &epoch, zooms, tiles.into_iter().map(Ok)))
            .await
            .map_err(|e| StoreError::Corrupt(format!("projection writer: {e}")))??
            .0;
        file.persist(&path).map_err(|e| StoreError::Io(e.error))?;
        self.attach_projection(&layer, zone, &path).await?;
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
        assert_eq!(bounds(Grid::Geographic, 1, 0, 1), [-180.0, 0.0, -90.0, 90.0]);
    }

    #[test]
    fn fine_tiles_are_kept_only_near_the_eye() {
        // An eye 1 km above the Pyrenees near Luchon.
        let fp = Footprint::from_eyes([Eye { lon: 0.6, lat: 42.8, height: 5_800.0 }], DEFAULT_TILE_FACTOR);
        let tile = |z: u8, lon: f64, lat: f64| {
            let n = f64::from(1u32 << z);
            let x = ((lon + 180.0) / 360.0 * n) as u32;
            let y = ((1.0 - (lat.to_radians().tan() + 1.0 / lat.to_radians().cos()).ln() / std::f64::consts::PI) / 2.0 * n) as u32;
            (x, y)
        };
        let (x, y) = tile(17, 0.6, 42.8);
        assert!(fp.keeps(Grid::WebMercator, 17, x, y), "the tile under the eye");
        let (x, y) = tile(17, 1.6, 42.8);
        assert!(!fp.keeps(Grid::WebMercator, 17, x, y), "a z17 tile 80 km away");
        let (x, y) = tile(10, 1.6, 42.8);
        assert!(fp.keeps(Grid::WebMercator, 10, x, y), "a z10 tile 80 km away");
        assert!(fp.keeps(Grid::WebMercator, 0, 0, 0), "the whole world at z0");
    }

    #[test]
    fn a_layer_can_have_its_own_factor_and_its_absences_follow_it() {
        let fp = Footprint::from_eyes([Eye { lon: 0.6, lat: 42.8, height: 5_800.0 }], 12.0)
            .with_layer_factor("imagery", 96.0);
        assert_eq!(fp.factor_for("terrain"), 12.0);
        assert_eq!(fp.factor_for("imagery"), 96.0);
        assert_eq!(fp.factor_for("imagery.absent"), 96.0);
        // A z17 tile ~20 km away: out at 12 widths (~2.4 km each), in at 96.
        let n = f64::from(1u32 << 17);
        let lat = 42.8f64.to_radians();
        let x = ((0.85 + 180.0) / 360.0 * n) as u32;
        let y = ((1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n) as u32;
        assert!(!fp.keeps_with(fp.factor_for("terrain"), Grid::WebMercator, 17, x, y));
        assert!(fp.keeps_with(fp.factor_for("imagery"), Grid::WebMercator, 17, x, y));
    }

    #[test]
    fn a_high_eye_does_not_keep_the_finest_tiles_under_it() {
        let fp = Footprint::from_eyes([Eye { lon: 0.6, lat: 42.8, height: 50_000.0 }], DEFAULT_TILE_FACTOR);
        let n = f64::from(1u32 << 19);
        let x = ((0.6 + 180.0) / 360.0 * n) as u32;
        let lat = 42.8f64.to_radians();
        let y = ((1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n) as u32;
        assert!(!fp.keeps(Grid::WebMercator, 19, x, y), "a z19 tile is 45 km below the eye");
    }
}
