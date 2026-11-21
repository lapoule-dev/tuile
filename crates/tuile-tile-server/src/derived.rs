// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Layers made from another layer: the **parent** layer of a source.
//!
//! A source's coarse levels are often another acquisition than its fine
//! ones — other dates, other processing, other colours — and a renderer that
//! stands on a coarser tile while a finer one loads shows the seam. A parent
//! layer rebuilds each tile `(z, x, y)` from the source's four children at
//! `z + 1` — `(2x, 2y)`, `(2x + 1, 2y)`, `(2x, 2y + 1)`, `(2x + 1, 2y + 1)` —
//! averaged two by two in linear light: the same pictures as the fine level,
//! a quarter of the size. One level only: a tile of the parent layer is made
//! from the source, never from the parent layer itself.
//!
//! It is a layer like any other, by composition:
//!
//! - **its source is injected**, as the [`Upstream`] every layer's origin
//!   is. In a server that is [`ServedLayer`]: the source layer exactly as the
//!   [`TileService`] serves it — hot tiles, store, the other instances, its
//!   own origin, single flight — so the parent knows nothing of where the
//!   children come from;
//! - **it is an [`Upstream`] itself** ([`ParentLayer`]): a [`TileService`]
//!   serves, stores, expires and shares its tiles like any fetched tile, and
//!   another parent layer could be made of it;
//! - **it is declared** in the catalog: a layer whose
//!   [`crate::LayerDef::derived_from`] names its source.
//!
//! **No imagery, no tile.** When any of the four children is missing — the
//! source has no such tile, or one that stands for no imagery — the parent
//! tile is an absence too, never a partial picture: the client stands on the
//! coarser tile above, as it does for any absence.
//!
//! **Kept up to date by its source.** A parent tile is made once and kept.
//! When one of its children changes, whoever stored that child names the
//! parent ([`Derivations::dependents`]) and has it made again
//! ([`TileService::refresh`]): the new copy replaces the old one, which is
//! served until it does.
//!
//! **Deterministic.** The same four children give the same bytes: an
//! integer downsample, and libwebp's encoder on one thread.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Semaphore;

use crate::catalog::{Catalog, LayerDef};
use crate::service::{ServiceError, TileService};
use crate::upstream::{Upstream, UpstreamError};
use crate::StoreError;

/// Default for [`ParentConfig::webp_quality`]: libwebp's lossy quality.
pub const DEFAULT_WEBP_QUALITY: f32 = 85.0;
/// Default for [`ParentConfig::builds`].
pub const DEFAULT_BUILDS: usize = 8;

/// How a parent layer makes its tiles.
#[derive(Debug, Clone, Copy)]
pub struct ParentConfig {
    /// libwebp's lossy quality, 0 to 100.
    pub webp_quality: f32,
    /// Tiles being made at once, per parent layer: what bounds the memory
    /// of the builds (each holds four children and their pictures), and the
    /// children asked of the source at once (four a build).
    pub builds: usize,
}

impl Default for ParentConfig {
    fn default() -> Self {
        Self {
            webp_quality: DEFAULT_WEBP_QUALITY,
            builds: DEFAULT_BUILDS,
        }
    }
}

/// The bytes one build holds at most, beside its four encoded children: the
/// four pictures decoded, the parent, and the parent encoded (at most its
/// own size again), for tiles `side` pixels wide.
pub fn build_bytes(side: u32) -> u64 {
    let picture = u64::from(side) * u64::from(side) * 3;
    4 * 4 * picture + 2 * picture
}

// ── The downsample ─────────────────────────────────────────────────────────

/// sRGB code → linear light on 16 bits: `round(65535 · srgb⁻¹(k / 255))`,
/// IEC 61966-2-1. Written out, so every build of every platform reads the
/// same numbers.
pub const SRGB_TO_LINEAR: [u16; 256] = [
    0, 20, 40, 60, 80, 99, 119, 139, 159, 179, 199, 219, 241, 264, 288, 313, 340, 367, 396, 427,
    458, 491, 526, 562, 599, 637, 677, 718, 761, 805, 851, 898, 947, 997, 1048, 1101, 1156, 1212,
    1270, 1330, 1391, 1453, 1517, 1583, 1651, 1720, 1790, 1863, 1937, 2013, 2090, 2170, 2250, 2333,
    2418, 2504, 2592, 2681, 2773, 2866, 2961, 3058, 3157, 3258, 3360, 3464, 3570, 3678, 3788, 3900,
    4014, 4129, 4247, 4366, 4488, 4611, 4736, 4864, 4993, 5124, 5257, 5392, 5530, 5669, 5810, 5953,
    6099, 6246, 6395, 6547, 6700, 6856, 7014, 7174, 7335, 7500, 7666, 7834, 8004, 8177, 8352, 8528,
    8708, 8889, 9072, 9258, 9445, 9635, 9828, 10022, 10219, 10417, 10619, 10822, 11028, 11235,
    11446, 11658, 11873, 12090, 12309, 12530, 12754, 12980, 13209, 13440, 13673, 13909, 14146,
    14387, 14629, 14874, 15122, 15371, 15623, 15878, 16135, 16394, 16656, 16920, 17187, 17456,
    17727, 18001, 18277, 18556, 18837, 19121, 19407, 19696, 19987, 20281, 20577, 20876, 21177,
    21481, 21787, 22096, 22407, 22721, 23038, 23357, 23678, 24002, 24329, 24658, 24990, 25325,
    25662, 26001, 26344, 26688, 27036, 27386, 27739, 28094, 28452, 28813, 29176, 29542, 29911,
    30282, 30656, 31033, 31412, 31794, 32179, 32567, 32957, 33350, 33745, 34143, 34544, 34948,
    35355, 35764, 36176, 36591, 37008, 37429, 37852, 38278, 38706, 39138, 39572, 40009, 40449,
    40891, 41337, 41785, 42236, 42690, 43147, 43606, 44069, 44534, 45002, 45473, 45947, 46423,
    46903, 47385, 47871, 48359, 48850, 49344, 49841, 50341, 50844, 51349, 51858, 52369, 52884,
    53401, 53921, 54445, 54971, 55500, 56032, 56567, 57105, 57646, 58190, 58737, 59287, 59840,
    60396, 60955, 61517, 62082, 62650, 63221, 63795, 64372, 64952, 65535,
];

/// Linear light on 16 bits → the nearest sRGB code: code `k` for every
/// value from the midpoint below `SRGB_TO_LINEAR[k]` to the one above
/// (integer midpoints, rounded up).
fn linear_to_srgb_table() -> &'static [u8; 65536] {
    static TABLE: OnceLock<Box<[u8; 65536]>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = Box::new([0u8; 65536]);
        let mut code = 0usize;
        for (v, slot) in table.iter_mut().enumerate() {
            while code < 255 {
                let above = (u32::from(SRGB_TO_LINEAR[code]) + u32::from(SRGB_TO_LINEAR[code + 1]))
                    .div_ceil(2);
                if (v as u32) < above {
                    break;
                }
                code += 1;
            }
            *slot = code as u8;
        }
        table
    })
}

/// The sRGB code nearest a linear value on 16 bits.
pub fn linear_to_srgb(v: u16) -> u8 {
    linear_to_srgb_table()[usize::from(v)]
}

/// Four children — north-west, north-east, south-west, south-east, each
/// `side × side` RGB, row by row — into their parent, `side × side`: every
/// parent pixel the mean of the four child pixels it covers, taken in linear
/// light and rounded to the nearest. Integers only.
///
/// # Panics
/// When a child is not `side × side × 3` bytes, or `side` is odd.
pub fn downsample(children: [&[u8]; 4], side: usize) -> Vec<u8> {
    assert!(
        side.is_multiple_of(2),
        "a tile of odd side has no 2×2 blocks"
    );
    for c in &children {
        assert_eq!(c.len(), side * side * 3, "a child is not {side}×{side} RGB");
    }
    let to_srgb = linear_to_srgb_table();
    let half = side / 2;
    let mut out = vec![0u8; side * side * 3];
    for py in 0..side {
        for px in 0..side {
            let child = children[usize::from(px >= half) + 2 * usize::from(py >= half)];
            let (cx, cy) = (2 * (px % half), 2 * (py % half));
            for ch in 0..3 {
                let at = |x: usize, y: usize| {
                    u32::from(SRGB_TO_LINEAR[usize::from(child[(y * side + x) * 3 + ch])])
                };
                let sum = at(cx, cy) + at(cx + 1, cy) + at(cx, cy + 1) + at(cx + 1, cy + 1);
                out[(py * side + px) * 3 + ch] = to_srgb[((sum + 2) / 4) as usize];
            }
        }
    }
    out
}

/// A picture as WebP, lossy, at `quality`, on one thread: the same bytes
/// for the same picture.
pub fn encode_webp(rgb: &[u8], side: u32, quality: f32) -> Result<Vec<u8>, String> {
    let mut config =
        webp::WebPConfig::new().map_err(|()| "libwebp: no default configuration".to_string())?;
    config.lossless = 0;
    config.quality = quality;
    config.thread_level = 0;
    let encoded = webp::Encoder::from_rgb(rgb, side, side)
        .encode_advanced(&config)
        .map_err(|e| format!("libwebp: {e:?}"))?;
    Ok(encoded.to_vec())
}

/// Why a parent tile is an absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyCause {
    /// The source has no such child.
    ChildAbsent,
    /// A child the source has stands for no imagery.
    ChildEmptyImagery,
}

impl EmptyCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChildAbsent => "child_absent",
            Self::ChildEmptyImagery => "child_empty_imagery",
        }
    }
}

/// Whether a child's bytes stand for no imagery: the source layer's own
/// recogniser.
pub type IsEmpty = Arc<dyn Fn(&[u8]) -> bool + Send + Sync>;

/// A parent layer: each tile made from four tiles of `source`.
pub struct ParentLayer {
    /// The parent layer's name: its metrics' label.
    layer: String,
    source: Arc<dyn Upstream>,
    is_empty: IsEmpty,
    builds: Semaphore,
    webp_quality: f32,
}

impl ParentLayer {
    pub fn new(
        layer: impl Into<String>,
        source: Arc<dyn Upstream>,
        is_empty: IsEmpty,
        cfg: ParentConfig,
    ) -> Self {
        Self {
            layer: layer.into(),
            source,
            is_empty,
            builds: Semaphore::new(cfg.builds.max(1)),
            webp_quality: cfg.webp_quality,
        }
    }

    /// The four children of `(level, x, y)`: NW, NE, SW, SE.
    pub fn children(level: u8, x: u32, y: u32) -> Option<[(u8, u32, u32); 4]> {
        let z = level.checked_add(1)?;
        let (cx, cy) = (x.checked_mul(2)?, y.checked_mul(2)?);
        Some([
            (z, cx, cy),
            (z, cx + 1, cy),
            (z, cx, cy + 1),
            (z, cx + 1, cy + 1),
        ])
    }

    fn empty(&self, cause: EmptyCause) -> Result<Option<Bytes>, UpstreamError> {
        metrics::counter!("tuile_derived_builds_total", "layer" => self.layer.clone(), "outcome" => "empty")
            .increment(1);
        metrics::counter!("tuile_derived_empty_total", "layer" => self.layer.clone(), "cause" => cause.as_str())
            .increment(1);
        Ok(None)
    }

    fn failed(&self, e: UpstreamError) -> Result<Option<Bytes>, UpstreamError> {
        metrics::counter!("tuile_derived_builds_total", "layer" => self.layer.clone(), "outcome" => "error")
            .increment(1);
        Err(e)
    }
}

/// One build in flight, counted while it lives.
struct InFlight(String);

impl InFlight {
    fn new(layer: &str) -> Self {
        metrics::gauge!("tuile_derived_builds_in_flight", "layer" => layer.to_string())
            .increment(1.0);
        Self(layer.to_string())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        metrics::gauge!("tuile_derived_builds_in_flight", "layer" => self.0.clone()).decrement(1.0);
    }
}

/// Decodes four children, checks they are one size, and makes the parent.
fn make(children: &[Bytes], quality: f32) -> Result<Vec<u8>, String> {
    let mut pictures = Vec::with_capacity(4);
    for (i, bytes) in children.iter().enumerate() {
        let picture = image::load_from_memory(bytes)
            .map_err(|e| format!("child {i}: {e}"))?
            .to_rgb8();
        pictures.push(picture);
    }
    let side = pictures[0].width();
    if !side.is_multiple_of(2)
        || pictures
            .iter()
            .any(|p| p.width() != side || p.height() != side)
    {
        let sizes: Vec<_> = pictures.iter().map(|p| (p.width(), p.height())).collect();
        return Err(format!(
            "children are not four squares of one even side: {sizes:?}"
        ));
    }
    let rgb = downsample(
        [
            pictures[0].as_raw(),
            pictures[1].as_raw(),
            pictures[2].as_raw(),
            pictures[3].as_raw(),
        ],
        side as usize,
    );
    encode_webp(&rgb, side, quality)
}

#[async_trait]
impl Upstream for ParentLayer {
    async fn fetch(&self, level: u8, x: u32, y: u32) -> Result<Option<Bytes>, UpstreamError> {
        let Some(children) = Self::children(level, x, y) else {
            return Ok(None);
        };
        // The bound on builds, and so on what they ask of the source at once.
        let asked = Instant::now();
        let _permit = self
            .builds
            .acquire()
            .await
            .map_err(|_| UpstreamError(format!("{}: closed", self.layer)))?;
        metrics::histogram!("tuile_derived_build_wait_seconds", "layer" => self.layer.clone())
            .record(asked.elapsed().as_secs_f64());
        let _counted = InFlight::new(&self.layer);
        let began = Instant::now();

        let got = futures_util::future::join_all(
            children
                .iter()
                .map(|&(z, cx, cy)| self.source.fetch(z, cx, cy)),
        )
        .await;
        // An error first: a child that could not be read is not a child the
        // source does not have, and an absence is not to be recorded for it.
        let mut bytes = Vec::with_capacity(4);
        let mut absent = false;
        for g in got {
            match g {
                Err(e) => {
                    return self.failed(UpstreamError(format!("{}: a child: {}", self.layer, e.0)))
                }
                Ok(None) => absent = true,
                Ok(Some(b)) => bytes.push(b),
            }
        }
        if absent {
            return self.empty(EmptyCause::ChildAbsent);
        }
        if bytes.iter().any(|b| (self.is_empty)(b)) {
            return self.empty(EmptyCause::ChildEmptyImagery);
        }
        let quality = self.webp_quality;
        let made = tokio::task::spawn_blocking(move || make(&bytes, quality))
            .await
            .map_err(|e| UpstreamError(format!("a build: {e}")))
            .and_then(|r| r.map_err(|e| UpstreamError(format!("{}: {e}", self.layer))));
        let made = match made {
            Ok(m) => m,
            Err(e) => return self.failed(e),
        };
        metrics::counter!("tuile_derived_builds_total", "layer" => self.layer.clone(), "outcome" => "built")
            .increment(1);
        metrics::histogram!("tuile_derived_build_seconds", "layer" => self.layer.clone())
            .record(began.elapsed().as_secs_f64());
        metrics::histogram!("tuile_derived_webp_bytes", "layer" => self.layer.clone())
            .record(made.len() as f64);
        metrics::counter!("tuile_derived_webp_bytes_total", "layer" => self.layer.clone())
            .increment(made.len() as u64);
        Ok(Some(Bytes::from(made)))
    }
}

// ── The source, as served ──────────────────────────────────────────────────

/// Where a [`ServedLayer`] finds its service, once it exists: the service
/// is built with its layers' upstreams, a parent layer's among them.
pub type ServiceSlot = Arc<OnceLock<Weak<TileService>>>;

/// A layer of a [`TileService`], as an [`Upstream`]: each tile asked of it
/// is asked of the service, which answers it the way it answers a client.
pub struct ServedLayer {
    layer: String,
    service: ServiceSlot,
}

impl ServedLayer {
    pub fn new(layer: impl Into<String>, service: ServiceSlot) -> Self {
        Self {
            layer: layer.into(),
            service,
        }
    }
}

#[async_trait]
impl Upstream for ServedLayer {
    async fn fetch(&self, level: u8, x: u32, y: u32) -> Result<Option<Bytes>, UpstreamError> {
        let service = self
            .service
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| UpstreamError(format!("{}: no service to ask", self.layer)))?;
        let began = Instant::now();
        let got = service.tile(&self.layer, level, x, y).await;
        let (tier, bytes) = match &got {
            Ok(Some(t)) => (t.source.as_str(), t.bytes.len() as u64),
            Ok(None) | Err(ServiceError::OutOfGrid(_)) => ("absent", 0),
            Err(_) => ("error", 0),
        };
        metrics::counter!("tuile_derived_child_fetches_total", "layer" => self.layer.clone(), "tier" => tier)
            .increment(1);
        metrics::histogram!("tuile_derived_child_fetch_seconds", "layer" => self.layer.clone(), "tier" => tier)
            .record(began.elapsed().as_secs_f64());
        metrics::counter!("tuile_derived_child_bytes_total", "layer" => self.layer.clone(), "tier" => tier)
            .increment(bytes);
        match got {
            Ok(t) => Ok(t.map(|t| t.bytes)),
            // A child past the grid's last level: no such tile.
            Err(ServiceError::OutOfGrid(_)) => Ok(None),
            Err(e) => Err(UpstreamError(e.to_string())),
        }
    }
}

// ── The catalog's parent layers ────────────────────────────────────────────

/// The parent layers a catalog declares, ready to be served.
pub struct Derivations {
    /// Parent layer → its source layer.
    sources: Vec<(String, String)>,
    upstreams: HashMap<String, Arc<dyn Upstream>>,
    slot: ServiceSlot,
}

impl Derivations {
    /// Every layer of `catalog` that names a source: a [`ParentLayer`] over
    /// that source as served ([`ServedLayer`]). A source the catalog does
    /// not have, on another grid, or a parent layer that is not WebP, is an
    /// error of the catalog.
    pub fn from_catalog(catalog: &Catalog, cfg: ParentConfig) -> Result<Self, StoreError> {
        let slot: ServiceSlot = Arc::default();
        let mut sources = Vec::new();
        let mut upstreams: HashMap<String, Arc<dyn Upstream>> = HashMap::new();
        for def in &catalog.layers {
            let Some(from) = &def.derived_from else {
                continue;
            };
            let source: &LayerDef =
                catalog
                    .layers
                    .iter()
                    .find(|l| &l.name == from)
                    .ok_or_else(|| {
                        StoreError::Corrupt(format!(
                            "catalog: {} is derived from {from}, which it does not list",
                            def.name
                        ))
                    })?;
            if source.name == def.name {
                return Err(StoreError::Corrupt(format!(
                    "catalog: {} is derived from itself",
                    def.name
                )));
            }
            if source.grid != def.grid {
                return Err(StoreError::Corrupt(format!(
                    "catalog: {} is not on the grid of {from}",
                    def.name
                )));
            }
            if def.tile_type != "webp" {
                return Err(StoreError::Corrupt(format!(
                    "catalog: {} is made as WebP, not {}",
                    def.name, def.tile_type
                )));
            }
            let layer = source.to_layer()?;
            let is_empty: IsEmpty =
                Arc::new(move |bytes: &[u8]| crate::index::is_empty_tile(&layer, bytes));
            let served = Arc::new(ServedLayer::new(from.clone(), slot.clone()));
            upstreams.insert(
                def.name.clone(),
                Arc::new(ParentLayer::new(def.name.clone(), served, is_empty, cfg)),
            );
            sources.push((def.name.clone(), from.clone()));
        }
        Ok(Self {
            sources,
            upstreams,
            slot,
        })
    }

    /// The parent layers' upstreams, by layer: for the service.
    pub fn upstreams(&self) -> HashMap<String, Arc<dyn Upstream>> {
        self.upstreams.clone()
    }

    /// The parent layers and their sources.
    pub fn sources(&self) -> &[(String, String)] {
        &self.sources
    }

    /// Hands the service to the parent layers' sources: what they ask their
    /// children of. Once; a second service is ignored.
    pub fn bind(&self, service: &Arc<TileService>) {
        let _ = self.slot.set(Arc::downgrade(service));
    }

    /// The parent tiles made from the tile `(level, x, y)` of `layer`: what
    /// is to be made again when that tile changes.
    pub fn dependents(
        &self,
        layer: &str,
        level: u8,
        x: u32,
        y: u32,
    ) -> Vec<(String, u8, u32, u32)> {
        if level == 0 {
            return Vec::new();
        }
        self.sources
            .iter()
            .filter(|(_, from)| from == layer)
            .map(|(parent, _)| (parent.clone(), level - 1, x / 2, y / 2))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_comes_back_as_itself() {
        for k in 0..=255u8 {
            assert_eq!(
                linear_to_srgb(SRGB_TO_LINEAR[usize::from(k)]),
                k,
                "code {k}"
            );
        }
    }

    #[test]
    fn the_table_is_the_standard_curve() {
        for (k, &v) in SRGB_TO_LINEAR.iter().enumerate() {
            let c = k as f64 / 255.0;
            let lin = if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            };
            assert_eq!(f64::from(v), (lin * 65535.0).round(), "code {k}");
        }
        assert!(
            SRGB_TO_LINEAR.windows(2).all(|w| w[0] < w[1]),
            "strictly increasing"
        );
    }

    #[test]
    fn a_child_s_dependents_are_its_parent_in_each_parent_layer() {
        let catalog: Catalog = serde_json::from_value(serde_json::json!({"layers": [
            {"name": "src", "grid": "web-mercator", "tile_type": "jpeg", "tile_compression": "none",
             "zone_level": 10, "content_type": "image/jpeg"},
            {"name": "src-parent", "grid": "web-mercator", "tile_type": "webp", "tile_compression": "none",
             "zone_level": 10, "content_type": "image/webp", "derived_from": "src"}
        ]}))
        .expect("catalog");
        let d = Derivations::from_catalog(&catalog, ParentConfig::default()).expect("derivations");
        assert_eq!(
            d.dependents("src", 13, 4265, 2987),
            vec![("src-parent".to_string(), 12, 2132, 1493)]
        );
        assert!(d.dependents("src", 0, 0, 0).is_empty());
        assert!(
            d.dependents("src-parent", 13, 1, 1).is_empty(),
            "one level only: a parent tile has no parent here"
        );
        assert!(d.upstreams().contains_key("src-parent"));
    }

    #[test]
    fn a_catalog_whose_parent_layer_cannot_be_made_is_refused() {
        let layer = |name: &str, grid: &str, ty: &str, from: Option<&str>| {
            serde_json::json!({"name": name, "grid": grid, "tile_type": ty, "tile_compression": "none",
                "zone_level": 10, "content_type": "x", "derived_from": from})
        };
        for layers in [
            vec![layer("p", "web-mercator", "webp", Some("nowhere"))],
            vec![
                layer("s", "geographic", "jpeg", None),
                layer("p", "web-mercator", "webp", Some("s")),
            ],
            vec![
                layer("s", "web-mercator", "jpeg", None),
                layer("p", "web-mercator", "jpeg", Some("s")),
            ],
            vec![layer("p", "web-mercator", "webp", Some("p"))],
        ] {
            let catalog: Catalog =
                serde_json::from_value(serde_json::json!({ "layers": layers })).expect("catalog");
            assert!(
                Derivations::from_catalog(&catalog, ParentConfig::default()).is_err(),
                "{layers:?}"
            );
        }
    }
}
