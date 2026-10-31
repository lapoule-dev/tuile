// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A counting global allocator, armed per thread around the code under
//! measurement: how many blocks `decide` asks the heap for, and how many
//! bytes a budget holds. Test-only; the library allocates through whatever
//! its host installs.
//!
//! The counters are thread-local and const-initialised, with no destructor:
//! touching them never registers a lazy initialiser nor allocates, so the
//! allocator never re-enters itself, and tests running in parallel on other
//! threads do not count into each other.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// The system allocator, counted while armed.
pub struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
    static LIVE: Cell<i64> = const { Cell::new(0) };
    static PEAK: Cell<i64> = const { Cell::new(0) };
}

fn on_alloc(n: usize) {
    let n = n as u64;
    let _ = ARMED.try_with(|a| {
        if a.get() {
            ALLOCS.with(|c| c.set(c.get() + 1));
            BYTES.with(|c| c.set(c.get() + n));
            LIVE.with(|l| {
                let v = l.get() + n as i64;
                l.set(v);
                PEAK.with(|p| p.set(p.get().max(v)));
            });
        }
    });
}

fn on_free(n: usize) {
    let _ = ARMED.try_with(|a| {
        if a.get() {
            LIVE.with(|l| l.set(l.get() - n as i64));
        }
    });
}

// SAFETY: every method forwards to `System` with the caller's arguments
// unchanged; the counting touches only const-initialised thread-locals and
// never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        on_alloc(l.size());
        // SAFETY: the caller's contract, forwarded.
        unsafe { System.alloc(l) }
    }

    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        on_alloc(l.size());
        // SAFETY: the caller's contract, forwarded.
        unsafe { System.alloc_zeroed(l) }
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        on_free(l.size());
        // SAFETY: the caller's contract, forwarded.
        unsafe { System.dealloc(p, l) }
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        // A growth is one more request to the heap: counted as one.
        on_free(l.size());
        on_alloc(new);
        // SAFETY: the caller's contract, forwarded.
        unsafe { System.realloc(p, l, new) }
    }
}

/// What the heap saw while armed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    /// Blocks asked for (allocations and reallocations).
    pub allocs: u64,
    /// Bytes asked for.
    pub bytes: u64,
    /// Bytes allocated minus bytes freed: negative when `f` freed more than
    /// it took.
    pub live: i64,
    /// The highest `live` reached.
    pub peak: i64,
}

/// Runs `f` with counting armed on this thread only.
pub fn counted<R>(f: impl FnOnce() -> R) -> (R, Tally) {
    ALLOCS.with(|x| x.set(0));
    BYTES.with(|x| x.set(0));
    LIVE.with(|x| x.set(0));
    PEAK.with(|x| x.set(0));
    ARMED.with(|a| a.set(true));
    let r = f();
    ARMED.with(|a| a.set(false));
    let t = Tally {
        allocs: ALLOCS.with(Cell::get),
        bytes: BYTES.with(Cell::get),
        live: LIVE.with(Cell::get),
        peak: PEAK.with(Cell::get),
    };
    (r, t)
}
