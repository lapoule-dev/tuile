// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// Sorts the visible pixels by tile, so that the resolve can run once per tile
// with that tile's geometry and texture bound — which is what lets it work on
// WebGPU, where there are no bindless texture arrays.
//
//   count:   pixels per slot
//   scan:    exclusive prefix sum → each slot's start in the list,
//            and its indirect dispatch size
//   scatter: pixel indices into the list, grouped by slot

const MAX_SLOTS: u32 = 8192u;
const SCAN_THREADS: u32 = 256u;
const PER_THREAD: u32 = MAX_SLOTS / SCAN_THREADS;
const RESOLVE_GROUP: u32 = 64u;
// Groups in a row of a tile's resolve. A dispatch is at most 65535 groups
// in a dimension: in one row, a tile of more than four million pixels —
// the near ground of a large picture — could not be resolved at all, and
// came out black.
const RESOLVE_ROW: u32 = 1024u;

@group(0) @binding(0) var vis: texture_2d<u32>;
@group(0) @binding(1) var<storage, read_write> counts: array<atomic<u32>>;
@group(0) @binding(2) var<storage, read_write> starts: array<u32>;
@group(0) @binding(3) var<storage, read_write> cursor: array<atomic<u32>>;
@group(0) @binding(4) var<storage, read_write> list: array<u32>;
@group(0) @binding(5) var<storage, read_write> args: array<u32>;

@compute @workgroup_size(8, 8)
fn count(@builtin(global_invocation_id) id: vec3u) {
    let size = textureDimensions(vis);
    if (id.x >= size.x || id.y >= size.y) { return; }
    let s = textureLoad(vis, id.xy, 0).x;
    if (s == 0u) { return; }
    atomicAdd(&counts[s - 1u], 1u);
}

var<workgroup> sums: array<u32, SCAN_THREADS>;

@compute @workgroup_size(256)
fn scan(@builtin(local_invocation_index) t: u32) {
    // Each thread owns a contiguous run of slots.
    let base = t * PER_THREAD;
    var total = 0u;
    for (var i = 0u; i < PER_THREAD; i++) {
        total += atomicLoad(&counts[base + i]);
    }
    sums[t] = total;
    workgroupBarrier();
    // Hillis–Steele inclusive scan over the run totals.
    for (var d = 1u; d < SCAN_THREADS; d *= 2u) {
        var add = 0u;
        if (t >= d) { add = sums[t - d]; }
        workgroupBarrier();
        sums[t] += add;
        workgroupBarrier();
    }
    var at = sums[t] - total;
    for (var i = 0u; i < PER_THREAD; i++) {
        let s = base + i;
        let n = atomicLoad(&counts[s]);
        starts[s] = at;
        atomicStore(&cursor[s], at);
        let groups = (n + RESOLVE_GROUP - 1u) / RESOLVE_GROUP;
        args[3u * s] = min(groups, RESOLVE_ROW);
        args[3u * s + 1u] = (groups + RESOLVE_ROW - 1u) / RESOLVE_ROW;
        args[3u * s + 2u] = 1u;
        at += n;
    }
}

@compute @workgroup_size(8, 8)
fn scatter(@builtin(global_invocation_id) id: vec3u) {
    let size = textureDimensions(vis);
    if (id.x >= size.x || id.y >= size.y) { return; }
    let s = textureLoad(vis, id.xy, 0).x;
    if (s == 0u) { return; }
    let at = atomicAdd(&cursor[s - 1u], 1u);
    list[at] = id.y * size.x + id.x;
}
