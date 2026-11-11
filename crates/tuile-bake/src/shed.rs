// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A bake that names its tiles holds none of their meshes.
//!
//! A session that finishes its tiles by identity ([`crate::Finish::Identity`])
//! draws nothing: a pack by reference records each tile's counts and where
//! it comes from, and the render builds the mesh again from the tile store.
//! What it does read of a mesh is the ground a camera is kept over, and that
//! is kept small ([`crate::ground::Kept`]). Yet every mesh came whole and was
//! held whole as long as its tile was resident — by the session and by the
//! server's own copy of what it delivered.
//!
//! So the loader is wrapped ([`Shedding`]): each terrain tile is loaded as
//! always, the ground the session keeps is taken from the whole mesh there
//! and handed over by the tile's key, and the content goes on with its
//! meshes shed ([`DecodedTileContent::shed`]) — their counts and colour, which
//! the budget charges exactly as it charged them whole, so the same tiles are
//! selected, loaded, acknowledged and let go, and the pack records the same
//! counts. A tile delivered again with nothing handed over — the server
//! sends what it still holds without loading it — is read again from the
//! store ([`Shedding::reread`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tuile_core::content::{DecodedMesh, DecodedTileContent, DrapeContent};
use tuile_core::source::{DrapeReach, LoadError, Loaded, TileId, TileLoader};

use crate::ground::Kept;

/// What the session keeps of a tile's mesh for the ground: the mesh kept
/// small, and the mesh as it came when the session was asked for it too
/// ([`crate::Session::keep_ground_and_meshes`], for tests).
#[derive(Clone)]
pub(crate) struct KeptTile {
    pub kept: Arc<Kept>,
    pub as_it_came: Option<Arc<DecodedMesh>>,
}

impl KeptTile {
    /// What is kept of `mesh`, tile `tile`'s first mesh about `origin`;
    /// the mesh as it came with it when `as_it_came`.
    pub(crate) fn of(
        tile: TileId,
        origin: glam::DVec3,
        mesh: &DecodedMesh,
        as_it_came: bool,
    ) -> Self {
        Self {
            kept: Arc::new(Kept::of(tile, origin, mesh)),
            as_it_came: as_it_came.then(|| {
                Arc::new(DecodedMesh {
                    positions: mesh.positions.clone(),
                    normals: None,
                    uvs: mesh.uvs.clone(),
                    indices: mesh.indices.clone(),
                    material: Default::default(),
                })
            }),
        }
    }

    /// [`Self::of`], from a mesh that is let go, its `skirts` apart when
    /// they came apart: the mesh as it came is moved, not copied, and its
    /// skirts hung back on it.
    fn of_owned(
        tile: TileId,
        origin: glam::DVec3,
        mesh: DecodedMesh,
        skirts: Option<&tuile_terrain::Skirts>,
        as_it_came: bool,
    ) -> Self {
        let kept = Arc::new(match skirts {
            Some(skirts) => Kept::of_apart(tile, origin, &mesh, skirts),
            None => Kept::of(tile, origin, &mesh),
        });
        Self {
            kept,
            as_it_came: as_it_came.then(|| {
                let mut mesh = DecodedMesh {
                    normals: None,
                    material: Default::default(),
                    ..mesh
                };
                if let Some(skirts) = skirts {
                    skirts.hang_on(origin, &mut mesh);
                }
                Arc::new(mesh)
            }),
        }
    }
}

/// A loader whose terrain tiles come with their meshes shed, the ground
/// the session keeps of each handed over beside them.
pub(crate) struct Shedding {
    inner: Arc<dyn TileLoader>,
    /// What was kept of each tile loaded and not yet delivered.
    handed: Mutex<HashMap<TileId, KeptTile>>,
    /// Whether the session keeps the ground ([`crate::Session::keep_ground`]):
    /// if not, nothing is kept and nothing handed.
    keep_ground: AtomicBool,
    /// Whether it keeps each mesh as it came too.
    keep_meshes: AtomicBool,
    /// Tests: hand nothing over, so that every ground is read again.
    #[cfg(test)]
    pub(crate) hand_nothing: AtomicBool,
    /// Tests: how many grounds were read again.
    #[cfg(test)]
    pub(crate) rereads: std::sync::atomic::AtomicUsize,
}

impl Shedding {
    pub(crate) fn new(inner: Arc<dyn TileLoader>) -> Self {
        Self {
            inner,
            handed: Mutex::new(HashMap::new()),
            keep_ground: AtomicBool::new(false),
            keep_meshes: AtomicBool::new(false),
            #[cfg(test)]
            hand_nothing: AtomicBool::new(false),
            #[cfg(test)]
            rereads: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// From now on the ground of each tile loaded is kept and handed over;
    /// with each mesh as it came too when `meshes`.
    pub(crate) fn keep_ground(&self, meshes: bool) {
        self.keep_ground.store(true, Ordering::SeqCst);
        if meshes {
            self.keep_meshes.store(true, Ordering::SeqCst);
        }
    }

    /// What was kept of `tile`'s mesh when it was last loaded, taken: what
    /// the session holds beside the content once it is delivered.
    pub(crate) fn take(&self, tile: TileId) -> Option<KeptTile> {
        self.handed.lock().ok()?.remove(&tile)
    }

    /// How many tiles' grounds wait to be delivered.
    #[cfg(test)]
    pub(crate) fn waiting(&self) -> usize {
        self.handed.lock().map_or(0, |h| h.len())
    }

    /// What is kept of `content`'s first mesh, and the content with every
    /// mesh shed — on the finish pool, the work being the CPU's.
    async fn shed(
        tile: TileId,
        mut content: DecodedTileContent,
        keep: bool,
        meshes: bool,
    ) -> (DecodedTileContent, Option<KeptTile>) {
        let (done, result) = tokio::sync::oneshot::channel();
        crate::session::finish_pool().spawn(move || {
            let origin = content.local_origin_ecef;
            // Skirts that came apart from the surface: counted as the walls
            // they would have been, and kept as what they hang from.
            let apart = content.apart.take();
            let skirts = apart
                .as_ref()
                .and_then(|a| a.get::<tuile_terrain::Skirts>());
            let mut let_go = content.shed_meshes().into_iter();
            if let (Some(skirts), Some(first)) = (skirts, content.shed.first_mut()) {
                let (vertices, indices) = skirts.counts();
                first.vertex_count += vertices as u32;
                first.index_count += indices as u32;
            }
            let kept = let_go
                .next()
                .filter(|_| keep)
                .map(|first| KeptTile::of_owned(tile, origin, first, skirts, meshes));
            drop(let_go);
            let _ = done.send((content, kept));
        });
        result.await.expect("shedding a tile's meshes panicked")
    }

    /// `tile`'s ground, read again from the store: the tile loaded again,
    /// kept, and its content let go. `None` when it does not load to a mesh.
    pub(crate) async fn reread(&self, tile: TileId) -> Option<KeptTile> {
        #[cfg(test)]
        self.rereads.fetch_add(1, Ordering::SeqCst);
        let Ok(Loaded::Content(content)) = self.inner.load(tile).await else {
            return None;
        };
        content.meshes.first()?;
        let meshes = self.keep_meshes.load(Ordering::SeqCst);
        Self::shed(tile, content, true, meshes).await.1
    }
}

#[async_trait]
impl TileLoader for Shedding {
    async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
        let content = match self.inner.load(id).await? {
            Loaded::Content(content) if !content.meshes.is_empty() => content,
            other => return Ok(other),
        };
        let keep = self.keep_ground.load(Ordering::SeqCst);
        #[cfg(test)]
        let keep = keep && !self.hand_nothing.load(Ordering::SeqCst);
        let meshes = self.keep_meshes.load(Ordering::SeqCst);
        let (content, kept) = Self::shed(id, content, keep, meshes).await;
        if let (Some(kept), Ok(mut handed)) = (kept, self.handed.lock()) {
            handed.insert(id, kept);
        }
        Ok(Loaded::Content(content))
    }

    fn drapes(&self, id: TileId) -> Option<DrapeReach> {
        self.inner.drapes(id)
    }

    async fn load_drape(
        &self,
        id: TileId,
        layer: tuile_core::budget::LayerId,
        level: u32,
    ) -> Result<DrapeContent, LoadError> {
        self.inner.load_drape(id, layer, level).await
    }

    fn holds(&self, r: tuile_core::budget::ResourceId) -> tuile_core::budget::Presence {
        self.inner.holds(r)
    }

    fn store_events(&self) -> u64 {
        self.inner.store_events()
    }

    fn store_changes(
        &self,
        since: u64,
        asked: &[(tuile_core::budget::ResourceId, tuile_core::budget::Presence)],
        touched: &mut Vec<tuile_core::budget::ResourceId>,
    ) -> u64 {
        self.inner.store_changes(since, asked, touched)
    }

    async fn warm_up(&self, through_level: u32) {
        self.inner.warm_up(through_level).await;
    }

    fn epoch(&self) -> u64 {
        self.inner.epoch()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::content::{DecodedMesh, MaterialDesc};
    use tuile_terrain::{to_ground, GeographicTilingScheme, Header, QuantizedMesh, TileCoord};

    /// Terrain tile 14/16253/12320, a relief on a 16 × 16 grid with its
    /// skirts, as the globe's loader hands it over.
    struct Relief;

    fn relief() -> DecodedTileContent {
        let (mesh, rect) = quantized();
        let mut content = to_ground(&mesh, &rect);
        content.meshes[0].material = MaterialDesc {
            base_color_factor: [0.3, 0.4, 0.2, 1.0],
            ..MaterialDesc::default()
        };
        content
    }

    /// [`relief`], its skirts apart, as a bake's globe hands it over.
    fn relief_apart() -> DecodedTileContent {
        let (mesh, rect) = quantized();
        let (mut content, skirts) = tuile_terrain::to_ground_apart(&mesh, &rect, false);
        content.apart = Some(tuile_core::content::Apart::new(skirts));
        content.meshes[0].material = MaterialDesc {
            base_color_factor: [0.3, 0.4, 0.2, 1.0],
            ..MaterialDesc::default()
        };
        content
    }

    fn quantized() -> (QuantizedMesh, tuile_terrain::GeoRect) {
        let coord = TileCoord::new(14, 16253, 12320);
        let rect = GeographicTilingScheme::default().tile_rect(coord);
        let n = 17usize;
        let q = |f: f64| (f * 32767.0).round() / 32767.0;
        let (mut u, mut v, mut height) = (Vec::new(), Vec::new(), Vec::new());
        for j in 0..n {
            for i in 0..n {
                let (fu, fv) = (i as f64 / 16.0, j as f64 / 16.0);
                u.push(q(fu));
                v.push(q(fv));
                height.push(q(0.5 + 0.5 * (7.0 * fu).sin() * (5.0 * fv).cos()));
            }
        }
        let mut indices = Vec::new();
        for j in 0..n - 1 {
            for i in 0..n - 1 {
                let a = (j * n + i) as u32;
                let (b, c, d) = (a + 1, a + n as u32, a + n as u32 + 1);
                indices.extend_from_slice(&[a, b, c, c, b, d]);
            }
        }
        let (lon, lat) = rect.center();
        let centre = tuile_core::geo::geodetic_to_ecef(tuile_core::geo::Geodetic {
            lon,
            lat,
            height: 400.0,
        });
        let line = |f: &dyn Fn(usize) -> usize| (0..n).map(|k| f(k) as u32).collect::<Vec<u32>>();
        let mesh = QuantizedMesh {
            header: Header {
                center: centre.to_array(),
                min_height: -35.5,
                max_height: 1832.25,
                bounding_sphere_center: centre.to_array(),
                bounding_sphere_radius: 1.0e5,
                horizon_occlusion: [0.0; 3],
            },
            u,
            v,
            height,
            indices,
            normals: None,
            edges: [
                line(&|k| k * n),
                line(&|k| k),
                line(&|k| k * n + n - 1),
                line(&|k| (n - 1) * n + k),
            ],
            metadata_available: None,
            cut: 0,
        };
        (mesh, rect)
    }

    struct ReliefApart;

    #[async_trait]
    impl TileLoader for ReliefApart {
        async fn load(&self, _id: TileId) -> Result<Loaded, LoadError> {
            Ok(Loaded::Content(relief_apart()))
        }
    }

    #[async_trait]
    impl TileLoader for Relief {
        async fn load(&self, _id: TileId) -> Result<Loaded, LoadError> {
            Ok(Loaded::Content(relief()))
        }
    }

    fn tile() -> TileId {
        TileId::from_terrain(14, 16253, 12320)
    }

    fn bits(mesh: &DecodedMesh) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
        (
            mesh.positions
                .iter()
                .flatten()
                .map(|f| f.to_bits())
                .collect(),
            mesh.uvs
                .iter()
                .flatten()
                .flatten()
                .map(|f| f.to_bits())
                .collect(),
            mesh.indices.clone(),
        )
    }

    /// A tile loaded through the shedding loader comes with no mesh, charged
    /// what it was charged whole, and its ground handed over: the mesh kept
    /// as the session would have kept it, and as it came when asked. Read
    /// again, it is kept the same.
    #[test]
    fn a_shed_tile_is_charged_as_it_was_and_its_ground_handed_over() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let whole = relief();
        let shedding = Shedding::new(Arc::new(Relief));
        shedding.keep_ground(true);
        let Ok(Loaded::Content(shed)) = runtime.block_on(shedding.load(tile())) else {
            unreachable!("the relief loads");
        };
        assert!(shed.meshes.is_empty(), "the mesh is let go");
        assert_eq!(shed.cost_held(true), whole.cost_held(true));
        assert_eq!(shed.cost_held(false), whole.cost_held(false));
        assert_eq!(shed.shed[0].base_color_factor, [0.3, 0.4, 0.2, 1.0]);
        assert_eq!(shed.local_origin_ecef, whole.local_origin_ecef);

        let came = &whole.meshes[0];
        let kept = shedding.take(tile()).expect("handed over");
        assert_eq!(shedding.waiting(), 0);
        let reference = Kept::of(tile(), whole.local_origin_ecef, came);
        assert!(
            kept.kept.same_as(&reference),
            "kept as the session keeps it"
        );
        assert_eq!(bits(&kept.kept.mesh(tile())), bits(came));
        assert_eq!(
            bits(kept.as_it_came.as_deref().expect("as it came")),
            bits(came)
        );

        let again = runtime
            .block_on(shedding.reread(tile()))
            .expect("read again");
        assert!(again.kept.same_as(&reference), "read again, kept the same");
        assert_eq!(shedding.waiting(), 0, "a reading again hands nothing over");
    }

    /// A session that does not keep the ground is handed nothing.
    #[test]
    fn nothing_is_handed_over_when_no_ground_is_kept() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let shedding = Shedding::new(Arc::new(Relief));
        let Ok(Loaded::Content(shed)) = runtime.block_on(shedding.load(tile())) else {
            unreachable!("the relief loads");
        };
        assert!(shed.meshes.is_empty());
        assert_eq!(shedding.waiting(), 0);
    }

    /// A tile whose skirts came apart from its surface is shed as the whole
    /// tile: charged and counted with its walls, its ground kept so that it
    /// comes back with them, bit for bit, as it came whole.
    #[test]
    fn a_tile_whose_skirts_came_apart_is_shed_as_the_whole_tile() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let (whole, apart) = (
            Shedding::new(Arc::new(Relief)),
            Shedding::new(Arc::new(ReliefApart)),
        );
        whole.keep_ground(true);
        apart.keep_ground(true);
        let shed = |s: &Shedding| match runtime.block_on(s.load(tile())) {
            Ok(Loaded::Content(content)) => content,
            _ => unreachable!("the relief loads"),
        };
        let (from_whole, from_apart) = (shed(&whole), shed(&apart));
        assert!(from_apart.apart.is_none(), "taken, not passed on");
        assert_eq!(from_apart.shed, from_whole.shed, "the same counts");
        assert_eq!(from_apart.cost(), from_whole.cost(), "the same charge");
        let came = relief();
        let (kept_whole, kept_apart) = (
            whole.take(tile()).expect("kept"),
            apart.take(tile()).expect("kept"),
        );
        assert_eq!(bits(&kept_apart.kept.mesh(tile())), bits(&came.meshes[0]));
        assert_eq!(bits(&kept_whole.kept.mesh(tile())), bits(&came.meshes[0]));
        assert!(kept_apart.kept.heap_bytes() < kept_whole.kept.heap_bytes());
        assert_eq!(
            bits(kept_apart.as_it_came.as_deref().expect("as it came")),
            bits(&came.meshes[0])
        );
    }
}
