// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Which archives a compaction merges: size tiers, as in a log-structured
//! merge tree.
//!
//! Merging everything each time rewrites the whole zone for every few small
//! deltas — gigabytes moved for megabytes added. Instead a compaction merges
//! only a run of archives of **similar size**: small deltas together into a
//! medium one, medium ones together later, and the large base only when as
//! much has accumulated next to it. Each byte is then rewritten about
//! `log_fanout(total / delta)` times instead of once per compaction.
//!
//! A run is always **contiguous** in the zone's order. Order is precedence —
//! on a tile present in several archives the later one wins — so merging
//! archives that are not neighbours would let the merged result jump over
//! whatever lies between them.

use std::ops::Range;

use crate::manifest::ArchiveRef;

/// The tiering policy.
#[derive(Debug, Clone, Copy)]
pub struct Tiering {
    /// Archives merged at once.
    pub fanout: usize,
    /// Archives are "similar" when the largest of a run is at most this many
    /// times the smallest.
    pub ratio: u64,
    /// Above this many archives in one epoch, the cheapest run is merged even
    /// if its sizes are not similar, so reads never walk too many archives.
    pub max_archives: usize,
}

/// The run to merge next, if any: indices into `archives`, all of `epoch`.
pub fn select(archives: &[ArchiveRef], epoch: &str, policy: Tiering) -> Option<Range<usize>> {
    let fanout = policy.fanout.max(2);
    let windows = || {
        (0..archives.len().saturating_sub(fanout - 1))
            .map(move |i| i..i + fanout)
            .filter(|w| archives[w.clone()].iter().all(|a| a.epoch == epoch))
    };
    let total = |w: &Range<usize>| archives[w.clone()].iter().map(|a| a.bytes).sum::<u64>();
    let similar = |w: &Range<usize>| {
        let run = &archives[w.clone()];
        let min = run.iter().map(|a| a.bytes).min().unwrap_or(0).max(1);
        let max = run.iter().map(|a| a.bytes).max().unwrap_or(0);
        max <= min.saturating_mul(policy.ratio)
    };

    if let Some(w) = windows().filter(similar).min_by_key(total) {
        return Some(w);
    }
    let in_epoch = archives.iter().filter(|a| a.epoch == epoch).count();
    if in_epoch > policy.max_archives {
        return windows().min_by_key(total);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(bytes: u64) -> ArchiveRef {
        ArchiveRef { key: format!("k{bytes}"), epoch: "e".into(), created: 0, tiles: 1, bytes }
    }

    const POLICY: Tiering = Tiering { fanout: 4, ratio: 4, max_archives: 12 };

    #[test]
    fn four_similar_small_deltas_after_a_large_base_are_merged_without_it() {
        let zone = [a(10_000), a(10), a(12), a(9), a(11)];
        assert_eq!(select(&zone, "e", POLICY), Some(1..5));
    }

    #[test]
    fn three_small_deltas_are_not_enough() {
        let zone = [a(10_000), a(10), a(12), a(9)];
        assert_eq!(select(&zone, "e", POLICY), None);
    }

    #[test]
    fn the_cheapest_similar_run_is_chosen() {
        let zone = [a(1000), a(1100), a(900), a(1000), a(10), a(12), a(9), a(11)];
        assert_eq!(select(&zone, "e", POLICY), Some(4..8));
    }

    #[test]
    fn past_the_archive_cap_a_dissimilar_run_is_forced() {
        // Sizes doubling: no four within a factor 4, so nothing is similar.
        let zone: Vec<_> = (0..13).map(|i| a(1 << (2 * i))).collect();
        assert_eq!(select(&zone[..12], "e", POLICY), None, "at the cap: still nothing");
        assert_eq!(select(&zone, "e", POLICY), Some(0..4), "past it: the cheapest run");
    }

    #[test]
    fn a_run_never_spans_two_epochs() {
        let mut zone = vec![a(10), a(10)];
        zone.extend([a(10), a(10)].map(|mut x| {
            x.epoch = "f".into();
            x
        }));
        assert_eq!(select(&zone, "e", POLICY), None);
        assert_eq!(select(&zone, "f", POLICY), None);
    }
}
