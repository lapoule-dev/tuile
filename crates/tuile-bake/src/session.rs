// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! One streaming session, driven a frame at a time.
//!
//! Everything the C ABI exposes is a thin projection of what is here, so this
//! module is testable in plain Rust without going through the boundary.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use glam::DVec3;
use tuile_core::budget::{BudgetConfig, DrawItem, Generation, ResourceId, Role};
use tuile_core::content::{DecodedMesh, DecodedTileContent, DrapeContent};
use tuile_core::drive::SceneState;
use tuile_core::protocol::{
    ClientMessage, GeometryStream, ResourceContent, ServerMessage, StreamError,
};
use tuile_core::runtime::in_process_with;
use tuile_core::source::{TileId, TileLoader, TileTree};
use tuile_core::traversal::{Config, ViewState, ViewStateParams};

/// What a renderer must decide before the first frame.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub traversal: Config,
    /// The budget the session's geometry server runs on:
    /// [`BudgetConfig::batch`] — unbounded, exact, never cancelling what a
    /// frame needs. A frame is answered when the server says every mesh and
    /// drape it needs is held (`frame_ready`, I13); a load that takes longer
    /// than its `load_timeout` or fails ends the session with the reason
    /// (`FailurePolicy::EndSession`): a bake never ships a hole. The
    /// timeout is 120 s by default and the host's to set — here, or by
    /// `TUILE_LOAD_TIMEOUT_S` — to cover its client's retry envelope.
    /// [`exact_budget`] applies the environment's knobs to it.
    pub budget: BudgetConfig,
    /// Longest a single frame may take to become ready.
    ///
    /// The outer bound, over the budget's per-load `load_timeout`: a frame
    /// of thousands of loads that each arrive in time can still take a
    /// long while, and a node that stopped must be told from one that is
    /// working. This turns a frame that never becomes ready into a
    /// reported failure.
    pub frame_timeout: Duration,
    /// Whether a frame that converged but lost tiles along the way is an error.
    ///
    /// Default `true`, and it should stay true anywhere the output is kept. A
    /// failed tile does not leave a hole: its ancestor stands in, so the frame
    /// renders plausibly at the wrong level of detail. That is the worst kind of
    /// defect — invisible until someone compares two frames rendered on
    /// different machines.
    pub fail_on_tile_errors: bool,
    /// Names the data this session serves, and scopes every asset URI it hands
    /// out. See [`Frame::texture_uri`] for why it exists and why it is a name
    /// rather than a counter.
    ///
    /// It must be stable for the same sources and distinct for different ones.
    /// [`Session::globe`] derives it from the ion asset ids; a host wiring its
    /// own sources chooses its own, and choosing badly means one session's
    /// textures answering another's requests.
    pub dataset: String,
    /// Largest side of the per-tile imagery mosaic baked before the boundary.
    ///
    /// Draped imagery crosses the ABI as one owned texture per tile
    /// ([`tuile_core::raster::bake_imagery`]) — a Hydra host authors one
    /// `UsdUVTexture` per tile and knows nothing about layers. This caps the
    /// bake; the bake itself picks the finest layer's scale below it.
    pub bake_max_size: u32,
    /// What each tile is finished into. See [`Finish`].
    pub finish: Finish,
    /// What the session keeps, in bytes, of what frames leave behind: once
    /// a frame is ready, everything the frame did not need is let go — both
    /// copies, the session's and the server's — down to this
    /// (`ClientMessage::ReleaseBeyond`, deterministic: the same flight lets
    /// go of the same resources). A frame never waits on less for it —
    /// nothing a frame needs is ever let go — so this bounds a long
    /// flight's memory, never a frame's; what was let go is loaded again
    /// when a later frame needs it. `None` keeps everything.
    /// `TUILE_BAKE_KEEP_GIB` when the environment says.
    pub release_beyond: Option<u64>,
}

/// What a session makes of a tile once its content has arrived.
///
/// A renderer is handed geometry and a picture. A bake that writes a pack
/// which only *refers* to the tile store is handed neither: such a pack names
/// each tile, its drape, and where both come from in the store, and whoever
/// renders it reads the terrain and the imagery there. Composing a mosaic and
/// encoding it to PNG for that pack is the cost of a frame — CPU, and the
/// memory of every picture held until the film is over — spent on bytes
/// nothing ever reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Finish {
    /// The mesh and the drape's mosaic, composed and encoded: what a renderer
    /// draws, and what a pack that carries its tiles stores.
    #[default]
    Pictures,
    /// The tile's identity only — its drape's fingerprint and what it refers
    /// to in the tile store. Nothing is composed, nothing encoded, and the
    /// mesh and the imagery are let go as soon as they have been named. The
    /// selection, the drapes and the fetches are exactly those of
    /// [`Finish::Pictures`]; a frame's tiles then carry no geometry, so this
    /// is for a pack by reference and nothing that draws.
    Identity,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            traversal: Config::default(),
            // Each load is bounded by the budget's `load_timeout` (120 s in
            // `BudgetConfig::batch`): a load that has not answered by then
            // ends the session, with the resource it was waiting on named
            // (`FailurePolicy::EndSession`). The store's patient retries
            // (`RetryConfig::patient`) run inside that bound.
            budget: BudgetConfig::batch(),
            // And the frame as a whole by this: thousands of loads that each
            // answer in time can still take a while, and a node that stopped
            // must be told from one that is working. Fifteen minutes: the
            // job is capped at 7200 s for 2880 frames, so one stuck frame
            // costs an eighth of the run and then *reports*, instead of
            // sitting in `wait` until the platform kills it with no reason
            // attached.
            frame_timeout: Duration::from_secs(900),
            fail_on_tile_errors: true,
            // Deliberately not "" — an empty dataset would collapse the URI to
            // `tuile:///tile/...`, which resolves but scopes nothing.
            dataset: "default".into(),
            // TUILE_BAKE_MAX caps the baked mosaic's side per job: sharper
            // shots raise it where the RAM is real, a laptop lowers it.
            bake_max_size: env_knob("TUILE_BAKE_MAX", 2048_u32).clamp(64, 8192),
            finish: Finish::Pictures,
            release_beyond: std::env::var("TUILE_BAKE_KEEP_GIB")
                .ok()
                .and_then(|gib| gib.parse::<f64>().ok())
                .map(|gib| (gib * f64::from(1u32 << 30)) as u64),
        }
    }
}

/// Why a frame did not produce geometry.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame did not converge within {0:?}")]
    TimedOut(Duration),
    #[error("the geometry server hung up")]
    ServerGone,
    #[error("{count} tile(s) failed to load; first: {first}")]
    TilesFailed { count: usize, first: String },
    /// The selection named tiles whose content the session never received.
    ///
    /// Unreachable while the eager contract holds, and loud on purpose if it
    /// ever stops: the server sends a tile's `Content` **once per residency**,
    /// so a consumer that keeps its own residency and drops a tile has no way
    /// to ask for it again — it would simply render a hole.
    #[error("{count} selected tile(s) had no content; first: {first:?}")]
    MissingContent { count: usize, first: TileId },
    #[error("encoding a texture: {0}")]
    TextureEncode(String),
    /// A thread panicked while holding the frame's encode map. Only reachable
    /// if something above already went wrong, but the boundary must not turn it
    /// into a second panic.
    #[error("the frame's texture cache was left poisoned")]
    Poisoned,
}

impl TextureKey {
    /// The drape's fingerprint. Zero for a texture the tile owns rather than
    /// one we composed.
    pub(crate) fn drape(&self) -> u64 {
        self.drape
    }
}

/// One tile's geometry, ready to hand to a renderer.
///
/// Positions are f32 **relative to `origin_ecef`** — the anti-jitter protocol.
/// A consumer places the tile with a double-precision transform built from
/// `origin_ecef` and its own render origin; going through f32 for that
/// subtraction is exactly what the protocol exists to avoid.
#[derive(Debug)]
pub struct TileGeometry {
    pub tile: TileId,
    pub origin_ecef: DVec3,
    pub content: DecodedTileContent,
    /// What this tile's baked mosaic is keyed by in the session's memo, when
    /// it has one. Computed before the bake so a repeat can be recognised.
    pub(crate) baked: Option<TextureKey>,
    /// The baked texture, when an earlier frame already produced it.
    ///
    /// Held as an `Arc` rather than looked up again on demand: the memo
    /// evicts, and a frame that promised a texture must be able to hand out
    /// its bytes however long it lives.
    pub(crate) memoized: Option<Arc<EncodedTexture>>,
    /// Where the tile comes from in the tile store, when the session's
    /// source notes it: the terrain tile its mesh was built from, and the
    /// imagery tiles of its drape with their placement.
    pub(crate) refs: Option<tuile_pack::TileRefs>,
    /// What is left of the mesh when the tile was finished by identity
    /// ([`Finish::Identity`]): its counts, which a pack by reference records
    /// beside each tile. `None` for a tile that still has its mesh.
    pub(crate) shape: Option<Shape>,
}

/// The size of a mesh that was let go, and its base colour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Shape {
    pub vertex_count: u32,
    pub index_count: u32,
    pub base_color_factor: [f32; 4],
}

impl TileGeometry {
    /// One tile as a pack hands it over: geometry already decoded, texture
    /// already encoded, nothing left to bake.
    ///
    /// The drape travels through a `TextureKey` rather than a field of its
    /// own, because that is what the key *is* — (tile, drape, index) — and a
    /// second place to put it is a second place for the two to disagree. The
    /// texture is `memoized`, which is exactly the path a live session takes
    /// for a drape an earlier frame already baked: `Frame::texture_png` hands
    /// it back untouched and nothing is re-encoded.
    pub(crate) fn packed(
        tile: TileId,
        origin_ecef: DVec3,
        content: DecodedTileContent,
        drape: u64,
        memoized: Option<Arc<EncodedTexture>>,
    ) -> Self {
        Self {
            tile,
            origin_ecef,
            content,
            baked: Some(TextureKey {
                tile,
                drape,
                texture_index: 0,
            }),
            memoized,
            refs: None,
            shape: None,
        }
    }

    /// What the baked mosaic on this tile was composed from, or `0` when the
    /// tile carries no drape of ours.
    ///
    /// Crossing the boundary so the consumer can tell "the same tile, redraped"
    /// from "the same tile, unchanged" with an integer compare — the one
    /// distinction that decides whether a kept prim needs its material dirtied.
    ///
    /// Public because a bake needs it for the same reason a renderer does, and
    /// for one more: it is half of a tile's identity in a pre-baked pack. A
    /// pack that deduplicated on the id alone would hand the last frame of a
    /// shot the imagery of the first.
    pub fn drape(&self) -> u64 {
        self.baked.map_or(0, |key| key.drape())
    }
}

/// A texture, as the renderer will ask for it.
#[derive(Debug)]
pub struct EncodedTexture {
    /// The `tuile://` URI a material points at, and the key the host's asset
    /// resolver will be handed back.
    pub uri: String,
    /// PNG bytes.
    pub png: Vec<u8>,
}

/// What makes one tile's texture unique across frames.
///
/// Identity alone is not enough. The same tile is re-draped at a different
/// imagery level when the camera moves, and serving the previous drape would
/// pin the ground at the sharpness of a frame that is gone. What decides the
/// pixels is the set of layers that went into the bake — so that is what
/// decides the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TextureKey {
    tile: TileId,
    /// Hash of the drape: every layer coordinate, in order, plus the bake
    /// size. Zero for a texture the tile owns rather than one we composed.
    drape: u64,
    texture_index: usize,
}

/// Baked, encoded textures kept **between** frames.
///
/// The reason this exists, measured on the farm: a camera orbit re-selects
/// almost the same tiles every frame, and without a memo each one is re-baked
/// (a 2048² mosaic composed from its layers) and re-encoded to PNG — around
/// sixty seconds of pure CPU per frame, on a GPU that draws it in
/// milliseconds. With it, only tiles that are new or re-draped pay.
///
/// Bounded on purpose. This holds megabytes per tile and a long shot visits
/// thousands: `TUILE_TEXTURE_MEMO_MB` (default 1024) caps it, and the
/// least-recently-used entries go first. Eviction is safe at any moment
/// because every frame holds an `Arc` to the textures it handed out.
#[derive(Debug)]
pub(crate) struct TextureMemo {
    inner: Mutex<MemoInner>,
    budget_bytes: usize,
}

#[derive(Debug, Default)]
struct MemoInner {
    entries: HashMap<TextureKey, (Arc<EncodedTexture>, u64)>,
    bytes: usize,
    clock: u64,
}

impl Default for TextureMemo {
    fn default() -> Self {
        Self::new(env_knob("TUILE_TEXTURE_MEMO_MB", 1024_usize).max(1) * 1024 * 1024)
    }
}

impl TextureMemo {
    pub(crate) fn new(budget_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(MemoInner::default()),
            budget_bytes,
        }
    }

    /// The texture for `key`, if it is still held. Marks it as just used.
    pub(crate) fn get(&self, key: TextureKey) -> Option<Arc<EncodedTexture>> {
        // A poisoned memo degrades to a miss: it costs a re-bake, and that is
        // strictly better than failing a render over a cache.
        let mut inner = self.inner.lock().ok()?;
        inner.clock += 1;
        let clock = inner.clock;
        let (entry, used) = inner.entries.get_mut(&key)?;
        *used = clock;
        Some(Arc::clone(entry))
    }

    /// Records `entry` under `key`, evicting the oldest entries if the budget
    /// is exceeded.
    pub(crate) fn insert(&self, key: TextureKey, entry: &Arc<EncodedTexture>) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.clock += 1;
        let clock = inner.clock;
        let size = entry.png.len();
        if inner
            .entries
            .insert(key, (Arc::clone(entry), clock))
            .is_none()
        {
            inner.bytes += size;
        }
        if inner.bytes <= self.budget_bytes {
            return;
        }
        // Bulk-evict down to 80 % rather than one entry at a time: the sort is
        // paid once instead of on every insert while a big frame streams in.
        let mut ages: Vec<(u64, TextureKey)> = inner
            .entries
            .iter()
            .map(|(k, (_, used))| (*used, *k))
            .collect();
        ages.sort_unstable_by_key(|(used, _)| *used);
        let target = self.budget_bytes * 4 / 5;
        for (_, key) in ages {
            if inner.bytes <= target {
                break;
            }
            if let Some((entry, _)) = inner.entries.remove(&key) {
                inner.bytes -= entry.png.len();
            }
        }
    }
}

/// A converged answer for one camera.
pub struct Frame {
    /// Tiles in traversal order.
    ///
    /// Ordered on purpose: the driver also returns a `HashMap`, whose iteration
    /// order is seeded per process, so a consumer that walked it would emit
    /// prims in a different order on every run — and a farm comparing two
    /// renders of the same frame would see a difference that is not there.
    pub tiles: Vec<Arc<TileGeometry>>,
    /// Textures encoded so far, keyed by (tile index, texture index).
    ///
    /// Filled on demand and never evicted, because the C boundary hands out
    /// borrows that stay valid until the frame is freed. Encoding is deferred
    /// rather than done up front for the ordinary reason: a renderer asks for
    /// the textures it will actually sample, which after frustum and material
    /// culling is rarely all of them.
    encoded: Mutex<HashMap<(usize, usize), Arc<EncodedTexture>>>,
    /// Textures kept across frames. The per-frame map above is what keeps a
    /// handed-out borrow alive; this is what stops the work being redone.
    memo: Arc<TextureMemo>,
    /// Scopes this frame's asset URIs. See [`Frame::texture_uri`].
    dataset: Arc<str>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("tiles", &self.tiles.len())
            .finish_non_exhaustive()
    }
}

impl Frame {
    /// Builds a frame from its tiles, with nothing encoded yet.
    pub fn new(dataset: impl Into<Arc<str>>, tiles: Vec<Arc<TileGeometry>>) -> Self {
        Self::with_memo(dataset, tiles, Arc::new(TextureMemo::default()))
    }

    /// Builds a frame that shares a session's texture memo.
    pub(crate) fn with_memo(
        dataset: impl Into<Arc<str>>,
        tiles: Vec<Arc<TileGeometry>>,
        memo: Arc<TextureMemo>,
    ) -> Self {
        Self {
            tiles,
            encoded: Mutex::new(HashMap::new()),
            memo,
            dataset: dataset.into(),
        }
    }

    /// What scopes this frame's asset URIs.
    pub fn dataset(&self) -> &str {
        &self.dataset
    }

    /// The URI under which a tile's texture is published.
    ///
    /// Defined here, in one place, because both sides must agree: the material
    /// a consumer authors names this string and the asset resolver is handed it
    /// back verbatim. Deriving it independently on each side is how they drift
    /// and every texture silently resolves to nothing.
    ///
    /// # Why the dataset is in the path
    ///
    /// A [`TileId`] is unique **within one tree**, not globally. Two sessions —
    /// two ion assets, or a terrain-only globe beside a textured one — hand out
    /// the same ids for different tiles, and a resolver keyed on the id alone
    /// would serve one session's texture to the other. Silently: the bytes are
    /// a valid PNG, so nothing errors, and the wrong imagery simply appears.
    ///
    /// # Why a name and not a counter
    ///
    /// The dataset is derived from what the session reads — for an ion globe,
    /// its asset ids — rather than assigned from a counter at open time. A
    /// counter depends on the order sessions happen to be opened, so two farm
    /// nodes rendering the same frame would emit different asset paths for
    /// identical data, and a comparison would report a difference that is not
    /// there. Same reason `BulkFrame.selected` is iterated rather than its
    /// `HashMap`.
    /// # Why the drape is in the name
    ///
    /// A tile keeps its id when the camera moves, but its imagery does not: it
    /// is re-draped at another level and the pixels change underneath. With a
    /// name that ignored that, every asset cache between here and the renderer
    /// would go on serving the picture it already had — the tile would render,
    /// plausibly, at yesterday's sharpness. Naming the bytes rather than the
    /// slot makes a re-drape a different asset, which is the only way a cache
    /// can be right by construction.
    pub fn texture_uri(dataset: &str, tile: TileId, texture: usize, drape: u64) -> String {
        format!(
            "tuile://{dataset}/tile/{}/texture/{texture}.{drape:016x}.png",
            tile.0
        )
    }

    /// The PNG for one tile's texture, encoding it on first request.
    ///
    /// Returns `None` for an out-of-range index. The `Arc` is what makes the
    /// borrow handed across the C boundary safe: the bytes live as long as the
    /// frame, whatever the caller does with the map afterwards.
    pub fn texture_png(
        &self,
        tile_index: usize,
        texture_index: usize,
    ) -> Result<Option<Arc<EncodedTexture>>, FrameError> {
        let Some(tile) = self.tiles.get(tile_index) else {
            return Ok(None);
        };

        let key = (tile_index, texture_index);
        if let Some(hit) = self
            .encoded
            .lock()
            .map_err(|_| FrameError::Poisoned)?
            .get(&key)
        {
            return Ok(Some(Arc::clone(hit)));
        }

        // Already baked and encoded by an earlier frame: the bake was skipped,
        // so this is the only copy that exists.
        if texture_index == 0 {
            if let Some(hit) = &tile.memoized {
                let mut map = self.encoded.lock().map_err(|_| FrameError::Poisoned)?;
                return Ok(Some(Arc::clone(
                    map.entry(key).or_insert_with(|| Arc::clone(hit)),
                )));
            }
        }

        let Some(texture) = tile.content.textures.get(texture_index) else {
            return Ok(None);
        };

        // Encoded outside the lock: this is milliseconds of CPU per texture,
        // and holding the map while it runs would serialise every other tile's
        // encode behind it for no reason.
        let rgba8 = texture.rgba8().ok_or_else(|| {
            FrameError::TextureEncode("block-compressed texels cannot be written as PNG".into())
        })?;
        let mut png = Vec::new();
        image::write_buffer_with_format(
            &mut std::io::Cursor::new(&mut png),
            rgba8,
            texture.width,
            texture.height,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .map_err(|e| FrameError::TextureEncode(e.to_string()))?;

        let entry = Arc::new(EncodedTexture {
            uri: Self::texture_uri(&self.dataset, tile.tile, texture_index, tile.drape()),
            png,
        });

        // Another thread may have won the race; keep whichever landed first so
        // every caller sees one buffer at one address.
        let mut map = self.encoded.lock().map_err(|_| FrameError::Poisoned)?;
        let entry = Arc::clone(map.entry(key).or_insert_with(|| Arc::clone(&entry)));
        drop(map);
        // Offered to the next frame. Only a baked drape is worth keeping: a
        // texture the tile owns is decoded from its own content anyway.
        if let (0, Some(baked)) = (texture_index, tile.baked) {
            self.memo.insert(baked, &entry);
        }
        Ok(Some(entry))
    }
}

impl std::fmt::Debug for Session {
    /// Deliberately says almost nothing. A live session owns a tokio runtime,
    /// a server thread and a stream, none of which have a useful textual form
    /// — and its configuration can carry an ion token, which must not end up
    /// in a log line because someone printed a `Result`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("source", &if self.is_packed() { "pack" } else { "live" })
            .finish_non_exhaustive()
    }
}

/// A live session over one tile source: the streaming half.
pub(crate) struct Live {
    stream: tuile_core::protocol::InProcessStream,
    runtime: tokio::runtime::Runtime,
    config: SessionConfig,
    /// The ground's heights as the resident terrain gives them, when the
    /// source keeps them (a globe does). See [`Session::heights`].
    heights: Option<Arc<tuile_terrain::TerrainHeights>>,
    /// What each tile is made from, when the source notes it (a globe does).
    provenance: Option<Arc<tuile_planetary::Provenance>>,
    /// Baked, encoded textures kept across frames. See [`TextureMemo`].
    textures: Arc<TextureMemo>,
    /// What the consumer holds of the stream: each mesh and drape as the
    /// server delivered it.
    ///
    /// The server sends a resource's `Content` **once per residency** and
    /// never again — the invariant the whole streaming protocol is built on.
    /// So what came is kept here, exactly as the interactive viewer keeps
    /// it: filled by `Content`, emptied by `Evict`, everything else survives.
    held: Held,
    /// Each drawn tile, finished from the mesh and the drapes it wears, with
    /// that dressing: a tile drawn again in the same dressing is not
    /// finished again.
    resident: HashMap<TileId, Finished>,
    /// The consumer's view of the selection, rebuilt from the server's
    /// messages and **kept across frames**.
    scene: SceneState,
    /// Monotone, one per frame. Echoed back on every `Select`, which is how a
    /// selection is known to answer the current camera rather than the
    /// previous one.
    generation: u64,
    /// What is kept of every resident tile's terrain, when the session is
    /// asked to keep it ([`Session::keep_ground`]): what
    /// [`Session::ground`] draws the ground from, whatever `finish` lets go.
    grounds: Option<Grounds>,
    /// The ground last drawn, tile by tile, prepared and held for
    /// stitching: a tile selected again is not prepared again.
    drawer: crate::drawer::GroundDrawer,
    /// Kept alive for as long as the session: dropping it ends the server.
    _server: std::thread::JoinHandle<()>,
}

/// A session over one scene, from one of two places.
///
/// The split is the whole architecture in one type. A **live** session
/// resolves ion and Bing, traverses, loads, drapes, bakes and encodes — the
/// CPU work that is 89 % of a farm job's cost and the reason a render job was
/// paying for a globe it could have been handed. A **packed** session opens a
/// file.
///
/// What a packed session does not have is the point of it: no network, no ion
/// token, no traversal, no convergence. A frame it answers is not reproducible
/// because a race was stabilised; there is no race.
pub struct Session(Source);

enum Source {
    Live(Box<Live>),
    Packed(crate::packed::Packed),
}

impl Session {
    /// Reads a scene from a pack instead of from the network.
    ///
    /// `scene` is the digest the caller believes it is rendering. A pack that
    /// answers a different one is refused here rather than rendered: that is
    /// the one failure a split pipeline adds and a live one does not have —
    /// rendering last week's bake of another trajectory, successfully.
    pub fn from_pack(
        path: &std::path::Path,
        scene: Option<&str>,
    ) -> Result<Self, crate::packed::PackedError> {
        Ok(Self(Source::Packed(crate::packed::Packed::open(
            path, scene,
        )?)))
    }

    /// Whether this session reads a pack rather than the network.
    pub fn is_packed(&self) -> bool {
        matches!(self.0, Source::Packed(_))
    }

    /// Starts a live session over an already-resolved tree and loader.
    pub fn new(
        tree: Box<dyn TileTree>,
        loader: Arc<dyn TileLoader>,
        config: SessionConfig,
    ) -> std::io::Result<Self> {
        Ok(Self(Source::Live(Box::new(Live::new(
            tree, loader, config,
        )?))))
    }

    pub(crate) fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
        Live::runtime()
    }

    pub(crate) fn from_parts(
        runtime: tokio::runtime::Runtime,
        tree: Box<dyn TileTree>,
        loader: Arc<dyn TileLoader>,
        config: SessionConfig,
    ) -> std::io::Result<Self> {
        Ok(Self(Source::Live(Box::new(Live::from_parts(
            runtime, tree, loader, config,
        )?))))
    }

    pub(crate) fn set_heights(&mut self, heights: Arc<tuile_terrain::TerrainHeights>) {
        if let Source::Live(live) = &mut self.0 {
            live.heights = Some(heights);
        }
    }

    pub(crate) fn set_provenance(&mut self, provenance: Arc<tuile_planetary::Provenance>) {
        if let Source::Live(live) = &mut self.0 {
            live.provenance = Some(provenance);
        }
    }

    /// The ground's heights, read from the terrain this session has resident.
    ///
    /// The same terrain the frames are selected from, so a height read here is
    /// the height the pack shows: a caller that places a camera against the
    /// ground — clearing a ridge between the eye and what it films — decides
    /// on the ground the bake will draw, not on a second copy of it. `None` for
    /// a packed session and for a live one over a source that keeps no heights.
    pub fn heights(&self) -> Option<Arc<tuile_terrain::TerrainHeights>> {
        match &self.0 {
            Source::Live(live) => live.heights.clone(),
            Source::Packed(_) => None,
        }
    }

    /// Keeps, from now on, the terrain mesh of every tile that becomes
    /// resident, for [`Self::ground`] — whatever the session's `finish`
    /// does with it. Tiles already resident have none. Nothing for a packed
    /// session.
    pub fn keep_ground(&mut self) {
        if let Source::Live(live) = &mut self.0 {
            live.grounds.get_or_insert_with(Grounds::default);
        }
    }

    /// [`Self::keep_ground`], and beside what it keeps, every mesh as it
    /// came — the whole of it, which is what keeping the ground used to
    /// cost — for [`Self::ground_as_it_came`]. For tests that compare the
    /// two grounds on one session; never for a bake.
    #[doc(hidden)]
    pub fn keep_ground_and_meshes(&mut self) {
        if let Source::Live(live) = &mut self.0 {
            live.grounds
                .get_or_insert_with(Grounds::default)
                .as_it_came
                .get_or_insert_with(HashMap::new);
        }
    }

    /// The ground [`Self::ground`] draws, drawn the way it was before the
    /// meshes were kept small: from the meshes as they came, each tile
    /// prepared again and stitched on a stitching of its own. The same
    /// selection, the same functions; `None` unless the session was asked
    /// to [keep the meshes](Self::keep_ground_and_meshes).
    #[doc(hidden)]
    pub fn ground_as_it_came(
        &self,
        how: tuile_film::ground::Prepare,
        stitch: bool,
    ) -> Option<tuile_film::ground::Ground> {
        let Source::Live(live) = &self.0 else {
            return None;
        };
        let grounds = live.grounds.as_ref()?;
        let meshes = grounds.as_it_came.as_ref()?;
        let mut stitching = tuile_film::stitching::Stitching::default();
        let mut keys = Vec::new();
        for tile in &drawn_tiles(&live.scene) {
            let (Some((origin, mesh)), true) = (meshes.get(tile), grounds.kept.contains_key(tile))
            else {
                continue;
            };
            let key = tuile_film::TileKey {
                id: tile.0,
                drape: 0,
            };
            let mesh = tuile_film::stitching::encoded(mesh, origin.to_array(), [1.0; 4]);
            let mesh = tuile_film::ground::prepared(mesh, how).0;
            let source = live
                .resident
                .get(tile)
                .and_then(|f| f.geometry.refs.as_ref())
                .map(|refs| u32::from(refs.terrain.level));
            stitching.enter(key, source, &mesh);
            keys.push(key);
        }
        if stitch {
            stitching.frame(&keys);
        }
        Some(tuile_film::ground::Ground::of(&stitching, &keys))
    }

    /// The ground the last frame's selection draws, as a film's renderer
    /// draws it: each tile's mesh prepared as `how` says (skirts,
    /// smoothing), then stitched to its neighbours when `stitch` — the
    /// renderer's own functions (`tuile_film::ground`). A camera placed
    /// against it is placed against the picture.
    ///
    /// `None` for a packed session, and for a live one not asked to
    /// [keep the ground](Self::keep_ground).
    pub fn ground(
        &mut self,
        how: tuile_film::ground::Prepare,
        stitch: bool,
    ) -> Option<tuile_film::ground::Ground> {
        let selected = self.selected_ground()?;
        let Source::Live(live) = &mut self.0 else {
            return None;
        };
        let ground = live.drawer.ground(&selected, how, stitch);
        tracing::debug!(memory = ?self.ground_memory(), "ground");
        Some(ground)
    }

    /// The last frame's selection, frozen for its ground: each selected
    /// tile whose terrain the session keeps, with that terrain. A
    /// [`crate::GroundDrawer`] builds its ground — anywhere, any time
    /// after — exactly as [`Self::ground`] would have now. `None` where
    /// [`Self::ground`] answers none.
    pub fn selected_ground(&self) -> Option<crate::drawer::Selected> {
        let Source::Live(live) = &self.0 else {
            return None;
        };
        let grounds = live.grounds.as_ref()?;
        Some(crate::drawer::Selected::new(
            drawn_tiles(&live.scene)
                .into_iter()
                .filter_map(|tile| {
                    let (_, kept) = grounds.kept.get(&tile)?;
                    Some(crate::drawer::SelectedTile {
                        tile,
                        kept: Arc::clone(kept),
                        source: live
                            .resident
                            .get(&tile)
                            .and_then(|f| f.geometry.refs.as_ref())
                            .map(|refs| u32::from(refs.terrain.level)),
                    })
                })
                .collect(),
        ))
    }

    /// What the last frame converged on, as one number: its selected tiles,
    /// in order ([`tuile_core::determinism::digest`]). Two runs that agree
    /// on every frame's digest selected the same tiles every frame. `None`
    /// for a packed session.
    pub fn selection_digest(&self) -> Option<u64> {
        match &self.0 {
            Source::Live(live) => Some(tuile_core::determinism::digest(
                drawn_tiles(&live.scene).iter().map(|t| t.0),
            )),
            Source::Packed(_) => None,
        }
    }

    /// The bytes the ground a camera is kept over takes: what is kept of
    /// every resident tile, what is prepared of the selected ones, and what
    /// the last [`Self::ground`] built on them. `None` where
    /// [`Self::ground`] answers none.
    pub fn ground_memory(&self) -> Option<GroundMemory> {
        let Source::Live(live) = &self.0 else {
            return None;
        };
        let grounds = live.grounds.as_ref()?;
        let (prepared_tiles, prepared_bytes) = live.drawer.prepared();
        Some(GroundMemory {
            kept_tiles: grounds.kept.len(),
            kept_bytes: grounds.kept.values().map(|(_, k)| k.heap_bytes()).sum(),
            prepared_tiles,
            prepared_bytes,
            drawn_bytes: live.drawer.built(),
        })
    }

    /// Resolves one frame for the given views.
    ///
    /// The two sources answer the same question by opposite means: one
    /// converges a traversal against a network, the other looks up the camera
    /// in a table. Which is why the packed side takes the *views* rather than
    /// a frame number — a Hydra host cooks at a timecode and hands a session a
    /// camera, and asking it for a frame number would mean changing the ABI
    /// and the procedural for a fact the camera already carries.
    pub fn frame(&mut self, views: Vec<ViewStateParams>) -> Result<Frame, FrameError> {
        match &mut self.0 {
            Source::Live(live) => live.frame(views),
            Source::Packed(packed) => packed.frame(&views),
        }
    }

    /// One frame of a camera path: **one pose, one convergence**.
    ///
    /// This is how an offline consumer drives a session, and the fact that it
    /// is *one* view is the whole content of the method. [`Self::frame`] takes
    /// a vector because a stereo rig or a cube map genuinely has several eyes
    /// looking at once; a film does not. A film has one eye that moves, and
    /// the temptation — converging a whole stretch of a path at once, on the
    /// theory that frusta which overlap by 99% ought to cost about as much as
    /// one — is a trap.
    ///
    /// It is a trap because a traversal keeps re-running as tiles land, and
    /// every re-run evaluates every view it was given. The work goes as
    /// *arrivals × views*, not as tiles. Measured over Cesium World Terrain at
    /// 3840×2160 and SSE 3, from cold:
    ///
    /// | views in one convergence | tiles selected | wall clock |
    /// |---|---|---|
    /// | 6 | 1711 | 28.6 s |
    /// | 60 | 3401 | 365 s |
    /// | 300 | — | did not converge in 900 s |
    ///
    /// Ten times the views buys twice the tiles and thirteen times the wait,
    /// and enough of them buys nothing at all. Frame by frame costs nothing
    /// extra in exchange, because residency carries: a camera moves a few
    /// metres between frames and re-selects almost the same ground.
    ///
    /// The viewport belongs to the call rather than to the pose because a pack
    /// is the bake of the viewport it was made for, while a tape of poses is
    /// not: the same path can be filmed at two sizes, and they are two packs.
    pub fn frame_for_pose(
        &mut self,
        pose: &tuile_tape::Frame,
        viewport: (f64, f64),
    ) -> Result<Frame, FrameError> {
        self.frame(vec![pose_view(pose, viewport)])
    }

    /// The views of the frames that come next, in their order: their tiles
    /// are loaded while this session converges and places the ones before
    /// — with the load slots the current frame leaves, never in its place —
    /// and kept between frames ([`SessionConfig::release_beyond`]) until
    /// they are drawn. Replaces the views given before. What any frame
    /// selects is the same with or without them: only when its tiles
    /// arrive changes (`ClientMessage::Ahead`). Nothing for a packed
    /// session, which loads nothing.
    pub fn ahead(&mut self, views: Vec<ViewStateParams>) -> Result<(), FrameError> {
        let Source::Live(live) = &mut self.0 else {
            return Ok(());
        };
        let views: Vec<ViewState> = views.into_iter().map(Into::into).collect();
        live.stream
            .send(ClientMessage::Ahead { views })
            .map_err(|_| FrameError::ServerGone)
    }
}

/// How many frames ahead of the one it converges a bake announces
/// ([`Session::ahead`]), unless `TUILE_BAKE_AHEAD` says: a tuning of how
/// far the loads may run before the frames that need them.
pub(crate) fn frames_ahead() -> usize {
    count_knob("TUILE_BAKE_AHEAD").unwrap_or(FRAMES_AHEAD)
}

/// [`frames_ahead`]'s default.
const FRAMES_AHEAD: usize = 8;

/// The frames a bake announces at the frame `index` of its sequence
/// ([`Session::ahead`]): every second one of the next [`frames_ahead`] —
/// a frame's tiles are its neighbours', and the session's pass over the
/// views ahead costs as many views as it is given — renewed each half of
/// that, so that the pass runs again only when the frames ahead moved on.
/// `None` where the frames announced last still hold.
pub(crate) fn announced(index: usize, len: usize) -> Option<std::ops::Range<usize>> {
    let ahead = frames_ahead();
    index
        .is_multiple_of((ahead / 2).max(1))
        .then(|| (index + 1).min(len)..(index + 1 + ahead).min(len))
}

/// The bytes the ground a camera is kept over takes
/// ([`Session::ground_memory`]): vertices and indices, not the maps' own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GroundMemory {
    /// Every resident tile's terrain, as kept for [`Session::ground`].
    pub kept_tiles: usize,
    pub kept_bytes: usize,
    /// The tiles last drawn, as prepared (skirts, smoothing) and held for
    /// stitching, with each tile as grounds read it, cut into cells — kept
    /// from one ground to the next.
    pub prepared_tiles: usize,
    pub prepared_bytes: usize,
    /// The ground it returned: its tiles as heights are read off them, with
    /// their cells. The stitching keeps them and the ground shares them, so
    /// they are in `prepared_bytes` too.
    pub drawn_bytes: usize,
}

/// What is kept of the resident tiles' terrain ([`Session::keep_ground`]).
#[derive(Default)]
struct Grounds {
    /// Each tile's, with the arrival of the mesh it was kept from — a tile
    /// that came again is told from the one before it — shared with the
    /// grounds being built of it ([`Session::selected_ground`]).
    kept: HashMap<TileId, (u64, Arc<crate::ground::Kept>)>,
    /// Each tile's mesh as it came, when asked
    /// ([`Session::keep_ground_and_meshes`]).
    as_it_came: Option<HashMap<TileId, (DVec3, DecodedMesh)>>,
}

/// What the finish pool keeps of a tile's mesh for the ground.
struct KeptTile {
    kept: crate::ground::Kept,
    as_it_came: Option<DecodedMesh>,
}

/// What the session holds of the stream, kept across frames: each mesh and
/// drape with the arrival it came by and the generation it was delivered
/// at, until the server evicts it.
#[derive(Default)]
struct Held {
    meshes: HashMap<TileId, (u64, Arc<DecodedTileContent>)>,
    drapes: HashMap<ResourceId, (u64, Arc<DrapeContent>)>,
    /// The generation each held resource came with: what a `Cancel` names.
    gens: HashMap<ResourceId, Generation>,
    /// Counts the deliveries, so that a resource that came again is told
    /// from the one before it.
    arrivals: u64,
}

impl Held {
    /// Takes in one delivery; returns the bytes it is acknowledged with.
    fn deliver(&mut self, resource: ResourceId, gen: Generation, content: ResourceContent) -> u64 {
        self.arrivals += 1;
        self.gens.insert(resource, gen);
        match content {
            ResourceContent::Mesh(mesh) => {
                let bytes = mesh.cost().gpu.0;
                self.meshes.insert(resource.tile(), (self.arrivals, mesh));
                bytes
            }
            ResourceContent::Drape(drape) => {
                let bytes = drape.cost().gpu.0;
                self.drapes.insert(resource, (self.arrivals, drape));
                bytes
            }
        }
    }

    /// Lets one resource go.
    fn forget(&mut self, resource: ResourceId) {
        self.gens.remove(&resource);
        match resource {
            ResourceId::Mesh(tile) => {
                self.meshes.remove(&tile);
            }
            ResourceId::Drape { .. } => {
                self.drapes.remove(&resource);
            }
        }
    }

    /// What a draw item is made of, as held: its mesh's arrival and each
    /// worn drape's. `None` while one of them is not held.
    fn dressing(&self, item: &DrawItem) -> Option<Dressing> {
        let (mesh, _) = self.meshes.get(&item.mesh.tile())?;
        let drapes = item
            .drapes
            .iter()
            .map(|c| self.drapes.get(&c.resource).map(|(a, _)| (c.resource, *a)))
            .collect::<Option<Vec<_>>>()?;
        Some(Dressing {
            mesh: *mesh,
            drapes,
        })
    }
}

/// What a finished tile was finished from: the arrival of its mesh and of
/// each drape it wears. Two equal dressings make the same tile.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Dressing {
    mesh: u64,
    drapes: Vec<(ResourceId, u64)>,
}

/// A drawn tile, finished, with the dressing it was finished from.
struct Finished {
    dressing: Dressing,
    geometry: Arc<TileGeometry>,
}

/// The tiles a frame draws: every drawn mesh but a fallback (a coarser
/// ancestor standing in for ground that is not its own), in the draw set's
/// order — by tile.
fn drawn_tiles(scene: &SceneState) -> Vec<TileId> {
    drawn_items(scene).map(|item| item.mesh.tile()).collect()
}

fn drawn_items(scene: &SceneState) -> impl Iterator<Item = &DrawItem> {
    scene
        .draw()
        .items
        .iter()
        .filter(|item| item.role != Role::Fallback)
}

/// A tape pose as the traversal's own camera.
///
/// One conversion, in one place. It was written out by hand at every call
/// site, and a consumer that transcribes four vectors is a consumer that can
/// transcribe them wrongly — the failure being a pack that answers no camera,
/// which costs a whole execution to discover.
pub fn pose_view(pose: &tuile_tape::Frame, viewport: (f64, f64)) -> ViewStateParams {
    ViewStateParams::perspective(
        glam::DVec3::from_array(pose.position),
        glam::DVec3::from_array(pose.direction),
        glam::DVec3::from_array(pose.up),
        glam::dvec2(viewport.0, viewport.1),
        pose.fovy,
    )
}

impl Live {
    /// Starts a session over an already-resolved tree and loader.
    ///
    /// The host resolves its own sources — this crate knows nothing about ion,
    /// Bing or HTTP, exactly like every other consumer of the core.
    pub fn new(
        tree: Box<dyn TileTree>,
        loader: Arc<dyn TileLoader>,
        config: SessionConfig,
    ) -> std::io::Result<Self> {
        Self::from_parts(Self::runtime()?, tree, loader, config)
    }

    /// The runtime a session drives everything on.
    ///
    /// Exposed to the crate because resolving sources is itself async — an ion
    /// endpoint and a Bing metadata document have to be fetched before there is
    /// a tree to hand over — and doing that on a second runtime would mean two
    /// thread pools and two sets of connections for one session.
    ///
    /// `TUILE_SESSION_THREADS` sets its worker count; unset, the threads
    /// available to the process ([`available_threads`]).
    pub(crate) fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(count_knob("TUILE_SESSION_THREADS").unwrap_or_else(available_threads))
            .enable_all()
            .build()
    }

    /// Starts a session on an already-built runtime.
    pub(crate) fn from_parts(
        runtime: tokio::runtime::Runtime,
        tree: Box<dyn TileTree>,
        loader: Arc<dyn TileLoader>,
        config: SessionConfig,
    ) -> std::io::Result<Self> {
        let traversal = exact_traversal(config.traversal.clone());
        let budget = exact_budget(config.budget.clone());
        // Every knob that can move a selection, once, at the top of the
        // stream. A run that differs because it was configured differently is
        // not the bug worth chasing, and this is what tells the two apart
        // before any time is spent.
        tuile_core::det!(
            "config",
            max_sse = traversal.maximum_screen_space_error,
            cull = traversal.cull,
            horizon = traversal.horizon_culling,
            uniform = traversal.uniform_detail,
            uniform_radius = traversal.uniform_detail_radius,
            forbid_holes = traversal.forbid_holes,
            policy = tuile_core::determinism::dbg(budget.policy),
            pinned_level = tuile_core::determinism::dbg(budget.pinned_level),
            imagery_cap = budget.imagery_cap(),
            max_drape_side = budget.max_drape_side,
            in_flight = tuile_core::determinism::dbg(budget.max_in_flight),
            local_reads = budget.max_local_reads,
            occluder = tree.occluder().map(|o| o.radius),
            bake_max = config.bake_max_size,
        );
        // Said once per session, because both halves of a cull can be right on
        // their own and still not be joined: the traversal can be told to cull
        // against an occluder the tree never offers, and the only symptom is a
        // selection that reaches the far side of the planet with nothing in
        // the logs to say why.
        tracing::info!(
            cull = traversal.cull,
            horizon_culling = traversal.horizon_culling,
            occluder = ?tree.occluder(),
            "traversal culling"
        );
        // Loads run as tasks on the session's runtime: they advance while the
        // server traverses, and arrivals ready together are answered by one
        // pass. What a frame settles on does not depend on the order they land
        // in: the refine hysteresis remembers what the previous frame chose,
        // not what a pass drew while tiles were on their way.
        let spawn = tuile_core::runtime::Spawn::Tokio(runtime.handle().clone());
        tracing::info!(loads = ?spawn, "where the session's loads run");
        let (stream, server) = in_process_with(tree, loader, traversal, budget)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
        let server = server.with_spawn(spawn);

        // The server runs on its own runtime handle so a frame call can block
        // the calling thread — which is what Hydra's cook wants — without
        // deadlocking against the loads it is waiting for. The loads
        // themselves are tasks on the same runtime's workers,
        // so they advance while this thread traverses.
        let handle = runtime.handle().clone();
        let server_thread = std::thread::Builder::new()
            .name("tuile-hydra-server".into())
            .spawn(move || handle.block_on(server.run()))?;

        Ok(Self {
            stream,
            runtime,
            config,
            heights: None,
            provenance: None,
            textures: Arc::new(TextureMemo::default()),
            held: Held::default(),
            resident: HashMap::new(),
            scene: SceneState::default(),
            generation: 0,
            grounds: None,
            drawer: crate::drawer::GroundDrawer::new(),
            _server: server_thread,
        })
    }

    /// Resolves one frame for the given views, blocking until it converges.
    ///
    /// Multiple views are a union, not a choice: stereo pairs must share one
    /// selection or the eyes drift apart at LOD boundaries.
    pub fn frame(&mut self, views: Vec<ViewStateParams>) -> Result<Frame, FrameError> {
        // The view this frame is a function of, printed before anything reads
        // it and in a form that round-trips. When two runs disagree about an
        // image, the first thing to rule out is that they were handed
        // different cameras — and ruling it out by argument rather than by
        // measurement is how a whole afternoon went into the wrong suspect.
        for v in &views {
            // Component by component rather than as one Debug string: a
            // camera that drifted in its third decimal is a different camera,
            // and a diff should say which axis moved.
            tuile_core::det!(
                "view",
                gen = self.generation + 1,
                pos_x = v.position.x,
                pos_y = v.position.y,
                pos_z = v.position.z,
                dir_x = v.direction.x,
                dir_y = v.direction.y,
                dir_z = v.direction.z,
                up_x = v.up.x,
                up_y = v.up.y,
                up_z = v.up.z,
                fovy = v.fovy_rad,
                vp_w = v.viewport_px.x,
                vp_h = v.viewport_px.y,
            );
        }
        let views: Vec<ViewState> = views.into_iter().map(Into::into).collect();

        // One camera, one generation. The server echoes it on every `Select`,
        // and until the echo catches up a selection may still describe the
        // previous camera — it keeps traversing as tiles land, so a stale
        // `Select` can arrive *and* look complete.
        self.generation += 1;
        let generation = self.generation;
        if self
            .stream
            .send(ClientMessage::ViewerState { views, generation })
            .is_err()
        {
            return Err(FrameError::ServerGone);
        }

        let timeout = self.config.frame_timeout;
        let finisher = Finisher {
            memo: Arc::clone(&self.textures),
            dataset: self.config.dataset.clone(),
            bake_max_size: self.config.bake_max_size,
            finish: self.config.finish,
            provenance: self.provenance.clone(),
        };
        // Destructured so the pump can borrow the pieces it needs while the
        // runtime is borrowed too.
        let Self {
            stream,
            runtime,
            held,
            resident,
            scene,
            grounds,
            ..
        } = self;
        let mut errors: Vec<(Option<TileId>, String)> = Vec::new();

        let pumped = runtime.block_on(async {
            tokio::time::timeout(
                timeout,
                converge(
                    stream,
                    scene,
                    held,
                    resident,
                    grounds.as_mut(),
                    &finisher,
                    generation,
                    &mut errors,
                ),
            )
            .await
        });
        match pumped {
            Ok(Ok(())) => {}
            // The stream closing on us is normally "the server went away", and
            // was reported as exactly that — which is useless when the server
            // has just told us, in the message before it stopped, which
            // resource it could not load and why. A load failure ENDS a batch
            // session (`FailurePolicy::EndSession`), so this is the ordinary
            // path for it, and the reason has to survive the closure.
            Ok(Err(_closed)) if !errors.is_empty() => {}
            Ok(Err(_closed)) => return Err(FrameError::ServerGone),
            Err(_elapsed) => return Err(FrameError::TimedOut(timeout)),
        }

        let drawn = drawn_tiles(&self.scene);
        tuile_core::det!(
            "converged",
            gen = generation,
            selected = drawn.len(),
            sel_digest = tuile_core::determinism::digest(drawn.iter().map(|t| t.0)),
            resident = self.resident.len(),
            errors = errors.len(),
            deferred = self.scene.stats().deferred_subtrees,
            gaps = self.scene.stats().gaps,
        );
        if self.config.fail_on_tile_errors && !errors.is_empty() {
            let first = errors
                .first()
                .map(|(_, message)| message.clone())
                .unwrap_or_default();
            return Err(FrameError::TilesFailed {
                count: errors.len(),
                first,
            });
        }

        // The draw set, resolved against what is finished. A drawn tile with
        // no finished geometry is not a tile to skip — it is the batch
        // contract broken (I13: a frame is ready only when everything it
        // draws is held).
        let mut missing: Option<TileId> = None;
        let mut missing_count = 0usize;
        let tiles: Vec<Arc<TileGeometry>> = drawn
            .iter()
            .filter_map(|tile| match self.resident.get(tile) {
                Some(finished) => Some(Arc::clone(&finished.geometry)),
                None => {
                    missing_count += 1;
                    missing.get_or_insert(*tile);
                    None
                }
            })
            .collect();
        if let Some(first) = missing {
            return Err(FrameError::MissingContent {
                count: missing_count,
                first,
            });
        }

        let reused = tiles.iter().filter(|t| t.memoized.is_some()).count();
        tracing::info!(
            tiles = tiles.len(),
            reused,
            resident = self.resident.len(),
            "frame resolved (reused = drapes already baked by an earlier frame)"
        );

        // The frame is whole: what it did not need may go now, down to what
        // the session keeps — announced back as evictions, taken in at the
        // next frame. Its tiles are held by the frame itself meanwhile.
        if let Some(bytes) = self.config.release_beyond {
            if self
                .stream
                .send(ClientMessage::ReleaseBeyond { bytes })
                .is_err()
            {
                return Err(FrameError::ServerGone);
            }
        }
        Ok(Frame::with_memo(
            self.config.dataset.as_str(),
            tiles,
            Arc::clone(&self.textures),
        ))
    }
}

/// How the session finishes a tile: everything a finish needs that is the
/// session's rather than the tile's.
struct Finisher {
    memo: Arc<TextureMemo>,
    dataset: String,
    bake_max_size: u32,
    finish: Finish,
    provenance: Option<Arc<tuile_planetary::Provenance>>,
}

/// A tile finished on the pool: the tile, the dressing it was finished
/// from, the result, and its ground when one is kept.
type Finishing = (TileId, Dressing, TileGeometry, Option<KeptTile>);

/// Drives one frame to readiness on a stream that outlives it.
///
/// Three conditions, all necessary: the server reports the frame ready
/// (`frame_ready`: every mesh and drape it draws is held, I13), the draw set
/// it reports answers the camera just sent — waiting only on the first
/// renders the previous camera's ground whenever a stale `Select` lands
/// first — and every drawn tile is finished from the dressing it is drawn
/// in.
#[allow(clippy::too_many_arguments)]
async fn converge(
    stream: &mut tuile_core::protocol::InProcessStream,
    scene: &mut SceneState,
    held: &mut Held,
    resident: &mut HashMap<TileId, Finished>,
    mut grounds: Option<&mut Grounds>,
    finisher: &Finisher,
    generation: u64,
    errors: &mut Vec<(Option<TileId>, String)>,
) -> Result<(), StreamError> {
    // Finishing a tile — composing its 2048² mosaic and encoding it — is the
    // cost of a frame, and it used to run inline on the thread that also
    // receives the stream: frame 1 of a bake spent 106 s composing 273
    // drapes on one core of eight. It runs on a dedicated pool of one thread
    // per core, while the pump keeps receiving.
    //
    // Each tile's result does not depend on any other's: only the order
    // tiles are finished in changes, and `resident` is a map.
    let mut finishing: tokio::task::JoinSet<Finishing> = tokio::task::JoinSet::new();
    // The dressing each tile's latest finish was started from: a finish that
    // comes back from an older one is not taken in.
    let mut started: HashMap<TileId, Dressing> = HashMap::new();
    loop {
        let ready = scene.generation() >= generation && scene.frame_ready();
        if ready {
            // Every drawn tile, from what it is drawn in.
            for item in drawn_items(scene) {
                start_finish(
                    item,
                    held,
                    resident,
                    &mut started,
                    grounds.as_deref(),
                    finisher,
                    &mut finishing,
                );
            }
            if finishing.is_empty() {
                return Ok(());
            }
        }
        tokio::select! {
            finished = finishing.join_next(), if !finishing.is_empty() => {
                match finished {
                    Some(Ok((tile, dressing, geometry, kept))) => {
                        if started.get(&tile) == Some(&dressing) {
                            started.remove(&tile);
                            if let (Some(grounds), Some(kept)) = (grounds.as_deref_mut(), kept) {
                                if let (Some(meshes), Some(mesh)) =
                                    (grounds.as_it_came.as_mut(), kept.as_it_came)
                                {
                                    meshes.insert(tile, (kept.kept.origin(), mesh));
                                }
                                grounds.kept.insert(tile, (dressing.mesh, Arc::new(kept.kept)));
                            }
                            resident.insert(tile, Finished { dressing, geometry: Arc::new(geometry) });
                        }
                    }
                    // A panicking finish is a bug, not a missing tile to wait
                    // for: say so and let the frame fail rather than hang.
                    Some(Err(e)) => errors.push((None, format!("finishing a tile: {e}"))),
                    None => {}
                }
            }
            message = stream.next_message(), if !ready => {
                let Some(message) = message else {
                    return Err(StreamError::Closed);
                };
                scene.apply(&message);
                // The pump's own view of convergence, message by message.
                // `ready` is the condition that ENDS a frame, so a run that
                // saw it one message earlier than another kept a different
                // selection — and that is invisible from anywhere else.
                tuile_core::det!(
                    "pump",
                    gen_seen = scene.generation(),
                    gen_want = generation,
                    complete = scene.frame_ready(),
                    selected = scene.draw().items.len(),
                    resident = resident.len(),
                );
                match message {
                    // Once per residency, and never again — so this is the only
                    // moment a resource can be taken in. Acknowledged at once
                    // with what holding it costs: the budget counts it held.
                    ServerMessage::Content { resource, gen, content, .. } => {
                        let bytes = held.deliver(resource, gen, content);
                        stream.send(ClientMessage::Ack { resource, gen, bytes })?;
                    }
                    // Delivered, not yet taken in for good: dropped.
                    ServerMessage::Cancel { resources } => {
                        for (resource, gen) in resources {
                            if held.gens.get(&resource) == Some(&gen) {
                                held.forget(resource);
                            }
                        }
                    }
                    ServerMessage::Evict { resources } => {
                        for resource in resources {
                            held.forget(resource);
                            if let ResourceId::Mesh(tile) = resource {
                                resident.remove(&tile);
                                started.remove(&tile);
                                if let Some(grounds) = grounds.as_deref_mut() {
                                    grounds.kept.remove(&tile);
                                    if let Some(meshes) = grounds.as_it_came.as_mut() {
                                        meshes.remove(&tile);
                                    }
                                }
                            }
                        }
                    }
                    ServerMessage::Error { resource, message } => {
                        errors.push((resource.map(|r| r.tile()), message));
                    }
                    // Started early: a tile drawn exactly, in drapes of its
                    // own, is finished while the rest of the frame loads. A
                    // dressing that changes before the frame is ready is
                    // finished again; the texture memo spares a repeat.
                    ServerMessage::Select { generation: answered, .. } if answered >= generation => {
                        for item in drawn_items(scene) {
                            let own = item.role == Role::Exact
                                && item.drapes.iter().all(|c| {
                                    c.resource.tile() == item.mesh.tile()
                                        && c.uv == tuile_core::budget::UvTransform::IDENTITY
                                });
                            if own {
                                start_finish(item, held, resident, &mut started, grounds.as_deref(), finisher, &mut finishing);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Starts finishing `item` on the pool, unless it is finished, or being
/// finished, from the dressing it is drawn in.
fn start_finish(
    item: &DrawItem,
    held: &Held,
    resident: &HashMap<TileId, Finished>,
    started: &mut HashMap<TileId, Dressing>,
    grounds: Option<&Grounds>,
    finisher: &Finisher,
    finishing: &mut tokio::task::JoinSet<Finishing>,
) {
    let tile = item.mesh.tile();
    let Some(dressing) = held.dressing(item) else {
        return;
    };
    if resident.get(&tile).is_some_and(|f| f.dressing == dressing)
        || started.get(&tile) == Some(&dressing)
    {
        return;
    }
    let Some((_, mesh)) = held.meshes.get(&tile) else {
        return;
    };
    let mesh = Arc::clone(mesh);
    let drapes: HashMap<ResourceId, Arc<DrapeContent>> = item
        .drapes
        .iter()
        .filter_map(|c| {
            held.drapes
                .get(&c.resource)
                .map(|(_, d)| (c.resource, Arc::clone(d)))
        })
        .collect();
    // The ground is kept once per mesh that came, whatever it wears.
    let keep_ground =
        grounds.is_some_and(|g| g.kept.get(&tile).map(|(a, _)| *a) != Some(dressing.mesh));
    let keep_meshes = keep_ground && grounds.is_some_and(|g| g.as_it_came.is_some());
    started.insert(tile, dressing.clone());
    let item = item.clone();
    let memo = Arc::clone(&finisher.memo);
    let dataset = finisher.dataset.clone();
    let provenance = finisher.provenance.clone();
    let (bake_max_size, finish) = (finisher.bake_max_size, finisher.finish);
    let (done, result) = tokio::sync::oneshot::channel();
    finish_pool().spawn(move || {
        // The mesh as it came, before a finish by identity lets it go: the
        // ground a camera is kept over. Kept on the pool, beside the finish,
        // rather than on the thread that receives the stream.
        let kept = mesh
            .meshes
            .first()
            .filter(|_| keep_ground)
            .map(|first| KeptTile {
                kept: crate::ground::Kept::of(tile, mesh.local_origin_ecef, first),
                as_it_came: keep_meshes.then(|| DecodedMesh {
                    positions: first.positions.clone(),
                    normals: None,
                    uvs: first.uvs.clone(),
                    indices: first.indices.clone(),
                    material: Default::default(),
                }),
            });
        let dressed = tuile_core::drive::dress(&mesh, &item, &drapes);
        let geometry = finish_tile_as(
            &memo,
            &dataset,
            tile,
            dressed,
            bake_max_size,
            provenance.as_deref(),
            finish,
        );
        let _ = done.send((geometry, kept));
    });
    finishing.spawn(async move {
        let (geometry, kept) = result.await.expect("a tile finish panicked");
        (tile, dressing, geometry, kept)
    });
}

/// The pool tiles are finished on: one thread per core (unless
/// `TUILE_FINISH_THREADS` says), built once per process.
fn finish_pool() -> &'static rayon::ThreadPool {
    static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::env::var("TUILE_FINISH_THREADS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|n: &usize| *n > 0)
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("tuile-finish-{i}"))
            .build()
            .expect("the tile finishing pool")
    })
}

/// A numeric knob from the environment, or its default.
///
/// The USD path's tuning lives in the environment on purpose: the stage
/// carries the SHOT (camera, assets, SSE), the job carries the MACHINE
/// (memory, concurrency, sharpness ceilings) — a farm pod and a laptop
/// render the same manifest with different envs.
fn env_knob<T: std::str::FromStr + PartialOrd>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<T>().ok())
        .unwrap_or(default)
}

/// A positive count from `raw`, or `None` when it is absent, unparsable or
/// zero — the caller's default then applies, since a pool of no threads or a
/// limit of nothing in flight would never finish anything.
pub(crate) fn parse_count(raw: Option<&str>) -> Option<usize> {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
}

/// The threads this process may run on: `available_parallelism`, which on
/// Linux honours a cgroup CPU quota, so a container gets its share rather
/// than the host's core count. Two when it cannot be told.
pub(crate) fn available_threads() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get())
}

/// [`parse_count`] of the environment variable `name`.
///
/// For concurrency knobs, read where the pool or runtime they size is built,
/// never per request.
pub(crate) fn count_knob(name: &str) -> Option<usize> {
    parse_count(std::env::var(name).ok().as_deref())
}

/// The traversal a bulk frame runs, whatever the caller asked for.
///
/// Holes are forbidden: a bulk frame has no "while", it is ready or it
/// fails, and what it is ready with must not depend on how fast the tiles
/// came. The rest of what makes a bulk frame exact — no stand-ins, no
/// residency limit, no cancelling what a frame waits on, no subtree given up
/// on — is the batch budget's ([`exact_budget`], `BudgetConfig::batch`), not
/// the traversal's.
///
/// Public because a **bake** has to be able to name what it baked. A pack is
/// the bake of the settings it was made with, not of the defaults, so the
/// scene digest is computed over the config this returns — the resolved one,
/// after every knob above has been read. Computing it over the config before
/// resolution would give two packs made with different maximum screen-space
/// errors the same name, and they would answer for each other.
pub fn exact_traversal(mut config: Config) -> Config {
    config.forbid_holes = true;
    // TUILE_CULL=0 keeps every tile the traversal reaches, however far off
    // screen. Expensive and not a mode to render in — it exists to answer one
    // question: whether ground that is missing was culled.
    config.cull = env_knob("TUILE_CULL", 1u32) != 0;
    // TUILE_MAX_SSE overrides the manifest's `tuile:maxSse` for a machine-side
    // sweep. The stage stays the authority; this is the knob that says what a
    // different threshold would have selected, without rewriting the stage and
    // rebuilding an image for each value.
    let sse: f64 = env_knob("TUILE_MAX_SSE", 0.0);
    if sse > 0.0 {
        config.maximum_screen_space_error = sse;
    }
    // The USD decree: the whole frame as fine as its nearest tile, meshes
    // and imagery both — LOD boundaries are walls across a rendered image.
    // TUILE_UNIFORM_RADIUS sizes the uniform disc (multiples of the nearest
    // content distance); 0 disables uniform detail, plain concentric SSE.
    // TUILE_NO_HORIZON_CULL=1 keeps the far side of the planet, the same
    // escape hatch the viewer carries. When ground goes missing, each of the
    // two things that can remove it without being asked has to be switchable
    // off on its own — and it is also the only way to measure what the cull is
    // actually worth on a real frame.
    config.horizon_culling = std::env::var("TUILE_NO_HORIZON_CULL").is_err();
    let radius: f64 = env_knob("TUILE_UNIFORM_RADIUS", 8.0);
    config.uniform_detail = radius > 0.0;
    if config.uniform_detail {
        config.uniform_detail_radius = radius;
    }
    config
}

/// The budget a bulk frame runs on: `budget` with the environment's knobs.
///
/// - `TUILE_PINNED_LEVEL` sizes the coarse pyramid every session primes: the
///   floor under the globe, never let go, and 2730 tiles of it is most of a
///   cold frame's cost. `-1` leaves no floor.
/// - `TUILE_IMAGERY_BOOST` is how many levels a drape may climb above its
///   terrain tile (`imagery_over_terrain_cap`).
/// - `TUILE_LOAD_TIMEOUT_S` is how long one load may take before the
///   session ends on it (`load_timeout`, 120 s in `BudgetConfig::batch`):
///   set it to cover the retry envelope of the client the tiles come
///   through, every attempt and backoff of one tile.
///
/// Public for the same reason as [`exact_traversal`]: what decides a
/// frame's content goes into the name of the scene a pack is the bake of
/// ([`budget_settings`]).
pub fn exact_budget(mut budget: BudgetConfig) -> BudgetConfig {
    if let Ok(raw) = std::env::var("TUILE_PINNED_LEVEL") {
        match raw.trim().parse::<i64>() {
            Ok(level) if level >= 0 => budget.pinned_level = u32::try_from(level).ok(),
            Ok(_) => budget.pinned_level = None,
            Err(_) => {}
        }
    }
    if let Ok(cap) = std::env::var("TUILE_IMAGERY_BOOST")
        .unwrap_or_default()
        .trim()
        .parse::<u32>()
    {
        budget.imagery_over_terrain_cap = cap;
    }
    if let Some(seconds) = std::env::var("TUILE_LOAD_TIMEOUT_S")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|s| s.is_finite() && *s > 0.0)
    {
        budget.load_timeout = tuile_core::budget::Micros((seconds * 1e6) as u64);
    }
    budget
}

/// What of a budget decides what a frame contains, as one line: the
/// policy, the floor, how far imagery may climb above its terrain and the
/// size of a drape. Its throughput settings (loads in flight, local reads,
/// timeouts) decide how fast, never what, and are left out.
pub fn budget_settings(budget: &BudgetConfig) -> String {
    format!(
        "policy={:?} pinned={:?} imagery_cap={} drape_side={} imagery_px={}",
        budget.policy,
        budget.pinned_level,
        budget.imagery_cap(),
        budget.max_drape_side,
        budget.imagery_tile_px,
    )
}

/// One decoded tile, made boundary-ready.
///
/// Draped imagery is baked to a single owned texture here — before anything
/// crosses the ABI — so the host sees `textures` and a `base_color_texture`
/// index and never the layer stack. Without this, every ion terrain tile
/// crosses with `textures` empty and `base_color_texture == -1`.
#[cfg(test)]
fn finish_tile(
    memo: &TextureMemo,
    dataset: &str,
    tile: TileId,
    decoded: DecodedTileContent,
    bake_max_size: u32,
) -> TileGeometry {
    finish_tile_from(memo, dataset, tile, decoded, bake_max_size, None)
}

/// Where a decoded tile comes from in the tile store: its terrain tile, and
/// the imagery tiles of its drape as they are stacked.
///
/// `None` when the source noted nothing of this tile, or of one of its
/// layers: a reference that names half of what a tile is made from is worse
/// than none, since it reads as complete.
fn refs_of(
    provenance: &tuile_planetary::Provenance,
    tile: TileId,
    decoded: &DecodedTileContent,
    bake_max_size: u32,
) -> Option<tuile_pack::TileRefs> {
    use tuile_pack::{ImageryPlacement, StoreTile, TileRefs};
    let (level, x, y) = tile.terrain_coord();
    // By the tile and by the centre of this mesh: a tile can be built twice
    // in a session, from two places, and this is one of the two.
    let origin = provenance.terrain(
        tuile_terrain::TileCoord::new(level, x, y),
        decoded.local_origin_ecef.to_array(),
    )?;
    let imagery = decoded
        .imagery
        .iter()
        .map(|layer| {
            Some(ImageryPlacement {
                tile: StoreTile {
                    level: u8::try_from(layer.coord.level).ok()?,
                    x: u32::try_from(layer.coord.x).ok()?,
                    y: u32::try_from(layer.coord.y).ok()?,
                    digest: provenance.imagery(layer.coord)?,
                },
                coverage: layer.coverage,
                translation: layer.translation,
                scale: layer.scale,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(TileRefs {
        terrain: StoreTile {
            level: u8::try_from(origin.source.level).ok()?,
            x: u32::try_from(origin.source.x).ok()?,
            y: u32::try_from(origin.source.y).ok()?,
            digest: origin.digest,
        },
        skirt_height: origin.skirt_height as f32,
        composed_side: tuile_core::raster::composed_side(&decoded.imagery, bake_max_size),
        imagery,
    })
}

/// [`finish_tile`], noting where the tile comes from when `provenance` knows.
#[cfg(test)]
fn finish_tile_from(
    memo: &TextureMemo,
    dataset: &str,
    tile: TileId,
    decoded: DecodedTileContent,
    bake_max_size: u32,
    provenance: Option<&tuile_planetary::Provenance>,
) -> TileGeometry {
    finish_tile_as(
        memo,
        dataset,
        tile,
        decoded,
        bake_max_size,
        provenance,
        Finish::Pictures,
    )
}

/// A tile as the session keeps it, finished as `finish` says.
///
/// Timed whatever the mode, into `finish_seconds`: an identity finish is the
/// cheap end of the same series, and a pack by reference reading near zero
/// there is the measurement, not a gap in it.
pub(crate) fn finish_tile_as(
    memo: &TextureMemo,
    dataset: &str,
    tile: TileId,
    decoded: DecodedTileContent,
    bake_max_size: u32,
    provenance: Option<&tuile_planetary::Provenance>,
    finish: Finish,
) -> TileGeometry {
    tuile_core::metrics::metrics().finish_seconds.time(|| {
        finished_as(
            memo,
            dataset,
            tile,
            decoded,
            bake_max_size,
            provenance,
            finish,
        )
    })
}

/// [`finish_tile_as`], untimed.
fn finished_as(
    memo: &TextureMemo,
    dataset: &str,
    tile: TileId,
    mut decoded: DecodedTileContent,
    bake_max_size: u32,
    provenance: Option<&tuile_planetary::Provenance>,
    finish: Finish,
) -> TileGeometry {
    // Before the layers are composed and gone: they are what a reference
    // into the tile store is made of.
    let refs = provenance.and_then(|p| refs_of(p, tile, &decoded, bake_max_size));
    if tracing::enabled!(tracing::Level::DEBUG) && !decoded.imagery.is_empty() {
        let mut levels: Vec<u32> = decoded.imagery.iter().map(|l| l.coord.level).collect();
        levels.sort_unstable();
        levels.dedup();
        tracing::debug!(
            tile = tile.0,
            layers = decoded.imagery.len(),
            ?levels,
            "draped imagery before bake"
        );
    }
    // A drape the loader withheld names itself; nothing else can name it,
    // because its layers were never fetched and `drape_key` reads the layers.
    //
    // This is the demand side of `withheld_drape`'s contract. Ignoring it here
    // would give the tile a drape of zero, which the pack would store as "this
    // tile has no imagery" — real terrain, no picture, no error anywhere.
    let withheld = decoded.withheld_drape;
    let baked = withheld
        .map(|drape| TextureKey {
            tile,
            drape,
            texture_index: 0,
        })
        .or_else(|| drape_key(tile, &decoded, bake_max_size));
    if finish == Finish::Identity {
        // Named — its drape from its layers, its references from the record
        // of what was fetched — and that is all a pack by reference keeps.
        // The layers, the mesh and any texture go now rather than at the end
        // of the film: they are megabytes a tile, and nothing will read them.
        let origin_ecef = decoded.local_origin_ecef;
        let shape = decoded.meshes.first().map(|mesh| Shape {
            vertex_count: mesh.positions.len() as u32,
            index_count: mesh.indices.len() as u32,
            base_color_factor: mesh.material.base_color_factor,
        });
        decoded.imagery.clear();
        decoded.textures.clear();
        decoded.meshes.clear();
        return TileGeometry {
            tile,
            origin_ecef,
            content: decoded,
            baked,
            memoized: None,
            refs,
            shape,
        };
    }
    let memoized = baked.and_then(|key| memo.get(key));
    match &memoized {
        // The same tile under the same drape was baked and encoded by an
        // earlier frame: skip both. This is where the sixty seconds went — an
        // orbit re-selects nearly the same tiles every frame, and composing a
        // 2048² mosaic then PNG-encoding it, per tile, per frame, is the
        // whole cost of a frame the GPU draws in milliseconds.
        Some(_) => {
            decoded.imagery.clear();
            for mesh in &mut decoded.meshes {
                mesh.material.base_color_texture = Some(0);
            }
        }
        // Withheld: there is nothing to compose — the layers were never
        // fetched — but the material must still point at texture 0, because
        // that is where the drape the consumer holds will be bound.
        None if withheld.is_some() => {
            for mesh in &mut decoded.meshes {
                mesh.material.base_color_texture = Some(0);
            }
        }
        None => tuile_core::raster::bake_imagery(&mut decoded, bake_max_size),
    }
    // Composée, encodée, et la mosaïque brute s'en va — dans la foulée, pas à
    // la demande.
    //
    // C'est ce qui a tué deux cuissons à 20 km (19 septembre 2026, statut 137,
    // zéro frame). Une frame en vrac ne rend la main que lorsque TOUTES ses
    // tuiles sont résidentes, et une tuile fraîchement cuite gardait sa
    // mosaïque en RGBA brut : 2048², soit seize mébioctets, multipliés par le
    // nombre de tuiles de la frame. À 90 km la frame 1 en sélectionne 444 —
    // sept gibioctets, ça passait. À 20 km l'imagerie descend de deux niveaux
    // et la sélection enfle d'autant : le conteneur meurt avant la première
    // frame.
    //
    // Le PNG du même contenu pèse quelques mébioctets. Encoder tout de suite
    // échange donc du CPU — qu'on paie de toute façon, puisque CHAQUE tuile
    // d'une cuisson finit dans le pack — contre un facteur cinq à huit sur la
    // mémoire résidente.
    //
    // Et ça unifie les deux branches : sur un succès de mémo la tuile ne
    // portait déjà que son PNG, et c'est aussi la forme exacte que produit le
    // lecteur de pack (`packed.rs`). Une seule forme de tuile résidente, quelle
    // que soit sa provenance.
    let memoized = memoized.or_else(|| encode_baked(memo, dataset, tile, &mut decoded, baked));
    TileGeometry {
        tile,
        origin_ecef: decoded.local_origin_ecef,
        content: decoded,
        baked,
        memoized,
        refs,
        shape: None,
    }
}

/// Encodes the mosaic just baked into `decoded`, and removes it from the tile.
///
/// Returns `None` when there is nothing to encode — no baked drape, or no
/// texture where one was expected — and in that case the tile keeps whatever
/// it had, so a failure here costs sharpness, never ground.
///
/// An encode that fails is **not** an error either: the raw texture stays where
/// it is and [`Frame::texture_png`] will try again on demand, exactly as before
/// this existed. Losing a render over a PNG encoder would be a poor trade for
/// memory.
fn encode_baked(
    memo: &TextureMemo,
    dataset: &str,
    tile: TileId,
    decoded: &mut DecodedTileContent,
    baked: Option<TextureKey>,
) -> Option<Arc<EncodedTexture>> {
    let baked = baked?;
    let texture = decoded.textures.first()?;
    let rgba8 = texture.rgba8()?;
    let mut png = Vec::new();
    image::write_buffer_with_format(
        &mut std::io::Cursor::new(&mut png),
        rgba8,
        texture.width,
        texture.height,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    )
    .ok()?;
    let entry = Arc::new(EncodedTexture {
        uri: Frame::texture_uri(dataset, tile, 0, baked.drape),
        png,
    });
    memo.insert(baked, &entry);
    // The tile now carries its texture in encoded form only. The material keeps
    // pointing at index 0, which is what a memo hit leaves behind too — and
    // what `Frame::texture_png` resolves through `memoized`.
    decoded.textures.clear();
    for mesh in &mut decoded.meshes {
        mesh.material.base_color_texture = Some(0);
    }
    Some(entry)
}

/// The memo key for a tile's baked mosaic, or `None` when there is nothing to
/// bake.
///
/// The guard mirrors [`tuile_core::raster::bake_imagery`]'s own, plus one: the
/// tile must own no texture of its own, so the baked mosaic is unambiguously
/// texture 0 — which is what lets a memo hit stand in for content that was
/// never composed at all.
fn drape_key(tile: TileId, content: &DecodedTileContent, bake_max_size: u32) -> Option<TextureKey> {
    if content.imagery.is_empty()
        || !content.textures.is_empty()
        || content.meshes.iter().any(|m| m.uvs.is_none())
    {
        return None;
    }
    // The same function the loader runs over the coords it is ABOUT to
    // request, and it has to be the same one: the loader withholds a drape by
    // recognising the identity this produces. Two implementations of one hash
    // would agree until the day one of them was edited.
    Some(TextureKey {
        tile,
        drape: tuile_core::raster::drape_identity(
            content.imagery.iter().map(|layer| layer.coord),
            bake_max_size,
        ),
        texture_index: 0,
    })
}

#[cfg(test)]
pub(crate) mod tests {

    #[test]
    fn a_count_knob_is_a_positive_number_or_nothing() {
        use super::parse_count;
        assert_eq!(parse_count(None), None);
        assert_eq!(parse_count(Some("")), None);
        assert_eq!(parse_count(Some("two")), None);
        assert_eq!(parse_count(Some("-2")), None);
        assert_eq!(parse_count(Some("0")), None);
        assert_eq!(parse_count(Some("3")), Some(3));
        assert_eq!(parse_count(Some(" 12\n")), Some(12));
    }

    use super::*;
    use tuile_core::content::DecodedTexture;

    /// A frame holding one tile with one small opaque texture.
    fn frame_with_a_texture() -> Frame {
        Frame::new(
            "ion-1-2",
            vec![Arc::new(TileGeometry {
                tile: TileId(7),
                origin_ecef: DVec3::ZERO,
                content: DecodedTileContent {
                    withheld_drape: None,
                    copyright: None,
                    meshes: Vec::new(),
                    textures: vec![DecodedTexture::from_rgba8(2, 2, vec![255u8; 2 * 2 * 4])],
                    imagery: Vec::new(),
                    local_origin_ecef: DVec3::ZERO,
                    transform_local: glam::Mat4::IDENTITY,
                },
                baked: None,
                memoized: None,
                refs: None,
                shape: None,
            })],
        )
    }

    /// Both sides derive the texture's name from this one function, so it is
    /// worth pinning: a change here silently unresolves every material.
    #[test]
    fn the_texture_uri_is_stable_and_scheme_qualified() {
        assert_eq!(
            Frame::texture_uri("ion-1-2", TileId(7), 0, 0),
            "tuile://ion-1-2/tile/7/texture/0.0000000000000000.png"
        );
    }

    /// The fetch wave may not exceed what can be on the wire.
    ///
    /// The two numbers live in different crates and drifted apart: the wave
    /// said 256 while the client held 64 connections per host, so three
    /// requests in four were queued inside the process rather than issued. The
    /// symptom is not a failure — it is memory held, timeouts that measure the
    /// queue, and an origin answering 500 to a flood.
    #[test]
    fn the_fetch_wave_fits_the_connection_pool() {
        let budget = exact_budget(SessionConfig::default().budget);
        for (class, in_flight) in budget.max_in_flight.iter() {
            assert!(
                in_flight as usize <= tuile_native_fetchers::CONNECTIONS_PER_HOST,
                "{class:?}: {in_flight} requests in flight against {} connections — \
                 the surplus only queues",
                tuile_native_fetchers::CONNECTIONS_PER_HOST,
            );
        }
    }

    /// The same tile re-draped is a different picture, so it must be a
    /// different asset: any cache between here and the renderer keys on this
    /// name, and one that did not change would keep serving the old pixels.
    #[test]
    fn a_redraped_tile_gets_a_new_asset_name() {
        assert_ne!(
            Frame::texture_uri("ion-1-2", TileId(7), 0, 0xdead),
            Frame::texture_uri("ion-1-2", TileId(7), 0, 0xbeef)
        );
    }

    /// And the name a frame hands out is the one built from the drape it
    /// actually baked — not a guess made at the boundary.
    ///
    /// The dataset is given to `finish_tile`, not only to the frame, because
    /// the name is now stamped when the mosaic is encoded — which is on
    /// arrival, so that the raw pixels can be dropped. In a session the two are
    /// the same string (`config.dataset`); here they have to be written twice.
    #[test]
    fn the_encoded_uri_carries_the_tile_s_own_drape() {
        let memo = Arc::new(TextureMemo::default());
        let tile = finish_tile(&memo, "ion-1-2", TileId(7), draped_content(), 256);
        let drape = tile.drape();
        assert_ne!(drape, 0, "a draped tile is keyed by its drape");
        let frame = Frame::with_memo("ion-1-2", vec![Arc::new(tile)], memo);
        let texture = frame.texture_png(0, 0).expect("encoding").expect("present");
        assert_eq!(
            texture.uri,
            Frame::texture_uri("ion-1-2", TileId(7), 0, drape)
        );
    }

    /// The whole reason the dataset is in the path: a TileId is unique within
    /// one tree, so two sessions hand out the same ids for different tiles and
    /// a resolver keyed on the id alone would answer with the wrong imagery —
    /// silently, since the bytes are a valid PNG either way.
    #[test]
    fn two_datasets_never_collide_on_a_tile_id() {
        assert_ne!(
            Frame::texture_uri("ion-1-2", TileId(7), 0, 0),
            Frame::texture_uri("ion-1-3812", TileId(7), 0, 0)
        );
    }

    #[test]
    fn a_texture_encodes_to_png() {
        let frame = frame_with_a_texture();
        let texture = frame
            .texture_png(0, 0)
            .expect("encoding")
            .expect("the texture exists");
        assert_eq!(
            texture.uri,
            "tuile://ion-1-2/tile/7/texture/0.0000000000000000.png"
        );
        // The PNG signature, so this is decodable bytes rather than the raw
        // RGBA the host's image plugin cannot read.
        assert_eq!(&texture.png[..8], b"\x89PNG\r\n\x1a\n");
    }

    /// The C boundary hands out a pointer into these bytes and promises it
    /// stays valid for the frame's life. That only holds if the second call
    /// returns the same allocation rather than encoding again.
    #[test]
    fn encoding_happens_once_and_the_bytes_do_not_move() {
        let frame = frame_with_a_texture();
        let first = frame.texture_png(0, 0).expect("encoding").expect("present");
        let second = frame.texture_png(0, 0).expect("encoding").expect("present");
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.png.as_ptr(), second.png.as_ptr());
    }

    /// An untextured tile and a past-the-end index are ordinary answers, not
    /// errors — a caller enumerates until one of them comes back.
    #[test]
    fn absent_textures_report_absence_rather_than_failing() {
        let frame = frame_with_a_texture();
        assert!(frame.texture_png(0, 1).expect("no error").is_none());
        assert!(frame.texture_png(9, 0).expect("no error").is_none());
    }

    /// A frame must give the patience room to work.
    ///
    /// The two numbers live in different crates and contradicted each other:
    /// 182 s of retry budget against a 120 s frame, so the chain could never
    /// finish inside a frame. Nothing failed loudly — the retries logged at
    /// `debug` — so the bake reported only that frame 1 had not converged,
    /// which is true and says nothing.
    ///
    /// Both shapes have to stay sound, because the profile is a knob:
    ///
    /// - bounded patience — the frame must outlast it, or the retries are
    ///   decoration;
    /// - unlimited patience — the frame is the sole bound, so it must be
    ///   finite *and* long enough for the waiting to amount to something. A
    ///   frame shorter than a handful of backoffs gives up after two attempts
    ///   and the word "patient" means nothing.
    #[test]
    fn a_frame_gives_the_patience_room_to_work() {
        let retry = tuile_native_fetchers::RetryConfig::patient();
        let frame = SessionConfig::default().frame_timeout;
        match retry.budget() {
            Some(budget) => assert!(
                frame > budget,
                "a tile may wait {budget:?} and the frame gives up at {frame:?}: \
                 the retry chain can never finish inside a frame"
            ),
            None => assert!(
                frame >= retry.max_backoff * 10,
                "unlimited patience inside a {frame:?} frame is {} attempts at \
                 a {:?} ceiling — not patience, decoration",
                frame.as_secs_f64() / retry.max_backoff.as_secs_f64(),
                retry.max_backoff
            ),
        }
    }

    #[test]
    fn the_default_frame_timeout_is_finite() {
        // A bulk frame can block forever on a hung fetch or a budget too small
        // to converge. The default must bound that, not inherit it.
        let config = SessionConfig::default();
        assert!(config.frame_timeout > Duration::ZERO);
        assert!(config.frame_timeout < Duration::from_secs(3600));
    }

    /// A failed tile does not leave a hole — an ancestor stands in — so a frame
    /// can look fine and be wrong. Anything keeping its output wants this loud.
    #[test]
    fn tile_errors_fail_the_frame_by_default() {
        assert!(SessionConfig::default().fail_on_tile_errors);
    }

    /// Whatever the caller's traversal config says, a bulk frame is exact:
    /// holes stay forbidden, and the session runs on the batch budget.
    #[test]
    fn bulk_traversal_is_exact_whatever_the_caller_asked() {
        let exact = exact_traversal(Config {
            forbid_holes: false,
            ..Config::default()
        });
        assert!(exact.forbid_holes);
        assert_eq!(
            SessionConfig::default().budget.policy,
            tuile_core::budget::Policy::Batch
        );
    }

    /// Une erreur d'écran posée par l'appelant survit à la résolution.
    ///
    /// C'est la moitié utile du correctif de septembre 2026 : le lanceur
    /// savait nommer `--sse 3` et le bake cuisait à 16, parce que la valeur
    /// n'arrivait jamais jusqu'ici. Elle arrive maintenant par
    /// `config.session.traversal`, et ce test dit que `exact_traversal` —
    /// qui écrase délibérément `forbid_holes` — ne l'écrase
    /// PAS au passage. Il ne couvre pas la plomberie du shell : que
    /// `bake_job.sh` passe bien `--sse` ne se vérifie qu'en lisant la ligne
    /// `BAKE-BEGIN` d'une vraie cuisson.
    #[test]
    fn an_explicit_screen_space_error_survives_resolution() {
        let exact = exact_traversal(Config {
            maximum_screen_space_error: 3.0,
            ..Config::default()
        });
        assert_eq!(exact.maximum_screen_space_error, 3.0);
        // Et le défaut reste le défaut quand personne ne demande rien.
        assert_eq!(
            exact_traversal(Config::default()).maximum_screen_space_error,
            Config::default().maximum_screen_space_error
        );
    }

    /// A tile's reference into the tile store is taken from what the loader
    /// noted, before the layers are composed and gone.
    #[test]
    fn a_finished_tile_says_where_it_comes_from_when_the_loader_noted_it() {
        use tuile_core::raster::ImageryCoord;
        let tile = TileId(7);
        let (level, x, y) = tile.terrain_coord();
        let coord = tuile_terrain::TileCoord::new(level, x, y);
        let imagery = ImageryCoord {
            level: 0,
            x: 0,
            y: 0,
        };
        let memo = TextureMemo::default();

        // Nothing noted, or no record at all: no reference.
        let silent = tuile_planetary::Provenance::default();
        for provenance in [None, Some(&silent)] {
            let finished = finish_tile_from(&memo, "t", tile, draped_content(), 256, provenance);
            assert_eq!(finished.refs, None);
        }

        // The terrain alone is half a reference, which is none.
        let noted = tuile_planetary::Provenance::default();
        let centre = draped_content().local_origin_ecef.to_array();
        noted.fetched_terrain(coord, centre, tuile_planetary::digest(b"terrain bytes"));
        let half = finish_tile_from(&memo, "t", tile, draped_content(), 256, Some(&noted));
        assert_eq!(half.refs, None);

        noted.fetched_imagery(imagery, b"imagery bytes");
        let content = draped_content();
        let layer = content.imagery[0].clone();
        let side = tuile_core::raster::composed_side(&content.imagery, 256);
        let finished = finish_tile_from(&memo, "t", tile, content, 256, Some(&noted));
        assert!(
            finished.content.imagery.is_empty(),
            "the layers are consumed"
        );
        let refs = finished.refs.expect("a reference");
        assert_eq!(
            refs.terrain.digest,
            tuile_planetary::digest(b"terrain bytes")
        );
        assert_eq!(
            (
                u32::from(refs.terrain.level),
                u64::from(refs.terrain.x),
                u64::from(refs.terrain.y)
            ),
            (level, x, y)
        );
        assert_eq!(refs.composed_side, side);
        assert_eq!(refs.imagery.len(), 1);
        let placed = refs.imagery[0];
        assert_eq!(
            placed.tile.digest,
            tuile_planetary::digest(b"imagery bytes")
        );
        assert_eq!(
            (placed.coverage, placed.translation, placed.scale),
            (layer.coverage, layer.translation, layer.scale)
        );
    }

    /// A tile with draped imagery must cross the boundary as one owned
    /// texture: the bake happens in the session, before the ABI, or every ion
    /// terrain tile reports `base_color_texture == -1` and renders untextured.
    /// One tile of terrain with one draped imagery layer — the shape every
    /// ion tile arrives in, and the only one the bake path acts on.
    pub(crate) fn draped_content() -> DecodedTileContent {
        use tuile_core::content::{DecodedMesh, MaterialDesc};
        use tuile_core::geo::{geodetic_to_ecef, Geodetic};
        use tuile_core::raster::{uvs_geographic, GeoRect, ImageryCoord, ImageryLayer};
        let rect = GeoRect {
            west: 0.0,
            south: 0.0,
            east: 0.01,
            north: 0.01,
        };
        let origin = geodetic_to_ecef(Geodetic {
            lon: 0.005,
            lat: 0.005,
            height: 0.0,
        });
        let positions = vec![[0.0f32; 3]; 3];
        let uvs = uvs_geographic(&positions, origin, &rect);
        DecodedTileContent {
            withheld_drape: None,
            copyright: None,
            meshes: vec![DecodedMesh {
                positions,
                normals: None,
                uvs: Some(uvs),
                indices: vec![0, 1, 2],
                material: MaterialDesc {
                    base_color_factor: [1.0; 4],
                    base_color_texture: None,
                    unlit: false,
                },
            }],
            textures: Vec::new(),
            imagery: vec![ImageryLayer {
                source: 0,
                coord: ImageryCoord {
                    level: 0,
                    x: 0,
                    y: 0,
                },
                texture: Arc::new(DecodedTexture::from_rgba8(2, 2, vec![128u8; 2 * 2 * 4])),
                coverage: [-0.1, -0.1, 1.1, 1.1],
                translation: [0.0, 0.0],
                scale: [1.0, 1.0],
            }],
            local_origin_ecef: origin,
            transform_local: glam::Mat4::IDENTITY,
        }
    }

    #[test]
    fn a_finished_tile_owns_its_mosaic() {
        use tuile_core::content::{DecodedMesh, MaterialDesc};
        use tuile_core::geo::{geodetic_to_ecef, Geodetic};
        use tuile_core::raster::{uvs_geographic, GeoRect, ImageryCoord, ImageryLayer};

        let rect = GeoRect {
            west: 0.0,
            south: 0.0,
            east: 0.01,
            north: 0.01,
        };
        let origin = geodetic_to_ecef(Geodetic {
            lon: 0.005,
            lat: 0.005,
            height: 0.0,
        });
        let positions = vec![[0.0f32; 3]; 3];
        let uvs = uvs_geographic(&positions, origin, &rect);
        let decoded = DecodedTileContent {
            withheld_drape: None,
            copyright: None,
            meshes: vec![DecodedMesh {
                positions,
                normals: None,
                uvs: Some(uvs),
                indices: vec![0, 1, 2],
                material: MaterialDesc {
                    base_color_factor: [1.0; 4],
                    base_color_texture: None,
                    unlit: false,
                },
            }],
            textures: Vec::new(),
            imagery: vec![ImageryLayer {
                source: 0,
                coord: ImageryCoord {
                    level: 0,
                    x: 0,
                    y: 0,
                },
                texture: Arc::new(DecodedTexture::from_rgba8(2, 2, vec![128u8; 2 * 2 * 4])),
                coverage: [-0.1, -0.1, 1.1, 1.1],
                translation: [0.0, 0.0],
                scale: [1.0, 1.0],
            }],
            local_origin_ecef: origin,
            transform_local: glam::Mat4::IDENTITY,
        };

        let memo = TextureMemo::default();
        let tile = finish_tile(&memo, "test", TileId(7), decoded, 256);
        assert!(tile.content.imagery.is_empty(), "the layers are consumed");
        assert_eq!(
            tile.content.meshes[0].material.base_color_texture,
            Some(0),
            "the material points at the baked texture"
        );
        // Owned, and owned ENCODED: the composed mosaic does not stay resident
        // as raw pixels. See `encode_baked`.
        assert!(
            tile.content.textures.is_empty(),
            "the raw mosaic is still resident"
        );
        assert!(tile.memoized.is_some(), "the tile carries its PNG");
    }

    /// A withheld drape keeps its identity, so the pack can reference it.
    ///
    /// The loader skips fetching a tile's imagery when the consumer already
    /// holds the composed drape, and hands back content with no layers and
    /// `withheld_drape` set. If `finish_tile` read that as "no imagery", the
    /// tile would get a drape of zero — and a drape of zero is what a pack
    /// stores for terrain that has no picture at all. Real ground, no texture,
    /// and every counter green.
    #[test]
    fn a_withheld_drape_survives_into_the_tile_s_identity() {
        let memo = TextureMemo::default();
        let mut content = draped_content();
        // What the loader produces: the identity, and none of the layers.
        let identity = tuile_core::raster::drape_identity(
            content.imagery.iter().map(|layer| layer.coord),
            256,
        );
        content.imagery.clear();
        content.withheld_drape = Some(identity);

        let tile = finish_tile(&memo, "ion-1-2", TileId(7), content, 256);
        assert_eq!(
            tile.drape(),
            identity,
            "the tile forgot which drape it stands for"
        );
        assert!(
            tile.content.textures.is_empty(),
            "nothing was fetched, so nothing can have been composed"
        );
        assert_eq!(
            tile.content.meshes[0].material.base_color_texture,
            Some(0),
            "the material must still point where the held drape will bind"
        );
    }

    /// And the identity it keeps is the one the layers would have produced.
    ///
    /// This is the whole contract between the two sides: the loader hashes the
    /// coords it is about to request, the consumer hashes the layers that
    /// arrived, and a drape is only withheld when those two numbers match. If
    /// they could differ for the same coords, a bake would skip a fetch and
    /// then fail to find what it skipped it for.
    #[test]
    fn the_withheld_identity_is_the_one_the_layers_would_have_given() {
        let memo = TextureMemo::default();
        let fetched = finish_tile(&memo, "ion-1-2", TileId(7), draped_content(), 256);

        let content = draped_content();
        let before = tuile_core::raster::drape_identity(
            content.imagery.iter().map(|layer| layer.coord),
            256,
        );
        assert_eq!(
            before,
            fetched.drape(),
            "the loader and the consumer disagree on what this drape is called"
        );
    }

    /// A freshly baked tile must not hold its mosaic in raw pixels.
    ///
    /// The measure, rather than the shape: a 2048² mosaic is sixteen mebibytes
    /// raw, and a bulk frame holds every tile it selected at once. Two bakes at
    /// 20 km died on exactly that (19 September 2026, status 137, before the
    /// first frame). PNG of the same picture is several times smaller, and a
    /// bake encodes every tile anyway — so the only thing eager encoding costs
    /// is the order the CPU is spent in.
    #[test]
    fn a_baked_tile_holds_no_raw_mosaic() {
        let memo = TextureMemo::default();
        let tile = finish_tile(&memo, "test", TileId(7), draped_content(), 256);
        let raw: usize = tile
            .content
            .textures
            .iter()
            .map(|t| t.texels.byte_len())
            .sum();
        assert_eq!(raw, 0, "{raw} bytes of raw mosaic stayed resident");
        let png = tile
            .memoized
            .as_ref()
            .expect("encoded on arrival")
            .png
            .len();
        // The header alone is 8 bytes; anything this small is not a picture.
        assert!(png > 32, "the PNG is {png} bytes — nothing was encoded");
    }

    /// A tree of two tiles with content — no file, no network, no GPU. Enough
    /// to exercise what the streaming protocol guarantees.
    struct TwoTiles;

    impl TileTree for TwoTiles {
        fn roots(&self) -> Vec<TileId> {
            vec![TileId(0)]
        }
        fn children(&self, id: TileId) -> Vec<TileId> {
            if id == TileId(0) {
                vec![TileId(1)]
            } else {
                Vec::new()
            }
        }
        fn parent(&self, id: TileId) -> Option<TileId> {
            (id == TileId(1)).then_some(TileId(0))
        }
        fn properties(&self, id: TileId) -> tuile_core::source::TileProperties {
            use tuile_core::math::{BoundingVolume, Sphere};
            tuile_core::source::TileProperties {
                bounding_volume: BoundingVolume::Sphere(Sphere {
                    center: DVec3::ZERO,
                    radius: if id == TileId(0) { 100.0 } else { 30.0 },
                }),
                geometric_error: if id == TileId(0) { 50.0 } else { 0.0 },
                refine: tuile_core::tileset::Refine::Replace,
                has_content: true,
            }
        }
    }

    /// Counts loads, so a test can say "the server never fetched it twice".
    struct CountingLoader(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait::async_trait]
    impl TileLoader for CountingLoader {
        async fn load(
            &self,
            _id: TileId,
        ) -> Result<tuile_core::source::Loaded, tuile_core::source::LoadError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(tuile_core::source::Loaded::Content(draped_content()))
        }
    }

    fn a_view() -> ViewStateParams {
        ViewStateParams {
            position: DVec3::new(0.0, 0.0, 200.0),
            direction: DVec3::new(0.0, 0.0, -1.0),
            up: DVec3::new(0.0, 1.0, 0.0),
            viewport_px: glam::DVec2::new(960.0, 720.0),
            fovy_rad: 45f64.to_radians(),
            fov: None,
            scale: 1.0,
        }
    }

    /// The reason this session keeps a residency at all.
    ///
    /// The server sends a tile's `Content` **once per residency** and never
    /// again. A consumer that rebuilt its state each frame — which is what
    /// happened while Blender rebuilt the whole scene index — saw the tiles on
    /// the first frame and nothing afterwards. Revert the residency and the
    /// second frame comes back empty, which is exactly what this pins.
    #[test]
    fn a_second_frame_still_sees_tiles_whose_content_came_once() {
        let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut session = Session::new(
            Box::new(TwoTiles),
            Arc::new(CountingLoader(Arc::clone(&loads))),
            SessionConfig::default(),
        )
        .expect("session");

        let first = session.frame(vec![a_view()]).expect("frame 1");
        assert!(!first.tiles.is_empty(), "the first frame has ground");
        let loaded_once = loads.load(std::sync::atomic::Ordering::Relaxed);
        assert!(loaded_once > 0, "something was actually fetched");

        let second = session.frame(vec![a_view()]).expect("frame 2");
        assert_eq!(
            second.tiles.len(),
            first.tiles.len(),
            "the second frame sees the same ground"
        );
        assert_eq!(
            loads.load(std::sync::atomic::Ordering::Relaxed),
            loaded_once,
            "and nothing was fetched a second time"
        );
        assert!(
            Arc::ptr_eq(&first.tiles[0], &second.tiles[0]),
            "it is the same tile, not a copy of it"
        );
    }

    /// The fix for sixty seconds a frame: a tile whose drape has already been
    /// baked and encoded is not composed again, and the second frame hands out
    /// the very same bytes rather than an identical copy.
    #[test]
    fn a_repeated_drape_is_baked_and_encoded_once() {
        let memo = Arc::new(TextureMemo::default());

        // Frame 1: nothing is known, so the mosaic is composed — then encoded
        // on the spot, which is why `textures` is already empty here.
        let first = finish_tile(&memo, "ion-1-2", TileId(7), draped_content(), 256);
        assert!(
            first.content.textures.is_empty(),
            "frame 1 composes, encodes, and lets the raw mosaic go"
        );
        let frame1 = Frame::with_memo("ion-1-2", vec![Arc::new(first)], Arc::clone(&memo));
        let png1 = frame1
            .texture_png(0, 0)
            .expect("encoding")
            .expect("present");

        // Frame 2: same tile, same drape — no bake, no encode.
        let second = finish_tile(&memo, "ion-1-2", TileId(7), draped_content(), 256);
        assert!(second.memoized.is_some(), "the drape is recognised");
        assert!(
            second.content.textures.is_empty(),
            "frame 2 skips the bake entirely"
        );
        assert_eq!(
            second.content.meshes[0].material.base_color_texture,
            Some(0),
            "the material still points at texture 0, served from the memo"
        );
        let frame2 = Frame::with_memo("ion-1-2", vec![Arc::new(second)], Arc::clone(&memo));
        let png2 = frame2
            .texture_png(0, 0)
            .expect("encoding")
            .expect("present");
        assert!(Arc::ptr_eq(&png1, &png2), "one allocation, two frames");
    }

    /// A drape at a different imagery level is a different picture, and must
    /// not be served from the memo — that is how a moving camera would keep
    /// yesterday's sharpness.
    #[test]
    fn a_different_drape_is_a_different_key() {
        let memo = Arc::new(TextureMemo::default());
        let baseline = finish_tile(&memo, "test", TileId(7), draped_content(), 256);
        let coarser = finish_tile(&memo, "test", TileId(7), draped_content(), 128);
        assert_ne!(
            baseline.baked.expect("keyed"),
            coarser.baked.expect("keyed"),
            "the bake size is part of what the pixels are"
        );
    }

    /// Terrain tiles where they are on the Earth: one at level 14 and its four
    /// children, flat, the parent at 600 m and the children at 640 m — so a
    /// height read says which of them is drawn.
    struct GeoQuad;

    const GEO_ROOT: (u32, u64, u64) = (14, 9000, 6200);

    fn geo_height(level: u32) -> f64 {
        600.0 + 40.0 * f64::from(level - GEO_ROOT.0)
    }

    fn geo_at(level: u32, x: u64, y: u64, u: f64, north: f64, height: f64) -> DVec3 {
        use std::f64::consts::PI;
        let size = PI / 2f64.powi(level as i32);
        tuile_core::geo::geodetic_to_ecef(tuile_core::geo::Geodetic {
            lon: -PI + (x as f64 + u) * size,
            lat: -PI / 2.0 + (y as f64 + north) * size,
            height,
        })
    }

    impl TileTree for GeoQuad {
        fn roots(&self) -> Vec<TileId> {
            let (l, x, y) = GEO_ROOT;
            vec![TileId::from_terrain(l, x, y)]
        }
        fn children(&self, id: TileId) -> Vec<TileId> {
            let (level, x, y) = id.terrain_coord();
            if level > GEO_ROOT.0 {
                return Vec::new();
            }
            [(0, 0), (1, 0), (0, 1), (1, 1)]
                .into_iter()
                .map(|(dx, dy)| TileId::from_terrain(level + 1, 2 * x + dx, 2 * y + dy))
                .collect()
        }
        fn parent(&self, id: TileId) -> Option<TileId> {
            let (level, x, y) = id.terrain_coord();
            (level > GEO_ROOT.0).then(|| TileId::from_terrain(level - 1, x / 2, y / 2))
        }
        fn properties(&self, id: TileId) -> tuile_core::source::TileProperties {
            use tuile_core::math::{BoundingVolume, Sphere};
            let (level, x, y) = id.terrain_coord();
            let h = geo_height(level);
            let center = geo_at(level, x, y, 0.5, 0.5, h);
            let radius = center.distance(geo_at(level, x, y, 0.0, 0.0, h)) + 10.0;
            tuile_core::source::TileProperties {
                bounding_volume: BoundingVolume::Sphere(Sphere { center, radius }),
                geometric_error: if level > GEO_ROOT.0 {
                    0.0
                } else {
                    radius / 8.0
                },
                refine: tuile_core::tileset::Refine::Replace,
                has_content: true,
            }
        }
    }

    struct GeoLoader;

    #[async_trait::async_trait]
    impl TileLoader for GeoLoader {
        async fn load(
            &self,
            id: TileId,
        ) -> Result<tuile_core::source::Loaded, tuile_core::source::LoadError> {
            let (level, x, y) = id.terrain_coord();
            let h = geo_height(level);
            let origin = geo_at(level, x, y, 0.5, 0.5, h);
            let n = 4u32;
            let (mut positions, mut uvs, mut indices) = (Vec::new(), Vec::new(), Vec::new());
            for j in 0..=n {
                for i in 0..=n {
                    let (u, north) = (f64::from(i) / f64::from(n), f64::from(j) / f64::from(n));
                    positions.push(
                        (geo_at(level, x, y, u, north, h) - origin)
                            .as_vec3()
                            .to_array(),
                    );
                    uvs.push([u as f32, 1.0 - north as f32]);
                }
            }
            for j in 0..n {
                for i in 0..n {
                    let k = j * (n + 1) + i;
                    indices.extend_from_slice(&[k, k + 1, k + n + 1, k + n + 1, k + 1, k + n + 2]);
                }
            }
            let mut content = draped_content();
            content.local_origin_ecef = origin;
            content.meshes[0].positions = positions;
            content.meshes[0].uvs = Some(uvs);
            content.meshes[0].indices = indices;
            content.imagery[0].coord = tuile_core::raster::ImageryCoord { level, x, y };
            Ok(tuile_core::source::Loaded::Content(content))
        }
    }

    fn geo_view(u: f64, north: f64, above: f64) -> ViewStateParams {
        let (l, x, y) = GEO_ROOT;
        let ground = geo_at(l, x, y, u, north, 0.0);
        let up = ground.normalize();
        ViewStateParams {
            position: geo_at(l, x, y, u, north, geo_height(l + 1) + above),
            direction: -up,
            up: up.cross(DVec3::Z).cross(up).normalize(),
            viewport_px: glam::DVec2::new(960.0, 720.0),
            fovy_rad: 45f64.to_radians(),
            fov: None,
            scale: 1.0,
        }
    }

    /// A session that keeps the ground answers the height the frame draws —
    /// the parent's from high up, a child's from low — even finishing by
    /// identity, which lets every mesh go; one not asked to keep it answers
    /// nothing.
    #[test]
    fn the_ground_of_a_frame_is_the_ground_it_draws_even_by_identity() {
        let session = || {
            Session::new(
                Box::new(GeoQuad),
                Arc::new(GeoLoader),
                SessionConfig {
                    finish: Finish::Identity,
                    ..SessionConfig::default()
                },
            )
            .expect("session")
        };
        let how = tuile_film::ground::Prepare::default();
        let (l, x, y) = GEO_ROOT;
        let place = |u: f64, north: f64| {
            let g = tuile_core::geo::ecef_to_geodetic(geo_at(l, x, y, u, north, 0.0));
            (g.lon, g.lat)
        };

        let mut unkept = session();
        unkept
            .frame(vec![geo_view(0.3, 0.3, 200_000.0)])
            .expect("frame");
        assert!(unkept.ground(how, true).is_none());

        let mut kept = session();
        kept.keep_ground();
        let high = kept
            .frame(vec![geo_view(0.3, 0.3, 200_000.0)])
            .expect("frame");
        assert_eq!(high.tiles.len(), 1, "the parent alone from high up");
        assert!(
            high.tiles[0].content.meshes.is_empty(),
            "finished by identity"
        );
        let ground = kept.ground(how, true).expect("kept");
        let (lon, lat) = place(0.3, 0.3);
        let h = ground.height_at(lon, lat).expect("on the parent");
        assert!((h - geo_height(l)).abs() < 0.05, "{h}");

        let low = kept.frame(vec![geo_view(0.3, 0.3, 300.0)]).expect("frame");
        assert!(low.tiles.len() > 1, "children from low down");
        let ground = kept.ground(how, true).expect("kept");
        let h = ground.height_at(lon, lat).expect("on a child");
        assert!((h - geo_height(l + 1)).abs() < 0.05, "{h}");
    }

    /// A flat square of ground as a quadtree five levels deep: 341 tiles, each
    /// with its own mesh and its own picture, so that two tiles can never be
    /// mistaken for one another in a digest.
    struct Quad;

    const QUAD_LEVELS: u32 = 5;
    const QUAD_SIDE_M: f64 = 1600.0;

    fn quad_base(level: u32) -> u64 {
        (4u64.pow(level) - 1) / 3
    }

    fn quad_id(level: u32, x: u64, y: u64) -> TileId {
        TileId(quad_base(level) + y * (1 << level) + x)
    }

    fn quad_coord(id: TileId) -> (u32, u64, u64) {
        let level = (0..QUAD_LEVELS)
            .rev()
            .find(|l| id.0 >= quad_base(*l))
            .expect("a tile of the quad");
        let within = id.0 - quad_base(level);
        (level, within % (1 << level), within / (1 << level))
    }

    impl TileTree for Quad {
        fn roots(&self) -> Vec<TileId> {
            vec![quad_id(0, 0, 0)]
        }
        /// The quad never changes shape, and says so: the flight's golden
        /// then holds with the session keeping its nearest-content distance
        /// from pass to pass, as it does over a real terrain globe.
        fn revision(&self) -> Option<u64> {
            Some(0)
        }
        fn children(&self, id: TileId) -> Vec<TileId> {
            let (level, x, y) = quad_coord(id);
            if level + 1 >= QUAD_LEVELS {
                return Vec::new();
            }
            [(0, 0), (1, 0), (0, 1), (1, 1)]
                .into_iter()
                .map(|(dx, dy)| quad_id(level + 1, 2 * x + dx, 2 * y + dy))
                .collect()
        }
        fn parent(&self, id: TileId) -> Option<TileId> {
            let (level, x, y) = quad_coord(id);
            (level > 0).then(|| quad_id(level - 1, x / 2, y / 2))
        }
        fn properties(&self, id: TileId) -> tuile_core::source::TileProperties {
            use tuile_core::math::{BoundingVolume, Sphere};
            let (level, x, y) = quad_coord(id);
            let side = QUAD_SIDE_M / f64::from(1u32 << level);
            tuile_core::source::TileProperties {
                bounding_volume: BoundingVolume::Sphere(Sphere {
                    center: DVec3::new((x as f64 + 0.5) * side, (y as f64 + 0.5) * side, 0.0),
                    radius: side * std::f64::consts::FRAC_1_SQRT_2,
                }),
                geometric_error: if level + 1 >= QUAD_LEVELS {
                    0.0
                } else {
                    side / 4.0
                },
                refine: tuile_core::tileset::Refine::Replace,
                has_content: true,
            }
        }
    }

    /// Serves the quad, counting loads; each tile's mesh and picture are its
    /// own.
    struct QuadLoader(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait::async_trait]
    impl TileLoader for QuadLoader {
        async fn load(
            &self,
            id: TileId,
        ) -> Result<tuile_core::source::Loaded, tuile_core::source::LoadError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (level, x, y) = quad_coord(id);
            let mut content = draped_content();
            content.meshes[0].positions[1] = [id.0 as f32, 1.0, 0.0];
            let layer = &mut content.imagery[0];
            layer.coord = tuile_core::raster::ImageryCoord { level, x, y };
            layer.texture = Arc::new(DecodedTexture::from_rgba8(
                2,
                2,
                [
                    (id.0 % 251) as u8,
                    (id.0 / 251) as u8,
                    level as u8 * 40,
                    255,
                ]
                .repeat(4),
            ));
            Ok(tuile_core::source::Loaded::Content(content))
        }
    }

    fn quad_view(x: f64, y: f64, height: f64) -> ViewStateParams {
        ViewStateParams {
            position: DVec3::new(x, y, height),
            direction: DVec3::new(0.0, 0.0, -1.0),
            up: DVec3::new(0.0, 1.0, 0.0),
            viewport_px: glam::DVec2::new(960.0, 720.0),
            fovy_rad: 45f64.to_radians(),
            fov: None,
            scale: 1.0,
        }
    }

    /// Bytes folded into one number, the same on every machine and toolchain.
    fn fnv(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    }

    /// A tile handed over: its id, its drape, and digests of its mesh and
    /// of its encoded picture.
    type Handed = (u64, u64, u64, u64);

    /// What a flight over the quad hands a consumer, frame by frame: which
    /// tiles, under which drape, with which mesh and which encoded picture —
    /// and how many tiles were fetched to get there.
    fn quad_flight() -> (Vec<Vec<Handed>>, usize) {
        let (frames, loads) = quad_flight_finished(Finish::Pictures);
        let frames = frames
            .into_iter()
            .map(|(frame, tiles)| {
                tiles
                    .into_iter()
                    .enumerate()
                    .map(|(index, (id, drape))| {
                        let tile = frame
                            .tiles
                            .iter()
                            .position(|t| t.tile.0 == id)
                            .expect("the tile is in its frame");
                        let _ = index;
                        let mesh = &frame.tiles[tile].content.meshes[0];
                        let positions: Vec<u8> = mesh
                            .positions
                            .iter()
                            .flatten()
                            .flat_map(|v| v.to_le_bytes())
                            .collect();
                        let png = frame
                            .texture_png(tile, 0)
                            .expect("encoding")
                            .expect("a draped tile has its picture");
                        (id, drape, fnv(&positions), fnv(&png.png))
                    })
                    .collect()
            })
            .collect();
        (frames, loads)
    }

    /// The flight, finished one way or the other: each frame, and the tiles
    /// it names with their drapes, in order.
    #[allow(clippy::type_complexity)]
    fn quad_flight_finished(finish: Finish) -> (Vec<(Frame, Vec<(u64, u64)>)>, usize) {
        let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut session = Session::new(
            Box::new(Quad),
            Arc::new(QuadLoader(Arc::clone(&loads))),
            SessionConfig {
                finish,
                ..SessionConfig::default()
            },
        )
        .expect("session");
        let poses = [
            (800.0, 800.0, 6000.0),
            (800.0, 800.0, 1500.0),
            (500.0, 700.0, 600.0),
            (1100.0, 300.0, 250.0),
            (1100.0, 300.0, 2500.0),
            (300.0, 1200.0, 400.0),
        ];
        let frames = poses
            .into_iter()
            .map(|(x, y, height)| {
                let frame = session
                    .frame(vec![quad_view(x, y, height)])
                    .expect("a frame");
                let mut tiles: Vec<(u64, u64)> =
                    frame.tiles.iter().map(|t| (t.tile.0, t.drape())).collect();
                tiles.sort_unstable();
                (frame, tiles)
            })
            .collect();
        (frames, loads.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// The quad's flight with `config`: each frame's tiles and drapes, the
    /// loads, and how many tiles the session held after the last frame.
    #[allow(clippy::type_complexity)]
    fn quad_flight_with(config: SessionConfig) -> (Vec<Vec<(u64, u64)>>, usize, usize) {
        let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut session = Session::new(
            Box::new(Quad),
            Arc::new(QuadLoader(Arc::clone(&loads))),
            config,
        )
        .expect("session");
        let poses = [
            (800.0, 800.0, 6000.0),
            (800.0, 800.0, 1500.0),
            (500.0, 700.0, 600.0),
            (1100.0, 300.0, 250.0),
            (1100.0, 300.0, 2500.0),
            (300.0, 1200.0, 400.0),
            (1300.0, 1300.0, 300.0),
        ];
        let frames = poses
            .into_iter()
            .map(|(x, y, height)| {
                let frame = session
                    .frame(vec![quad_view(x, y, height)])
                    .expect("a frame");
                let mut tiles: Vec<(u64, u64)> =
                    frame.tiles.iter().map(|t| (t.tile.0, t.drape())).collect();
                tiles.sort_unstable();
                tiles
            })
            .collect();
        let held = match &session.0 {
            Source::Live(live) => live.held.meshes.len(),
            Source::Packed(_) => unreachable!("a live session"),
        };
        (
            frames,
            loads.load(std::sync::atomic::Ordering::Relaxed),
            held,
        )
    }

    /// Letting go between frames bounds what a long flight holds, and keeps
    /// a bake a function of its flight: two bakes that let go give the same
    /// frames, tile for tile and drape for drape, each one whole. What was
    /// let go is fetched again when a frame needs it.
    ///
    /// Not the frames of a bake that keeps everything, in general: a frame
    /// converges over passes, refinement has a dead band against the pass
    /// before (`Config::refine_hysteresis`), and a zoom-out that has to
    /// fetch its coarse ground again settles on the other side of it.
    #[test]
    fn letting_go_between_frames_bounds_what_is_held_and_is_the_same_twice() {
        for finish in [Finish::Pictures, Finish::Identity] {
            // The quad is five levels deep: the default floor of pinned
            // levels would keep all of it.
            let config = |release| SessionConfig {
                budget: {
                    let mut budget = BudgetConfig::batch();
                    budget.pinned_level = None;
                    budget
                },
                finish,
                release_beyond: release,
                ..SessionConfig::default()
            };
            let (kept, kept_loads, kept_held) = quad_flight_with(config(None));
            let (trimmed, trimmed_loads, trimmed_held) = quad_flight_with(config(Some(1)));
            let (again, _, _) = quad_flight_with(config(Some(1)));
            let differ = trimmed.iter().zip(&kept).filter(|(t, k)| t != k).count();
            println!(
                "{finish:?}: everything kept, {kept_loads} loads and {kept_held} held; \
                 let go, {trimmed_loads} loads and {trimmed_held} held; \
                 {differ} frame(s) of {} drawn otherwise",
                kept.len()
            );
            assert_eq!(trimmed, again, "{finish:?}: the same frames twice");
            assert!(
                trimmed_held < kept_held / 2,
                "{finish:?}: {trimmed_held} held against {kept_held}"
            );
            assert!(
                trimmed_loads > kept_loads,
                "{finish:?}: what was let go came back"
            );
        }
    }

    /// The behaviour every change to how a frame is resolved must leave alone:
    /// the same flight gives the same tiles, drapes, meshes and pictures, and
    /// fetches no more than it did. Printed in full when it moves, so the
    /// difference can be read rather than guessed.
    #[test]
    fn a_flight_over_the_quad_resolves_as_it_always_has() {
        let (loads, summary) = quad_summary(quad_flight());
        println!("QUAD-FLIGHT loads={loads} frames={summary:?}");
        assert_eq!(
            (loads, summary.as_slice()),
            QUAD_FLIGHT,
            "the flight no longer resolves as it did"
        );
    }

    /// Loads as tasks on a multi-thread runtime land in whatever order the
    /// workers finish them. Ten flights, ten times the golden: what a frame
    /// selects does not depend on that order.
    ///
    /// What this does not cover: a loader that reshapes the tree (the quad's
    /// does not), whose deferred edits are tested in `tuile-core` and
    /// `tuile-terrain`; and arrival orders a real network produces that ten
    /// runs of an instant loader happen not to.
    #[test]
    fn ten_flights_with_loads_as_tasks_resolve_alike() {
        for run in 0..10 {
            let (loads, summary) = quad_summary(quad_flight());
            assert_eq!(
                (loads, summary.as_slice()),
                QUAD_FLIGHT,
                "run {run}: the flight resolved otherwise"
            );
        }
    }

    /// A flight's frames, each as its tile count and one digest.
    fn quad_summary((frames, loads): (Vec<Vec<Handed>>, usize)) -> (usize, Vec<(usize, u64)>) {
        let summary: Vec<(usize, u64)> = frames
            .iter()
            .map(|tiles| {
                let bytes: Vec<u8> = tiles
                    .iter()
                    .flat_map(|(id, drape, mesh, png)| {
                        [id, drape, mesh, png]
                            .into_iter()
                            .flat_map(|v| v.to_le_bytes())
                    })
                    .collect();
                (tiles.len(), fnv(&bytes))
            })
            .collect();
        (loads, summary)
    }

    /// A pack that refers to the tile store holds no mesh and no picture: a
    /// tile's identity and its drape's are all a bake by reference needs. The
    /// same flight then names exactly the same tiles under the same drapes,
    /// fetches the same, and composes and encodes nothing.
    #[test]
    fn a_flight_finished_by_identity_names_the_same_tiles_and_makes_no_picture() {
        let (pictures, loads_with) = quad_flight_finished(Finish::Pictures);
        let (identity, loads_without) = quad_flight_finished(Finish::Identity);
        assert_eq!(loads_without, loads_with, "the same tiles are fetched");
        let named = |flight: &[(Frame, Vec<(u64, u64)>)]| -> Vec<Vec<(u64, u64)>> {
            flight.iter().map(|(_, tiles)| tiles.clone()).collect()
        };
        assert_eq!(
            named(&identity),
            named(&pictures),
            "the same tiles, the same drapes"
        );
        assert!(named(&identity)
            .iter()
            .flatten()
            .all(|(_, drape)| *drape != 0));
        for (frame, _) in &identity {
            for (index, tile) in frame.tiles.iter().enumerate() {
                assert!(
                    tile.memoized.is_none(),
                    "tile {}: a picture was encoded",
                    tile.tile.0
                );
                assert!(tile.content.textures.is_empty() && tile.content.imagery.is_empty());
                assert!(
                    tile.content.meshes.is_empty(),
                    "tile {}: a mesh was kept",
                    tile.tile.0
                );
                assert!(frame.texture_png(index, 0).expect("asked").is_none());
            }
        }
        // And with pictures, every tile still has its own.
        for (frame, _) in &pictures {
            assert!(frame
                .tiles
                .iter()
                .all(|t| t.memoized.is_some() && !t.content.meshes.is_empty()));
        }
    }

    /// A bake over the quad moves its timers: one frame and one pose per
    /// frame baked, and a finish for every tile the pack holds at least.
    ///
    /// The metrics are the process's, and other tests bake, finish and pose
    /// alongside this one: the counts are compared as deltas, and as at least
    /// what this bake did. What this does not cover: that a sample's duration
    /// is right — only that each one is taken.
    #[test]
    fn a_bake_times_its_frames_poses_and_finishes() {
        let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut session = Session::new(
            Box::new(Quad),
            Arc::new(QuadLoader(loads)),
            SessionConfig::default(),
        )
        .expect("session");
        let poses: Vec<tuile_tape::Frame> = [
            (800.0, 800.0, 6000.0),
            (800.0, 800.0, 1500.0),
            (500.0, 700.0, 600.0),
            (1100.0, 300.0, 250.0),
        ]
        .into_iter()
        .map(|(x, y, height)| tuile_tape::Frame {
            position: [x, y, height],
            direction: [0.0, 0.0, -1.0],
            up: [0.0, 1.0, 0.0],
            fovy: 45f64.to_radians(),
        })
        .collect();
        let m = tuile_core::metrics::metrics();
        let counts = || {
            (
                m.bake_frame_seconds.count(),
                m.pose_seconds.count(),
                m.finish_seconds.count(),
            )
        };
        let before = counts();
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("quad.tuilepack");
        crate::bake::bake_frames(
            &mut session,
            &crate::bake::BakeFrames {
                first: 1,
                last: poses.len() as u32,
                viewport: (960.0, 720.0),
                out: &out,
                scene: crate::bake::SceneName::Given("quad".into()),
                culling: "full",
                content: &crate::bake::PackContent::embedded(),
                packed: &crate::bake::PackedMirror::new(),
            },
            &mut crate::bake::Poses(&poses),
        )
        .expect("a bake");
        let after = counts();
        let bytes = std::fs::read(&out).expect("the pack");
        let tiles = tuile_pack::Pack::open(&bytes).expect("open").tile_count() as u64;
        let frames = poses.len() as u64;
        assert!(tiles > 0, "the bake packed tiles");
        assert!(
            after.0 - before.0 >= frames,
            "bake frames: {before:?} -> {after:?}"
        );
        assert!(
            after.1 - before.1 >= frames,
            "poses: {before:?} -> {after:?}"
        );
        assert!(
            after.2 - before.2 >= tiles,
            "finishes for {tiles} tiles: {before:?} -> {after:?}"
        );
    }

    /// Building a ground and reading heights off it are timed and counted:
    /// one sample per ground built; the queries by samples, each standing
    /// for `SAMPLED` of them.
    #[test]
    fn a_ground_and_its_heights_are_timed_and_counted() {
        let mut session = Session::new(
            Box::new(GeoQuad),
            Arc::new(GeoLoader),
            SessionConfig {
                finish: Finish::Identity,
                ..SessionConfig::default()
            },
        )
        .expect("session");
        session.keep_ground();
        session
            .frame(vec![geo_view(0.3, 0.3, 300.0)])
            .expect("frame");
        let (l, x, y) = GEO_ROOT;
        let g = tuile_core::geo::ecef_to_geodetic(geo_at(l, x, y, 0.3, 0.3, 0.0));
        let m = tuile_core::metrics::metrics();
        let counts = || {
            (
                m.ground_seconds.count(),
                m.height_queries.get(),
                m.height_query_seconds.count(),
            )
        };
        let before = counts();
        let how = tuile_film::ground::Prepare::default();
        let sampled = tuile_film::ground::SAMPLED;
        for stitch in [true, false] {
            let ground = session.ground(how, stitch).expect("kept");
            for _ in 0..2 * sampled {
                assert!(ground.height_at(g.lon, g.lat).is_some());
            }
        }
        // Four times `SAMPLED` queries on this thread: four samples at
        // least, whatever it had asked before.
        let after = counts();
        assert!(after.0 - before.0 >= 2, "grounds: {before:?} -> {after:?}");
        assert!(
            after.1 - before.1 >= 4 * sampled,
            "queries: {before:?} -> {after:?}"
        );
        assert!(
            after.2 - before.2 >= 4 * sampled,
            "query samples: {before:?} -> {after:?}"
        );
    }

    /// What a tile refers to in the tile store — its terrain tile, its imagery
    /// tiles and how they are placed, the side its mosaic would have — is the
    /// same whether or not the picture is made: that is what a pack by
    /// reference stores.
    #[test]
    fn what_a_tile_refers_to_does_not_depend_on_its_picture_being_made() {
        let tile = TileId(7);
        let (level, x, y) = tile.terrain_coord();
        let noted = tuile_planetary::Provenance::default();
        noted.fetched_terrain(
            tuile_terrain::TileCoord::new(level, x, y),
            draped_content().local_origin_ecef.to_array(),
            tuile_planetary::digest(b"terrain bytes"),
        );
        noted.fetched_imagery(draped_content().imagery[0].coord, b"imagery bytes");
        let memo = TextureMemo::default();
        let finished = |finish| {
            finish_tile_as(
                &memo,
                "t",
                tile,
                draped_content(),
                256,
                Some(&noted),
                finish,
            )
        };
        let (with, without) = (finished(Finish::Pictures), finished(Finish::Identity));
        assert!(with.refs.is_some(), "a reference");
        assert_eq!(without.refs, with.refs);
        assert_eq!(without.drape(), with.drape());
        assert_eq!(without.origin_ecef, with.origin_ecef);
        assert!(with.memoized.is_some() && without.memoized.is_none());
    }

    /// A residency budget is an interactive notion: it keeps a viewer inside
    /// its device by letting go of tiles it can fetch again. A batch frame is
    /// exact or it is nothing — it is ready when every tile it draws is
    /// there — so evicting on a budget while it loads only takes away tiles
    /// it is waiting on, and the frame never ends (measured: frame 19 of a
    /// 4K bake spent its fifteen minutes this way). The session's budget is
    /// the batch one: unbounded, nothing needed ever cancelled.
    #[test]
    fn the_batch_path_has_no_residency_budget() {
        use tuile_core::budget::Bytes;
        let budget = exact_budget(SessionConfig::default().budget);
        for (class, target) in budget.target.iter() {
            assert_eq!(target, Bytes::MAX, "{class:?}");
        }
        for (class, hard) in budget.hard_limit.iter() {
            assert_eq!(hard, Bytes::MAX, "{class:?}");
        }
        assert_eq!(budget.cancel_grace, tuile_core::budget::Micros::NEVER);
    }

    /// What [`quad_flight`] gave before anything was optimised.
    const QUAD_FLIGHT: (usize, &[(usize, u64)]) = (
        341,
        &[
            (16, 16587390133593125620),
            (256, 16996631967489129171),
            (80, 15948968281700671764),
            (24, 15794992583552988641),
            (256, 16996631967489129171),
            (42, 3053308558410328886),
        ],
    );

    /// Eviction must never strand a frame that already promised a texture:
    /// the frame holds its own reference to the bytes.
    #[test]
    fn eviction_cannot_strand_a_live_frame() {
        // A budget of one byte evicts on every insert.
        let memo = Arc::new(TextureMemo::new(1));
        let tile = finish_tile(&memo, "test", TileId(7), draped_content(), 256);
        let frame = Frame::with_memo("ion-1-2", vec![Arc::new(tile)], Arc::clone(&memo));
        let png = frame.texture_png(0, 0).expect("encoding").expect("present");
        // Push another texture through; the first is evicted from the memo.
        let other = finish_tile(&memo, "test", TileId(9), draped_content(), 256);
        let other_frame = Frame::with_memo("ion-1-2", vec![Arc::new(other)], Arc::clone(&memo));
        let _ = other_frame.texture_png(0, 0).expect("encoding");
        // The first frame still answers, from its own map.
        let again = frame.texture_png(0, 0).expect("encoding").expect("present");
        assert!(Arc::ptr_eq(&png, &again));
    }
}
