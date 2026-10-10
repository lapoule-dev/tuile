// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! One tile store, read two ways.
//!
//! The native store writes it — catalog, zones, manifests, archives — and
//! then both it and the portable reader, which knows only `Objects`, are
//! asked the same questions. Wherever one may stand for the other, their
//! answers are the same.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::TryStreamExt;
use object_store::memory::InMemory;
use object_store::{ObjectStore, ObjectStoreExt};
use tuile_farm::{ObjectRunStore, Tuning};
use tuile_repository::{
    ArchivedTiles, Bench, Get, Got, Objects, RemoteBlocks, RemoteLive, RepoError, StoreObjects,
    TileRepository,
};
use tuile_tile_server::catalog;
use tuile_tile_server::{Catalog, Grid, LayerDef, StoreConfig, TileStore};

const DAY: u64 = 24 * 3600;

/// The API, called in place of a network: every path asked for is noted.
struct Api {
    bench: Arc<Bench>,
    asked: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Get for Api {
    async fn get(&self, path: &str) -> Result<Got, String> {
        let path = format!("/api/{path}");
        self.asked.lock().expect("lock").push(path.clone());
        // A network answers later: without this every request would finish
        // before the next began, and nothing asked at once would overlap.
        tokio::task::yield_now().await;
        let reply = self
            .bench
            .get(&path, "", None)
            .await
            .ok_or("no such route")?;
        let (status, object_size) = (reply.status, reply.object_size);
        let body = reply.whole().await.map_err(|e| e.to_string())?;
        Ok(Got {
            status,
            object_size,
            etag: None,
            body,
        })
    }
}

fn layer(name: &str, grid: Grid, expiry_days: Option<u64>) -> LayerDef {
    LayerDef {
        name: name.into(),
        grid,
        tile_type: "png".into(),
        tile_compression: "none".into(),
        zone_level: 4,
        expiry_days,
        content_type: "image/png".into(),
    }
}

/// Bytes that say which tile they are.
fn body(layer: &str, level: u8, x: u32, y: u32) -> Vec<u8> {
    format!("{layer}/{level}/{x}/{y}").into_bytes()
}

/// What a repository answers, in terms two of them can be compared by.
async fn answer(tiles: &dyn TileRepository, layer: &str, level: u8, x: u32, y: u32) -> String {
    match tiles.tile(layer, level, x, y).await {
        Ok(Some(tile)) => format!(
            "{} {}",
            tile.content_type,
            String::from_utf8_lossy(&tile.bytes)
        ),
        Ok(None) => "absent".into(),
        Err(RepoError::NotFound(_)) => "not found".into(),
        Err(e) => format!("error: {e}"),
    }
}

#[tokio::test]
async fn the_portable_reader_answers_as_the_store_that_wrote() {
    // Written in memory — the store publishes a manifest by a conditional
    // write, which a directory cannot do — then copied out, object for
    // object, to a directory the portable reader is pointed at.
    let memory: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let written = Catalog {
        layers: vec![
            layer("imagery", Grid::WebMercator, None),
            layer("terrain", Grid::Geographic, Some(30)),
        ],
    };
    catalog::write(memory.as_ref(), &written)
        .await
        .expect("catalog");
    let store = TileStore::open(memory.clone(), StoreConfig::default())
        .await
        .expect("open");

    // Above the zone level, at it, below it; two zones; both grids. Enough
    // tiles in one zone that its archive needs leaf directories would take
    // thousands: the root alone is what these exercise.
    let mut put = Vec::new();
    for (name, level, x, y) in [
        ("imagery", 2, 1, 3),
        ("imagery", 4, 9, 6),
        ("imagery", 7, 77, 50),
        ("imagery", 7, 78, 50),
        ("imagery", 7, 5, 120),
        ("terrain", 0, 1, 0),
        ("terrain", 6, 100, 40),
        ("imagery.absent", 7, 79, 50),
    ] {
        store
            .put(name, level, x, y, Bytes::from(body(name, level, x, y)))
            .await
            .expect("put");
        put.push((name, level, x, y));
    }
    store.flush_all().await.expect("flush");
    // A second archive in a zone already written: the newest is read first.
    store
        .put("imagery", 7, 77, 50, Bytes::from_static(b"replaced"))
        .await
        .expect("put");
    store.flush_all().await.expect("flush");

    let dir = tempfile::tempdir().expect("tempdir");
    let listed: Vec<_> = memory.list(None).try_collect().await.expect("list");
    assert!(listed.len() >= 8, "a catalog, manifests and archives");
    for meta in listed {
        let bytes = memory
            .get(&meta.location)
            .await
            .expect("get")
            .bytes()
            .await
            .expect("bytes");
        let path = dir.path().join(meta.location.as_ref());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, bytes).expect("write");
    }
    let objects: Arc<dyn Objects> =
        Arc::new(ObjectRunStore::local(dir.path(), Tuning::default()).expect("store"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let portable = ArchivedTiles::open(objects.clone(), objects.clone(), Arc::new(move || now))
        .await
        .expect("open");
    let native: &dyn TileRepository = &store;

    // The same layers; the store that wrote keeps them in no order.
    let by_name = |mut layers: Vec<tuile_repository::LayerInfo>| {
        layers.sort_by(|a, b| a.name.cmp(&b.name));
        layers
    };
    assert_eq!(by_name(portable.layers()), by_name(native.layers()));
    assert_eq!(portable.layers().len(), 4, "each layer and its shadow");

    let mut asked = 0;
    let questions = put.iter().copied().chain([
        // Never written: in a zone that exists, in one that does not.
        ("imagery", 7, 76, 50),
        ("imagery", 9, 0, 0),
        ("terrain", 6, 0, 0),
        // Not a layer; not a tile of the grid.
        ("nothing", 1, 0, 0),
        ("imagery", 2, 4, 0),
        ("terrain", 0, 2, 0),
        ("imagery", 40, 0, 0),
    ]);
    for (name, level, x, y) in questions {
        let got = answer(&portable, name, level, x, y).await;
        assert_eq!(
            got,
            answer(native, name, level, x, y).await,
            "{name}/{level}/{x}/{y}"
        );
        assert!(!got.starts_with("error"), "{name}/{level}/{x}/{y}: {got}");
        asked += 1;
    }
    assert_eq!(asked, 15);
    assert_eq!(
        answer(&portable, "imagery", 7, 77, 50).await,
        "image/png replaced"
    );
    assert_eq!(
        answer(&portable, "imagery", 7, 78, 50).await,
        "image/png imagery/7/78/50"
    );
    assert_eq!(answer(&portable, "imagery", 7, 76, 50).await, "absent");
    assert_eq!(answer(&portable, "nothing", 1, 0, 0).await, "not found");

    // The same store once more, from the far side of the API: its catalog
    // and manifests whole, its archives by blocks, and the tile found by the
    // reader rather than by the server.
    let bench = Arc::new(Bench {
        projects: Vec::new(),
        tiles: None,
        store: Some(StoreObjects {
            live: objects.clone(),
            archives: objects.clone(),
        }),
        store_at: None,
    });
    let api = Arc::new(Api {
        bench,
        asked: Default::default(),
    });
    let blocks = Arc::new(RemoteBlocks::new(api.clone(), "store"));
    let remote = ArchivedTiles::open(
        Arc::new(RemoteLive::new(api.clone(), "store")),
        blocks.clone(),
        Arc::new(move || now),
    )
    .await
    .expect("open through the API");
    assert_eq!(by_name(remote.layers()), by_name(native.layers()));
    for (name, level, x, y) in put.iter().copied().chain([
        ("imagery", 7, 76, 50),
        ("imagery", 9, 0, 0),
        ("nothing", 1, 0, 0),
        ("imagery", 40, 0, 0),
    ]) {
        assert_eq!(
            answer(&remote, name, level, x, y).await,
            answer(native, name, level, x, y).await,
            "through the API: {name}/{level}/{x}/{y}"
        );
    }
    // Many tiles asked for at once, as a frame does: readers after the
    // same manifest or the same block wait for one request.
    let first_reader = api.asked.lock().expect("lock").len();
    let together = ArchivedTiles::open(
        Arc::new(RemoteLive::new(api.clone(), "store")),
        Arc::new(RemoteBlocks::new(api.clone(), "store")),
        Arc::new(move || now),
    )
    .await
    .expect("open through the API");
    let before = api.asked.lock().expect("lock").len();
    let answers = futures_util::future::join_all(
        put.iter()
            .map(|(name, level, x, y)| answer(&together, name, *level, *x, *y)),
    )
    .await;
    assert!(answers
        .iter()
        .all(|a| a.starts_with("image/") || a.starts_with("application/")));
    let at_once: Vec<String> = api.asked.lock().expect("lock")[before..].to_vec();
    let mut once = at_once.clone();
    once.sort();
    once.dedup();
    assert_eq!(at_once.len(), once.len(), "asked twice: {at_once:?}");
    // The same for a block read by several at once, with nothing above it
    // to line the readers up.
    let alone = RemoteBlocks::new(api.clone(), "store");
    let before = api.asked.lock().expect("lock").len();
    let reads =
        futures_util::future::join_all((0..6).map(|_| alone.read("catalog.json", 0..10))).await;
    assert!(reads
        .iter()
        .all(|r| r.as_deref().ok() == Some(&b"{\n  \"layer"[..])));
    assert_eq!(api.asked.lock().expect("lock").len() - before, 1);
    assert_eq!(alone.counts().fetched, 1);

    // Back to what the first reader asked, which the next lines count.
    api.asked.lock().expect("lock").truncate(first_reader);

    // What crossed the API: the catalog once, a manifest per zone, and
    // blocks of archives — never a tile, and no block twice.
    let asked = api.asked.lock().expect("lock").clone();
    assert!(
        asked.iter().all(|p| p.starts_with("/api/store/")),
        "{asked:?}"
    );
    assert_eq!(
        asked.iter().filter(|p| p.ends_with("catalog.json")).count(),
        1
    );
    let block_paths: Vec<&String> = asked.iter().filter(|p| p.contains("/b8/")).collect();
    let mut distinct = block_paths.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(
        block_paths.len(),
        distinct.len(),
        "a block was asked for twice"
    );
    let counts = blocks.counts();
    assert_eq!(counts.fetched as usize, block_paths.len());
    assert!(counts.held > counts.fetched, "{counts:?}");

    // Archives rewritten under a manifest this reader still holds: both of
    // a zone's archives move to new keys and the manifest is rewritten to
    // name those, as a compaction does when it replaces what it merged. The
    // reader meets the missing archives, reads the manifest again, and finds
    // the tile where it now is.
    let held = ArchivedTiles::open(objects.clone(), objects.clone(), Arc::new(move || now))
        .await
        .expect("open");
    assert_eq!(
        answer(&held, "imagery", 7, 77, 50).await,
        "image/png replaced"
    );
    let zone = "imagery/zones/z4/9/6";
    let manifest_path = dir.path().join(zone).join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).expect("manifest")).expect("json");
    let archives = manifest["archives"].as_array_mut().expect("archives");
    assert_eq!(archives.len(), 2, "the zone was written twice");
    for (n, entry) in archives.iter_mut().enumerate() {
        let moved = format!("{zone}/rewritten-{n}.pmtiles");
        let from = dir.path().join(entry["key"].as_str().expect("key"));
        std::fs::rename(from, dir.path().join(&moved)).expect("rename");
        entry["key"] = moved.into();
    }
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest).expect("json")).expect("write");
    assert_eq!(
        answer(&held, "imagery", 7, 77, 50).await,
        "image/png replaced",
        "the tile is read from the archive only the fresh manifest names"
    );

    // An expiring layer stops answering once its epoch is a lifetime behind;
    // a durable one does not.
    let later = now + 80 * DAY;
    let late = ArchivedTiles::open(objects.clone(), objects, Arc::new(move || later))
        .await
        .expect("open");
    assert_eq!(answer(&late, "terrain", 6, 100, 40).await, "absent");
    assert_eq!(
        answer(&late, "imagery", 4, 9, 6).await,
        "image/png imagery/4/9/6"
    );
}

/// A small object that changes is read again only when it has: the store's
/// own validator says so, and it changes whenever the object is written,
/// even to the same size — which a size alone cannot tell.
#[tokio::test]
async fn an_object_is_read_again_only_when_it_was_written_again() {
    use tuile_repository::Read;

    let root = tempfile::tempdir().expect("dir");
    let store = ObjectRunStore::local(root.path(), Tuning::default()).expect("store");
    let objects: &dyn Objects = &store;
    let path = root.path().join("zone/manifest.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, b"{\"n\":1}").expect("write");

    let Read::Changed { bytes, etag } = objects
        .read_if_changed("zone/manifest.json", None)
        .await
        .expect("read")
    else {
        panic!("nothing was known: the object must come");
    };
    assert_eq!(bytes, b"{\"n\":1}");
    let etag = etag.expect("a store of files gives a validator");
    assert_eq!(
        objects
            .read_if_changed("zone/manifest.json", Some(&etag))
            .await
            .expect("read"),
        Read::Unchanged
    );

    // Written again, to the very same size, a moment later.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    std::fs::write(&path, b"{\"n\":2}").expect("write");
    let Read::Changed { bytes, etag: next } = objects
        .read_if_changed("zone/manifest.json", Some(&etag))
        .await
        .expect("read")
    else {
        panic!("the object was written again: it must come");
    };
    assert_eq!(bytes, b"{\"n\":2}");
    assert_ne!(next.as_deref(), Some(etag.as_str()));

    // Gone is gone, not unchanged.
    assert!(matches!(
        objects.read_if_changed("zone/none.json", Some(&etag)).await,
        Err(RepoError::NotFound(_))
    ));
}

/// The store's small objects from the far side of the API: each carries
/// its store's validator, a client that holds one is sent nothing — and the
/// object is not read to find that out —, one written again is sent whole,
/// and what is not there says so for as long as what is.
#[tokio::test]
async fn a_live_object_is_sent_again_only_when_it_was_written_again() {
    use tuile_repository::{Asked, Read};

    /// Counts the bytes that leave the store.
    struct Counted(Arc<dyn Objects>, std::sync::atomic::AtomicUsize);
    #[async_trait::async_trait]
    impl Objects for Counted {
        fn label(&self) -> String {
            self.0.label()
        }
        async fn list(&self, p: &str) -> Result<Vec<tuile_repository::Entry>, RepoError> {
            self.0.list(p).await
        }
        async fn browse(&self, p: &str) -> Result<tuile_repository::Listing, RepoError> {
            self.0.browse(p).await
        }
        async fn size(&self, key: &str) -> Result<u64, RepoError> {
            self.0.size(key).await
        }
        async fn read(&self, key: &str, range: std::ops::Range<u64>) -> Result<Vec<u8>, RepoError> {
            let bytes = self.0.read(key, range).await?;
            self.1
                .fetch_add(bytes.len(), std::sync::atomic::Ordering::Relaxed);
            Ok(bytes)
        }
        async fn read_if_changed(&self, key: &str, known: Option<&str>) -> Result<Read, RepoError> {
            let read = self.0.read_if_changed(key, known).await?;
            if let Read::Changed { bytes, .. } = &read {
                self.1
                    .fetch_add(bytes.len(), std::sync::atomic::Ordering::Relaxed);
            }
            Ok(read)
        }
    }

    let root = tempfile::tempdir().expect("dir");
    let path = root.path().join("layer/top/manifest.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, b"{\"archives\":[]}").expect("write");
    let files: Arc<dyn Objects> =
        Arc::new(ObjectRunStore::local(root.path(), Tuning::default()).expect("store"));
    let live = Arc::new(Counted(files.clone(), Default::default()));
    let sent = || live.1.load(std::sync::atomic::Ordering::Relaxed);
    let bench = Bench {
        projects: Vec::new(),
        tiles: None,
        store: Some(StoreObjects {
            live: live.clone(),
            archives: files,
        }),
        store_at: None,
    };
    let url = "/api/store/live/layer/top/manifest.json";

    let first = bench.get(url, "", None).await.expect("reply");
    assert_eq!(first.status, 200);
    assert_eq!(first.cache_control, "public, max-age=30");
    let tag = first.etag.clone().expect("a live object has a validator");
    let read_once = sent();
    assert_eq!(read_once, first.body.len());

    let asking = |tag: &str| Asked {
        range: None,
        if_none_match: Some(Box::leak(tag.to_string().into_boxed_str())),
    };
    let held = bench.get(url, "", asking(&tag)).await.expect("reply");
    assert_eq!((held.status, held.body.len()), (304, 0));
    assert_eq!(held.etag.as_deref(), Some(tag.as_str()));
    assert_eq!(
        sent(),
        read_once,
        "an object the client holds was read from the store"
    );

    // Written again, to the same size.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    std::fs::write(&path, b"{\"archives\":[]} ".map(|b| b)[..15].to_vec()).expect("write");
    std::fs::write(&path, b"{\"archivez\":[]}").expect("write");
    let again = bench.get(url, "", asking(&tag)).await.expect("reply");
    assert_eq!(again.status, 200);
    assert_eq!(again.body, b"{\"archivez\":[]}");
    assert_ne!(again.etag.as_deref(), Some(tag.as_str()));

    // Nothing there: said for as long as something there would be.
    let none = bench
        .get("/api/store/live/layer/tone/9/1/1.json", "", None)
        .await
        .expect("reply");
    assert_eq!(none.status, 404);
    assert_eq!(none.cache_control, "public, max-age=30");
    // Not a live object at all: an error like any other, asked again.
    let refused = bench
        .get("/api/store/live/layer/top/a.pmtiles", "", None)
        .await
        .expect("reply");
    assert_eq!((refused.status, refused.cache_control), (400, "no-cache"));
}
