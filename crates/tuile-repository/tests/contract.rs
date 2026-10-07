// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! One contract, every implementation.
//!
//! The same checks run against each film repository, over each kind of
//! `Objects`: a caller holding a `dyn FilmRepository` must be able to rely on
//! what the trait promises without knowing which layout, or which transport,
//! is behind it.

use std::path::Path;
use std::sync::Arc;

use tuile_farm::{ObjectRunStore, Tuning};
use tuile_pack::{blob_start, BakedTile, BakedView, Pack, PackWriter, TextureFormat, PREAMBLE};
use tuile_repository::{
    Cached, DiskChunks, FilmRepository, Objects, RepoError, RunFilms, RunLayout, ScenePacks,
};

fn tile(id: u64) -> BakedTile {
    let f = |v: &[f32]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
    BakedTile {
        id,
        drape: 0,
        origin_ecef: [6_378_137.0, 0.0, 0.0],
        positions: f(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]),
        normals: Vec::new(),
        uvs: Vec::new(),
        indices: [0u32, 1, 2].iter().flat_map(|x| x.to_le_bytes()).collect(),
        vertex_count: 3,
        index_count: 3,
        base_color_factor: [1.0; 4],
        texture: None,
        texture_format: TextureFormat::None,
        refs: None,
    }
}

/// A real pack of frames `first..=last`, written where the layout wants it.
fn pack(path: &Path, first: u32, last: u32) {
    pack_of(path, first, last, [640.0, 480.0]);
}

/// The same, baked for a viewport. A wider one is given more tiles, so that
/// no two of these packs are the same size.
fn pack_of(path: &Path, first: u32, last: u32, viewport_px: [f64; 2]) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let scratch = tempfile::tempdir().expect("scratch");
    let mut w = PackWriter::new("scene", [0.0; 3], scratch.path().join("blob")).expect("writer");
    for frame in first..=last {
        let view = BakedView {
            position: [7_000_000.0 + f64::from(frame), 0.0, 0.0],
            direction: [-1.0, 0.0, 0.0],
            up: [0.0, 0.0, 1.0],
            viewport_px,
            fovy_rad: 1.0,
        };
        let tiles = if viewport_px[0] > 640.0 { 2 } else { 1 };
        w.frame(
            frame,
            view,
            (0..tiles).map(|i| tile(u64::from(frame % 3) + 10 * i)),
        );
    }
    w.finish_to(path).expect("finish");
}

fn put(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, bytes).expect("write");
}

/// The engine's own layout, as its launcher writes it: one directory per
/// scene.
fn scene_bucket(root: &Path) {
    pack(&root.join("packs/aaaa000000000001/1-8.tuilepack"), 1, 8);
    put(
        &root.join("packs/aaaa000000000001/1-8.tuilepack.scene"),
        b"0123456789abcdef\n",
    );
    pack(&root.join("packs/bbbb000000000002/1-4.tuilepack"), 1, 4);
    put(&root.join("packs/notes.txt"), b"not a scene");
    // A scene whose only pack is not one this build reads, and one whose
    // name disagrees with what it holds: still films, with nothing to render.
    put(
        &root.join("packs/cccc000000000003/1-9.tuilepack"),
        b"an older format, or noise",
    );
    pack(&root.join("packs/dddd000000000004/1-9.tuilepack"), 1, 7);
}

/// How the run directories below are laid out. The names are this test's:
/// a layout is configuration, and nothing in the crate knows them.
fn layout() -> RunLayout {
    RunLayout {
        depth: 2,
        chunk_pack: Some("cuts/k{index:03}.tuilepack".into()),
        chunk_ready: Some("cuts/k{index:03}.done".into()),
        whole_pack: Some("all.tuilepack".into()),
        whole_ready: Some("all.done".into()),
    }
}

/// Run directories, as an orchestrator leaves them.
fn run_bucket(root: &Path) {
    // Cut in three: two chunks ready, the third uploaded but not marked.
    let run = root.join("100/first");
    pack(&run.join("cuts/k000.tuilepack"), 1, 4);
    put(&run.join("cuts/k000.done"), b"digest-0\n");
    pack(&run.join("cuts/k001.tuilepack"), 5, 8);
    put(&run.join("cuts/k001.done"), b"digest-1\n");
    pack(&run.join("cuts/k002.tuilepack"), 9, 12);
    put(&run.join("log.txt"), b"a log");

    // Not cut: one pack for the whole film.
    let whole = root.join("200/second");
    pack(&whole.join("all.tuilepack"), 1, 6);
    put(&whole.join("all.done"), b"digest-whole\n");

    // Both: its chunks are the film, the whole pack is kept beside.
    let both = root.join("250/third");
    pack(&both.join("cuts/k000.tuilepack"), 1, 5);
    put(&both.join("cuts/k000.done"), b"d0\n");
    pack(&both.join("cuts/k001.tuilepack"), 6, 9);
    put(&both.join("cuts/k001.done"), b"d1\n");
    pack(&both.join("all.tuilepack"), 1, 9);

    // One chunk of two that this build cannot read.
    let old = root.join("260/fourth");
    pack(&old.join("cuts/k000.tuilepack"), 1, 4);
    put(&old.join("cuts/k000.done"), b"d0\n");
    put(&old.join("cuts/k001.tuilepack"), b"not a pack");
    put(&old.join("cuts/k001.done"), b"d1\n");

    // Not films: nothing baked; and only a chunk still being uploaded.
    put(&root.join("300/empty/notes.txt"), b"nothing here");
    pack(&root.join("400/uploading/cuts/k000.tuilepack"), 1, 3);
}

/// What `FilmRepository` promises, checked through the trait alone.
async fn holds_its_contract(repo: &dyn FilmRepository, objects: &dyn Objects) -> usize {
    let films = repo.films().await.expect("films");
    assert!(!films.is_empty(), "{}: no film found", repo.layout());
    for summary in &films {
        let film = repo.film(&summary.id).await.expect("a listed film opens");
        assert_eq!(film.id, summary.id);
        // Every pack the listing counted is accounted for: read, or said to
        // be unreadable and why. None makes the film fail to open.
        assert!(
            !film.chunks.is_empty() || !film.unreadable.is_empty(),
            "{}: a film with no pack",
            film.id
        );
        assert_eq!(
            film.chunks.len() + film.unreadable.len(),
            summary.packs,
            "{}: the summary miscounts",
            film.id
        );
        let bytes: u64 = film
            .chunks
            .iter()
            .map(|c| c.bytes)
            .chain(film.unreadable.iter().map(|u| u.bytes))
            .sum();
        assert_eq!(bytes, summary.bytes);
        for u in &film.unreadable {
            assert!(!u.why.is_empty());
            assert!(film.chunks.iter().all(|c| c.key != u.key));
        }
        for pair in film.chunks.windows(2) {
            assert!(
                pair[0].last < pair[1].first,
                "{}: chunks overlap or are unsorted",
                film.id
            );
        }
        for chunk in &film.chunks {
            assert!(chunk.first <= chunk.last);
            assert_eq!(
                objects.size(&chunk.key).await.expect("chunk is there"),
                chunk.bytes
            );
            // The pack's own table agrees with what the repository said.
            let preamble = objects
                .read(&chunk.key, 0..PREAMBLE as u64)
                .await
                .expect("preamble");
            let start = blob_start(&preamble).expect("a pack");
            let head = objects.read(&chunk.key, 0..start).await.expect("head");
            let table = Pack::open_table(&head).expect("table");
            assert_eq!(
                table.frame_range(),
                (chunk.first, chunk.last),
                "{}",
                chunk.key
            );
            assert_eq!(chunk.frames as usize, table.views().len(), "{}", chunk.key);
            assert_eq!(chunk.viewport, Some([640, 480]), "{}", chunk.key);
        }
    }
    assert!(matches!(
        repo.film("no/such-film").await,
        Err(RepoError::NotFound(_))
    ));
    assert!(matches!(
        repo.film("../outside").await,
        Err(RepoError::NotFound(_))
    ));
    films.len()
}

/// The same bucket, reached directly and through the chunk cache.
fn stores(root: &Path, cache: &Path) -> Vec<Arc<dyn Objects>> {
    let cached: Arc<dyn Objects> = Arc::new(Cached::new(direct(root), DiskChunks::new(cache)));
    vec![direct(root), cached]
}

#[tokio::test]
async fn every_repository_over_every_store_holds_the_contract() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (scenes_root, runs_root) = (dir.path().join("scenes"), dir.path().join("runs"));
    scene_bucket(&scenes_root);
    run_bucket(&runs_root);
    for objects in stores(&scenes_root, &dir.path().join("cache-scenes")) {
        let repo = ScenePacks::new(objects.clone(), ["packs"]);
        assert_eq!(holds_its_contract(&repo, objects.as_ref()).await, 4);
        // And the same of a repository that remembers, asked twice: what it
        // recalls is held to what it read.
        let repo = repo.remembering(Arc::new(Memory::default()));
        for _ in 0..2 {
            assert_eq!(holds_its_contract(&repo, objects.as_ref()).await, 4);
        }
    }
    for objects in stores(&runs_root, &dir.path().join("cache-runs")) {
        let repo = RunFilms::new(objects.clone(), layout()).expect("layout");
        assert_eq!(holds_its_contract(&repo, objects.as_ref()).await, 4);
        let repo = repo.remembering(Arc::new(Memory::default()));
        for _ in 0..2 {
            assert_eq!(holds_its_contract(&repo, objects.as_ref()).await, 4);
        }
    }
}

/// A store of bytes that keeps everything it is given.
#[derive(Default)]
struct Memory(std::sync::Mutex<std::collections::HashMap<String, bytes::Bytes>>);

#[async_trait::async_trait]
impl tuile_core::storage::ContentStore for Memory {
    async fn get(&self, key: &str) -> Option<bytes::Bytes> {
        self.0.lock().expect("lock").get(key).cloned()
    }

    async fn put(&self, key: &str, value: bytes::Bytes, _: Option<std::time::Duration>) {
        self.0.lock().expect("lock").insert(key.to_string(), value);
    }
}

/// Objects that count the reads made of packs.
struct Counted {
    inner: Arc<dyn Objects>,
    pack_reads: std::sync::atomic::AtomicUsize,
}

impl Counted {
    fn taken(&self) -> usize {
        self.pack_reads
            .swap(0, std::sync::atomic::Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl Objects for Counted {
    fn label(&self) -> String {
        self.inner.label()
    }
    async fn list(&self, prefix: &str) -> Result<Vec<tuile_repository::Entry>, RepoError> {
        self.inner.list(prefix).await
    }
    async fn browse(&self, prefix: &str) -> Result<tuile_repository::Listing, RepoError> {
        self.inner.browse(prefix).await
    }
    async fn size(&self, key: &str) -> Result<u64, RepoError> {
        self.inner.size(key).await
    }
    async fn read(&self, key: &str, range: std::ops::Range<u64>) -> Result<Vec<u8>, RepoError> {
        if key.ends_with(".tuilepack") {
            self.pack_reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.inner.read(key, range).await
    }
}

#[tokio::test]
async fn a_pack_already_listed_is_not_read_to_be_listed_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    scene_bucket(dir.path());
    let objects = Arc::new(Counted {
        inner: direct(dir.path()),
        pack_reads: Default::default(),
    });
    let keeper = Arc::new(Memory::default());
    let film = "packs/aaaa000000000001";
    // Each listing is a repository of its own, as each request to a host is:
    // what is remembered is in the keeper, not in the repository.
    let listed = async || {
        ScenePacks::new(objects.clone(), ["packs"])
            .remembering(keeper.clone())
            .film(film)
            .await
            .expect("film")
    };
    let first = listed().await;
    assert!(objects.taken() > 0, "the pack was never read");
    let again = listed().await;
    assert_eq!(objects.taken(), 0, "a pack already listed was read again");
    assert_eq!(again, first);
    assert_eq!(first.chunks[0].frames, 8);
    assert_eq!(first.chunks[0].viewport, Some([640, 480]));

    // A pack written again under its key is another pack: it is read, and
    // listed as what it now holds.
    let key = dir.path().join(film).join("1-8.tuilepack");
    let before = std::fs::metadata(&key).expect("pack").len();
    std::fs::remove_file(&key).expect("remove");
    pack_of(&key, 1, 8, [1920.0, 1080.0]);
    assert_ne!(std::fs::metadata(&key).expect("pack").len(), before);
    let replaced = listed().await;
    assert!(objects.taken() > 0, "a replaced pack was not read");
    assert_eq!(replaced.chunks[0].viewport, Some([1920, 1080]));
}

fn direct(root: &Path) -> Arc<dyn Objects> {
    Arc::new(ObjectRunStore::local(root, Tuning::default()).expect("store"))
}

#[tokio::test]
async fn a_scene_directory_is_a_film_with_its_digest() {
    let dir = tempfile::tempdir().expect("tempdir");
    scene_bucket(dir.path());
    let film = ScenePacks::new(direct(dir.path()), ["packs"])
        .film("packs/aaaa000000000001")
        .await
        .expect("film");
    assert_eq!(film.chunks.len(), 1);
    assert_eq!((film.chunks[0].first, film.chunks[0].last), (1, 8));
    assert_eq!(film.chunks[0].scene.as_deref(), Some("0123456789abcdef"));
    assert!(film.others.is_empty(), "{:?}", film.others);
}

#[tokio::test]
async fn a_cut_run_gives_its_ready_chunks_and_holds_back_the_rest() {
    let dir = tempfile::tempdir().expect("tempdir");
    run_bucket(dir.path());
    let film = RunFilms::new(direct(dir.path()), layout())
        .expect("layout")
        .film("100/first")
        .await
        .expect("film");
    // Ranges come from the packs: nothing else says them.
    let ranges: Vec<_> = film.chunks.iter().map(|c| (c.first, c.last)).collect();
    assert_eq!(ranges, [(1, 4), (5, 8)]);
    assert_eq!(film.chunks[1].scene.as_deref(), Some("digest-1"));
    // The third pack is in the bucket; its marker is not. It is not a chunk,
    // and it is not hidden.
    assert!(film
        .others
        .iter()
        .any(|e| e.key.ends_with("cuts/k002.tuilepack")));
}

#[tokio::test]
async fn an_uncut_run_is_one_chunk_and_a_run_with_both_is_read_by_its_chunks() {
    let dir = tempfile::tempdir().expect("tempdir");
    run_bucket(dir.path());
    let repo = RunFilms::new(direct(dir.path()), layout()).expect("layout");

    let whole = repo.film("200/second").await.expect("film");
    assert_eq!(whole.chunks.len(), 1);
    assert_eq!((whole.chunks[0].first, whole.chunks[0].last), (1, 6));
    assert_eq!(whole.chunks[0].scene.as_deref(), Some("digest-whole"));

    let both = repo.film("250/third").await.expect("film");
    let ranges: Vec<_> = both.chunks.iter().map(|c| (c.first, c.last)).collect();
    assert_eq!(ranges, [(1, 5), (6, 9)]);
    assert!(both
        .others
        .iter()
        .any(|e| e.key.ends_with("/all.tuilepack")));

    // Nothing baked, or only an upload in progress: not films, not listed.
    let ids: Vec<_> = repo
        .films()
        .await
        .expect("films")
        .into_iter()
        .map(|f| f.id)
        .collect();
    assert_eq!(ids, ["100/first", "200/second", "250/third", "260/fourth"]);

    // A chunk that cannot be read is reported, and the film still opens.
    let partly = repo.film("260/fourth").await.expect("film");
    assert_eq!(partly.chunks.len(), 1);
    assert_eq!(partly.unreadable.len(), 1);
    assert!(partly.unreadable[0].key.ends_with("cuts/k001.tuilepack"));
    assert!(matches!(
        repo.film("400/uploading").await,
        Err(RepoError::NotFound(_))
    ));
}

#[test]
fn a_layout_that_can_find_no_pack_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let none = RunLayout {
        chunk_pack: None,
        whole_pack: None,
        ..layout()
    };
    assert!(RunFilms::new(direct(dir.path()), none).is_err());
    let unnumbered = RunLayout {
        chunk_pack: Some("cuts/chunk.tuilepack".into()),
        ..layout()
    };
    assert!(RunFilms::new(direct(dir.path()), unnumbered).is_err());
}

#[tokio::test]
async fn a_pack_this_build_cannot_read_is_reported_not_fatal() {
    let dir = tempfile::tempdir().expect("tempdir");
    scene_bucket(dir.path());
    let repo = ScenePacks::new(direct(dir.path()), ["packs"]);
    let old = repo
        .film("packs/cccc000000000003")
        .await
        .expect("an old film still opens");
    assert!(old.chunks.is_empty());
    assert_eq!(old.unreadable.len(), 1);
    let renamed = repo.film("packs/dddd000000000004").await.expect("film");
    assert!(renamed.chunks.is_empty());
    assert!(
        renamed.unreadable[0].why.contains("named 1–9, holds 1–7"),
        "{}",
        renamed.unreadable[0].why
    );
}
