// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A trace built to be **diffed**, not read.
//!
//! A render is supposed to be a function of `(stage, time)`: two farm nodes
//! given the same frame owe the same image. Measured on 2026-09-08, they do
//! not — the same stage, the same binary and the same tile cache produced 106
//! selected tiles on one run and 7 on the next, alternating. The only
//! observable was the final count, which says two runs disagreed and never
//! says *where*.
//!
//! This is the where. Every decision that can change a selection emits one
//! record; two runs are then `diff`ed and the first differing line names the
//! moment they parted company.
//!
//! # What makes it comparable
//!
//! Everything that varies **by nature** is excluded, or the diff is all noise
//! and tells you nothing:
//!
//! * no wall-clock time, no thread ids, no pointers;
//! * a per-process sequence number, so ordering is explicit rather than
//!   inferred from arrival;
//! * an elapsed time **since the process started**, in milliseconds, at a
//!   fixed column — because "when did it arrive" is half of any ordering
//!   question, and a trace that cannot answer it explains nothing about a
//!   race. It is the one field that legitimately differs between two runs, so
//!   it sits where one `sed` removes it (see below) rather than being left
//!   out and missed;
//! * floating point printed with `{:?}`, which round-trips — a rounded
//!   position would hide exactly the drift worth finding;
//! * sets summarised by a **digest over sorted ids**, so "the same 106 tiles"
//!   and "a different 106 tiles" do not read alike.
//!
//! Concurrency remains: load completions genuinely arrive in whatever order
//! the network allows, so those records differ between runs without anything
//! being wrong. That is why loads carry their own event name — the analysis
//! can drop them and still compare the decisions.
//!
//! # Using it
//!
//! Off unless asked for, at no cost when off (`tracing` compiles an absent
//! subscriber down to a check the optimiser removes):
//!
//! ```text
//! TUILE_LOG=tuile_det=info … 2>&1 | grep '^DET ' > run-a.log
//! TUILE_LOG=tuile_det=info … 2>&1 | grep '^DET ' > run-b.log
//!
//! # when it happened, kept:
//! diff run-a.log run-b.log | head
//! # what was decided, with the timing stripped so the diff is only decisions:
//! strip() { sed 's/^DET \([0-9]*\) +[0-9]*ms /DET \1 /'; }
//! diff <(strip < run-a.log) <(strip < run-b.log) | head
//! ```
//!
//! Each record reads:
//!
//! ```text
//! DET <seq> +<ms since process start> <event> <key>=<value> …
//! ```

use std::sync::atomic::{AtomicU64, Ordering};

/// The record counter for this process. Sessions are compared across
/// processes, so it starts at zero every time and means "how many decisions
/// have been taken so far", not wall time.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// The next record number.
pub fn next_seq() -> u64 {
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// When this process started, fixed on first use.
///
/// Not on wasm, which has no `Instant`; there the elapsed column is always
/// zero rather than the module being unavailable, since the rule this crate
/// lives under is that it builds for wasm.
#[cfg(not(target_arch = "wasm32"))]
static STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Milliseconds since the first record of this process.
///
/// Relative, not absolute: two runs are compared against each other, and a
/// wall clock would differ in every line for no reason anyone cares about.
/// Relative time answers the question that matters — *how long into the job
/// did this happen* — and is the only way to see that one run converged
/// before an arrival that the other had already taken in.
#[cfg(not(target_arch = "wasm32"))]
pub fn elapsed_ms() -> u64 {
    STARTED
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

#[cfg(target_arch = "wasm32")]
pub fn elapsed_ms() -> u64 {
    0
}

/// Back to zero — for a test that compares two sessions in one process.
pub fn reset_seq() {
    SEQ.store(0, Ordering::Relaxed);
}

/// A stable digest of a set of tile ids.
///
/// FNV-1a over the **sorted** ids: order of arrival must not change the
/// answer, since the question is "is this the same set of ground?". Stable
/// across processes and architectures, which `DefaultHasher` explicitly is
/// not — and a digest that changed between runs for its own reasons would be
/// worse than no digest at all.
pub fn digest(ids: impl IntoIterator<Item = u64>) -> u64 {
    let mut sorted: Vec<u64> = ids.into_iter().collect();
    sorted.sort_unstable();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for id in sorted {
        for byte in id.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

/// Debug-formats a value that is not a `tracing` primitive — an `Option`, a
/// small array — so it still travels as a real field rather than being
/// stringified into the message.
pub fn dbg<T: std::fmt::Debug>(value: T) -> tracing::field::DebugValue<T> {
    tracing::field::debug(value)
}

/// Emits one comparable record as **structured fields**, not as text.
///
/// The fields are handed to `tracing` individually, so a JSON subscriber
/// writes one valid JSON object per record and every field is typed. The
/// first version of this formatted JSON into the message string instead,
/// which meant the subscriber wrapped a JSON blob inside its own text format
/// and the reader had to grep the object back out of a log line — a
/// structured trace in appearance only, and unparseable the moment a message
/// contained a brace.
///
/// Anything that is not a `tracing` primitive goes through [`macro@dbg`].
#[macro_export]
macro_rules! det {
    ($event:expr) => {
        ::tracing::info!(
            target: "tuile_det",
            seq = $crate::determinism::next_seq(),
            ms = $crate::determinism::elapsed_ms(),
            event = $event,
        );
    };
    ($event:expr, $($k:ident = $v:expr),+ $(,)?) => {
        ::tracing::info!(
            target: "tuile_det",
            seq = $crate::determinism::next_seq(),
            ms = $crate::determinism::elapsed_ms(),
            event = $event,
            $($k = $v,)+
        );
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digest_ignores_order_but_not_membership() {
        assert_eq!(digest([3, 1, 2]), digest([1, 2, 3]));
        assert_ne!(digest([1, 2, 3]), digest([1, 2, 4]));
        // A repeat is not the same set as a single — the traversal must never
        // select a tile twice, and a digest that hid it would be complicit.
        assert_ne!(digest([1, 2, 3]), digest([1, 2, 3, 3]));
        assert_ne!(digest([]), digest([0]));
    }

    #[test]
    fn the_digest_is_the_same_number_every_run() {
        // Pinned, because the whole point is comparing two processes: a digest
        // seeded from anything process-local would compare nothing.
        assert_eq!(digest([1, 2, 3]), 15_720_935_049_292_226_309);
    }

    /// The trace has to survive a machine reading it.
    ///
    /// Asserted rather than assumed, because the first version *looked* like
    /// JSON — it formatted an object into the message string — and a
    /// subscriber then wrapped that in its own text format. Everything
    /// downstream had to grep the object back out of a log line, which works
    /// until any other message contains a brace. This parses a real record
    /// back and checks the fields arrived typed.
    #[test]
    #[allow(clippy::panic, reason = "the parse failure IS the assertion")]
    fn a_record_is_one_valid_json_object_with_typed_fields() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Shared(Arc<Mutex<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("buffer").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink = Shared::default();
        let made = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_current_span(false)
            .with_span_list(false)
            .with_writer(move || made.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            crate::det!(
                "pass",
                n = 3_u64,
                selected = 106_usize,
                sel_digest = digest([1, 2, 3]),
                gaps = 0_u32,
                camera_moved = true,
                d_near = Some(612.5_f64),
            );
        });

        let raw = String::from_utf8(sink.0.lock().expect("buffer").clone()).expect("utf8");
        let line = raw.lines().next().expect("one record");
        let v: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON: {e}\n{line}"));
        assert_eq!(v["event"], "pass");
        assert_eq!(v["selected"], 106, "a count must arrive as a number");
        assert_eq!(v["camera_moved"], true, "a flag must arrive as a bool");
        assert_eq!(v["sel_digest"], digest([1, 2, 3]));
        assert!(v["seq"].is_number(), "every record is ordered");
        assert!(v["ms"].is_number(), "every record says when");
        // `tracing` unwraps an Option, so a present value stays a typed
        // number rather than becoming the string "Some(612.5)". Asserted
        // because the analysis leans on it: an absent field means None, and
        // nothing else does.
        assert_eq!(v["d_near"], 612.5);
        assert!(
            v.get("occluder").is_none(),
            "a field never emitted must be absent, not null"
        );
    }

    #[test]
    fn sequence_numbers_are_dense_and_ordered() {
        reset_seq();
        let a = next_seq();
        let b = next_seq();
        assert_eq!(a, 0);
        assert_eq!(b, 1);
    }
}
