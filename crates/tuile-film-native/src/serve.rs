// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The render as a service: a request a segment, answered once the segment is
//! in the store.
//!
//! The bake runs elsewhere and puts its tiles in the tile store, its pack by
//! reference in the run store. A request names the pack; the instance fetches
//! the extracts of the zones the pack's frames read — when the store is a
//! server's, see [`crate::extract`] — renders from them, puts the segment and
//! its receipt in the store, and answers. An instance kept warm keeps its GPU
//! set up for the next segment.
//!
//! What it keeps, it keeps in a generation of caches: one directory, the
//! sources opened over it. Where the disk is memory, as on a serverless
//! container, a cache that only grows ends the instance; so once the
//! directory is past its budget, or the generation past its age, the next
//! segment opens a new one, and the old goes with the last segment that
//! reads it.
//!
//! A host draws its own overlays on the service's films by giving it an
//! [`OverlayFactory`]: the factory's typed order travels in the segment's
//! order ([`SegmentOrder::overlay`]), read straight from the request into
//! the host's own type — fields it does not know refused — and the factory
//! turns it into the segment's [`Overlays`]. The stock service has
//! [`NoOverlays`], whose order says nothing.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use serde::de::DeserializeOwned;
use tuile_farm::receipt::{Instance, Io, Profile, Stages, RECEIPT_VERSION};
use tuile_farm::{ObjectRunStore, RunStore};
use tuile_film::Overlays;

use crate::engine::RenderSpec;
use crate::extract::{local_key, ExtractRef, Fetched};
use crate::observe::Timings;
use crate::profile::Sampling;
use crate::render::render;
use crate::setup::{film_sink, placed_sun};
use crate::source::{Film, Sources};
use crate::Error;

/// What a segment came to, and where a render's time went: the receipt's
/// own types, which the service's callers read too ([`tuile_farm::receipt`]).
pub use tuile_farm::receipt::{RenderTimes, SegmentDone};

/// A segment to render, and `O`, what the host's overlays are to be made
/// of ([`OverlayFactory::Order`]).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, bound(deserialize = "O: DeserializeOwned"))]
pub struct SegmentOrder<O = NoOrder> {
    /// The pack's key in the run store; it may not be there yet.
    pub pack: String,
    /// First and last frame.
    pub frames: (u32, u32),
    /// The segment's key in the store `store` names; its receipt beside it,
    /// `.json`.
    pub out: String,
    /// Where the segment goes: the run store, or the mezzanine store — a
    /// segment only the final stage reads ([`Service::compress`]).
    #[serde(default)]
    pub store: tuile_farm::compress::Store,
    #[serde(default)]
    pub rendering: RenderSpec,
    /// The film's store tiles for these frames, prepared by the tile
    /// store's server as the bake ended, one per layer: fetched before the
    /// first frame and read in place of the store. None: the store is read.
    #[serde(default)]
    pub extracts: Vec<ExtractRef>,
    /// The sun's direction, earth-centred, for every segment of a film
    /// alike; else `rendering.sun`, placed at this segment's first frame.
    #[serde(default)]
    pub to_sun: Option<[f32; 3]>,
    /// How long to wait for the pack. Default: an hour.
    #[serde(default)]
    pub pack_within_seconds: Option<u64>,
    /// The asker's own, handed back with the outcome of a segment accepted
    /// in the background ([`Notify`]): which film, which chunk.
    #[serde(default)]
    pub context: serde_json::Value,
    /// The host's overlays for the segment, in the host's own type: handed
    /// to its [`OverlayFactory`]. None: the segment has none.
    #[serde(default = "none", skip_serializing_if = "Option::is_none")]
    pub overlay: Option<O>,
}

fn none<O>() -> Option<O> {
    None
}

/// What a segment's overlays are made for: the segment as its order says.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SegmentContext<'a> {
    pub pack: &'a str,
    pub frames: (u32, u32),
    pub out: &'a str,
    pub rendering: &'a RenderSpec,
    pub to_sun: Option<[f32; 3]>,
}

/// What makes a segment's overlays, from the host's order for them.
///
/// Called once a segment, before its pack is waited for: an order the host
/// cannot make overlays of fails the segment before anything is rendered.
/// The overlays it makes are then asked for frame by frame on the render's
/// own thread.
#[async_trait::async_trait]
pub trait OverlayFactory: Send + Sync + 'static {
    /// The host's order, read from the request as it stands — a field the
    /// type does not know is refused, as it is in the rest of the order.
    type Order: DeserializeOwned + serde::Serialize + std::fmt::Debug + Clone + Send + Sync;

    async fn overlays(
        &self,
        order: &Self::Order,
        segment: &SegmentContext<'_>,
    ) -> Result<Box<dyn Overlays + Send>, Error>;
}

/// The order of a service that draws no overlays: it says nothing, and
/// anything said in it is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoOrder {}

/// A service that draws no overlays: what `tuile-film-serve` is.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOverlays;

#[async_trait::async_trait]
impl OverlayFactory for NoOverlays {
    type Order = NoOrder;

    async fn overlays(
        &self,
        _: &NoOrder,
        _: &SegmentContext<'_>,
    ) -> Result<Box<dyn Overlays + Send>, Error> {
        Ok(Box::new(()))
    }
}

/// Told how a segment accepted in the background ended: the host's way of
/// hearing of it — a message to whoever waits for the segment.
#[async_trait::async_trait]
pub trait Notify<O = NoOrder>: Send + Sync {
    async fn segment_done(&self, order: &SegmentOrder<O>, outcome: Result<&SegmentDone, String>);
}

/// When a request arrived, as the host's outermost layer saw it.
///
/// A host whose own layers run before the service reads the order sets it
/// as an extension of the request (`request.extensions_mut().insert(..)`):
/// their time is then the receipt's [`Stages::before_seconds`], and the
/// arrival its [`SegmentDone::received_at_unix`]. Without it the service
/// counts from its own handler.
#[derive(Debug, Clone, Copy)]
pub struct Arrived(pub Instant);

/// How often the pack is looked for.
const LOOK_EVERY: Duration = Duration::from_secs(2);

/// A render's steps, the costliest first, in seconds.
fn phases(t: &Timings) -> String {
    let mut all = t.steps();
    all.sort_by(|a, b| b.1.total_cmp(&a.1));
    all.iter()
        .map(|(name, ms)| format!("{name} {:.1}", ms / 1000.0))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Seconds since `t`.
fn since(t: Instant) -> f64 {
    t.elapsed().as_secs_f64()
}

/// Now, in Unix seconds.
fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// What a render service holds for as long as it lives, and `F`, what
/// makes the host's overlays.
pub struct Service<F: OverlayFactory = NoOverlays> {
    pub runs: Arc<ObjectRunStore>,
    /// Where a render's mezzanine is kept (`TUILE_MEZZANINE_STORE`); the run
    /// store when there is none.
    pub mezzanine: Option<Arc<ObjectRunStore>>,
    /// The tile store's server, when it is one: zones are fetched from it
    /// as extracts before a render.
    pub remote: Option<Arc<dyn tuile_repository::Get>>,
    /// The tile store, as the bucket or the routes give it.
    pub tiles: Arc<dyn tuile_repository::Objects>,
    /// Where generations of caches are opened.
    pub cache: PathBuf,
    pub limits: Limits,
    kept: tokio::sync::Mutex<Option<Arc<Generation>>>,
    /// Renders at once on this instance.
    renders: tokio::sync::Semaphore,
    overlays: F,
    /// When the service began opening, by both clocks, and how long it
    /// took: what a receipt says of its instance.
    started: Instant,
    started_at_unix: f64,
    opened_seconds: f64,
    /// Segments begun on this instance.
    begun: AtomicU64,
    /// The platform's name for this revision of the service (`K_REVISION`).
    revision: String,
}

/// When a generation of caches is let go, and how many renders share the
/// card.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub cache_bytes: u64,
    pub generation_age: Duration,
    pub renders_at_once: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            cache_bytes: 6 << 30,
            // Sources believe the store as it was when they opened.
            generation_age: Duration::from_secs(3600),
            renders_at_once: 1,
        }
    }
}

/// Sources and the directory their caches live in, gone together.
struct Generation {
    sources: Sources,
    dir: tempfile::TempDir,
    opened: Instant,
}

impl Generation {
    fn spent(&self, limits: &Limits) -> bool {
        self.opened.elapsed() > limits.generation_age
            || bytes_under(self.dir.path()) > limits.cache_bytes
    }
}

/// Bytes the files under `dir` hold. Held, not said: a cache preallocates
/// its files sparse, and a disk in memory pays only for what is written.
fn bytes_under(dir: &Path) -> u64 {
    let Ok(read) = std::fs::read_dir(dir) else {
        return 0;
    };
    read.flatten()
        .map(|entry| match entry.file_type() {
            Ok(t) if t.is_dir() => bytes_under(&entry.path()),
            Ok(_) => entry.metadata().map(|m| held(&m)).unwrap_or(0),
            Err(_) => 0,
        })
        .sum()
}

#[cfg(unix)]
fn held(file: &std::fs::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::blocks(file) * 512
}

#[cfg(not(unix))]
fn held(file: &std::fs::Metadata) -> u64 {
    file.len()
}

impl Service {
    /// A service that draws no overlays.
    pub fn new(
        runs: Arc<ObjectRunStore>,
        tiles: Arc<dyn tuile_repository::Objects>,
        cache: PathBuf,
        limits: Limits,
    ) -> Self {
        Self::with_overlays(runs, tiles, cache, limits, NoOverlays)
    }
}

impl<F: OverlayFactory> Service<F> {
    /// A service whose segments' overlays `overlays` makes.
    pub fn with_overlays(
        runs: Arc<ObjectRunStore>,
        tiles: Arc<dyn tuile_repository::Objects>,
        cache: PathBuf,
        limits: Limits,
        overlays: F,
    ) -> Self {
        Self {
            runs,
            mezzanine: None,
            remote: None,
            tiles,
            cache,
            limits,
            kept: tokio::sync::Mutex::new(None),
            renders: tokio::sync::Semaphore::new(limits.renders_at_once.max(1)),
            overlays,
            started: Instant::now(),
            started_at_unix: unix_now(),
            opened_seconds: 0.0,
            begun: AtomicU64::new(0),
            revision: std::env::var("K_REVISION").unwrap_or_default(),
        }
    }

    /// The generation a segment begins in: the one kept, unless it is
    /// spent; and whether it was the one kept.
    async fn generation(&self) -> Result<(Arc<Generation>, bool), Error> {
        let mut kept = self.kept.lock().await;
        if let Some(generation) = kept.as_ref().filter(|g| !g.spent(&self.limits)) {
            return Ok((generation.clone(), true));
        }
        std::fs::create_dir_all(&self.cache)?;
        let dir = tempfile::tempdir_in(&self.cache)?;
        let sources = Sources::open(self.runs.clone(), self.tiles.clone(), dir.path()).await?;
        let generation = Arc::new(Generation {
            sources,
            dir,
            opened: Instant::now(),
        });
        *kept = Some(generation.clone());
        Ok((generation, false))
    }
}

impl<F: OverlayFactory> Service<F> {
    /// The extracts `order` names, fetched under `dir`, and every zone the
    /// frames of `film` read pointed at its layer's; `None` when it names
    /// none.
    async fn fetch_extracts(
        &self,
        sources: &Sources,
        film: &Film,
        order: &SegmentOrder<F::Order>,
        dir: &Path,
    ) -> Result<Option<Fetched>, Error> {
        if order.extracts.is_empty() {
            return Ok(None);
        }
        let get = self
            .remote
            .as_ref()
            .ok_or("extracts named and no tile store server to fetch them from")?;
        let fetched = crate::extract::fetch(get.clone(), &order.extracts, dir).await?;
        let local: Arc<dyn tuile_repository::Objects> =
            Arc::new(ObjectRunStore::local(dir, tuile_farm::Tuning::default())?);
        let store = &sources.store.tiles;
        store.forget_extracts();
        let frames = order.frames;
        for file in &film.packs {
            let (a, b) = (file.first.max(frames.0), file.last.min(frames.1));
            if a > b {
                continue;
            }
            let pack = tuile_film::Pack::open_table(&file.head)?;
            let Some((terrain_layer, imagery_layer)) = pack.store_layers() else {
                continue;
            };
            let (terrain, imagery) = tuile_film::referred(&pack, a, b)?;
            for (layer, tiles) in [(terrain_layer, terrain), (imagery_layer, imagery)] {
                let (extract, band) = order
                    .extracts
                    .iter()
                    .find_map(|e| Some((e, e.band_of(layer)?)))
                    .ok_or_else(|| format!("no extract of {layer}"))?;
                let zones: std::collections::BTreeSet<String> = tiles
                    .iter()
                    .filter_map(|at| store.zone_of(layer, at.0, at.1, at.2))
                    .collect();
                for zone in zones {
                    store.read_zone_from(&zone, local.clone(), &local_key(extract), band);
                }
            }
        }
        Ok(Some(fetched))
    }

    /// The overlays `order` asks for, made by the host's factory; none if
    /// it asks for none.
    async fn overlays_of(
        &self,
        order: &SegmentOrder<F::Order>,
    ) -> Result<Box<dyn Overlays + Send>, Error> {
        let Some(overlay) = &order.overlay else {
            return Ok(Box::new(()));
        };
        let segment = SegmentContext {
            pack: &order.pack,
            frames: order.frames,
            out: &order.out,
            rendering: &order.rendering,
            to_sun: order.to_sun,
        };
        self.overlays.overlays(overlay, &segment).await
    }

    /// Renders `order` to the store, once its pack is there.
    pub async fn segment(&self, order: &SegmentOrder<F::Order>) -> Result<SegmentDone, Error> {
        self.segment_arrived(order, Arrived(Instant::now())).await
    }

    /// [`Self::segment`], for a request that arrived at `arrived`: the
    /// time before it is the receipt's [`Stages::before_seconds`].
    pub async fn segment_arrived(
        &self,
        order: &SegmentOrder<F::Order>,
        arrived: Arrived,
    ) -> Result<SegmentDone, Error> {
        let began = Instant::now();
        let before_seconds = began.saturating_duration_since(arrived.0).as_secs_f64();
        let received_at_unix = unix_now() - arrived.0.elapsed().as_secs_f64();
        let segments_before = self.begun.fetch_add(1, Ordering::Relaxed);
        let mut instance = Instance {
            started_at_unix: self.started_at_unix,
            opened_seconds: self.opened_seconds,
            uptime_seconds: arrived
                .0
                .saturating_duration_since(self.started)
                .as_secs_f64(),
            segments_before,
            cold: segments_before == 0,
            revision: self.revision.clone(),
            ..Instance::default()
        };
        let mut stages = Stages {
            before_seconds,
            ..Stages::default()
        };
        // The segment's CPU, from here to its answer.
        let sampling = Sampling::start();
        // Before anything is waited for: an order the host cannot draw
        // fails now, not an hour on.
        let t = Instant::now();
        let mut overlays = self.overlays_of(order).await?;
        stages.overlays_seconds = since(t);
        let within = Duration::from_secs(order.pack_within_seconds.unwrap_or(3600));
        let t = Instant::now();
        let (generation, reused) = self.generation().await?;
        stages.generation_seconds = since(t);
        instance.generation_reused = reused;
        instance.generation_age_seconds = since(generation.opened);
        let sources = &generation.sources;
        // What the generation's sources read so far: the segment's are
        // what it adds.
        let read_before = reads(sources);
        let t = Instant::now();
        loop {
            if self.runs.exists(&order.pack).await? {
                break;
            }
            if began.elapsed() > within {
                return Err(
                    format!("{} did not come within {} s", order.pack, within.as_secs()).into(),
                );
            }
            tokio::time::sleep(LOOK_EVERY).await;
        }
        stages.pack_wait_seconds = since(t);
        let t = Instant::now();
        // The pack's tiles were published before it: zones read before they
        // were, by an earlier segment of this generation, are read again —
        // its clock stands still, and a manifest held is believed.
        sources.store.tiles.forget_manifests();
        let rendering = order.rendering.rendering()?;
        let film = Film::of(sources.packs.objects.as_ref(), &[order.pack.as_str()]).await?;
        let mut render_order = rendering.order.clone();
        render_order.frames = Some(order.frames);
        match (order.to_sun, rendering.sun) {
            (Some(to_sun), _) => {
                render_order.look.to_sun = glam::Vec3::from_array(to_sun).normalize()
            }
            // Placed where the segment begins: the one asked for, or the
            // default sun — never the look's unoriented one.
            (None, sun) => {
                let head = &film.packs.first().ok_or("a film of no pack")?.head;
                let eye = tuile_film::Pack::open_table(head)?
                    .view_of(order.frames.0)?
                    .position;
                render_order.look.to_sun = placed_sun(eye, sun);
            }
        }
        stages.film_seconds = since(t);
        // Under the cache: on a serverless container, the disk it was given
        // rather than a /tmp in memory.
        std::fs::create_dir_all(&self.cache)?;
        let scratch = tempfile::tempdir_in(&self.cache)?;
        let local = scratch.path().join("segment.mp4");
        let t = Instant::now();
        let extract = self
            .fetch_extracts(sources, &film, order, &scratch.path().join("store"))
            .await?;
        stages.fetch_seconds = since(t);
        if let Some(fetched) = &extract {
            eprintln!(
                "segment {}: {} extracts, {} parts, {:.1} MB in {:.1} s ({:.0} MB/s)",
                order.out,
                fetched.extracts,
                fetched.parts,
                fetched.bytes as f64 / 1e6,
                fetched.seconds,
                fetched.mb_per_second()
            );
        }
        let t = Instant::now();
        let _card = self.renders.acquire().await.map_err(|e| e.to_string())?;
        stages.card_wait_seconds = since(t);
        let waited_seconds = began.elapsed().as_secs_f64();

        // The render holds its thread: on one of its own, its reads waited
        // for on this runtime.
        let rendered = Instant::now();
        let runtime = tokio::runtime::Handle::current();
        let (to, kept) = (local.clone(), generation.clone());
        let (done, spent) = tokio::task::spawn_blocking(
            move || -> Result<(crate::render::Done, Vec<(String, f64)>), Error> {
                let mut sink = film_sink(
                    to.to_string_lossy().into_owned(),
                    rendering.bitrate,
                    rendering.codec.as_deref(),
                )?;
                let done = runtime.block_on(render(
                    &kept.sources,
                    &film,
                    &render_order,
                    overlays.as_mut(),
                    sink.as_mut(),
                    &mut (),
                ))?;
                // The encoder's own thread, beside the render's.
                let spent = sink.spent();
                drop(sink);
                Ok((done, spent))
            },
        )
        .await
        .map_err(|e| e.to_string())??;
        let render_seconds = rendered.elapsed().as_secs_f64();
        drop(_card);
        let render = RenderTimes {
            setup_seconds: done.setup_seconds,
            opening: done.opening,
            frames: done.timings,
            tiles_from_store: done.tiles_from_store,
            tiles_from_pack: done.tiles_from_pack,
            seconds: done.seconds,
            ahead: done.ahead,
            encode_seconds: spent.iter().map(|s| s.1).sum(),
            encoder: spent
                .iter()
                .map(|s| s.0.as_str())
                .collect::<Vec<_>>()
                .join("; "),
            gpu: done.gpu,
            adapter: done.adapter,
        };
        eprintln!(
            "segment {}: {} frames in {render_seconds:.1} s — setup {:.1} s, ahead {:.1} s, encoder {:.1} s, card {:.1} s; frames: {}",
            order.out,
            done.frames,
            render.setup_seconds,
            render.ahead.seconds,
            render.encode_seconds,
            render.gpu.seconds(),
            phases(&render.frames)
        );
        let frames = done.frames;

        let store = self.store(order.store);
        let t = Instant::now();
        let bytes = store.put(&local, &order.out).await?;
        stages.put_seconds = since(t);
        let stem = order.out.trim_end_matches(".mp4");
        let profile = match sampling {
            Some(sampling) => Self::put_profile(store, sampling, stem, scratch.path()).await,
            None => None,
        };
        let read = read_since(reads(sources), &read_before);
        let receipt = SegmentDone {
            version: RECEIPT_VERSION,
            out: order.out.clone(),
            frames: u64::from(frames),
            bytes,
            waited_seconds,
            render_seconds,
            extract,
            render,
            received_at_unix,
            service_seconds: arrived.0.elapsed().as_secs_f64(),
            stages,
            instance,
            io: Io {
                put_bytes: bytes + profile.as_ref().map_or(0, |p| p.bytes),
                ..read
            },
            profile,
        };
        let receipt_path = scratch.path().join("segment.json");
        std::fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt)?)?;
        store.put(&receipt_path, &format!("{stem}.json")).await?;
        Ok(receipt)
    }

    /// The segment's CPU profile, beside it as `<stem>.pprof` in the store it
    /// went to; none if it could not be made or put — a segment never fails
    /// for its profile.
    async fn put_profile(
        store: &ObjectRunStore,
        sampling: Sampling,
        stem: &str,
        dir: &Path,
    ) -> Option<Profile> {
        let t = Instant::now();
        let key = format!("{stem}.pprof");
        let made = tokio::task::spawn_blocking(move || sampling.pprof_gz()).await;
        let gz = match made {
            Ok(Ok(gz)) => gz,
            Ok(Err(e)) => {
                eprintln!("segment {stem}: no profile: {e}");
                return None;
            }
            Err(e) => {
                eprintln!("segment {stem}: no profile: {e}");
                return None;
            }
        };
        let path = dir.join("segment.pprof");
        let put = async {
            std::fs::write(&path, &gz)?;
            store.put(&path, &key).await?;
            Ok::<_, Error>(())
        };
        if let Err(e) = put.await {
            eprintln!("segment {stem}: profile not put: {e}");
            return None;
        }
        Some(Profile {
            key,
            bytes: gz.len() as u64,
            hz: crate::profile::HZ,
            seconds: since(t),
        })
    }
}

/// What was read between `before` and `now`.
fn read_since(now: Io, before: &Io) -> Io {
    Io {
        pack_asked_bytes: now.pack_asked_bytes.saturating_sub(before.pack_asked_bytes),
        pack_fetched_bytes: now
            .pack_fetched_bytes
            .saturating_sub(before.pack_fetched_bytes),
        archive_asked_bytes: now
            .archive_asked_bytes
            .saturating_sub(before.archive_asked_bytes),
        archive_fetched_bytes: now
            .archive_fetched_bytes
            .saturating_sub(before.archive_fetched_bytes),
        live_bytes: now.live_bytes.saturating_sub(before.live_bytes),
        put_bytes: 0,
    }
}

/// What a generation's sources have read so far, as [`Io`]'s reads.
fn reads(sources: &Sources) -> Io {
    let (pack_asked, pack_fetched) = sources.packs.reads();
    let (archive_asked, archive_fetched) = sources.archives.reads();
    Io {
        pack_asked_bytes: pack_asked.bytes,
        pack_fetched_bytes: pack_fetched.bytes,
        archive_asked_bytes: archive_asked.bytes,
        archive_fetched_bytes: archive_fetched.bytes,
        live_bytes: sources.live.so_far().bytes,
        put_bytes: 0,
    }
}

impl<F: OverlayFactory> Service<F> {
    /// The store `store` names.
    fn store(&self, store: tuile_farm::compress::Store) -> &ObjectRunStore {
        match (store, &self.mezzanine) {
            (tuile_farm::compress::Store::Mezzanine, Some(mezzanine)) => mezzanine,
            _ => &self.runs,
        }
    }

    /// The final stage of a film: every segment its receipts vouch for,
    /// fetched from where they are kept and joined here, then delivered as
    /// the order says ([`crate::compress`]) into the run store. The film
    /// that comes out is checked as the join was: every frame, at its
    /// cadence. The encode waits its turn for the card with the renders.
    pub async fn compress(
        &self,
        order: &tuile_farm::compress::CompressOrder,
    ) -> Result<tuile_farm::compress::CompressDone, Error> {
        use tuile_farm::assemble::{gather, Gathered, RunKeys};
        std::fs::create_dir_all(&self.cache)?;
        let scratch = tempfile::tempdir_in(&self.cache)?;
        let run = RunKeys::new(&order.run, &order.render_id);
        let gathered = gather(
            self.runs.as_ref(),
            self.store(order.segments),
            &run,
            order.chunks,
            &scratch.path().join("join"),
            Some(order.fps),
        )
        .await?;
        let Gathered::Joined {
            path: joined,
            frames,
            bytes: joined_bytes,
            fetch_seconds,
            join_seconds,
        } = gathered
        else {
            return Err(format!(
                "{}: the receipts of render {} are not all in",
                order.run, order.render_id
            )
            .into());
        };
        let card = self.renders.acquire().await.map_err(|e| e.to_string())?;
        let delivered = scratch.path().join("film.mp4");
        let (to, delivery) = (delivered.clone(), order.delivery.clone());
        let fps = order.fps.round().max(1.0) as u32;
        let device = std::env::var("TUILE_CUDA_DEVICE")
            .ok()
            .and_then(|d| d.parse().ok())
            .unwrap_or(0);
        let compressed = tokio::task::spawn_blocking(move || {
            crate::compress::compress(&joined, &to, &delivery, fps, device)
        })
        .await
        .map_err(|e| e.to_string())??;
        drop(card);
        let probe = tuile_farm::concat::probe(&delivered)?;
        if probe.frames != frames {
            return Err(format!(
                "the delivered film carries {} frames, the joined one {frames}",
                probe.frames
            )
            .into());
        }
        tuile_farm::concat::check_cadence(probe, order.fps)?;
        let key = order
            .out
            .clone()
            .unwrap_or_else(|| tuile_farm::compress::film_key(&order.run, &order.delivery));
        let bytes = self.runs.put(&delivered, &key).await?;
        let bitrate = (bytes as f64 * 8.0 / probe.seconds.max(1e-3)).round() as u64;
        eprintln!(
            "compress {key}: {frames} frames, {:.1} MB from {:.1} MB joined ({:.1} Mb/s), \
             {} pass(es) in {:.1} s",
            bytes as f64 / 1e6,
            joined_bytes as f64 / 1e6,
            bitrate as f64 / 1e6,
            compressed.passes,
            compressed.seconds
        );
        Ok(tuile_farm::compress::CompressDone {
            key,
            frames,
            bytes,
            joined_bytes,
            passes: compressed.passes,
            bitrate,
            fetch_seconds,
            join_seconds,
            encode_seconds: compressed.seconds,
        })
    }
}

impl<F: OverlayFactory> Service<F> {
    /// Accepts `order`, renders it in the background and tells `notify` how
    /// it ended; returns at once. Renders wait their turn for the card
    /// ([`Limits::renders_at_once`]).
    pub fn accept(
        self: &Arc<Self>,
        order: SegmentOrder<F::Order>,
        notify: Arc<dyn Notify<F::Order>>,
    ) {
        let service = self.clone();
        tokio::spawn(async move {
            let outcome = service.segment(&order).await;
            match &outcome {
                Ok(done) => eprintln!(
                    "segment {} done: {} frames, waited {:.0} s, rendered in {:.0} s",
                    done.out, done.frames, done.waited_seconds, done.render_seconds
                ),
                Err(e) => eprintln!("segment {} failed: {e}", order.out),
            }
            notify
                .segment_done(&order, outcome.as_ref().map_err(|e| e.to_string()))
                .await;
        });
    }
}

/// A number from the environment.
fn number(name: &str) -> Result<Option<u64>, Error> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Ok(Some(v.parse().map_err(|e| format!("{name}={v}: {e}"))?)),
        _ => Ok(None),
    }
}

/// The limits the environment says, the defaults otherwise:
/// `TUILE_SERVE_CACHE_GIB`, `TUILE_SERVE_GENERATION_MINUTES`,
/// `TUILE_SERVE_RENDERS`.
pub fn limits_from_env() -> Result<Limits, Error> {
    let default = Limits::default();
    Ok(Limits {
        cache_bytes: number("TUILE_SERVE_CACHE_GIB")?.map_or(default.cache_bytes, |gib| gib << 30),
        generation_age: number("TUILE_SERVE_GENERATION_MINUTES")?
            .map_or(default.generation_age, |m| Duration::from_secs(m * 60)),
        renders_at_once: number("TUILE_SERVE_RENDERS")?
            .map_or(default.renders_at_once, |n| n as usize),
    })
}

/// The service the environment describes: the run store
/// (`TUILE_STORE_*`), the tile store (`TUILE_TILES_*`), its caches under
/// `TUILE_SERVE_CACHE`, its limits.
pub async fn service_from_env() -> Result<Service, Error> {
    service_from_env_with(NoOverlays).await
}

/// [`service_from_env`], its segments' overlays made by `overlays`.
pub async fn service_from_env_with<F: OverlayFactory>(overlays: F) -> Result<Service<F>, Error> {
    let (started, started_at_unix) = (Instant::now(), unix_now());
    let runs = Arc::new(ObjectRunStore::from_env()?);
    let cache: PathBuf = std::env::var("TUILE_SERVE_CACHE")
        .map(Into::into)
        .unwrap_or_else(|_| std::env::temp_dir().join("tuile-film-serve"));
    let tiles = crate::setup::tile_store(&cache).await?;
    let mut service =
        Service::with_overlays(runs, tiles.objects, cache, limits_from_env()?, overlays);
    service.mezzanine = ObjectRunStore::mezzanine_from_env()?.map(Arc::new);
    // The tile store's server, when the store is one: extracts come from it.
    service.remote = tuile_repository::HttpGet::from_env()?
        .map(|get| Arc::new(get) as Arc<dyn tuile_repository::Get>);
    // The instance began with this function: what its receipts say of it.
    service.started = started;
    service.started_at_unix = started_at_unix;
    service.opened_seconds = since(started);
    Ok(service)
}

/// The service's routes: `GET /`, `POST /segments` — answered once the
/// segment is in the store, or at once (202) when there is a `notify` to
/// tell how it ends — and `POST /compress`, a film's final stage
/// ([`Service::compress`]), answered once the delivered film is in the run
/// store.
pub fn router<F: OverlayFactory>(
    service: Arc<Service<F>>,
    notify: Option<Arc<dyn Notify<F::Order>>>,
) -> axum::Router {
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::Json;
    type Held<F> = (
        Arc<Service<F>>,
        Option<Arc<dyn Notify<<F as OverlayFactory>::Order>>>,
    );
    async fn segment<F: OverlayFactory>(
        State((service, notify)): State<Held<F>>,
        arrived: Option<axum::Extension<Arrived>>,
        Json(order): Json<SegmentOrder<F::Order>>,
    ) -> axum::response::Response {
        let arrived = arrived.map_or_else(|| Arrived(Instant::now()), |a| a.0);
        eprintln!(
            "segment {} ← {} {}:{}",
            order.out, order.pack, order.frames.0, order.frames.1
        );
        match notify {
            Some(notify) => {
                let out = order.out.clone();
                service.accept(order, notify);
                (
                    StatusCode::ACCEPTED,
                    Json(serde_json::json!({ "accepted": out })),
                )
                    .into_response()
            }
            None => match service.segment_arrived(&order, arrived).await {
                Ok(done) => {
                    eprintln!(
                        "segment {} done: {} frames, waited {:.0} s, rendered in {:.0} s",
                        done.out, done.frames, done.waited_seconds, done.render_seconds
                    );
                    Json(done).into_response()
                }
                Err(e) => {
                    eprintln!("segment {} failed: {e}", order.out);
                    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
                }
            },
        }
    }
    async fn compress<F: OverlayFactory>(
        State((service, _)): State<Held<F>>,
        Json(order): Json<tuile_farm::compress::CompressOrder>,
    ) -> axum::response::Response {
        eprintln!(
            "compress {} ← render {} as {}",
            order.run,
            order.render_id,
            order.delivery.key()
        );
        match service.compress(&order).await {
            Ok(done) => Json(done).into_response(),
            Err(e) => {
                eprintln!("compress {} failed: {e}", order.run);
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
            }
        }
    }
    axum::Router::new()
        .route("/", axum::routing::get(|| async { "tuile-film-serve" }))
        .route("/segments", axum::routing::post(segment::<F>))
        .route("/compress", axum::routing::post(compress::<F>))
        .with_state((service, notify))
}

/// Serves `router` on `PORT` (8080 unless said) until the process ends.
pub async fn serve(router: axum::Router) -> Result<(), Error> {
    let port = number("PORT")?.unwrap_or(8080);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port as u16)).await?;
    eprintln!("tuile-film-serve: on :{port}");
    axum::serve(listener, router).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_order_reads_from_json_with_only_what_it_must_say() {
        let order: SegmentOrder = serde_json::from_str(
            r#"{"pack": "packs/a/1-450.tuilepack", "frames": [1, 450], "out": "films/a/x/parts/1-450.mp4"}"#,
        )
        .expect("an order");
        assert_eq!(order.frames, (1, 450));
        assert_eq!(order.rendering, RenderSpec::default());
        assert!(order.to_sun.is_none());
        // A field nobody knows is refused, not ignored: a typo would render
        // another film.
        assert!(serde_json::from_str::<SegmentOrder>(
            r#"{"pack": "p", "frames": [1, 2], "out": "o", "exposur": 1.5}"#
        )
        .is_err());
        let spec: RenderSpec =
            serde_json::from_str(r#"{"exposure": 1.6, "sun_light": "3.2,1.7,0.8", "mbps": 45}"#)
                .expect("a spec");
        let rendering = spec.rendering().expect("a rendering");
        assert_eq!(rendering.bitrate, 45_000_000);
        assert_eq!(rendering.order.look.exposure_ev, 1.6);
        // The ground smoothed and sewn unless said: geometry, 2 m at most.
        let plain = RenderSpec::default().rendering().expect("a rendering");
        assert_eq!(
            plain.order.smooth,
            tuile_film::smoothing::Smooth::Geometry { bound: 2.0 }
        );
        assert!(plain.order.stitch);
        let asked: RenderSpec =
            serde_json::from_str(r#"{"smooth": "off", "stitch": false}"#).expect("a spec");
        let asked = asked.rendering().expect("a rendering");
        assert_eq!(asked.order.smooth, tuile_film::smoothing::Smooth::Off);
        assert!(!asked.order.stitch);
        assert!(serde_json::from_str::<RenderSpec>(r#"{"smooth": "bumpy"}"#)
            .expect("parsed")
            .rendering()
            .is_err());
        assert!(
            serde_json::from_str::<RenderSpec>(r#"{"sky_light": "1,2"}"#)
                .expect("parsed")
                .rendering()
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_segment_whose_pack_never_comes_is_refused_once_its_time_is_over() {
        let dir = tempfile::tempdir().expect("dir");
        // A tile store with no layer: its catalog is all it needs.
        std::fs::write(dir.path().join("catalog.json"), br#"{"layers": []}"#).expect("catalog");
        let runs = Arc::new(
            ObjectRunStore::local(dir.path(), tuile_farm::Tuning::default()).expect("store"),
        );
        let tiles: Arc<dyn tuile_repository::Objects> = runs.clone();
        let service = Service::new(runs, tiles, dir.path().join("cache"), Limits::default());
        let order = SegmentOrder {
            pack: "packs/never.tuilepack".into(),
            frames: (1, 2),
            out: "out.mp4".into(),
            store: Default::default(),
            rendering: RenderSpec::default(),
            extracts: Vec::new(),
            context: serde_json::Value::Null,
            to_sun: None,
            pack_within_seconds: Some(0),
            overlay: None,
        };
        let refused = service.segment(&order).await.expect_err("no pack");
        assert!(refused.to_string().contains("did not come"), "{refused}");
    }

    /// A film's final stage on the service: its receipts read from the run
    /// store, its segments from the mezzanine store, joined, delivered under
    /// the run as the order says — every frame there — and a render whose
    /// receipts are not all in refused.
    #[tokio::test]
    async fn a_film_is_compressed_from_its_mezzanine_into_the_run_store() {
        use tuile_farm::assemble::{Receipt, RunKeys};
        use tuile_farm::compress::{CompressOrder, Delivery, Store};
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("catalog.json"), br#"{"layers": []}"#).expect("catalog");
        let tuning = tuile_farm::Tuning::default;
        let runs =
            Arc::new(ObjectRunStore::local(&dir.path().join("runs"), tuning()).expect("store"));
        let mezzanine =
            Arc::new(ObjectRunStore::local(&dir.path().join("mezz"), tuning()).expect("store"));
        let tiles: Arc<dyn tuile_repository::Objects> =
            Arc::new(ObjectRunStore::local(dir.path(), tuning()).expect("store"));
        let mut service = Service::new(
            runs.clone(),
            tiles,
            dir.path().join("cache"),
            Limits::default(),
        );
        service.mezzanine = Some(mezzanine.clone());
        let keys = RunKeys::new("1557732/20261020T1000Z-ab12", "video-9-run");
        let scratch = dir.path().join("scratch");
        std::fs::create_dir_all(&scratch).expect("scratch");
        for (task, first, last) in [(0u32, 1u64, 3u64), (1, 4, 5)] {
            let segment = scratch.join(format!("{task}.mp4"));
            let entry =
                tuile_mp4::film::avc1(64, 48, &[0x67, 0x64, 0, 0x28], &[0x68]).expect("entry");
            let mut w = tuile_mp4::film::FilmWriter::create(&segment, entry, 30).expect("writer");
            for i in 0..(last + 1 - first) {
                w.push(&[0, 0, 0, 2, 0x65, i as u8], i == 0, i)
                    .expect("push");
            }
            w.finish().expect("finish");
            mezzanine
                .put(&segment, &keys.segment(u64::from(task)))
                .await
                .expect("segment");
            let receipt = scratch.join(format!("{task}.json"));
            let r = Receipt {
                task,
                first,
                last,
                segments: vec![u64::from(task)],
            };
            std::fs::write(&receipt, serde_json::to_vec(&r).expect("json")).expect("receipt");
            runs.put(&receipt, &keys.receipt(task))
                .await
                .expect("receipt");
        }
        let order: CompressOrder = serde_json::from_value(serde_json::json!({
            "run": "1557732/20261020T1000Z-ab12", "render_id": "video-9-run", "chunks": 2,
            "fps": 30.0, "segments": "mezzanine", "delivery": {"kind": "copy"},
        }))
        .expect("order");
        assert_eq!(order.segments, Store::Mezzanine);
        let done = service.compress(&order).await.expect("compressed");
        assert_eq!(done.key, "1557732/20261020T1000Z-ab12/films/copy-v1.mp4");
        assert_eq!(done.frames, 5);
        assert!(runs.exists(&done.key).await.expect("exists"));
        let refused = service
            .compress(&CompressOrder {
                chunks: 3,
                delivery: Delivery::Copy,
                ..order
            })
            .await
            .expect_err("a chunk missing");
        assert!(refused.to_string().contains("not all in"), "{refused}");
    }

    #[tokio::test]
    async fn a_generation_is_kept_until_its_caches_pass_their_budget_or_their_age() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("catalog.json"), br#"{"layers": []}"#).expect("catalog");
        let runs = Arc::new(
            ObjectRunStore::local(dir.path(), tuile_farm::Tuning::default()).expect("store"),
        );
        let tiles: Arc<dyn tuile_repository::Objects> = runs.clone();
        let limits = Limits {
            cache_bytes: 1 << 20,
            ..Limits::default()
        };
        let service = Service::new(runs, tiles, dir.path().join("cache"), limits);
        let (first, kept) = service.generation().await.expect("first");
        assert!(!kept, "the first is opened");
        let (again, kept) = service.generation().await.expect("again");
        assert!(
            Arc::ptr_eq(&first, &again) && kept,
            "a generation under its budget is kept"
        );

        // Past its budget: the next segment begins in another, and the old
        // directory goes with the last segment that holds it.
        std::fs::write(first.dir.path().join("big"), vec![0u8; 2 << 20]).expect("fill");
        let (other, kept) = service.generation().await.expect("other");
        assert!(
            !Arc::ptr_eq(&first, &other) && !kept,
            "a generation past its budget is let go"
        );
        let old = first.dir.path().to_path_buf();
        drop((first, again));
        assert!(!old.exists(), "its caches go with it");

        assert!(!other.spent(&limits));
        assert!(other.spent(&Limits {
            generation_age: Duration::ZERO,
            ..limits
        }));
    }

    /// A host's order for its overlays: its own type, its own fields.
    #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Trail {
        run: String,
        chunk: u32,
    }

    /// Remembers every order it is handed; refuses the chunk numbered 0.
    #[derive(Default)]
    struct Trails(std::sync::Mutex<Vec<(Trail, (u32, u32))>>);

    #[async_trait::async_trait]
    impl OverlayFactory for Arc<Trails> {
        type Order = Trail;

        async fn overlays(
            &self,
            order: &Trail,
            segment: &SegmentContext<'_>,
        ) -> Result<Box<dyn Overlays + Send>, Error> {
            self.0
                .lock()
                .expect("lock")
                .push((order.clone(), segment.frames));
            if order.chunk == 0 {
                return Err("no chunk 0".into());
            }
            Ok(Box::new(()))
        }
    }

    #[test]
    fn a_hosts_overlay_order_is_read_into_its_own_type_and_nothing_else() {
        let order: SegmentOrder<Trail> = serde_json::from_str(
            r#"{"pack": "p", "frames": [1, 2], "out": "o",
                "overlay": {"run": "r/1", "chunk": 3}}"#,
        )
        .expect("an order");
        assert_eq!(
            order.overlay,
            Some(Trail {
                run: "r/1".into(),
                chunk: 3
            })
        );
        // A field the host's type does not know is refused, inside its
        // order as beside it…
        for refused in [
            r#"{"pack": "p", "frames": [1, 2], "out": "o", "overlay": {"run": "r", "chunk": 3, "chunck": 4}}"#,
            r#"{"pack": "p", "frames": [1, 2], "out": "o", "overlay": {"run": "r"}}"#,
            r#"{"pack": "p", "frames": [1, 2], "out": "o", "overlay": {"run": "r", "chunk": 3}, "overlays": 1}"#,
        ] {
            assert!(
                serde_json::from_str::<SegmentOrder<Trail>>(refused).is_err(),
                "{refused}"
            );
        }
        // …and an order with none has none.
        let bare: SegmentOrder<Trail> =
            serde_json::from_str(r#"{"pack": "p", "frames": [1, 2], "out": "o"}"#).expect("bare");
        assert_eq!(bare.overlay, None);
        // The stock service's order says nothing, and takes nothing.
        let stock: SegmentOrder =
            serde_json::from_str(r#"{"pack": "p", "frames": [1, 2], "out": "o", "overlay": {}}"#)
                .expect("stock");
        assert_eq!(stock.overlay, Some(NoOrder {}));
        assert!(serde_json::from_str::<SegmentOrder>(
            r#"{"pack": "p", "frames": [1, 2], "out": "o", "overlay": {"run": "r"}}"#
        )
        .is_err());
        // What goes out comes back.
        let again: SegmentOrder<Trail> =
            serde_json::from_str(&serde_json::to_string(&order).expect("json")).expect("again");
        assert_eq!(again, order);
    }

    #[tokio::test]
    async fn a_segment_hands_its_overlay_order_to_the_hosts_factory_before_it_waits() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("catalog.json"), br#"{"layers": []}"#).expect("catalog");
        let runs = Arc::new(
            ObjectRunStore::local(dir.path(), tuile_farm::Tuning::default()).expect("store"),
        );
        let tiles: Arc<dyn tuile_repository::Objects> = runs.clone();
        let trails = Arc::new(Trails::default());
        let service = Service::with_overlays(
            runs,
            tiles,
            dir.path().join("cache"),
            Limits::default(),
            trails.clone(),
        );
        let order = |chunk| -> SegmentOrder<Trail> {
            serde_json::from_value(serde_json::json!({
                "pack": "packs/never.tuilepack",
                "frames": [31, 60],
                "out": "out.mp4",
                "pack_within_seconds": 0,
                "overlay": {"run": "r/1", "chunk": chunk},
            }))
            .expect("order")
        };
        // Made, then the pack waited for.
        let late = service.segment(&order(2)).await.expect_err("no pack");
        assert!(late.to_string().contains("did not come"), "{late}");
        // An order the host cannot draw fails the segment, by the host's word.
        let refused = service.segment(&order(0)).await.expect_err("refused");
        assert!(refused.to_string().contains("no chunk 0"), "{refused}");
        // A segment with no overlay order does not call the factory.
        let mut none = order(5);
        none.overlay = None;
        service.segment(&none).await.expect_err("no pack");
        let asked = trails.0.lock().expect("lock").clone();
        let trail = |chunk| Trail {
            run: "r/1".into(),
            chunk,
        };
        assert_eq!(asked, vec![(trail(2), (31, 60)), (trail(0), (31, 60))]);
    }

    struct Heard(tokio::sync::mpsc::UnboundedSender<(serde_json::Value, Result<u64, String>)>);

    #[async_trait::async_trait]
    impl Notify for Heard {
        async fn segment_done(&self, order: &SegmentOrder, outcome: Result<&SegmentDone, String>) {
            let _ = self
                .0
                .send((order.context.clone(), outcome.map(|d| d.frames)));
        }
    }

    #[tokio::test]
    async fn a_segment_accepted_in_the_background_says_how_it_ended_with_its_context() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("catalog.json"), br#"{"layers": []}"#).expect("catalog");
        let runs = Arc::new(
            ObjectRunStore::local(dir.path(), tuile_farm::Tuning::default()).expect("store"),
        );
        let tiles: Arc<dyn tuile_repository::Objects> = runs.clone();
        let service = Arc::new(Service::new(
            runs,
            tiles,
            dir.path().join("cache"),
            Limits::default(),
        ));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let order: SegmentOrder = serde_json::from_value(serde_json::json!({
            "pack": "packs/never.tuilepack",
            "frames": [1, 2],
            "out": "out.mp4",
            "pack_within_seconds": 0,
            "context": {"workflow": "film-1", "chunk": 3},
        }))
        .expect("order");
        service.accept(order, Arc::new(Heard(tx)));
        let (context, outcome) = rx.recv().await.expect("told");
        assert_eq!(
            context,
            serde_json::json!({"workflow": "film-1", "chunk": 3})
        );
        assert!(outcome.expect_err("no pack").contains("did not come"));
    }
}
