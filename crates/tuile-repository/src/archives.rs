// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The tile store, read through [`Objects`] and nothing else.
//!
//! The tile store is written by `tuile-tile-server`, which is native: it
//! buffers, publishes and compacts, over an S3 client. Reading it needs none
//! of that — only where things are — so this is the reading half alone, in
//! terms a Worker has: ranged reads of objects.
//!
//! What is where, as the store writes it:
//!
//! - `catalog.json` names the layers: grid, zone level, expiry, media type.
//!   Each layer has a shadow, `<name>.absent`, for the tiles its source said
//!   it does not have;
//! - a layer's tiles are split into **zones**: `<layer>/top` for levels above
//!   the zone level, `<layer>/zones/z<level>/<x>/<y>` below, the zone being
//!   the tile's ancestor at that level;
//! - a zone's `manifest.json` lists its archives, oldest first. A tile is
//!   looked for in the newest first, skipping any whose epoch has expired;
//! - an archive is a PMTiles v3 file: a header, a root directory, leaf
//!   directories, tile bytes. A tile is addressed by its Hilbert id, which
//!   for the geographic grid is taken one level down (two root columns).
//!
//! The contract test reads one store both ways — through the native store
//! that wrote it and through this — and asks for the same answers.

use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use pmtiles::{Compression, Directory, Header, TileCoord, TileId, MAX_ZOOM};

use crate::{LayerInfo, Objects, RepoError, Tile, TileRepository};

const CATALOG_KEY: &str = "catalog.json";
const ABSENT_SUFFIX: &str = ".absent";
/// What a first read of an archive asks for: its header and, nearly always,
/// its whole root directory.
const INITIAL_BYTES: u64 = 16_384;
const HEADER_BYTES: usize = 127;
/// A root, then at most this many leaves down to a tile.
const MAX_DEPTH: usize = 4;
/// Opened archives kept: a header and a root directory each.
const OPENED: usize = 64;
const SECONDS_PER_DAY: u64 = 24 * 3600;

/// The time, in seconds since the Unix epoch. Handed in because a Worker
/// has no system clock to ask.
pub type Now = Arc<dyn Fn() -> u64 + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Grid {
    WebMercator,
    Geographic,
}

impl Grid {
    fn name(self) -> &'static str {
        match self {
            Grid::WebMercator => "web-mercator",
            Grid::Geographic => "geographic",
        }
    }

    fn max_level(self) -> u8 {
        match self {
            Grid::WebMercator => MAX_ZOOM,
            Grid::Geographic => MAX_ZOOM - 1,
        }
    }

    /// The tile's id in an archive, or `None` when the grid has no such tile.
    fn id(self, level: u8, x: u32, y: u32) -> Option<u64> {
        if level > self.max_level() {
            return None;
        }
        let n = 1u64 << level;
        let (width, z) = match self {
            Grid::WebMercator => (n, level),
            // Two columns at level 0: a square pyramid one level down.
            Grid::Geographic => (2 * n, level + 1),
        };
        if u64::from(x) >= width || u64::from(y) >= n {
            return None;
        }
        Some(TileId::from(TileCoord::new(z, x, y).ok()?).value())
    }
}

#[derive(serde::Deserialize)]
struct Catalog {
    layers: Vec<LayerDef>,
}

#[derive(serde::Deserialize)]
struct LayerDef {
    name: String,
    grid: Grid,
    zone_level: u8,
    #[serde(default)]
    expiry_days: Option<u64>,
    content_type: String,
}

struct Layer {
    name: String,
    grid: Grid,
    zone_level: u8,
    /// After how long an epoch's tiles are no longer served. `None`: never.
    expiry: Option<u64>,
    content_type: String,
}

impl Layer {
    fn zone_prefix(&self, level: u8, x: u32, y: u32) -> String {
        if level < self.zone_level {
            return format!("{}/top", self.name);
        }
        let shift = level - self.zone_level;
        format!(
            "{}/zones/z{}/{}/{}",
            self.name,
            self.zone_level,
            x >> shift,
            y >> shift
        )
    }

    /// Whether every tile of `epoch` is past its expiry — measured from the
    /// end of the epoch, so that none is dropped before its full lifetime.
    fn is_expired(&self, epoch: &str, now: u64) -> bool {
        match (self.expiry, epoch_end(epoch)) {
            (Some(expiry), Some(end)) => now.saturating_sub(end) > expiry && now > end,
            _ => false,
        }
    }
}

/// The end of a monthly epoch, `YYYYMM`: the first second of the next month.
fn epoch_end(epoch: &str) -> Option<u64> {
    if epoch.len() != 6 || !epoch.is_ascii() {
        return None;
    }
    let year: i64 = epoch[..4].parse().ok()?;
    let month: i64 = epoch[4..].parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    let (year, month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    u64::try_from(days_from_civil(year, month, 1) * SECONDS_PER_DAY as i64).ok()
}

/// Days from 1970-01-01 to a date of the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    // Years run March to February, so the leap day is a year's last.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y.rem_euclid(400);
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[derive(serde::Deserialize)]
struct Manifest {
    archives: Vec<ArchiveRef>,
}

#[derive(serde::Deserialize)]
struct ArchiveRef {
    key: String,
    epoch: String,
}

/// An archive's header and root directory: what every lookup in it starts
/// from, read once.
struct Opened {
    header: Header,
    root: Directory,
}

/// The tile store, as a [`TileRepository`] over any [`Objects`].
///
/// It is handed the store twice, because the store holds two kinds of
/// object. The catalog and the manifests are rewritten as tiles are added:
/// they are read through `live`, which must answer with what is there now.
/// An archive never changes once written: it is read, by ranges, through
/// `archives`, which may be a cache that keeps what it has read for good.
pub struct ArchivedTiles {
    live: Arc<dyn Objects>,
    archives: Arc<dyn Objects>,
    layers: Vec<Layer>,
    now: Now,
    opened: Mutex<HashMap<String, Arc<Opened>>>,
}

fn malformed(key: &str, what: impl std::fmt::Display) -> RepoError {
    RepoError::Malformed {
        key: key.to_string(),
        what: what.to_string(),
    }
}

impl ArchivedTiles {
    /// Reads the store's catalog. A store without one is not a tile store.
    pub async fn open(
        live: Arc<dyn Objects>,
        archives: Arc<dyn Objects>,
        now: Now,
    ) -> Result<Self, RepoError> {
        let bytes = live.read_all(CATALOG_KEY).await?;
        let catalog: Catalog =
            serde_json::from_slice(&bytes).map_err(|e| malformed(CATALOG_KEY, e))?;
        let mut layers = Vec::new();
        for def in catalog.layers {
            if def.name.ends_with(ABSENT_SUFFIX) {
                return Err(malformed(
                    CATALOG_KEY,
                    format!("{} uses a reserved suffix", def.name),
                ));
            }
            let expiry = def.expiry_days.map(|d| d * SECONDS_PER_DAY);
            layers.push(Layer {
                name: format!("{}{ABSENT_SUFFIX}", def.name),
                grid: def.grid,
                zone_level: def.zone_level,
                expiry,
                content_type: "application/octet-stream".into(),
            });
            layers.push(Layer {
                name: def.name,
                grid: def.grid,
                zone_level: def.zone_level,
                expiry,
                content_type: def.content_type,
            });
        }
        Ok(Self {
            live,
            archives,
            layers,
            now,
            opened: Mutex::new(HashMap::new()),
        })
    }

    async fn manifest(&self, zone_prefix: &str) -> Result<Option<Manifest>, RepoError> {
        let key = format!("{zone_prefix}/manifest.json");
        match self.live.read_all(&key).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| malformed(&key, e)),
            // A zone nothing was ever written to.
            Err(RepoError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn directory(
        &self,
        key: &str,
        header: &Header,
        offset: u64,
        length: u64,
    ) -> Result<Directory, RepoError> {
        let bytes = self.archives.read(key, offset..offset + length).await?;
        parse_directory(key, header, bytes)
    }

    async fn opened(&self, key: &str) -> Result<Arc<Opened>, RepoError> {
        if let Some(held) = self.opened.lock().ok().and_then(|o| o.get(key).cloned()) {
            return Ok(held);
        }
        let size = self.archives.size(key).await?;
        let mut initial = self.archives.read(key, 0..size.min(INITIAL_BYTES)).await?;
        if initial.len() < HEADER_BYTES {
            return Err(malformed(key, "shorter than a PMTiles header"));
        }
        let header = Header::try_from_bytes(Bytes::copy_from_slice(&initial[..HEADER_BYTES]))
            .map_err(|e| malformed(key, e))?;
        let (start, length) = (header.root_offset(), header.root_length());
        let end = start + length;
        let root = if end <= initial.len() as u64 {
            let bytes = initial.drain(start as usize..end as usize).collect();
            parse_directory(key, &header, bytes)?
        } else {
            self.directory(key, &header, start, length).await?
        };
        let opened = Arc::new(Opened { header, root });
        if let Ok(mut held) = self.opened.lock() {
            if held.len() >= OPENED {
                // Archives are immutable: forgetting one costs only a header
                // read the next time.
                held.clear();
            }
            held.insert(key.to_string(), opened.clone());
        }
        Ok(opened)
    }

    /// The stored bytes of tile `id` in one archive, or `None` when the
    /// archive does not hold it.
    async fn read_tile(&self, key: &str, id: u64) -> Result<Option<Vec<u8>>, RepoError> {
        let opened = self.opened(key).await?;
        let header = &opened.header;
        let id = TileId::new(id).map_err(|e| malformed(key, e))?;
        let mut leaf: Option<Directory> = None;
        for _ in 0..MAX_DEPTH {
            let directory = leaf.as_ref().unwrap_or(&opened.root);
            let Some(entry) = directory.find_tile_id(id) else {
                return Ok(None);
            };
            let (offset, length) = (entry.offset(), u64::from(entry.length()));
            if !entry.points_to_leaf() {
                let start = header.data_offset() + offset;
                return Ok(Some(self.archives.read(key, start..start + length).await?));
            }
            let start = header.leaf_offset() + offset;
            leaf = Some(self.directory(key, header, start, length).await?);
        }
        Err(malformed(key, "directories nested too deep"))
    }
}

/// A directory from its bytes as stored: compressed as the header says.
fn parse_directory(key: &str, header: &Header, bytes: Vec<u8>) -> Result<Directory, RepoError> {
    let plain = match header.internal_compression() {
        Compression::None => bytes,
        Compression::Gzip => {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(bytes.as_slice())
                .read_to_end(&mut out)
                .map_err(|e| malformed(key, format!("directory: {e}")))?;
            out
        }
        other => {
            return Err(malformed(
                key,
                format!("directories compressed with {other:?} are not read here"),
            ))
        }
    };
    Directory::try_from(Bytes::from(plain)).map_err(|e| malformed(key, e))
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl TileRepository for ArchivedTiles {
    fn layers(&self) -> Vec<LayerInfo> {
        self.layers
            .iter()
            .map(|l| LayerInfo {
                name: l.name.clone(),
                grid: l.grid.name().to_string(),
                max_level: l.grid.max_level(),
                content_type: l.content_type.clone(),
            })
            .collect()
    }

    async fn tile(
        &self,
        layer: &str,
        level: u8,
        x: u32,
        y: u32,
    ) -> Result<Option<Tile>, RepoError> {
        let Some(l) = self.layers.iter().find(|l| l.name == layer) else {
            return Err(RepoError::NotFound(format!("layer {layer}")));
        };
        let Some(id) = l.grid.id(level, x, y) else {
            return Err(RepoError::NotFound(format!(
                "{} has no tile {level}/{x}/{y}",
                l.grid.name()
            )));
        };
        let Some(manifest) = self.manifest(&l.zone_prefix(level, x, y)).await? else {
            return Ok(None);
        };
        let now = (self.now)();
        for archive in manifest.archives.iter().rev() {
            if l.is_expired(&archive.epoch, now) {
                continue;
            }
            match self.read_tile(&archive.key, id).await {
                Ok(Some(bytes)) => {
                    return Ok(Some(Tile {
                        bytes,
                        content_type: l.content_type.clone(),
                    }))
                }
                // Not in this archive, or the archive was compacted away
                // under a manifest read a moment too early: an older one may
                // still hold the tile.
                Ok(None) | Err(RepoError::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_epoch_ends_with_its_month() {
        // 2026-10-01T00:00:00Z and 2027-01-01T00:00:00Z.
        assert_eq!(epoch_end("202609"), Some(1_790_812_800));
        assert_eq!(epoch_end("202612"), Some(1_798_761_600));
        // A leap February: 2024-03-01T00:00:00Z.
        assert_eq!(epoch_end("202402"), Some(1_709_251_200));
        assert_eq!(epoch_end("d"), None);
        assert_eq!(epoch_end("202613"), None);
    }

    #[test]
    fn a_durable_layer_never_expires_and_a_dated_one_does_after_its_lifetime() {
        let layer = |expiry| Layer {
            name: "l".into(),
            grid: Grid::WebMercator,
            zone_level: 10,
            expiry,
            content_type: String::new(),
        };
        let end = epoch_end("202609").expect("epoch");
        assert!(!layer(None).is_expired("202609", end + 10 * SECONDS_PER_DAY));
        let dated = layer(Some(30 * SECONDS_PER_DAY));
        assert!(!dated.is_expired("202609", end + 30 * SECONDS_PER_DAY));
        assert!(dated.is_expired("202609", end + 30 * SECONDS_PER_DAY + 1));
        // The durable epoch has no end to measure from.
        assert!(!dated.is_expired("d", u64::MAX));
    }

    #[test]
    fn a_tile_is_filed_under_its_ancestor_at_the_zone_level() {
        let layer = Layer {
            name: "imagery".into(),
            grid: Grid::WebMercator,
            zone_level: 10,
            expiry: None,
            content_type: String::new(),
        };
        assert_eq!(layer.zone_prefix(9, 300, 200), "imagery/top");
        assert_eq!(layer.zone_prefix(10, 522, 373), "imagery/zones/z10/522/373");
        assert_eq!(
            layer.zone_prefix(12, 2091, 1495),
            "imagery/zones/z10/522/373"
        );
    }

    #[test]
    fn the_geographic_grid_has_two_columns_and_sits_one_level_down() {
        assert_eq!(Grid::WebMercator.id(0, 0, 0), Some(0));
        assert_eq!(Grid::WebMercator.id(0, 1, 0), None);
        assert!(Grid::Geographic.id(0, 1, 0).is_some());
        assert_eq!(Grid::Geographic.id(0, 2, 0), None);
        assert_eq!(Grid::Geographic.id(0, 0, 1), None);
        assert_eq!(
            Grid::Geographic.id(3, 5, 2),
            Grid::WebMercator.id(4, 5, 2),
            "a geographic tile is the square pyramid's, one level down"
        );
    }
}
