// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a task leaves behind: its logs, traces and crash reports, shipped as
//! tarballs to the run — on every way out, and periodically while it runs.
//!
//! A job that failed is the one whose logs are worth having, and a pod that is
//! reclaimed runs nothing at the end: three measurements died with their
//! machine in two days before this existed.

use std::path::{Path, PathBuf};

use flate2::write::GzEncoder;
use flate2::Compression;

use super::log::{human, Log};
use crate::RunStore;

/// The logs: per-process output, the job's own log, and each process's crash
/// report (its own TMPDIR, `tmp-s<i>`).
pub const LOGS: &[&str] = &["log-s*.txt", "job.log", "tmp-s*/blender.crash.txt"];
pub const TRACES: &[&str] = &["trace-s*.jsonl"];
pub const PROFILE: &[&str] = &["profile"];

/// Files under `dir` matching `patterns`, relative to it, each once.
pub fn matching(dir: &Path, patterns: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for pat in patterns {
        let full = dir.join(pat);
        let Some(full) = full.to_str() else { continue };
        let Ok(paths) = glob::glob(full) else { continue };
        for p in paths.flatten() {
            if let Ok(rel) = p.strip_prefix(dir) {
                if !out.iter().any(|q: &PathBuf| q == rel) {
                    out.push(rel.to_path_buf());
                }
            }
        }
    }
    out
}

/// A gzipped tar of `files` (relative to `dir`, directories recursed), each
/// file read whole before it is written: a log still growing is archived as it
/// was when read, and the archive stays well-formed — `tar` on a file that
/// changed under it returned 1, and treating that as fatal once killed every
/// periodic flush.
pub fn tarball(dir: &Path, files: &[PathBuf], dest: &Path) -> std::io::Result<()> {
    let out = std::fs::File::create(dest)?;
    let mut tar = tar::Builder::new(GzEncoder::new(out, Compression::default()));
    let mut stack: Vec<PathBuf> = files.to_vec();
    while let Some(rel) = stack.pop() {
        let path = dir.join(&rel);
        if path.is_dir() {
            for e in std::fs::read_dir(&path)?.flatten() {
                stack.push(rel.join(e.file_name()));
            }
            continue;
        }
        let Ok(data) = std::fs::read(&path) else { continue };
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()));
        header.set_cksum();
        tar.append_data(&mut header, &rel, data.as_slice())?;
    }
    tar.into_inner()?.finish()?;
    Ok(())
}

/// Where the archives of one task go.
pub struct Shipper<'a> {
    pub store: &'a dyn RunStore,
    pub prefix: &'a str,
    pub tag: String,
    pub dir: PathBuf,
}

impl Shipper<'_> {
    /// The key of `name` for this task: `logs.tar.gz` → `<run>/logs-t3.tar.gz`.
    pub fn key(&self, name: &str) -> String {
        format!("{}/{}{}.tar.gz", self.prefix, name.trim_end_matches(".tar.gz"), self.tag)
    }

    /// Ships one tarball. Nothing to ship is not a failure; a failure is said
    /// aloud unless `quiet`, and returned.
    pub async fn ship(&self, name: &str, patterns: &[&str], log: &Log, quiet: bool) -> Result<(), String> {
        let files = matching(&self.dir, patterns);
        if files.is_empty() {
            return Ok(());
        }
        let dest = self.dir.join(name);
        if let Err(e) = tarball(&self.dir, &files, &dest) {
            log.line(format!("ARCHIVE-TAR-FAILED {name} ({e})"));
            return Err(e.to_string());
        }
        let size = std::fs::metadata(&dest).map_or(0, |m| m.len());
        if !quiet {
            log.line(format!("{name}: {}", human(size)));
        }
        let key = self.key(name);
        match self.store.put(&dest, &key).await {
            Ok(_) => {
                if !quiet {
                    log.line(format!("ARCHIVE-UP {key}"));
                }
                Ok(())
            }
            Err(e) => {
                log.line(format!("  {e}"));
                log.line(format!("ARCHIVE-UP-FAILED {name}"));
                Err(e.to_string())
            }
        }
    }

    /// Everything, at the end: logs, traces, profile.
    pub async fn ship_all(&self, log: &Log) {
        let _ = self.ship("logs.tar.gz", LOGS, log, false).await;
        let _ = self.ship("trace.tar.gz", TRACES, log, false).await;
        let _ = self.ship("profile.tar.gz", PROFILE, log, false).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ObjectRunStore;

    fn names(tgz: &Path) -> Vec<String> {
        let file = std::fs::File::open(tgz).expect("test");
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(file));
        let mut v: Vec<String> =
            ar.entries().expect("test").map(|e| e.expect("test").path().expect("test").to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn the_crash_reports_travel_with_the_logs() {
        let out = tempfile::tempdir().expect("test");
        let store_dir = tempfile::tempdir().expect("test");
        let d = out.path();
        std::fs::write(d.join("log-s0.txt"), "frame 3991: start\n").expect("test");
        std::fs::write(d.join("job.log"), "job\n").expect("test");
        std::fs::create_dir(d.join("tmp-s1")).expect("test");
        std::fs::write(d.join("tmp-s1/blender.crash.txt"), "# backtrace\n").expect("test");
        std::fs::write(d.join("trace-s0.jsonl"), "{}\n").expect("test");
        let store = ObjectRunStore::local(store_dir.path(), crate::Tuning::default()).expect("test");
        let shipper = Shipper { store: &store, prefix: "renders/r1", tag: "-t3".into(), dir: d.to_path_buf() };
        shipper.ship_all(&Log::stdout_only()).await;
        let landed = store_dir.path().join("renders/r1/logs-t3.tar.gz");
        assert_eq!(names(&landed), ["job.log", "log-s0.txt", "tmp-s1/blender.crash.txt"]);
        assert!(store_dir.path().join("renders/r1/trace-t3.tar.gz").exists());
        assert!(!store_dir.path().join("renders/r1/profile-t3.tar.gz").exists(), "nothing to ship is not a file");
    }

    #[test]
    fn a_file_growing_while_archived_leaves_a_whole_archive() {
        let out = tempfile::tempdir().expect("test");
        let d = out.path().to_path_buf();
        std::fs::write(d.join("job.log"), "x".repeat(100_000)).expect("test");
        let writer = {
            let d = d.clone();
            std::thread::spawn(move || {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new().append(true).open(d.join("job.log")).expect("test");
                for _ in 0..2000 {
                    let _ = f.write_all(b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
                }
            })
        };
        let dest = d.join("logs.tar.gz");
        tarball(&d, &matching(&d, LOGS), &dest).expect("test");
        writer.join().expect("test");
        assert_eq!(names(&dest), ["job.log"], "readable, whole");
    }

    #[test]
    fn patterns_expand_inside_the_directory_only() {
        let out = tempfile::tempdir().expect("test");
        let d = out.path();
        std::fs::write(d.join("log-s0.txt"), "").expect("test");
        std::fs::write(d.join("log-s12.txt"), "").expect("test");
        std::fs::write(d.join("other.txt"), "").expect("test");
        let mut got: Vec<_> = matching(d, &["log-s*.txt", "log-s0.txt"]).into_iter().map(|p| p.display().to_string()).collect();
        got.sort();
        assert_eq!(got, ["log-s0.txt", "log-s12.txt"], "each once, nothing else");
    }
}
