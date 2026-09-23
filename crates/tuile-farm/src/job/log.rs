// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Everything the job says about itself, on stdout and in `job.log`.
//!
//! One writer, flushed line by line. The script piped itself through `tee`,
//! a process with a buffer of its own: a shell that exited right after an
//! error was killed with its message still in that buffer. Measured 17
//! September 2026: two tasks dead on "range too short", not one line in Cloud
//! Logging, not one `logs.tar.gz`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Log {
    file: Arc<Mutex<Option<File>>>,
}

impl Log {
    /// Appends to `path`; stdout alone when it cannot be opened, said once.
    pub fn open(path: &Path) -> Log {
        let file = OpenOptions::new().create(true).append(true).open(path);
        if let Err(e) = &file {
            println!("JOB-LOG-UNAVAILABLE {}: {e}", path.display());
        }
        Log { file: Arc::new(Mutex::new(file.ok())) }
    }

    /// A log that writes nowhere but stdout.
    pub fn stdout_only() -> Log {
        Log { file: Arc::new(Mutex::new(None)) }
    }

    pub fn line(&self, s: impl AsRef<str>) {
        let s = s.as_ref();
        println!("{s}");
        let mut guard = self.file.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(f) = guard.as_mut() {
            let _ = writeln!(f, "{s}");
            let _ = f.flush();
        }
    }
}

/// `1.2M`, `12K`, `3.0G`: what `du -h` printed.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes}B")
    } else if v < 10.0 {
        format!("{v:.1}{}", UNITS[u])
    } else {
        format!("{v:.0}{}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_line_reaches_the_file_at_once() {
        let dir = tempfile::tempdir().expect("test");
        let path = dir.path().join("job.log");
        let log = Log::open(&path);
        log.line("frame 1: start");
        log.line("VIDEO-MISSING");
        // Read while the log is still alive: nothing waits in a buffer.
        assert_eq!(std::fs::read_to_string(&path).expect("test"), "frame 1: start\nVIDEO-MISSING\n");
    }

    #[test]
    fn sizes_read_like_du() {
        assert_eq!(human(512), "512B");
        assert_eq!(human(12 * 1024), "12K");
        assert_eq!(human(716 * 1024), "716K");
        assert_eq!(human(2_402_370_213), "2.2G");
    }
}
