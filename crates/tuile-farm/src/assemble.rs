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
    let progress = crate::progress::Progress::new();
    match gather(store, store, run, task_count, work, fps, &progress).await? {
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
/// with `fps`, its cadence checked. Each segment's bytes are told to
/// `progress` as it comes in.
pub async fn gather(
    receipts_store: &dyn RunStore,
    segments_store: &dyn RunStore,
    run: &RunKeys,
    task_count: u32,
    work: &Path,
    fps: Option<f64>,
    progress: &crate::progress::Progress,
) -> Result<Gathered, AssembleError> {
    gather_joined(
        receipts_store,
        segments_store,
        run,
        task_count,
        work,
        fps,
        Box::new(|segments, _, film| crate::concat::concat_into(segments, film, true)),
        progress,
    )
    .await
}

/// How a render's segments become its film, for a host that joins them
/// its own way: handed the segments fetched — in film order, their frames
/// numbered from the second argument (the receipts' first) — it writes
/// the film at the third, letting go of each segment once it is read for
/// the last time, and returns the frames written. It may cut segments on
/// their sync samples and put pieces of its own between them
/// ([`tuile_mp4::film::join_pieces`]). [`gather`]'s is
/// [`crate::concat::concat_into`].
pub type Join =
    Box<dyn FnOnce(&[PathBuf], u64, &Path) -> Result<u64, crate::concat::ConcatError> + Send>;

/// [`gather`], the segments joined by the host's `join`. The film it
/// writes is checked as [`gather`]'s is: it carries every frame the
/// receipts add up to, at its cadence, or the gather fails. Each
/// segment's bytes are told to `progress` as it comes in.
#[allow(clippy::too_many_arguments)]
pub async fn gather_joined(
    receipts_store: &dyn RunStore,
    segments_store: &dyn RunStore,
    run: &RunKeys,
    task_count: u32,
    work: &Path,
    fps: Option<f64>,
    join_by: Join,
    progress: &crate::progress::Progress,
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
            let bytes = segments_store.get(&run.segment(g), &local).await?;
            progress.bytes(bytes);
            Ok::<_, StoreError>(local)
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
    let probe = join(locals, first_frame, film.clone(), join_by).await?;
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

/// The receipts of `run` in `store`, those deposited so far, by task.
pub async fn read_receipts(
    store: &dyn RunStore,
    run: &RunKeys,
) -> Result<Vec<Receipt>, AssembleError> {
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
    film: PathBuf,
    join_by: Join,
) -> Result<crate::concat::Probe, AssembleError> {
    tokio::task::spawn_blocking(move || {
        join_by(&segments, first_frame, &film)?;
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
        let segment_bytes: u64 = {
            use crate::RunStore;
            mezzanine
                .list(&run.prefix)
                .await
                .expect("list")
                .iter()
                .map(|e| e.size)
                .sum()
        };
        let work = tempfile::tempdir().expect("tempdir");
        let progress = crate::progress::Progress::new();
        let gathered = gather(&receipts, &mezzanine, &run, 2, work.path(), None, &progress)
            .await
            .expect("gather");
        assert_eq!(
            progress.report().bytes,
            segment_bytes,
            "every segment's bytes told as it came in"
        );
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
            gather(&receipts, &receipts, &run, 2, work.path(), None, &progress)
                .await
                .is_err(),
            "the segments are not in the run store"
        );
    }

    /// A host's join: handed the render's two segments (frames 1..=6,
    /// 7..=12) in film order with their first frame, it puts two frames of
    /// its own in place of frames 7..=8, the second segment cut on the key
    /// frame the render put at 9 — and the film it writes is checked as any
    /// gather's: one that does not carry the receipts' frames is refused.
    #[tokio::test]
    async fn a_host_joins_the_segments_its_own_way_and_the_film_is_checked() {
        use crate::concat::join_pieces;
        use tuile_mp4::film::Piece;
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
        let own = root.path().join("own.mp4");
        crate::concat::write_test_segment_with(&own, 2, &sps, 9, &|i| i == 0, false).expect("own");
        let work = tempfile::tempdir().expect("tempdir");
        let handed = std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen = handed.clone();
        let piece = own.clone();
        let gathered = gather_joined(
            &receipts,
            &mezzanine,
            &run,
            2,
            work.path(),
            Some(60.0),
            Box::new(move |segments, first, film| {
                *seen.lock().unwrap() = Some((segments.to_vec(), first));
                join_pieces(
                    &[
                        Piece::whole(&segments[0]),
                        Piece::whole(&piece),
                        Piece::cut(&segments[1], 2..6),
                    ],
                    film,
                    true,
                )
            }),
            &crate::progress::Progress::new(),
        )
        .await
        .expect("gather");
        let Gathered::Joined { path, frames, .. } = gathered else {
            panic!("joined: {gathered:?}")
        };
        assert_eq!(frames, 12);
        let (segments, first) = handed
            .lock()
            .unwrap()
            .take()
            .expect("the join was the host's");
        assert_eq!(first, 1);
        let names: Vec<_> = segments
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["seg0.mp4", "seg1.mp4"]);
        let bytes = std::fs::read(&path).expect("film");
        let t = tuile_mp4::film::read_track(&mut std::io::Cursor::new(&bytes))
            .expect("read")
            .expect("track");
        let tags: Vec<u8> = t
            .samples
            .iter()
            .map(|s| bytes[s.offset as usize + 5])
            .collect();
        assert_eq!(tags, [0, 0, 0, 0, 0, 0, 9, 9, 1, 1, 1, 1]);
        // A join that leaves frames out: the receipts say twelve.
        let err = gather_joined(
            &receipts,
            &mezzanine,
            &run,
            2,
            work.path(),
            None,
            Box::new(|segments, _, film| join_pieces(&[Piece::whole(&segments[0])], film, true)),
            &crate::progress::Progress::new(),
        )
        .await
        .expect_err("frames missing");
        assert!(
            matches!(
                err,
                AssembleError::FrameCount {
                    got: 6,
                    expected: 12
                }
            ),
            "{err}"
        );
    }

    async fn store_put(store: &crate::ObjectRunStore, from: &Path, key: &str) {
        use crate::RunStore;
        store.put(from, key).await.expect("put");
    }
}
