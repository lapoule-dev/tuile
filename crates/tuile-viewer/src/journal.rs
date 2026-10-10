// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A few lines per session, in a file: when it started, and why it ended.
//!
//! This is not the log. The log is `tracing`, on stderr, as verbose as
//! `RUST_LOG` asks. This is for the one question the log cannot answer when a
//! session was started from an icon — *the window is gone; what happened?* —
//! because by then stderr has gone wherever the desktop sends it and nothing
//! was watching. An application that stops without a trace is
//! indistinguishable from one that was closed, and the difference matters.
//!
//! So every way out leaves a line: the window closed, Esc, quit from the menu,
//! a signal, an error on the way up, a panic. A session whose last line is
//! `started` was killed outright or lost with the machine, which is itself an
//! answer.
//!
//! ```text
//! macOS      ~/Library/Logs/Tuile/viewer.log
//! elsewhere  $XDG_STATE_HOME/tuile/viewer.log   (or ~/.local/state/…)
//! ```
//!
//! Appended to, never read, and kept short: past [`LIMIT`] bytes the file is
//! started afresh. No coordinates and no credential are ever written — only
//! the reason, the time and the process id.

use std::io::Write;

/// Past this size the file is truncated rather than left to grow: at a few
/// lines a session, that is thousands of sessions.
const LIMIT: u64 = 256 * 1024;

/// A UTC timestamp, `2026-10-11T00:29:25Z`, from the system clock.
///
/// By hand, because the alternative is a calendar crate for one line of text:
/// days since the epoch to a civil date is a dozen lines of integer
/// arithmetic that has not changed since the Gregorian reform.
fn timestamp(unix_seconds: u64) -> String {
    let (days, rest) = (unix_seconds / 86_400, unix_seconds % 86_400);
    // Days since 0000-03-01, in 400-year eras of 146 097 days.
    let z = days + 719_468;
    let (era, day_of_era) = (z / 146_097, z % 146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Appends one line. Failing to is silent: a journal that cannot be written
/// must not be the reason a session stops.
pub(crate) fn note(what: &str) {
    let Some(path) = crate::host::log_file(&crate::host::Machine) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let oversized = std::fs::metadata(&path).is_ok_and(|m| m.len() > LIMIT);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(!oversized)
        .write(true)
        .truncate(oversized)
        .open(&path);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if let Ok(mut file) = file {
        let _ = writeln!(file, "{} [{}] {what}", timestamp(now), std::process::id());
    }
}

/// Marks the start of a session, and makes a panic leave its line before the
/// process goes.
pub(crate) fn open() {
    note(&format!("started, version {}", env!("CARGO_PKG_VERSION")));
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        note(&format!("session ended: panic: {info}"));
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::timestamp;

    /// Known instants, a leap day and a century boundary among them.
    #[test]
    fn the_timestamp_is_the_civil_date_in_utc() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(timestamp(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(timestamp(951_868_799), "2000-02-29T23:59:59Z");
        assert_eq!(timestamp(951_868_800), "2000-03-01T00:00:00Z");
        assert_eq!(timestamp(1_791_678_565), "2026-10-11T00:29:25Z");
        assert_eq!(timestamp(4_107_542_400), "2100-03-01T00:00:00Z");
    }
}
