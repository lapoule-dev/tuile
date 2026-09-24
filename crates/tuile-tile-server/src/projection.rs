// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Projections: a scene's slice of the store, as local archives.
//!
//! A bake reads the same ground hundreds of times, frame after frame. Reading
//! it from the bucket costs several round trips per tile; reading it from a
//! local file costs nothing. So before the first frame, every remote zone the
//! scene can see is **projected**: its directories are read, the entries the
//! scene can use are kept, their bytes are fetched by coalesced ranges (tiles
//! are in Hilbert order, so neighbours on the ground are neighbours in the
//! file), and one local PMTiles archive per zone is written. The `top` zone —
//! the coarse tiles every flight shares — is filtered like any other.
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
use object_store::path::Path;
use object_store::ObjectStore;
use pmtiles::TileCoord;

use crate::archive;
use crate::grid::Grid;
use crate::layer::{Layer, Zone};
use crate::manifest;
use crate::store::TileStore;
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
/// Two tile ranges this close are fetched as one request.
const COALESCE_GAP: u64 = 64 * 1024;
/// …as long as the request stays under this size.
const MAX_RANGE: u64 = 16 * 1024 * 1024;
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
        Self { eyes: kept, tile_factor }
    }

    pub fn eyes(&self) -> &[Eye] {
        &self.eyes
    }

    /// Whether some eye can use this tile.
    pub fn keeps(&self, grid: Grid, level: u8, x: u32, y: u32) -> bool {
        let [w, s, e, n] = bounds(grid, level, x, y);
        let mid_lat = ((s + n) / 2.0).to_radians();
        let width = ((e - w) * M_PER_DEG * mid_lat.cos()).max((n - s) * M_PER_DEG);
        let reach = self.tile_factor * width;
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
    /// Bytes fetched, and range requests made.
    pub bytes: u64,
    pub requests: u64,
    pub millis: u64,
}

/// A kept tile: which archive, where in it.
#[derive(Clone, Copy)]
struct Located {
    archive: usize,
    start: u64,
    length: u64,
}

fn coalesce(mut spans: Vec<(u64, u64)>) -> Vec<Range<u64>> {
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

        // Newest first: the first archive to list a tile wins it.
        let mut chosen: HashMap<u64, Located> = HashMap::new();
        for (i, a) in archives.iter().enumerate().rev() {
            let reader = match archive::open_remote(self.object_store().clone(), &a.key).await {
                Ok(r) => Arc::new(r),
                // Vanished under us (expired, compacted): nothing to project.
                Err(_) => continue,
            };
            let data_offset = reader.get_header().data_offset();
            let mut entries = reader.clone().entries();
            while let Some(entry) = entries.try_next().await? {
                for tid in entry.iter_coords() {
                    let id = tid.value();
                    report.tiles_listed += 1;
                    if chosen.contains_key(&id) {
                        continue;
                    }
                    let Some((level, x, y)) = layer.grid.from_archive(TileCoord::from(tid)) else { continue };
                    if footprint.keeps(layer.grid, level, x, y) {
                        chosen.insert(
                            id,
                            Located { archive: i, start: data_offset + entry.offset(), length: u64::from(entry.length()) },
                        );
                    }
                }
            }
        }
        if chosen.is_empty() {
            return Ok(report);
        }

        // Fetch by coalesced ranges, archive by archive.
        let mut fetched: HashMap<(usize, u64), Bytes> = HashMap::new();
        let mut by_archive: BTreeMap<usize, Vec<(u64, u64)>> = BTreeMap::new();
        for l in chosen.values() {
            by_archive.entry(l.archive).or_default().push((l.start, l.length));
        }
        for (i, spans) in by_archive {
            let ranges = coalesce(spans.clone());
            let path = Path::from(archives[i].key.as_str());
            let blobs = self.object_store().get_ranges(&path, &ranges).await?;
            report.requests += ranges.len() as u64;
            for (range, blob) in ranges.iter().zip(blobs) {
                report.bytes += blob.len() as u64;
                for &(start, length) in spans.iter().filter(|(s, _)| range.contains(s)) {
                    let at = (start - range.start) as usize;
                    fetched.insert((i, start), blob.slice(at..at + length as usize));
                }
            }
        }
        let tiles: BTreeMap<u64, Bytes> = chosen
            .iter()
            .filter_map(|(id, l)| fetched.get(&(l.archive, l.start)).map(|b| (*id, b.clone())))
            .collect();
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
    fn nearby_ranges_are_fetched_together_and_far_ones_apart() {
        let r = coalesce(vec![(0, 10), (10, 10), (100, 5), (10_000_000, 10)]);
        assert_eq!(r, vec![0..105, 10_000_000..10_000_010]);
        let far = coalesce(vec![(0, 10), (COALESCE_GAP + 11, 10)]);
        assert_eq!(far.len(), 2);
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
    fn a_high_eye_does_not_keep_the_finest_tiles_under_it() {
        let fp = Footprint::from_eyes([Eye { lon: 0.6, lat: 42.8, height: 50_000.0 }], DEFAULT_TILE_FACTOR);
        let n = f64::from(1u32 << 19);
        let x = ((0.6 + 180.0) / 360.0 * n) as u32;
        let lat = 42.8f64.to_radians();
        let y = ((1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n) as u32;
        assert!(!fp.keeps(Grid::WebMercator, 19, x, y), "a z19 tile is 45 km below the eye");
    }
}
