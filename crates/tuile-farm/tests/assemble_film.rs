// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A film from segments, through a directory store: three tasks, segments
//! numbered so that lexical order would be wrong, receipts deposited out of
//! order. The segments are real MP4 containers with placeholder pictures,
//! written in Rust — nothing here needs ffmpeg, and neither does the job.

use tuile_farm::assemble::{assemble, Outcome, Receipt, RunKeys};
use tuile_farm::concat::{count_frames, write_test_segment};
use tuile_farm::{ObjectRunStore, RunStore, Tuning};

/// A task: its index, its frames, and its segments as (number, frames).
type Task = (u32, u64, u64, &'static [(u64, u32)]);

const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x0c, 0xac, 0xd9, 0x42];

fn segment(path: &std::path::Path, frames: u32, tag: u8) {
    write_test_segment(path, frames, SPS, tag).expect("test segment");
}

#[tokio::test]
async fn three_tasks_make_one_film_in_frame_order() {
    let root = tempfile::tempdir().expect("tempdir");
    let make = tempfile::tempdir().expect("tempdir");
    // Small parts, so every segment goes through the ranged and multipart paths.
    let store = ObjectRunStore::local(root.path(), Tuning { part_bytes: 8192, concurrency: 4 })
        .expect("store");
    let run = RunKeys::new("renders/film", "e1");

    // Task t renders frames and owns segments t*4 .. t*4+3; only some of
    // them are used, as when a task has fewer frames than processes. Task 2
    // uses seg8 and seg10 — lexically, seg10 sorts before seg2.
    let plan: [Task; 3] = [
        (0, 1, 20, &[(0, 10), (1, 10)]),
        (1, 21, 35, &[(4, 8), (5, 7)]),
        (2, 36, 50, &[(8, 9), (10, 6)]),
    ];
    // Deposited last task first: arrival order must not matter.
    for (i, (task, first, last, segs)) in plan.iter().enumerate().rev() {
        for (g, frames) in segs.iter() {
            let local = make.path().join(format!("seg{g}.mp4"));
            segment(&local, *frames, *g as u8);
            store.put(&local, &run.segment(*g)).await.expect("put segment");
        }
        let receipt = Receipt {
            task: *task,
            first: *first,
            last: *last,
            segments: segs.iter().map(|(g, _)| *g).collect(),
        };
        let body = make.path().join(format!("r{task}.json"));
        std::fs::write(&body, serde_json::to_vec(&receipt).expect("json")).expect("write");
        store.put(&body, &run.receipt(*task)).await.expect("put receipt");

        let work = make.path().join(format!("work{task}"));
        let outcome = assemble(&store, &run, 3, &work, Some(60.0)).await.expect("assemble");
        if i > 0 {
            assert!(matches!(outcome, Outcome::Waiting { .. }), "assembled early: {outcome:?}");
        } else {
            assert!(
                matches!(outcome, Outcome::Assembled { frames: 50, .. }),
                "last task must assemble 50 frames: {outcome:?}"
            );
        }
    }

    let film = make.path().join("film.mp4");
    store.get(&run.film(), &film).await.expect("get film");
    assert_eq!(count_frames(&film).expect("count"), 50);
}

#[tokio::test]
async fn a_short_segment_fails_the_frame_count() {
    let root = tempfile::tempdir().expect("tempdir");
    let make = tempfile::tempdir().expect("tempdir");
    let store = ObjectRunStore::local(root.path(), Tuning::default()).expect("store");
    let run = RunKeys::new("renders/short", "e1");
    // The receipt promises 20 frames; the segment carries 12.
    let local = make.path().join("seg0.mp4");
    segment(&local, 12, 0);
    store.put(&local, &run.segment(0)).await.expect("put");
    let receipt = Receipt { task: 0, first: 1, last: 20, segments: vec![0] };
    let body = make.path().join("r.json");
    std::fs::write(&body, serde_json::to_vec(&receipt).expect("json")).expect("write");
    store.put(&body, &run.receipt(0)).await.expect("put");
    let err = assemble(&store, &run, 1, &make.path().join("w"), None).await.expect_err("must refuse");
    assert!(err.to_string().contains("12 frames"), "{err}");
    assert!(!store.exists(&run.film()).await.expect("exists"), "a short film was uploaded");
}
