// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the container is actually using, said out loud on a fixed beat.
//!
//! Two bakes died with status 137 on 19 September 2026 and left **no evidence
//! at all**: the container is gone before it can complain, the logs stop
//! mid-sentence, and `run.googleapis.com/container/memory/utilizations` needs
//! an IAM role the farm account did not have. Five and a half minutes of a job
//! that never reached its first frame, and nothing to say where the memory
//! went.
//!
//! The number that matters is not the process's RSS. A Cloud Run container has
//! no disk: `/tmp` and the writable layer are tmpfs, so every file the job
//! writes — the fetch cache, the pack's spill, the finished pack — is charged
//! to the same limit as the heap and is invisible in RSS. The kernel keeps the
//! total in the cgroup, and that total is exactly what the OOM killer compares
//! against the limit. So that is what this reads.
//!
//! It runs on a plain thread rather than a task on purpose. The moment worth
//! measuring is the one where the runtime is saturated, and a task scheduled
//! behind a thousand decodes reports nothing precisely then.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// One reading of the memory a container is charged for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    /// Bytes in use: anonymous memory, page cache and tmpfs together.
    pub used: u64,
    /// The ceiling, when one is set. Absent means unlimited — which on a farm
    /// means the cgroup was not the thing imposing the limit.
    pub limit: Option<u64>,
    /// Which accounting answered, so a surprising number can be traced.
    pub source: &'static str,
}

impl Reading {
    /// Share of the limit in use, `None` when nothing bounds it.
    pub fn fraction(&self) -> Option<f64> {
        let limit = self.limit.filter(|l| *l > 0)?;
        Some(self.used as f64 / limit as f64)
    }
}

/// The first line of a cgroup file as bytes. `max` means no limit.
fn scalar(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    let first = text.split_whitespace().next()?;
    if first == "max" {
        return None;
    }
    first.parse().ok()
}

/// Reads cgroup v2, then v1, then the process's own resident size.
///
/// `root` is the mount point, injected so the parsing can be tested without a
/// container — the real caller passes `/`.
pub fn read_at(root: &Path) -> Option<Reading> {
    let v2 = root.join("sys/fs/cgroup/memory.current");
    if let Some(used) = scalar(&v2) {
        return Some(Reading {
            used,
            limit: scalar(&root.join("sys/fs/cgroup/memory.max")),
            source: "cgroup2",
        });
    }
    let v1 = root.join("sys/fs/cgroup/memory/memory.usage_in_bytes");
    if let Some(used) = scalar(&v1) {
        // v1 writes an enormous sentinel rather than "max" when unlimited.
        let limit = scalar(&root.join("sys/fs/cgroup/memory/memory.limit_in_bytes"))
            .filter(|l| *l < (1 << 50));
        return Some(Reading {
            used,
            limit,
            source: "cgroup1",
        });
    }
    // Last resort, and it under-reports on purpose-built containers: RSS does
    // not include the tmpfs the job writes its pack into.
    let statm = std::fs::read_to_string(root.join("proc/self/statm")).ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(Reading {
        used: pages * 4096,
        limit: None,
        source: "rss",
    })
}

/// The reading for this process, on this machine.
pub fn read() -> Option<Reading> {
    read_at(Path::new("/"))
}

/// Total bytes held under `dir`, following no symlinks.
///
/// Counted because on Cloud Run these bytes ARE memory, and they are the half
/// no heap profiler will ever show: the fetch cache and the pack.
pub fn bytes_under(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(t) if t.is_dir() => bytes_under(&entry.path()),
            Ok(t) if t.is_file() => entry.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

/// Logs the memory picture every `interval`, for as long as the process lives.
///
/// `watched` are directories whose contents count against the same limit —
/// the fetch cache and the output directory on a container with no disk.
pub fn watch(interval: Duration, watched: Vec<PathBuf>) {
    std::thread::Builder::new()
        .name("tuile-memory".into())
        .spawn(move || {
            let mut peak = 0u64;
            loop {
                std::thread::sleep(interval);
                let m = tuile_core::metrics::metrics();
                let files: u64 = watched.iter().map(|d| bytes_under(d)).sum();
                let (used, limit, source) = match read() {
                    Some(r) => (r.used, r.limit, r.source),
                    None => (0, None, "unknown"),
                };
                peak = peak.max(used);
                // One line, every field named: this is read from a log viewer
                // after the container is gone, not from a dashboard.
                tracing::info!(
                    used_gib = format!("{:.2}", gib(used)),
                    peak_gib = format!("{:.2}", gib(peak)),
                    limit_gib = limit.map(|l| format!("{:.2}", gib(l))),
                    files_gib = format!("{:.2}", gib(files)),
                    resident_gib = format!("{:.2}", gib(m.resident_bytes.get())),
                    imagery_gib = format!("{:.2}", gib(m.imagery_bytes.get())),
                    selected = m.tiles_selected.get(),
                    in_flight = m.loads_in_flight.get(),
                    priming_pending = m.priming_pending.get(),
                    // Whether the bandwidth is being spent twice. A store that
                    // is written and never read looks exactly like a store that
                    // is working, from anywhere but here.
                    store_hits = m.store_hits.get(),
                    store_misses = m.store_misses.get(),
                    // Drapes skipped whole — no request, no decode, no compose.
                    // Counted apart from the store so the hit rate keeps
                    // meaning "the cache answered" rather than "nothing ran".
                    withheld = m.drapes_withheld.get(),
                    // Why a frame is not converging, which `in_flight` alone
                    // cannot say. One load stuck at `in_flight=1` looks the
                    // same as a load cancelled and reissued a thousand times:
                    // the first is a tile waiting out its backoff, the second
                    // is a livelock, and they want opposite fixes. Measured
                    // 19 September 2026, ninety seconds of `in_flight=1` beside
                    // 185 021 traversals, with no way to tell which.
                    loads_started = m.loads_started.get(),
                    loads_done = m.loads_completed.get(),
                    loads_failed = m.loads_failed.get(),
                    loads_cancelled = m.loads_cancelled.get(),
                    traversals = m.traversals.get(),
                    source,
                    "MEMORY"
                );
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(path, body).expect("write");
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tuile-mem-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    /// The reading a Cloud Run container gives: cgroup v2, with a ceiling.
    #[test]
    fn cgroup_v2_carries_the_use_and_the_ceiling() {
        let root = scratch("v2");
        write(&root, "sys/fs/cgroup/memory.current", "1073741824\n");
        write(&root, "sys/fs/cgroup/memory.max", "34359738368\n");
        let r = read_at(&root).expect("a reading");
        assert_eq!(r.source, "cgroup2");
        assert_eq!(r.used, 1 << 30);
        assert_eq!(r.limit, Some(32 << 30));
        assert!((r.fraction().expect("bounded") - 0.03125).abs() < 1e-9);
    }

    /// `max` is not a number, and parsing it as one would report a ceiling of
    /// zero — which reads as "full" on every line.
    #[test]
    fn an_unlimited_cgroup_reports_no_ceiling() {
        let root = scratch("unbounded");
        write(&root, "sys/fs/cgroup/memory.current", "4096\n");
        write(&root, "sys/fs/cgroup/memory.max", "max\n");
        let r = read_at(&root).expect("a reading");
        assert_eq!(r.limit, None);
        assert_eq!(r.fraction(), None);
    }

    /// v1 writes a sentinel near `u64::MAX` rather than a word, and a farm line
    /// claiming a ceiling of eight exbibytes is a line nobody reads twice.
    #[test]
    fn the_v1_sentinel_is_not_a_ceiling() {
        let root = scratch("v1");
        write(
            &root,
            "sys/fs/cgroup/memory/memory.usage_in_bytes",
            "2147483648\n",
        );
        write(
            &root,
            "sys/fs/cgroup/memory/memory.limit_in_bytes",
            "9223372036854771712\n",
        );
        let r = read_at(&root).expect("a reading");
        assert_eq!(r.source, "cgroup1");
        assert_eq!(r.used, 2 << 30);
        assert_eq!(r.limit, None);
    }

    /// The half that RSS cannot see: files on a tmpfs are charged to the same
    /// limit as the heap, and this is what counts them.
    #[test]
    fn files_under_a_directory_are_counted_through_its_subdirectories() {
        let root = scratch("files");
        write(&root, "cache/blob/000", "0123456789");
        write(&root, "cache/blob/001", "01234");
        write(&root, "cache/meta", "0");
        assert_eq!(bytes_under(&root), 16);
    }

    /// A directory that is not there is zero bytes, not a panic: the watcher
    /// starts before the cache does.
    #[test]
    fn a_missing_directory_weighs_nothing() {
        assert_eq!(bytes_under(Path::new("/tuile-no-such-dir")), 0);
    }
}
