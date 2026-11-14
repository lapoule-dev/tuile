// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The film, put together by the last task to finish.
//!
//! # Who assembles, and how it knows it is last
//!
//! A render of N tasks produces segments that no single task holds. The film
//! used to be assembled by the launcher once every task had exited — which
//! means somebody had to be watching at the end, and a launcher that returned
//! early (it did: at the first succeeded task) produced no film at all.
//!
//! Now every task, once its segments are up, deposits a **receipt** —
//! `<run>/tasks/<t>.json`, naming its frames and its segments — and then asks
//! for the film. Whoever finds all N receipts assembles; everybody else is
//! told [`Outcome::Waiting`] and exits. Two tasks finishing in the same second
//! may both assemble: they write the same film from the same segments, and the
//! second upload replaces the first with identical bytes. No lock needed.
//!
//! # What counts as done
//!
//! Receipts, not listings. A segment's presence says nothing about whether its
//! task finished — a process that died leaves a 48-byte container behind, and
//! the old `ffmpeg concat -c copy` stopped at the first empty file and reported
//! success with 180 frames of 1440. So the film is assembled only from
//! segments a receipt vouches for, each must exist with a plausible size, and
//! the result must carry exactly the frames the receipts add up to.

use std::path::{Path, PathBuf};

use futures_util::stream::{self, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};

use crate::{RunStore, StoreError};

/// What one task says about itself once its work is up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub task: u32,
    /// Inclusive frame range this task rendered.
    pub first: u64,
    pub last: u64,
    /// Global segment numbers, in frame order.
    pub segments: Vec<u64>,
}

impl Receipt {
    pub fn frames(&self) -> u64 {
        self.last + 1 - self.first
    }
}

/// Keys of one render of one run, in one place.
///
/// Receipts live under the RENDER, not just the run: a run is rendered more
/// than once — a 600-frame check, then the whole flight — and the second
/// render's last task must not count the first render's receipts as its own.
/// Found on 23 September 2026 with three stale receipts sitting in
/// `tasks/` when a nine-task render was launched on the same run.
pub struct RunKeys {
    prefix: String,
    render: String,
}

impl RunKeys {
    pub fn new(prefix: &str, render: &str) -> Self {
        RunKeys {
            prefix: prefix.trim_end_matches('/').to_string(),
            render: render.to_string(),
        }
    }
    pub fn segment(&self, g: u64) -> String {
        format!("{}/seg{g}.mp4", self.prefix)
    }
    pub fn receipts(&self) -> String {
        format!("{}/tasks/{}", self.prefix, self.render)
    }
    pub fn receipt(&self, task: u32) -> String {
        format!("{}/tasks/{}/{task}.json", self.prefix, self.render)
    }
    pub fn film(&self) -> String {
        format!("{}/render.mp4", self.prefix)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Not every task has deposited its receipt yet.
    Waiting { done: usize, of: u32 },
    /// The film is up.
    Assembled {
        key: String,
        frames: u64,
        bytes: u64,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum AssembleError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("receipt {key}: {reason}")]
    BadReceipt { key: String, reason: String },
    #[error("segment {key} is {size} bytes — a process that died, not a segment")]
    EmptySegment { key: String, size: u64 },
    #[error("segment {0} is vouched for by a receipt and absent from the store")]
    MissingSegment(String),
    #[error("receipts cover frames with a gap or an overlap at frame {0}")]
    Discontinuous(u64),
    #[error(transparent)]
    Concat(#[from] crate::concat::ConcatError),
    #[error("join task: {0}")]
    Join(String),
    #[error("cadence: {0}")]
    Cadence(String),
    #[error("the film carries {got} frames, the receipts add up to {expected}")]
    FrameCount { got: u64, expected: u64 },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// A segment under this is not one. `render_usd.py` opens its output up front,
/// so a process that died leaves an MP4 header of a few dozen bytes.
const PLAUSIBLE_SEGMENT: u64 = 200;

/// Reads every receipt and, if all `task_count` are there, assembles the film
/// in `work` and uploads it.
///
/// With `fps`, the film must also last `frames / fps` seconds to within two
/// frames — see [`crate::concat::check_cadence`].
pub async fn assemble(
    store: &dyn RunStore,
    run: &RunKeys,
    task_count: u32,
    work: &Path,
    fps: Option<f64>,
) -> Result<Outcome, AssembleError> {
    match gather(store, store, run, task_count, work, fps).await? {
        Gathered::Waiting { done, of } => Ok(Outcome::Waiting { done, of }),
        Gathered::Joined { path, frames, .. } => {
            let bytes = store.put(&path, &run.film()).await?;
            Ok(Outcome::Assembled {
                key: run.film(),
                frames,
                bytes,
            })
        }
    }
}

/// The film joined on this machine, not uploaded.
#[derive(Debug, PartialEq)]
pub enum Gathered {
    /// Not every task has deposited its receipt yet.
    Waiting { done: usize, of: u32 },
    /// The film, at `path` under the work directory.
    Joined {
        path: PathBuf,
        frames: u64,
        bytes: u64,
        /// The segments' fetch, seconds.
        fetch_seconds: f64,
        /// Their join, seconds.
        join_seconds: f64,
    },
}

/// What [`assemble`] does short of the upload: the receipts read from
/// `receipts`, the segments they vouch for fetched from `segments` (the
/// same store, or the one that keeps a render's mezzanine), checked, and
/// joined into `work/render.mp4` — counted by reading the film back, and
/// with `fps`, its cadence checked.
pub async fn gather(
    receipts_store: &dyn RunStore,
    segments_store: &dyn RunStore,
    run: &RunKeys,
    task_count: u32,
    work: &Path,
    fps: Option<f64>,
) -> Result<Gathered, AssembleError> {
    gather_with(
        receipts_store,
        segments_store,
        run,
        task_count,
        work,
        fps,
        &[],
    )
    .await
}

/// [`gather`], frames laid over by `inserts` ([`crate::concat::splice`]):
/// each insert's segment fetched from `segments_store` with the render's,
/// and spliced in at the join. The film carries the frames the receipts
/// add up to all the same.
pub async fn gather_with(
    receipts_store: &dyn RunStore,
    segments_store: &dyn RunStore,
    run: &RunKeys,
    task_count: u32,
    work: &Path,
    fps: Option<f64>,
    inserts: &[crate::concat::Insert],
) -> Result<Gathered, AssembleError> {
    let receipts = read_receipts(receipts_store, run).await?;
    if receipts.len() < task_count as usize {
        return Ok(Gathered::Waiting {
            done: receipts.len(),
            of: task_count,
        });
    }
    let segments = check_receipts(&receipts, task_count)?;
    let first_frame = receipts.iter().map(|r| r.first).min().unwrap_or(1);
    let expected: u64 = receipts.iter().map(Receipt::frames).sum();

    // Every vouched-for segment, present and plausible, before downloading a
    // single byte of any of them.
    let listed = segments_store.list(&run.prefix).await?;
    for g in &segments {
        let key = run.segment(*g);
        match listed.iter().find(|e| e.key == key) {
            None => return Err(AssembleError::MissingSegment(key)),
            Some(e) if e.size < PLAUSIBLE_SEGMENT => {
                return Err(AssembleError::EmptySegment { key, size: e.size })
            }
            Some(_) => {}
        }
    }
    for insert in inserts {
        if !segments_store.exists(&insert.key).await? {
            return Err(AssembleError::MissingSegment(insert.key.clone()));
        }
    }

    tokio::fs::create_dir_all(work)
        .await
        .map_err(|source| AssembleError::Io {
            path: work.to_path_buf(),
            source,
        })?;
    let began = std::time::Instant::now();
    // Segments in parallel, each itself in parallel ranges: the store's own
    // concurrency bounds the parts in flight per segment, four segments at a
    // time (`TUILE_ASSEMBLE_SEGMENTS`) bound the whole.
    let locals: Vec<PathBuf> = stream::iter(segments.iter().copied())
        .map(|g| async move {
            let local = work.join(format!("seg{g}.mp4"));
            segments_store.get(&run.segment(g), &local).await?;
            Ok::<_, StoreError>(local)
        })
        .buffered(crate::assemble_segments())
        .try_collect()
        .await?;
    let inserted: Vec<(crate::concat::Insert, PathBuf)> =
        stream::iter(inserts.iter().cloned().enumerate())
            .map(|(n, insert)| async move {
                let local = work.join(format!("insert{n}.mp4"));
                segments_store.get(&insert.key, &local).await?;
                Ok::<_, StoreError>((insert, local))
            })
            .buffered(crate::assemble_segments())
            .try_collect()
            .await?;
    let fetch_seconds = began.elapsed().as_secs_f64();

    let film = work.join("render.mp4");
    let began = std::time::Instant::now();
    // Counted by reading the written film back, not by adding up what went
    // in: the number that matters is the one a player will find.
    // Each segment let go once it is in: where the disk is memory, the
    // film and its segments are never held whole twice.
    let probe = join(locals, first_frame, inserted, film.clone(), true).await?;
    let join_seconds = began.elapsed().as_secs_f64();
    let got = probe.frames;
    if got != expected {
        return Err(AssembleError::FrameCount { got, expected });
    }
    if let Some(fps) = fps {
        crate::concat::check_cadence(probe, fps).map_err(AssembleError::Cadence)?;
    }
    let bytes = tokio::fs::metadata(&film)
        .await
        .map_err(|source| AssembleError::Io {
            path: film.clone(),
            source,
        })?
        .len();
    Ok(Gathered::Joined {
        path: film,
        frames: got,
        bytes,
        fetch_seconds,
        join_seconds,
    })
}

/// The render's frames `first..=last` fetched into `work`: the segments
/// that hold them, and only those, each with the run of its samples (from
/// 0) that are those frames — in film order. The run starts on a key frame,
/// where it decodes on its own, or the fetch is refused, the frame named:
/// the render was not asked for one there.
///
/// Only the receipts of the tasks that hold the range need be in: a range
/// is taken as soon as its own segments are rendered, while the rest of
/// the film still is.
pub async fn fetch_range(
    receipts_store: &dyn RunStore,
    segments_store: &dyn RunStore,
    run: &RunKeys,
    (first, last): (u64, u64),
    work: &Path,
) -> Result<Vec<(PathBuf, std::ops::Range<usize>)>, AssembleError> {
    let mut receipts = read_receipts(receipts_store, run).await?;
    receipts.sort_by_key(|r| r.first);
    receipts.retain(|r| r.last >= first && r.first <= last);
    // The range covered from its first frame to its last, without a gap.
    let mut next = first;
    for r in &receipts {
        if r.first > next || r.last < r.first || r.segments.is_empty() {
            break;
        }
        next = next.max(r.last + 1);
    }
    if last < first || next <= last {
        return Err(AssembleError::BadReceipt {
            key: run.receipts(),
            reason: format!(
                "frames {first}..={last}: not all rendered yet (in up to {})",
                next.saturating_sub(1)
            ),
        });
    }
    tokio::fs::create_dir_all(work)
        .await
        .map_err(|source| AssembleError::Io {
            path: work.to_path_buf(),
            source,
        })?;
    let mut pieces = Vec::new();
    for r in receipts
        .iter()
        .filter(|r| r.last >= first && r.first <= last)
    {
        let mut at = r.first;
        for &g in &r.segments {
            let local = work.join(format!("seg{g}.mp4"));
            segments_store.get(&run.segment(g), &local).await?;
            let path = local.clone();
            let track = tokio::task::spawn_blocking(move || {
                let mut file = std::fs::File::open(&path)?;
                tuile_mp4::film::read_track(&mut file)
                    .map_err(|e| std::io::Error::other(e.to_string()))?
                    .ok_or_else(|| std::io::Error::other("no video track"))
            })
            .await
            .map_err(|e| AssembleError::Join(e.to_string()))?
            .map_err(|source| AssembleError::Io {
                path: local.clone(),
                source,
            })?;
            let frames = track.samples.len() as u64;
            let (a, b) = (at, at + frames.saturating_sub(1));
            at += frames;
            if frames == 0 || b < first || a > last {
                continue;
            }
            let (from, to) = (first.max(a) - a, last.min(b) - a + 1);
            if pieces.is_empty() && !track.samples[from as usize].key {
                return Err(AssembleError::BadReceipt {
                    key: run.segment(g),
                    reason: format!(
                        "frame {} is not a key frame: the render was not asked for one there",
                        first.max(a)
                    ),
                });
            }
            pieces.push((local, from as usize..to as usize));
        }
    }
    let held: u64 = pieces.iter().map(|(_, r)| r.len() as u64).sum();
    if held != last + 1 - first {
        return Err(AssembleError::FrameCount {
            got: held,
            expected: last + 1 - first,
        });
    }
    Ok(pieces)
}

async fn read_receipts(store: &dyn RunStore, run: &RunKeys) -> Result<Vec<Receipt>, AssembleError> {
    let mut receipts = Vec::new();
    for entry in store.list(&run.receipts()).await? {
        let bytes = store.get_range(&entry.key, 0..entry.size).await?;
        let receipt: Receipt =
            serde_json::from_slice(&bytes).map_err(|e| AssembleError::BadReceipt {
                key: entry.key.clone(),
                reason: e.to_string(),
            })?;
        receipts.push(receipt);
    }
    receipts.sort_by_key(|r| r.task);
    Ok(receipts)
}

/// Tasks `0..task_count`, each once, frames contiguous across them; returns
/// the segments in film order.
///
/// Film order is the tasks' frame order and, inside a task, the order its
/// receipt lists — never the keys' lexical order, under which `seg10` sorts
/// before `seg2`.
fn check_receipts(receipts: &[Receipt], task_count: u32) -> Result<Vec<u64>, AssembleError> {
    let tasks: Vec<u32> = receipts.iter().map(|r| r.task).collect();
    if tasks != (0..task_count).collect::<Vec<_>>() {
        return Err(AssembleError::BadReceipt {
            key: "tasks/".into(),
            reason: format!("expected tasks 0..{task_count}, found {tasks:?}"),
        });
    }
    let mut by_frame: Vec<&Receipt> = receipts.iter().collect();
    by_frame.sort_by_key(|r| r.first);
    let mut segments = Vec::new();
    let mut next = by_frame.first().map(|r| r.first);
    for r in by_frame {
        if r.last < r.first || r.segments.is_empty() {
            return Err(AssembleError::BadReceipt {
                key: format!("tasks/{}.json", r.task),
                reason: format!(
                    "frames {}:{} over {} segments",
                    r.first,
                    r.last,
                    r.segments.len()
                ),
            });
        }
        if Some(r.first) != next {
            return Err(AssembleError::Discontinuous(r.first));
        }
        next = Some(r.last + 1);
        segments.extend(&r.segments);
    }
    Ok(segments)
}

/// The join and the count run on a blocking thread: they are file I/O from
/// start to end, and the runtime's workers have uploads to drive meanwhile.
async fn join(
    segments: Vec<PathBuf>,
    first_frame: u64,
    inserts: Vec<(crate::concat::Insert, PathBuf)>,
    film: PathBuf,
    consume: bool,
) -> Result<crate::concat::Probe, AssembleError> {
    tokio::task::spawn_blocking(move || {
        crate::concat::splice(&segments, first_frame, &inserts, &film, consume)?;
        Ok(crate::concat::probe(&film)?)
    })
    .await
    .map_err(|e| AssembleError::Join(e.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(task: u32, first: u64, last: u64, segments: &[u64]) -> Receipt {
        Receipt {
            task,
            first,
            last,
            segments: segments.to_vec(),
        }
    }

    #[test]
    fn segments_follow_frames_not_keys() {
        // Task 1 rendered the start: its segments come first, and seg10 comes
        // after seg9 rather than after seg1.
        let r = [receipt(0, 51, 100, &[9, 10]), receipt(1, 1, 50, &[1, 2])];
        assert_eq!(check_receipts(&r, 2).expect("valid"), vec![1, 2, 9, 10]);
    }

    #[test]
    fn a_gap_between_tasks_is_refused() {
        let r = [receipt(0, 1, 50, &[0]), receipt(1, 52, 100, &[1])];
        assert!(matches!(
            check_receipts(&r, 2),
            Err(AssembleError::Discontinuous(52))
        ));
    }

    #[test]
    fn a_missing_or_foreign_task_is_refused() {
        let r = [receipt(0, 1, 50, &[0]), receipt(2, 51, 100, &[1])];
        assert!(matches!(
            check_receipts(&r, 2),
            Err(AssembleError::BadReceipt { .. })
        ));
    }

    #[tokio::test]
    async fn waits_until_every_receipt_is_in() {
        let root = tempfile::tempdir().expect("tempdir");
        let store =
            crate::ObjectRunStore::local(root.path(), crate::Tuning::default()).expect("store");
        let run = RunKeys::new("renders/r", "e1");
        let src = root.path().join("r0.json");
        std::fs::write(
            &src,
            serde_json::to_vec(&receipt(0, 1, 5, &[0])).expect("json"),
        )
        .expect("write");
        store.put(&src, &run.receipt(0)).await.expect("put");
        let work = tempfile::tempdir().expect("tempdir");
        let outcome = assemble(&store, &run, 2, work.path(), None)
            .await
            .expect("assemble");
        assert_eq!(outcome, Outcome::Waiting { done: 1, of: 2 });
    }

    #[tokio::test]
    async fn another_renders_receipts_do_not_count() {
        let root = tempfile::tempdir().expect("tempdir");
        let store =
            crate::ObjectRunStore::local(root.path(), crate::Tuning::default()).expect("store");
        // An earlier render of the same run left receipts for tasks 0 and 1.
        let old = RunKeys::new("renders/r", "e0");
        let src = root.path().join("r.json");
        for t in 0..2 {
            std::fs::write(
                &src,
                serde_json::to_vec(&receipt(t, 1, 5, &[0])).expect("json"),
            )
            .expect("write");
            store.put(&src, &old.receipt(t)).await.expect("put");
        }
        // This render has only task 1 in: it must wait, not assemble.
        let run = RunKeys::new("renders/r", "e1");
        std::fs::write(
            &src,
            serde_json::to_vec(&receipt(1, 6, 9, &[1])).expect("json"),
        )
        .expect("write");
        store.put(&src, &run.receipt(1)).await.expect("put");
        let work = tempfile::tempdir().expect("tempdir");
        let outcome = assemble(&store, &run, 2, work.path(), None)
            .await
            .expect("assemble");
        assert_eq!(outcome, Outcome::Waiting { done: 1, of: 2 });
    }

    #[tokio::test]
    async fn an_empty_segment_is_refused_before_any_download() {
        let root = tempfile::tempdir().expect("tempdir");
        let store =
            crate::ObjectRunStore::local(root.path(), crate::Tuning::default()).expect("store");
        let run = RunKeys::new("renders/r", "e1");
        let src = root.path().join("r0.json");
        std::fs::write(
            &src,
            serde_json::to_vec(&receipt(0, 1, 5, &[0])).expect("json"),
        )
        .expect("write");
        store.put(&src, &run.receipt(0)).await.expect("put");
        let seg = root.path().join("dead.mp4");
        std::fs::write(&seg, [0u8; 48]).expect("write");
        store.put(&seg, &run.segment(0)).await.expect("put");
        let work = tempfile::tempdir().expect("tempdir");
        let err = assemble(&store, &run, 1, work.path(), None)
            .await
            .expect_err("must refuse");
        assert!(
            matches!(err, AssembleError::EmptySegment { size: 48, .. }),
            "{err}"
        );
    }

    /// A render's segments kept in another store than its receipts (its
    /// mezzanine, near the cards): read from there, joined here, not
    /// uploaded anywhere.
    #[tokio::test]
    async fn segments_are_gathered_from_their_own_store_and_joined_here() {
        let root = tempfile::tempdir().expect("tempdir");
        let receipts =
            crate::ObjectRunStore::local(&root.path().join("run"), crate::Tuning::default())
                .expect("store");
        let mezzanine =
            crate::ObjectRunStore::local(&root.path().join("mezz"), crate::Tuning::default())
                .expect("store");
        let run = RunKeys::new("renders/r", "e1");
        let src = root.path().join("r.json");
        for (t, first, last) in [(0u32, 1u64, 4u64), (1, 5, 7)] {
            std::fs::write(
                &src,
                serde_json::to_vec(&receipt(t, first, last, &[u64::from(t)])).expect("json"),
            )
            .expect("write");
            store_put(&receipts, &src, &run.receipt(t)).await;
            let seg = root.path().join(format!("s{t}.mp4"));
            crate::concat::write_test_segment(
                &seg,
                (last + 1 - first) as u32,
                &[0x67, 0x64, 0x00, 0x0c, 0xac, 0xd9, 0x42],
                t as u8,
            )
            .expect("segment");
            store_put(&mezzanine, &seg, &run.segment(u64::from(t))).await;
        }
        let work = tempfile::tempdir().expect("tempdir");
        let gathered = gather(&receipts, &mezzanine, &run, 2, work.path(), None)
            .await
            .expect("gather");
        let Gathered::Joined { path, frames, .. } = gathered else {
            panic!("joined: {gathered:?}")
        };
        assert_eq!(frames, 7);
        assert!(path.starts_with(work.path()));
        assert!(
            !work.path().join("seg0.mp4").exists(),
            "each segment let go once joined"
        );
        assert!(
            !receipts.exists(&run.film()).await.expect("exists"),
            "nothing uploaded"
        );
        assert!(
            gather(&receipts, &receipts, &run, 2, work.path(), None)
                .await
                .is_err(),
            "the segments are not in the run store"
        );
    }

    /// A render of two segments (frames 1..=6, 7..=12) and an insert over
    /// frames 5..=8 kept beside them: fetched with them and spliced in,
    /// the film's frames those the receipts add up to; an insert the store
    /// does not hold is refused before anything is fetched.
    #[tokio::test]
    async fn an_insert_is_fetched_with_the_segments_and_spliced_in() {
        let root = tempfile::tempdir().expect("tempdir");
        let receipts =
            crate::ObjectRunStore::local(&root.path().join("run"), crate::Tuning::default())
                .expect("store");
        let mezzanine =
            crate::ObjectRunStore::local(&root.path().join("mezz"), crate::Tuning::default())
                .expect("store");
        let run = RunKeys::new("renders/r", "e1");
        let src = root.path().join("r.json");
        let sps = [0x67, 0x64, 0x00, 0x0c, 0xac, 0xd9, 0x42];
        // Key frames where the inserts end: the second segment's frame 9.
        for (t, first, last, key) in [(0u32, 1u64, 6u64, 99u32), (1, 7, 12, 2)] {
            std::fs::write(
                &src,
                serde_json::to_vec(&receipt(t, first, last, &[u64::from(t)])).expect("json"),
            )
            .expect("write");
            store_put(&receipts, &src, &run.receipt(t)).await;
            let seg = root.path().join(format!("s{t}.mp4"));
            crate::concat::write_test_segment_with(
                &seg,
                (last + 1 - first) as u32,
                &sps,
                t as u8,
                &|i| i == 0 || i == key,
                false,
            )
            .expect("segment");
            store_put(&mezzanine, &seg, &run.segment(u64::from(t))).await;
        }
        let seg = root.path().join("i.mp4");
        crate::concat::write_test_segment_with(&seg, 4, &sps, 9, &|i| i == 0, false)
            .expect("insert");
        store_put(&mezzanine, &seg, "renders/r/inserts/0.mp4").await;
        let insert = crate::concat::Insert {
            first: 5,
            last: 8,
            key: "renders/r/inserts/0.mp4".into(),
        };
        let work = tempfile::tempdir().expect("tempdir");
        let gathered = gather_with(
            &receipts,
            &mezzanine,
            &run,
            2,
            work.path(),
            Some(60.0),
            std::slice::from_ref(&insert),
        )
        .await
        .expect("gather");
        let Gathered::Joined { path, frames, .. } = gathered else {
            panic!("joined: {gathered:?}")
        };
        assert_eq!(frames, 12);
        let bytes = std::fs::read(&path).expect("film");
        let t = tuile_mp4::film::read_track(&mut std::io::Cursor::new(&bytes))
            .expect("read")
            .expect("track");
        let tags: Vec<u8> = t
            .samples
            .iter()
            .map(|s| bytes[s.offset as usize + 5])
            .collect();
        assert_eq!(tags, [0, 0, 0, 0, 9, 9, 9, 9, 1, 1, 1, 1]);
        let missing = crate::concat::Insert {
            key: "renders/r/inserts/none.mp4".into(),
            ..insert
        };
        let err = gather_with(
            &receipts,
            &mezzanine,
            &run,
            2,
            work.path(),
            None,
            &[missing],
        )
        .await
        .expect_err("no such insert");
        assert!(matches!(err, AssembleError::MissingSegment(_)), "{err}");
    }

    /// Frames 5..=8 of a render of three segments (1..=6, 7..=12,
    /// 13..=20) whose fourth is not rendered yet: the two that hold them
    /// fetched, each with its run of samples; a range that starts on no key
    /// frame, or one not all rendered yet, is refused.
    #[tokio::test]
    async fn a_range_is_fetched_from_the_segments_that_hold_it() {
        let root = tempfile::tempdir().expect("tempdir");
        let receipts =
            crate::ObjectRunStore::local(&root.path().join("run"), crate::Tuning::default())
                .expect("store");
        let mezzanine =
            crate::ObjectRunStore::local(&root.path().join("mezz"), crate::Tuning::default())
                .expect("store");
        let run = RunKeys::new("renders/r", "e1");
        let src = root.path().join("r.json");
        let sps = [0x67, 0x64, 0x00, 0x0c, 0xac, 0xd9, 0x42];
        for (t, first, last) in [(0u32, 1u64, 6u64), (1, 7, 12), (2, 13, 20)] {
            std::fs::write(
                &src,
                serde_json::to_vec(&receipt(t, first, last, &[u64::from(t)])).expect("json"),
            )
            .expect("write");
            store_put(&receipts, &src, &run.receipt(t)).await;
            let seg = root.path().join(format!("s{t}.mp4"));
            crate::concat::write_test_segment_with(
                &seg,
                (last + 1 - first) as u32,
                &sps,
                t as u8,
                &|i| i == 0 || (t == 0 && i == 4),
                false,
            )
            .expect("segment");
            store_put(&mezzanine, &seg, &run.segment(u64::from(t))).await;
        }
        let work = tempfile::tempdir().expect("tempdir");
        let pieces = fetch_range(&receipts, &mezzanine, &run, (5, 8), work.path())
            .await
            .expect("fetched");
        let runs: Vec<_> = pieces
            .iter()
            .map(|(p, r)| {
                (
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    r.clone(),
                )
            })
            .collect();
        assert_eq!(
            runs,
            [
                ("seg0.mp4".to_string(), 4..6),
                ("seg1.mp4".to_string(), 0..2)
            ]
        );
        assert!(
            !work.path().join("seg2.mp4").exists(),
            "only what holds the range"
        );
        let err = fetch_range(&receipts, &mezzanine, &run, (4, 8), work.path())
            .await
            .expect_err("no key at 4");
        assert!(
            err.to_string().contains("frame 4 is not a key frame"),
            "{err}"
        );
        // Frames 18..=22: task 3 (21..) has not rendered yet.
        let err = fetch_range(&receipts, &mezzanine, &run, (18, 22), work.path())
            .await
            .expect_err("not rendered");
        assert!(err.to_string().contains("not all rendered yet"), "{err}");
    }

    async fn store_put(store: &crate::ObjectRunStore, from: &Path, key: &str) {
        use crate::RunStore;
        store.put(from, key).await.expect("put");
    }
}
