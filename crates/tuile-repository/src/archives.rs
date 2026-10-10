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
use futures_util::lock::Mutex as AsyncMutex;
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
/// Leaf directories kept, parsed: a few thousand entries each.
const LEAVES: usize = 256;
const SECONDS_PER_DAY: u64 = 24 * 3600;
/// How long a manifest read is trusted, in seconds.
const MANIFEST_TTL: u64 = 60;

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
    /// Leaf directories read lately, by archive and offset. A frame's tiles
    /// are neighbours, and neighbours share a leaf: without this, each of
    /// them reads, inflates and parses the same few thousand entries.
    leaves: Mutex<HashMap<(String, u64), Arc<Directory>>>,
    /// Manifests read lately, by zone, with when: a film asks for thousands
    /// of tiles of a handful of zones.
    manifests: Mutex<HashMap<String, (u64, Option<Arc<Manifest>>)>>,
    /// One lock per zone and per archive being read for the first time: a
    /// frame asks for many tiles of one zone at once, and they wait for one
    /// read of its manifest and of each archive's head.
    opening: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
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
            leaves: Mutex::new(HashMap::new()),
            manifests: Mutex::new(HashMap::new()),
            opening: Mutex::new(HashMap::new()),
        })
    }

    /// A zone's manifest, read again once it is [`MANIFEST_TTL`] old: that
    /// bounds how late a reader sees tiles another process added.
    async fn manifest(&self, zone_prefix: &str) -> Result<Option<Arc<Manifest>>, RepoError> {
        let now = (self.now)();
        let held = |tiles: &Self| {
            let held = tiles.manifests.lock().ok()?;
            let (at, manifest) = held.get(zone_prefix)?;
            (now.saturating_sub(*at) < MANIFEST_TTL).then(|| manifest.clone())
        };
        if let Some(manifest) = held(self) {
            return Ok(manifest);
        }
        let turn = self.turn(zone_prefix);
        let _mine = turn.lock().await;
        if let Some(manifest) = held(self) {
            return Ok(manifest);
        }
        let key = format!("{zone_prefix}/manifest.json");
        let manifest = match self.live.read_all(&key).await {
            Ok(bytes) => Some(Arc::new(
                serde_json::from_slice::<Manifest>(&bytes).map_err(|e| malformed(&key, e))?,
            )),
            // A zone nothing was ever written to.
            Err(RepoError::NotFound(_)) => None,
            Err(e) => return Err(e),
        };
        if let Ok(mut held) = self.manifests.lock() {
            held.insert(zone_prefix.to_string(), (now, manifest.clone()));
        }
        Ok(manifest)
    }

    /// Reads, together, the manifests of the zones these tiles of `layer`
    /// lie in, and holds them as if each tile had asked: what a reader does
    /// before a stretch of frames, so that its several hundred zones cost
    /// one exchange with the store ([`Objects::read_many`]) rather than one
    /// each. Zones whose manifest is already held are not asked about.
    /// Returns how many manifests were asked for.
    ///
    /// Nothing fails here but an unknown layer. A manifest that could not
    /// be read, or is not one, is simply not held: the tile that needs it
    /// asks for it alone, and it is that read which says what is wrong.
    pub async fn open_zones(
        &self,
        layer: &str,
        tiles: impl IntoIterator<Item = (u8, u32, u32)>,
    ) -> Result<usize, RepoError> {
        let Some(l) = self.layers.iter().find(|l| l.name == layer) else {
            return Err(RepoError::NotFound(format!("layer {layer}")));
        };
        let now = (self.now)();
        let zones: std::collections::BTreeSet<String> = tiles
            .into_iter()
            .map(|(level, x, y)| l.zone_prefix(level, x, y))
            .collect();
        let zones: Vec<String> = {
            let held = self.manifests.lock().ok();
            zones
                .into_iter()
                .filter(|zone| {
                    !held
                        .as_ref()
                        .and_then(|held| held.get(zone))
                        .is_some_and(|(at, _)| now.saturating_sub(*at) < MANIFEST_TTL)
                })
                .collect()
        };
        let keys: Vec<String> = zones
            .iter()
            .map(|zone| format!("{zone}/manifest.json"))
            .collect();
        if keys.is_empty() {
            return Ok(0);
        }
        let read = self.live.read_many(&keys).await;
        for (zone, read) in zones.iter().zip(read) {
            let manifest = match read {
                Ok(bytes) => match serde_json::from_slice::<Manifest>(&bytes) {
                    Ok(manifest) => Some(Arc::new(manifest)),
                    Err(_) => continue,
                },
                // A zone nothing was ever written to.
                Err(RepoError::NotFound(_)) => None,
                Err(_) => continue,
            };
            if let Ok(mut held) = self.manifests.lock() {
                held.insert(zone.clone(), (now, manifest));
            }
        }
        Ok(keys.len())
    }

    /// The lock of whatever is read once under `name`.
    fn turn(&self, name: &str) -> Arc<AsyncMutex<()>> {
        match self.opening.lock() {
            Ok(mut opening) => opening.entry(name.to_string()).or_default().clone(),
            Err(_) => Arc::default(),
        }
    }

    /// Forgets a zone's manifest: it named an archive that is gone.
    fn forget(&self, zone_prefix: &str) {
        if let Ok(mut held) = self.manifests.lock() {
            held.remove(zone_prefix);
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

    /// A leaf directory, parsed once and kept: archives never change, so
    /// what was at an offset of one is there for good.
    async fn leaf(
        &self,
        key: &str,
        header: &Header,
        offset: u64,
        length: u64,
    ) -> Result<Arc<Directory>, RepoError> {
        let at = (key.to_string(), offset);
        if let Some(held) = self.leaves.lock().ok().and_then(|l| l.get(&at).cloned()) {
            return Ok(held);
        }
        let leaf = Arc::new(self.directory(key, header, offset, length).await?);
        if let Ok(mut held) = self.leaves.lock() {
            if held.len() >= LEAVES {
                held.clear();
            }
            held.insert(at, leaf.clone());
        }
        Ok(leaf)
    }

    async fn opened(&self, key: &str) -> Result<Arc<Opened>, RepoError> {
        if let Some(held) = self.opened.lock().ok().and_then(|o| o.get(key).cloned()) {
            return Ok(held);
        }
        let turn = self.turn(key);
        let _mine = turn.lock().await;
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
        let mut leaf: Option<Arc<Directory>> = None;
        for _ in 0..MAX_DEPTH {
            let directory = leaf.as_deref().unwrap_or(&opened.root);
            let Some(entry) = directory.find_tile_id(id) else {
                return Ok(None);
            };
            let (offset, length) = (entry.offset(), u64::from(entry.length()));
            if !entry.points_to_leaf() {
                let start = header.data_offset() + offset;
                return Ok(Some(self.archives.read(key, start..start + length).await?));
            }
            let start = header.leaf_offset() + offset;
            leaf = Some(self.leaf(key, header, start, length).await?);
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
        let zone = l.zone_prefix(level, x, y);
        let now = (self.now)();
        // Twice at most: a manifest held from a moment ago may name an
        // archive that has since been merged into another and removed. The
        // tile is then in an archive only a fresh manifest names.
        for fresh in [false, true] {
            if fresh {
                self.forget(&zone);
            }
            let Some(manifest) = self.manifest(&zone).await? else {
                return Ok(None);
            };
            let mut gone = false;
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
                    Ok(None) => {}
                    Err(RepoError::NotFound(_)) => gone = true,
                    Err(e) => return Err(e),
                }
            }
            if !gone {
                break;
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
