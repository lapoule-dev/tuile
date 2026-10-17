// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Extracts: the tiles a reader asks for, as one archive.
//!
//! A renderer that knows ahead every tile it will draw asks for them once,
//! and reads them from a local file for as long as it renders. The store
//! gathers them: every zone they lie in, each of its live archives fetched
//! whole — newest first, the first to hold a tile wins it, as a reader of the
//! zone would see — and keeps the tiles asked for. They go out as **one
//! PMTiles archive** of the layer, the tile ids the layer's own, whatever
//! zones they came from.
//!
//! What is asked for is tile addresses, and/or levels: an extract of
//! `tiles` holds those of them the store has; `levels` keeps only those
//! levels; `whole_zones` takes the zones of the given addresses whole.
//!
//! One extract may hold tiles of several layers — a terrain and its imagery,
//! what one reader draws together. Their grids would give two tiles of two
//! layers the same id, so each layer of an extract has its band: the
//! layer's own ids for the first, the same plus [`BANDS`]`[1]` for the
//! second, in the PMTiles numbering's zoom 30 where no grid tile reaches.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};
use pmtiles::TileCoord;

use crate::archive;
use crate::layer::{Layer, Zone};
use crate::manifest;
use crate::store::{download, TileStore};
use crate::StoreError;

/// What is added to the ids of each layer of an extract, in the order they
/// were asked for.
pub const BANDS: [u64; 2] = [0, 1 << 60];

/// An extract goes into the bucket in parts of this size…
pub const PUT_PART_BYTES: usize = 32 << 20;
/// …this many of them in flight at once.
const PUT_PARTS_AT_ONCE: usize = 8;

/// Tiles of one archive read at once, each by its own range.
const TILES_AT_ONCE: usize = 32;

/// Zones read at once while extracting.
const ZONES_AT_ONCE: usize = 16;

/// What an extract holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractAsk {
    pub layer: String,
    /// Tile addresses, `(level, x, y)` on the layer's grid.
    #[serde(default)]
    pub tiles: Vec<(u8, u32, u32)>,
    /// Addresses whose zones are taken whole.
    #[serde(default)]
    pub whole_zones: Vec<(u8, u32, u32)>,
    /// The lowest and highest level kept; every level when absent.
    #[serde(default)]
    pub levels: Option<(u8, u8)>,
}

/// What an extract took and made.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExtractReport {
    pub zones: u64,
    /// Archives read, and the bytes read of them: every byte for a zone
    /// taken whole, the asked tiles' alone otherwise.
    pub archives: u64,
    pub archive_bytes: u64,
    /// Tiles kept, and the archive's bytes.
    pub tiles: u64,
    pub bytes: u64,
    pub millis: u64,
    /// Putting it in the bucket, when it went there.
    #[serde(default)]
    pub put_millis: u64,
    /// Tiles named in an ask's `tiles` (within its levels) that no live
    /// archive of the store holds, and the first [`ABSENT_NAMED`] of them,
    /// each as its layer, level, x and y.
    #[serde(default)]
    pub absent: u64,
    #[serde(default)]
    pub absent_first: Vec<(String, u8, u32, u32)>,
}

/// Absent tiles an [`ExtractReport`] names, at most.
pub const ABSENT_NAMED: usize = 16;

impl ExtractReport {
    /// Says what is missing, if anything is: how many tiles, and the first.
    pub fn incomplete(&self) -> Option<String> {
        if self.absent == 0 {
            return None;
        }
        let mut said = format!("{} tiles asked for are not in the store", self.absent);
        if let Some((layer, level, x, y)) = self.absent_first.first() {
            said.push_str(&format!(", {layer} {level}/{x}/{y} the first of them"));
        }
        Some(said)
    }
}

/// Which tiles of a zone are kept.
struct Keep {
    ids: HashSet<u64>,
    /// The ids asked for, by the zone they lie in.
    by_zone: std::collections::HashMap<Zone, Vec<u64>>,
    whole: HashSet<Zone>,
    levels: Option<(u8, u8)>,
}

impl Keep {
    fn of(layer: &Layer, ask: &ExtractAsk) -> Result<(Self, BTreeSet<Zone>), StoreError> {
        let mut zones = BTreeSet::new();
        let mut ids = HashSet::with_capacity(ask.tiles.len());
        let mut by_zone: std::collections::HashMap<Zone, Vec<u64>> = Default::default();
        for &(level, x, y) in &ask.tiles {
            if ask
                .levels
                .is_some_and(|(low, high)| level < low || level > high)
            {
                continue;
            }
            let (zone, id) = (layer.zone_of(level, x, y)?, layer.grid.to_id(level, x, y)?);
            zones.insert(zone);
            if ids.insert(id) {
                by_zone.entry(zone).or_default().push(id);
            }
        }
        let mut whole = HashSet::new();
        for &(level, x, y) in &ask.whole_zones {
            let zone = layer.zone_of(level, x, y)?;
            zones.insert(zone);
            whole.insert(zone);
        }
        Ok((
            Self {
                ids,
                by_zone,
                whole,
                levels: ask.levels,
            },
            zones,
        ))
    }

    fn keeps(&self, layer: &Layer, zone: Zone, id: u64) -> bool {
        let level = pmtiles::TileId::new(id)
            .ok()
            .and_then(|t| layer.grid.from_archive(TileCoord::from(t)))
            .map(|(level, _, _)| level);
        let Some(level) = level else { return false };
        if let Some((low, high)) = self.levels {
            if level < low || level > high {
                return false;
            }
        }
        self.whole.contains(&zone) || self.ids.contains(&id)
    }
}

impl TileStore {
    /// The tiles `asks` name — one layer each, at most [`BANDS`] of them —
    /// as one archive: a temporary file, gone when dropped.
    pub async fn extract(
        &self,
        asks: &[ExtractAsk],
    ) -> Result<(tempfile::NamedTempFile, ExtractReport), StoreError> {
        let began = Instant::now();
        if asks.len() > BANDS.len() {
            return Err(StoreError::Corrupt(format!(
                "an extract of {} layers: {} at most",
                asks.len(),
                BANDS.len()
            )));
        }
        let mut report = ExtractReport::default();
        let mut tiles = BTreeMap::new();
        let mut layers = Vec::with_capacity(asks.len());
        for (ask, band) in asks.iter().zip(BANDS) {
            let layer = self.layer(&ask.layer)?.clone();
            let (keep, zones) = Keep::of(&layer, ask)?;
            let (layer, keep) = (Arc::new(layer), Arc::new(keep));
            report.zones += zones.len() as u64;
            let read: Vec<(BTreeMap<u64, Bytes>, ExtractReport)> =
                futures_util::stream::iter(zones)
                    .map(|zone| {
                        let (layer, keep) = (layer.clone(), keep.clone());
                        async move { self.extract_zone(&layer, zone, &keep).await }
                    })
                    .buffer_unordered(ZONES_AT_ONCE)
                    .try_collect()
                    .await?;
            // Zones do not share tiles, nor bands layers: the maps join
            // without a choice to make.
            for (zone_tiles, zone) in read {
                report.archives += zone.archives;
                report.archive_bytes += zone.archive_bytes;
                tiles.extend(zone_tiles.into_iter().map(|(id, b)| (id + band, b)));
            }
            // Every tile named, found or said absent: an extract is read by
            // a renderer that draws nothing it lacks.
            let mut named = HashSet::with_capacity(ask.tiles.len());
            for &(level, x, y) in &ask.tiles {
                if ask
                    .levels
                    .is_some_and(|(low, high)| level < low || level > high)
                {
                    continue;
                }
                let id = layer.grid.to_id(level, x, y)?;
                if named.insert(id) && !tiles.contains_key(&(id + band)) {
                    report.absent += 1;
                    if report.absent_first.len() < ABSENT_NAMED {
                        report.absent_first.push((layer.name.clone(), level, x, y));
                    }
                }
            }
            layers.push(serde_json::json!({
                "name": layer.name,
                "grid": layer.grid,
                "band": band,
                "content_type": layer.content_type,
            }));
        }
        report.tiles = tiles.len() as u64;
        let metadata = serde_json::json!({ "tuile:extract": layers }).to_string();
        let file = tokio::task::spawn_blocking(move || {
            archive::write_raw(
                &metadata,
                pmtiles::TileType::Unknown,
                pmtiles::Compression::None,
                tiles,
            )
        })
        .await
        .map_err(|e| StoreError::Corrupt(format!("extract writer: {e}")))??;
        report.bytes = std::fs::metadata(file.path())?.len();
        report.millis = began.elapsed().as_millis() as u64;
        Ok((file, report))
    }

    /// [`TileStore::extract`], put in this store's bucket under `key`: what
    /// a server of the bucket then serves in parts ([`Extracts`]).
    pub async fn extract_into(
        &self,
        asks: &[ExtractAsk],
        key: &str,
    ) -> Result<ExtractReport, StoreError> {
        let (file, mut report) = self.extract(asks).await?;
        let began = Instant::now();
        put_file(self.object_store().as_ref(), file.path(), key).await?;
        report.put_millis = began.elapsed().as_millis() as u64;
        Ok(report)
    }

    /// [`TileStore::extract_into`], only if the store holds every tile the
    /// asks name: otherwise nothing is put under `key` and the error says
    /// what is missing ([`StoreError::Incomplete`]). For a reader that is
    /// to draw every tile it names — a render — an extract with holes is no
    /// extract, and one left under `key` from before is not replaced.
    pub async fn extract_whole_into(
        &self,
        asks: &[ExtractAsk],
        key: &str,
    ) -> Result<ExtractReport, StoreError> {
        let (file, mut report) = self.extract(asks).await?;
        if let Some(said) = report.incomplete() {
            return Err(StoreError::Incomplete {
                absent: report.absent,
                said,
            });
        }
        let began = Instant::now();
        put_file(self.object_store().as_ref(), file.path(), key).await?;
        report.put_millis = began.elapsed().as_millis() as u64;
        Ok(report)
    }

    /// The tiles `keep` wants of one zone, from its live archives, newest
    /// first.
    async fn extract_zone(
        &self,
        layer: &Layer,
        zone: Zone,
        keep: &Keep,
    ) -> Result<(BTreeMap<u64, Bytes>, ExtractReport), StoreError> {
        if keep.whole.contains(&zone) {
            return self.extract_zone_whole(layer, zone, keep).await;
        }
        // The tiles asked for, and only them: each live archive, newest
        // first, opened by its header and directories, and a range read for
        // each tile it holds that no newer one did.
        let mut report = ExtractReport::default();
        let now = self.now();
        let m = manifest::read(self.object_store().as_ref(), &layer.zone_prefix(zone))
            .await?
            .manifest;
        let mut missing: Vec<u64> = keep.by_zone.get(&zone).cloned().unwrap_or_default();
        let mut tiles = BTreeMap::new();
        for archive in m.archives.iter().rev() {
            if missing.is_empty() {
                break;
            }
            if layer.is_expired(&archive.epoch, now) {
                continue;
            }
            let reader = match archive::open_remote(self.object_store().clone(), &archive.key).await
            {
                Ok(reader) => Arc::new(reader),
                // Gone under us (merged, expired): nothing of it to take.
                Err(e) => match object_store::ObjectStoreExt::head(
                    self.object_store().as_ref(),
                    &object_store::path::Path::from(archive.key.as_str()),
                )
                .await
                {
                    Err(object_store::Error::NotFound { .. }) => continue,
                    _ => return Err(e),
                },
            };
            report.archives += 1;
            let found: Vec<(u64, Option<Bytes>)> = futures_util::stream::iter(missing.clone())
                .map(|id| {
                    let reader = reader.clone();
                    async move {
                        let tile = reader.get_tile(pmtiles::TileId::new(id)?).await?;
                        Ok::<_, StoreError>((id, tile))
                    }
                })
                .buffer_unordered(TILES_AT_ONCE)
                .try_collect()
                .await?;
            for (id, tile) in found {
                if let Some(bytes) = tile {
                    report.archive_bytes += bytes.len() as u64;
                    tiles.insert(id, bytes);
                }
            }
            missing.retain(|id| !tiles.contains_key(id));
        }
        Ok((tiles, report))
    }

    /// A zone taken whole: each live archive fetched whole, newest first.
    async fn extract_zone_whole(
        &self,
        layer: &Layer,
        zone: Zone,
        keep: &Keep,
    ) -> Result<(BTreeMap<u64, Bytes>, ExtractReport), StoreError> {
        let mut report = ExtractReport::default();
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
        let scratch = tempfile::tempdir()?;
        // Fetched together, read newest first.
        let local: Vec<Option<std::path::PathBuf>> =
            futures_util::stream::iter(archives.iter().enumerate())
                .map(|(i, a)| {
                    let to = scratch.path().join(format!("{i}.pmtiles"));
                    async move {
                        match download(self.object_store().as_ref(), &a.key, &to).await {
                            Ok(()) => Ok(Some(to)),
                            // Gone under us (merged, expired): nothing of it to take.
                            Err(StoreError::ObjectStore(object_store::Error::NotFound {
                                ..
                            })) => Ok(None),
                            Err(e) => Err(e),
                        }
                    }
                })
                .buffered(ZONES_AT_ONCE)
                .try_collect()
                .await?;
        let mut tiles = BTreeMap::new();
        for path in local.into_iter().rev().flatten() {
            report.archives += 1;
            report.archive_bytes += std::fs::metadata(&path)?.len();
            let reader = Arc::new(archive::open_local(&path).await?);
            for id in archive::ids(reader.clone()).await? {
                if tiles.contains_key(&id) || !keep.keeps(layer, zone, id) {
                    continue;
                }
                if let Some(bytes) = reader.get_tile(pmtiles::TileId::new(id)?).await? {
                    tiles.insert(id, bytes);
                }
            }
        }
        Ok((tiles, report))
    }
}

/// Puts the file at `path` in `store` under `key`, in parts of
/// [`PUT_PART_BYTES`], [`PUT_PARTS_AT_ONCE`] of them in flight.
async fn put_file(
    store: &dyn object_store::ObjectStore,
    path: &std::path::Path,
    key: &str,
) -> Result<(), StoreError> {
    use object_store::{ObjectStoreExt, PutPayload, WriteMultipart};
    use tokio::io::AsyncReadExt;
    let to = object_store::path::Path::from(key);
    let mut file = tokio::fs::File::open(path).await?;
    let size = file.metadata().await?.len();
    if size <= PUT_PART_BYTES as u64 {
        let mut data = Vec::with_capacity(size as usize);
        file.read_to_end(&mut data).await?;
        store.put(&to, PutPayload::from(data)).await?;
        return Ok(());
    }
    let upload = store.put_multipart(&to).await?;
    let mut writer = WriteMultipart::new_with_chunk_size(upload, PUT_PART_BYTES);
    let mut buf = vec![0u8; PUT_PART_BYTES];
    loop {
        // A short part in the middle of a multipart upload is refused: fill
        // each one unless the file ends.
        let mut filled = 0;
        while filled < buf.len() {
            let n = file.read(&mut buf[filled..]).await?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }
        if let Err(e) = writer.wait_for_capacity(PUT_PARTS_AT_ONCE).await {
            let _ = writer.abort().await;
            return Err(e.into());
        }
        writer.write(&buf[..filled]);
    }
    writer.finish().await?;
    Ok(())
}

/// Extracts of a bucket, served in parts from a local copy.
///
/// An extract is built once and put in the bucket, under a key. A reader
/// takes it in parts, each a request of its own, as many at once as it
/// likes; the first part asked of a key brings the whole object to this
/// server's disk — once, however many parts ask at the same time — and every
/// part after it is read from there. A file read a moment ago is in the page
/// cache: what memory is free serves it.
///
/// Copies are kept within a budget of bytes: the least recently read go
/// first to make room.
pub struct Extracts {
    dir: std::path::PathBuf,
    held: std::sync::Mutex<std::collections::HashMap<String, Held>>,
    /// One copy at a time per key.
    fetching: tokio::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    budget: u64,
}

struct Held {
    path: std::path::PathBuf,
    bytes: u64,
    read: Instant,
}

impl Drop for Held {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The number of parts of `bytes`, each of `part` bytes but the last.
pub fn parts(bytes: u64, part: u64) -> u64 {
    bytes.div_ceil(part.max(1)).max(1)
}

impl Extracts {
    /// Copies kept under `dir`, at most `budget` bytes of them.
    pub fn new(dir: impl Into<std::path::PathBuf>, budget: u64) -> Result<Self, StoreError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            held: Default::default(),
            fetching: Default::default(),
            budget,
        })
    }

    /// Part `n`, of `part` bytes, of the object `key` of `bucket`: from the
    /// local copy, made first if there is none. `None` when the object has
    /// no such part or is not in the bucket.
    pub async fn part(
        &self,
        bucket: &dyn object_store::ObjectStore,
        key: &str,
        part: u64,
        n: u64,
    ) -> Result<Option<Bytes>, StoreError> {
        if self.local(key).is_none() {
            let turn = {
                let mut fetching = self.fetching.lock().await;
                fetching.entry(key.to_string()).or_default().clone()
            };
            let _mine = turn.lock().await;
            if self.local(key).is_none() {
                let copy = tempfile::NamedTempFile::new_in(&self.dir)?;
                match download(bucket, key, copy.path()).await {
                    Ok(()) => {}
                    Err(StoreError::ObjectStore(object_store::Error::NotFound { .. })) => {
                        return Ok(None)
                    }
                    Err(e) => return Err(e),
                }
                self.keep(key, copy)?;
            }
        }
        let Some((path, bytes)) = self.local(key) else {
            return Ok(None);
        };
        let count = parts(bytes, part);
        if n >= count {
            return Ok(None);
        }
        let range = n * part..((n + 1) * part).min(bytes);
        let read = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            let file = std::fs::File::open(&path)?;
            let mut out = vec![0u8; (range.end - range.start) as usize];
            std::os::unix::fs::FileExt::read_exact_at(&file, &mut out, range.start)?;
            Ok(out)
        })
        .await
        .map_err(|e| StoreError::Corrupt(format!("extract reader: {e}")))??;
        Ok(Some(Bytes::from(read)))
    }

    /// The local copy of `key`, marked as read now.
    fn local(&self, key: &str) -> Option<(std::path::PathBuf, u64)> {
        let mut held = self.held.lock().ok()?;
        let h = held.get_mut(key)?;
        h.read = Instant::now();
        Some((h.path.clone(), h.bytes))
    }

    /// Keeps `copy` as the local copy of `key`, letting the least recently
    /// read go until it fits.
    fn keep(&self, key: &str, copy: tempfile::NamedTempFile) -> Result<(), StoreError> {
        let bytes = copy.as_file().metadata()?.len();
        let path = self.dir.join(format!(
            "{:016x}{:016x}.pmtiles",
            rand::random::<u64>(),
            rand::random::<u64>()
        ));
        copy.persist(&path).map_err(|e| StoreError::Io(e.error))?;
        let mut held = self.held.lock().map_err(|_| StoreError::Poisoned)?;
        while held.values().map(|h| h.bytes).sum::<u64>() + bytes > self.budget {
            let Some(oldest) = held
                .iter()
                .min_by_key(|(_, h)| h.read)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            held.remove(&oldest);
        }
        held.insert(
            key.to_string(),
            Held {
                path,
                bytes,
                read: Instant::now(),
            },
        );
        Ok(())
    }

    /// Bytes kept now.
    pub fn held_bytes(&self) -> u64 {
        self.held
            .lock()
            .map(|h| h.values().map(|h| h.bytes).sum())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{self, Catalog, LayerDef};
    use crate::grid::Grid;
    use crate::store::StoreConfig;
    use object_store::memory::InMemory;
    use object_store::ObjectStore;

    async fn store() -> TileStore {
        let memory: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let def = LayerDef {
            name: "imagery".into(),
            grid: Grid::WebMercator,
            tile_type: "png".into(),
            tile_compression: "none".into(),
            zone_level: 4,
            expiry_days: None,
            content_type: "image/png".into(),
            index: false,
            licence: None,
            attribution: None,
        };
        catalog::write(memory.as_ref(), &Catalog { layers: vec![def] })
            .await
            .expect("catalog");
        TileStore::open(memory, StoreConfig::default())
            .await
            .expect("open")
    }

    async fn put(store: &TileStore, (level, x, y): (u8, u32, u32), body: &str) {
        store
            .put("imagery", level, x, y, Bytes::from(body.to_string()))
            .await
            .expect("put");
    }

    async fn held(file: &tempfile::NamedTempFile) -> BTreeMap<(u8, u32, u32), String> {
        let reader = Arc::new(archive::open_local(file.path()).await.expect("open"));
        let mut out = BTreeMap::new();
        for id in archive::ids(reader.clone()).await.expect("ids") {
            let c = TileCoord::from(pmtiles::TileId::new(id).expect("id"));
            let bytes = reader.get_tile(c).await.expect("get").expect("tile");
            out.insert(
                (c.z(), c.x(), c.y()),
                String::from_utf8_lossy(&bytes).into(),
            );
        }
        out
    }

    #[tokio::test]
    async fn an_extract_holds_the_tiles_asked_for_as_the_zone_s_newest_archive_has_them() {
        let store = store().await;
        // Two zones; in the first, a tile written again in a newer archive.
        for (at, body) in [
            ((7, 77, 50), "old"),
            ((7, 78, 50), "b"),
            ((8, 154, 100), "c"),
            ((7, 5, 120), "far"),
        ] {
            put(&store, at, body).await;
        }
        store.flush_all().await.expect("flush");
        put(&store, (7, 77, 50), "new").await;
        store.flush_all().await.expect("flush");

        let ask = ExtractAsk {
            layer: "imagery".into(),
            tiles: vec![(7, 77, 50), (8, 154, 100), (7, 5, 120), (7, 76, 50)],
            ..Default::default()
        };
        let (file, report) = store.extract(&[ask]).await.expect("extract");
        assert_eq!(
            held(&file).await,
            BTreeMap::from([
                ((7, 5, 120), "far".to_string()),
                ((7, 77, 50), "new".to_string()),
                ((8, 154, 100), "c".to_string()),
            ]),
            "the newest wins, the tile not asked for and the one never written are not there"
        );
        assert_eq!((report.zones, report.archives, report.tiles), (2, 3, 3));
        // Read by ranges: the bytes of the three tiles kept ("new", "c",
        // "far"), not the archives they lie in.
        assert_eq!(report.archive_bytes, 7);
        assert_eq!(
            report.bytes,
            std::fs::metadata(file.path()).expect("meta").len()
        );

        // Levels cut what the tiles asked for; a zone taken whole brings
        // what was not named.
        let ask = ExtractAsk {
            layer: "imagery".into(),
            whole_zones: vec![(7, 77, 50)],
            levels: Some((7, 7)),
            ..Default::default()
        };
        let (file, _) = store.extract(&[ask]).await.expect("extract");
        assert_eq!(
            held(&file).await.into_keys().collect::<Vec<_>>(),
            [(7, 77, 50), (7, 78, 50)]
        );
    }

    #[tokio::test]
    async fn two_layers_share_one_extract_each_in_its_band() {
        let memory: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let def = |name: &str, grid| LayerDef {
            name: name.into(),
            grid,
            tile_type: "png".into(),
            tile_compression: "none".into(),
            zone_level: 4,
            expiry_days: None,
            content_type: "image/png".into(),
            index: false,
            licence: None,
            attribution: None,
        };
        catalog::write(
            memory.as_ref(),
            &Catalog {
                layers: vec![
                    def("imagery", Grid::WebMercator),
                    def("terrain", Grid::Geographic),
                ],
            },
        )
        .await
        .expect("catalog");
        let store = TileStore::open(memory, StoreConfig::default())
            .await
            .expect("open");
        // The same address in both: two grids, the same numbers.
        for layer in ["imagery", "terrain"] {
            store
                .put(layer, 7, 77, 50, Bytes::from(layer.to_string()))
                .await
                .expect("put");
        }
        store.flush_all().await.expect("flush");
        let ask = |layer: &str| ExtractAsk {
            layer: layer.into(),
            tiles: vec![(7, 77, 50)],
            ..Default::default()
        };
        let (file, report) = store
            .extract(&[ask("terrain"), ask("imagery")])
            .await
            .expect("extract");
        assert_eq!((report.tiles, report.zones), (2, 2));
        let reader = Arc::new(archive::open_local(file.path()).await.expect("open"));
        let ids = archive::ids(reader.clone()).await.expect("ids");
        let terrain = Grid::Geographic.to_id(7, 77, 50).expect("id");
        let imagery = Grid::WebMercator.to_id(7, 77, 50).expect("id");
        assert_eq!(
            ids,
            [terrain, imagery + BANDS[1]],
            "the first asked at its own ids"
        );
        let at = |id| {
            let reader = reader.clone();
            async move {
                let tile = reader
                    .get_tile(pmtiles::TileId::new(id).expect("id"))
                    .await
                    .expect("get")
                    .expect("a tile");
                String::from_utf8_lossy(&tile).to_string()
            }
        };
        assert_eq!(at(terrain).await, "terrain");
        assert_eq!(at(imagery + BANDS[1]).await, "imagery");
        let three = [ask("terrain"), ask("imagery"), ask("imagery")];
        assert!(store.extract(&three).await.is_err());
    }

    #[tokio::test]
    async fn an_extract_put_in_the_bucket_is_the_archive_it_built() {
        use object_store::ObjectStoreExt;
        let store = store().await;
        put(&store, (7, 77, 50), "a").await;
        store.flush_all().await.expect("flush");
        let ask = ExtractAsk {
            layer: "imagery".into(),
            tiles: vec![(7, 77, 50)],
            ..Default::default()
        };
        let (built, _) = store
            .extract(std::slice::from_ref(&ask))
            .await
            .expect("built");
        let report = store
            .extract_into(&[ask], "_extracts/run/c0001.pmtiles")
            .await
            .expect("put");
        let got = store
            .object_store()
            .get(&object_store::path::Path::from(
                "_extracts/run/c0001.pmtiles",
            ))
            .await
            .expect("there")
            .bytes()
            .await
            .expect("bytes");
        assert_eq!(got.to_vec(), std::fs::read(built.path()).expect("read"));
        assert_eq!(report.bytes, got.len() as u64);
    }

    /// A tile named and not stored is counted and named; an extract for a
    /// reader that draws every tile it names is then not put at all.
    #[tokio::test]
    async fn an_extract_lacking_a_named_tile_says_so_and_is_not_put_whole() {
        use object_store::ObjectStoreExt;
        let store = store().await;
        put(&store, (7, 77, 50), "a").await;
        store.flush_all().await.expect("flush");
        let ask = ExtractAsk {
            layer: "imagery".into(),
            // The second was never stored; the third is out of the levels.
            tiles: vec![(7, 77, 50), (7, 78, 50), (9, 1, 1)],
            levels: Some((7, 8)),
            ..Default::default()
        };
        let (_, report) = store
            .extract(std::slice::from_ref(&ask))
            .await
            .expect("extract");
        assert_eq!((report.tiles, report.absent), (1, 1));
        assert_eq!(report.absent_first, [("imagery".to_string(), 7, 78, 50)]);
        let key = "_extracts/run/c0002.pmtiles";
        match store
            .extract_whole_into(std::slice::from_ref(&ask), key)
            .await
        {
            Err(StoreError::Incomplete { absent, said }) => {
                assert_eq!(absent, 1);
                assert!(said.contains("imagery 7/78/50"), "{said}");
            }
            other => panic!("expected an incomplete extract, got {other:?}"),
        }
        assert!(
            store
                .object_store()
                .head(&object_store::path::Path::from(key))
                .await
                .is_err(),
            "nothing put"
        );
        // Once stored, the same ask goes up whole.
        put(&store, (7, 78, 50), "b").await;
        store.flush_all().await.expect("flush");
        let report = store.extract_whole_into(&[ask], key).await.expect("whole");
        assert_eq!((report.tiles, report.absent), (2, 0));
    }

    #[tokio::test]
    async fn an_object_is_copied_once_and_served_in_parts_within_a_budget() {
        use object_store::{ObjectStoreExt, PutPayload};
        let bucket = InMemory::new();
        for (key, body) in [
            ("_extracts/a", &b"0123456789"[..]),
            ("_extracts/b", &[7u8; 60][..]),
        ] {
            bucket
                .put(
                    &object_store::path::Path::from(key),
                    PutPayload::from(body.to_vec()),
                )
                .await
                .expect("put");
        }
        let dir = tempfile::tempdir().expect("dir");
        let extracts = Arc::new(Extracts::new(dir.path(), 64).expect("extracts"));
        // Many parts at once, before any copy: one copy made.
        let parts: Vec<_> = futures_util::future::join_all((0..3).map(|n| {
            let (extracts, bucket) = (extracts.clone(), &bucket);
            async move {
                extracts
                    .part(bucket, "_extracts/a", 4, n)
                    .await
                    .expect("part")
            }
        }))
        .await;
        assert_eq!(
            parts.iter().map(|p| p.as_deref()).collect::<Vec<_>>(),
            [Some(&b"0123"[..]), Some(&b"4567"[..]), Some(&b"89"[..])]
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("dir").count(),
            1,
            "one copy"
        );
        assert_eq!(
            extracts
                .part(&bucket, "_extracts/a", 4, 3)
                .await
                .expect("part"),
            None
        );
        assert_eq!(
            extracts
                .part(&bucket, "_extracts/none", 4, 0)
                .await
                .expect("part"),
            None
        );

        // Over the budget: the least recently read goes for the new one.
        let b = extracts
            .part(&bucket, "_extracts/b", 32, 1)
            .await
            .expect("part");
        assert_eq!(b.map(|b| b.len()), Some(28));
        assert_eq!(extracts.held_bytes(), 60);
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("dir").count(),
            1,
            "a let go, its file too"
        );
    }
}
