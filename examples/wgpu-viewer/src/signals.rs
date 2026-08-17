// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Ending the session rather than the process.
//!
//! A recording is most valuable in exactly the run that ends badly, so the
//! signals a person actually sends have to reach the normal exit path — the one
//! that closes the tape.

/// Set by a signal handler when the session is asked to stop.
///
/// An `AtomicBool` and nothing else, because a signal handler may only touch
/// async-signal-safe things — writing a file from one is undefined behaviour.
/// The event loop reads it and unwinds normally, which is what gets the
/// recording closed.
pub static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Asks Ctrl-C, `kill` and a closing terminal to end the session rather than
/// end the process.
///
/// A recording is most valuable in exactly the run that ends badly, so the
/// signals a person actually sends have to reach the normal exit path — where
/// the tape gets closed. `SIGKILL` cannot be caught by anyone, and neither can
/// a segfault or the power going out: the journal beside the MCAP file is
/// what covers those.
pub fn catch_interruptions() {
    if let Err(e) = ctrlc::set_handler(|| {
        INTERRUPTED.store(true, std::sync::atomic::Ordering::Relaxed);
    }) {
        tracing::warn!("no signal handler ({e}); Ctrl-C will lose an open recording");
    }
}
