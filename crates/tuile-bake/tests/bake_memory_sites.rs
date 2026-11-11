// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **Where a bake's bytes were allocated, at its peak.**
//!
//! The flight of `bake_memory`, under dhat's heap profiler: what was live
//! at the moment the heap was largest, by the place it was allocated, into
//! `BAKE_MEMORY_DHAT` (default `target/dhat-heap.json`, dhat's own format,
//! read by its viewer or by a script). Slower than the counting allocator
//! by a large factor: run it on smaller flights than the bench's.
//!
//! ```text
//! cargo test -p tuile-bake --release --test bake_memory_sites -- --ignored --nocapture
//! ```

#[path = "bake_memory_support/mod.rs"]
mod flight;
#[path = "../../tuile-terrain/tests/support/mod.rs"]
mod support;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[test]
#[ignore = "heavy: a bake's stages under a heap profiler; run on the builder with --ignored"]
fn where_a_bake_s_bytes_were_allocated() {
    let out =
        std::env::var("BAKE_MEMORY_DHAT").unwrap_or_else(|_| "target/dhat-heap.json".to_string());
    let profiler = dhat::Profiler::builder().file_name(out).build();
    flight::run(
        || dhat::HeapStats::get().curr_bytes,
        || dhat::HeapStats::get().max_bytes,
    );
    drop(profiler);
}
