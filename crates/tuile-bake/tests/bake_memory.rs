// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **Where a 4K bake's memory goes, frame by frame.**
//!
//! Farm workers were killed for memory at 12 GiB on 4K films (export 191,
//! 3840 × 2160, SSE 3, a high follow camera), every pod, inside the bake.
//! The engine's gauges say what the budget's ledger charges, not what the
//! heap holds. This flies a bake's stages over the made-up globe of
//! `bounded_settle` at the film's size — a survey that settles a window of
//! frames and keeps their selections, the window's grounds read again from
//! them, then the bake's own frames — and counts every byte the process
//! allocates (a counting allocator), beside each structure's own figure.
//!
//! One row a frame goes to a Parquet table (`BAKE_MEMORY_OUT`, default
//! `target/bake-memory.parquet`); one `BENCH` line sums it up.
//!
//! Heavy: ignored by default. On the builder:
//!
//! ```text
//! cargo test -p tuile-bake --release --test bake_memory -- --ignored --nocapture
//! ```
//!
//! Where the bytes were allocated: `bake_memory_sites`, the same flight
//! under a heap profiler.
//!
//! Knobs, by environment: `BAKE_MEMORY_FRAMES` (frames a stage, 96),
//! `BAKE_MEMORY_WIDTH`/`_HEIGHT` (3840 × 2160), `BAKE_MEMORY_SSE` (3),
//! `BAKE_MEMORY_STEPS` (spacings a side of a terrain tile, 32),
//! `BAKE_MEMORY_KEEP_GIB` (what the session keeps between frames, 1),
//! `BAKE_MEMORY_EYE_M` (the camera's height, 700 m), `BAKE_MEMORY_PITCH`
//! (how far down it looks, 0.45).
//!
//! What it does not reproduce: the real terrain's tiles (a regular grid of
//! `STEPS` here, decimated meshes there), the tile store and its indexes,
//! the register, the request slots and the pack writer's spill — the
//! process's fixed costs are measured on the farm, not here.

#[path = "bake_memory_support/mod.rs"]
mod flight;
#[path = "../../tuile-terrain/tests/support/mod.rs"]
mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Every byte the process holds on the heap, and the most it held since
/// the peak was last reset.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let now = LIVE.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

// SAFETY: every call is forwarded to the system allocator unchanged; the
// counters only observe the sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                grew(new_size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

#[test]
#[ignore = "heavy: a 4K bake's stages; run on the builder with --ignored"]
fn where_a_4k_bake_s_memory_goes() {
    flight::run(
        || LIVE.load(Ordering::Relaxed),
        || PEAK.load(Ordering::Relaxed),
    );
}
