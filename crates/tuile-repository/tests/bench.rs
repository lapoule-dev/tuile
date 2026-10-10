// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The API as its clients see it, through `Bench::get` alone — which is all
//! a native server and a Worker have in common, and all they need to.

use std::path::Path;
use std::sync::Arc;

use tuile_farm::{ObjectRunStore, Tuning};
use tuile_pack::{BakedView, PackWriter};
use tuile_repository::{Asked, Bench, Objects, Project, ScenePacks};

fn pack(path: &Path, first: u32, last: u32) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let scratch = tempfile::tempdir().expect("scratch");
    let mut w = PackWriter::new("scene", [0.0; 3], scratch.path().join("blob")).expect("writer");
    for frame in first..=last {
        let view = BakedView {
            position: [7_000_000.0, 0.0, 0.0],
            direction: [-1.0, 0.0, 0.0],
            up: [0.0, 0.0, 1.0],
            viewport_px: [640.0, 480.0],
            fovy_rad: 1.0,
        };
        w.frame(frame, view, []);
    }
    w.finish_to(path).expect("finish");
}

fn bench(root: &Path) -> Bench {
    pack(&root.join("packs/aaaa000000000001/1-8.tuilepack"), 1, 8);
    std::fs::write(
        root.join("packs/aaaa000000000001/note with space.txt"),
        b"hello",
    )
    .expect("write");
    let objects: Arc<dyn Objects> =
        Arc::new(ObjectRunStore::local(root, Tuning::default()).expect("store"));
    Bench {
        projects: vec![Project {
            name: "engine".into(),
            films: Arc::new(ScenePacks::new(objects.clone(), ["packs"])),
            objects,
        }],
        tiles: None,
        store: None,
        store_at: None,
    }
}

fn json(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body).expect("json")
}

#[tokio::test]
async fn the_routes_answer_as_documented() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bench = bench(dir.path());
    let key = "packs/aaaa000000000001/1-8.tuilepack";
    let size = std::fs::metadata(dir.path().join(key)).expect("meta").len();

    // A path that is not the API's is not answered at all.
    assert!(bench.get("/index.html", "", None).await.is_none());

    let projects = bench.get("/api/projects", "", None).await.expect("reply");
    assert_eq!(projects.status, 200);
    assert_eq!(json(&projects.body)["projects"][0]["layout"], "scene-packs");
    assert!(json(&projects.body)["tiles"].is_null());

    let films = bench
        .get("/api/p/engine/films", "", None)
        .await
        .expect("reply");
    assert_eq!(json(&films.body)[0]["id"], "packs/aaaa000000000001");

    // A film id is a path, sent percent-encoded or not.
    for path in [
        "/api/p/engine/films/packs/aaaa000000000001",
        "/api/p/engine/films/packs%2Faaaa000000000001",
    ] {
        let film = bench.get(path, "", None).await.expect("reply");
        assert_eq!(film.status, 200, "{path}");
        assert_eq!(json(&film.body)["chunks"][0]["last"], 8);
    }
    assert_eq!(
        bench
            .get("/api/p/engine/films/packs/none", "", None)
            .await
            .expect("reply")
            .status,
        404
    );
    assert_eq!(
        bench
            .get("/api/p/other/films", "", None)
            .await
            .expect("reply")
            .status,
        404
    );
    assert_eq!(
        bench
            .get("/api/nothing", "", None)
            .await
            .expect("reply")
            .status,
        404
    );
    assert_eq!(
        bench
            .get("/api/tiles/catalog", "", None)
            .await
            .expect("reply")
            .status,
        404
    );

    // Bytes, by range.
    let object = format!("/api/p/engine/o/{key}");
    let head = bench
        .get(&object, "", Some("bytes=0-7"))
        .await
        .expect("reply");
    assert_eq!(
        (head.status, head.body.as_slice()),
        (206, &b"TUILEPK\0"[..])
    );
    assert_eq!(
        head.content_range.as_deref(),
        Some(format!("bytes 0-7/{size}").as_str())
    );
    assert!(head.ranged);
    // Bytes are immutable under their key and say so, with a validator: what
    // lets a browser answer the same range again without the network.
    assert_eq!(head.cache_control, "public, max-age=31536000, immutable");
    let tag = head.etag.clone().expect("an object has a validator");
    assert!(tag.starts_with('"') && tag.ends_with('"'));
    let again = bench
        .get(&object, "", Some("bytes=8-15"))
        .await
        .expect("reply");
    assert_eq!(
        again.etag.as_deref(),
        Some(tag.as_str()),
        "one object, one validator, whatever the range"
    );
    // A listing is not: what a bucket holds changes. It is asked for again
    // each time — and, carrying a validator of what it says, answered with
    // nothing when it says the same.
    assert_eq!(films.cache_control, "no-cache");
    let listed = films.etag.clone().expect("a listing has a validator");
    let asking = |if_none_match: &'static str| Asked {
        range: None,
        if_none_match: Some(if_none_match),
    };
    let held: &'static str = Box::leak(listed.clone().into_boxed_str());
    let same = bench
        .get("/api/p/engine/films", "", asking(held))
        .await
        .expect("reply");
    assert_eq!((same.status, same.body.len()), (304, 0));
    assert_eq!(same.etag.as_deref(), Some(listed.as_str()));
    assert_eq!(same.cache_control, "no-cache");
    // Weakened on the way, in a list, or anything at all: still held.
    for theirs in [
        format!("W/{listed}"),
        format!("\"other\", {listed}"),
        "*".to_string(),
    ] {
        let theirs: &'static str = Box::leak(theirs.into_boxed_str());
        let reply = bench
            .get("/api/p/engine/films", "", asking(theirs))
            .await
            .expect("reply");
        assert_eq!(reply.status, 304, "{theirs}");
    }
    // Another validator is another listing: the whole of it again.
    let other = bench
        .get("/api/p/engine/films", "", asking("\"0000000000000000-0\""))
        .await
        .expect("reply");
    assert_eq!((other.status, other.body), (200, films.body.clone()));
    // Bytes held are not sent again either, by range or by block, and a
    // block the client holds is answered without one byte of it being read.
    let held: &'static str = Box::leak(tag.clone().into_boxed_str());
    let bytes = bench
        .get(
            &object,
            "",
            Asked {
                range: Some("bytes=0-7"),
                if_none_match: Some(held),
            },
        )
        .await
        .expect("reply");
    assert_eq!((bytes.status, bytes.body.len()), (304, 0));
    let block = object.replace("/o/", "/b8/0/");
    let whole = bench.get(&block, "", None).await.expect("reply");
    let block_tag: &'static str = Box::leak(
        whole
            .etag
            .clone()
            .expect("a block has a validator")
            .into_boxed_str(),
    );
    let unread = bench
        .get(&block, "", asking(block_tag))
        .await
        .expect("reply");
    assert_eq!(unread.status, 304);
    assert!(unread.later.is_none() && unread.body.is_empty());
    // A pack replaced is a 409 whatever the client holds: never "unchanged".
    let replaced = bench
        .get(&block, "s=1", asking(block_tag))
        .await
        .expect("reply");
    assert_eq!(replaced.status, 409);
    // An error has nothing to validate.
    let missing = bench
        .get("/api/p/engine/films/none", "", asking("*"))
        .await
        .expect("reply");
    assert_eq!(missing.status, 404);
    let past = bench
        .get(&object, "", Some(format!("bytes={size}-").as_str()))
        .await
        .expect("reply");
    assert_eq!(
        (past.status, past.content_range.as_deref()),
        (416, Some(format!("bytes */{size}").as_str()))
    );
    let whole = bench.get(&object, "", None).await.expect("reply");
    assert_eq!((whole.status, whole.body.len() as u64), (200, size));
    assert_eq!(
        bench
            .get("/api/p/engine/o/packs/../x", "", None)
            .await
            .expect("reply")
            .status,
        400
    );
    assert_eq!(
        bench
            .get("/api/p/engine/o/packs/missing", "", None)
            .await
            .expect("reply")
            .status,
        404
    );

    // A block asked for with the size of the object it is meant to be of is
    // served when that is the object there, and refused when it is not: a
    // pack replaced under its key is another pack.
    let named = |meant: u64| {
        let (bench, key) = (&bench, key);
        async move {
            bench
                .get(
                    &format!("/api/p/engine/b8/0/{key}"),
                    &format!("s={meant}"),
                    None,
                )
                .await
                .expect("reply")
        }
    };
    assert_eq!(named(size).await.status, 200);
    let replaced = named(size + 1).await;
    assert_eq!(replaced.status, 409);
    assert!(String::from_utf8_lossy(&replaced.body).contains("replaced"));

    // Blocks: the object cut at fixed offsets, each a whole reply of its own.
    // The size is in the address: a block cut another way is another URL, and
    // the one without a size names nothing.
    assert_eq!(tuile_repository::block_segment(), "b8");
    assert_eq!(
        bench
            .get(&format!("/api/p/engine/b/0/{key}"), "", None)
            .await
            .expect("reply")
            .status,
        404
    );
    let block = bench
        .get(&format!("/api/p/engine/b8/0/{key}"), "", None)
        .await
        .expect("reply");
    assert_eq!(
        block.status, 200,
        "a block is a whole reply, not a partial one"
    );
    let bytes = block.clone().whole().await.expect("the block's bytes");
    assert_eq!(
        bytes.len() as u64,
        size,
        "this object fits in its first block"
    );
    assert_eq!(&bytes[..8], b"TUILEPK\0");
    assert_eq!(block.object_size, Some(size));
    // Kept a moment and then asked about, by its validator: an object can
    // be written again under its key.
    assert_eq!(block.cache_control, "public, max-age=30");
    assert!(block.content_range.is_none());
    assert_ne!(block.etag, head.etag, "a block is not the object");
    assert_eq!(
        bench
            .get(&format!("/api/p/engine/b8/1/{key}"), "", None)
            .await
            .expect("reply")
            .status,
        404
    );
    assert_eq!(
        bench
            .get(&format!("/api/p/engine/b8/x/{key}"), "", None)
            .await
            .expect("reply")
            .status,
        400
    );
    assert_eq!(
        bench
            .get("/api/p/engine/b8/0/packs/../x", "", None)
            .await
            .expect("reply")
            .status,
        400
    );

    // A key with a space, percent-encoded on the way in.
    let note = bench
        .get(
            "/api/p/engine/o/packs/aaaa000000000001/note%20with%20space.txt",
            "",
            None,
        )
        .await
        .expect("reply");
    assert_eq!((note.status, note.body.as_slice()), (200, &b"hello"[..]));
    assert!(note.content_type.starts_with("text/plain"));

    let ls = bench
        .get("/api/p/engine/ls", "prefix=packs%2Faaaa000000000001", None)
        .await
        .expect("reply");
    assert_eq!(json(&ls.body)["files"].as_array().expect("files").len(), 2);
    let root = bench
        .get("/api/p/engine/ls", "", None)
        .await
        .expect("reply");
    assert_eq!(json(&root.body)["dirs"][0], "packs");
}
