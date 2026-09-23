// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Who renders which frames: the task's slice of the range, each process's
//! share of the slice, and the global number of each segment.

use super::config::Config;

/// Task `index` of `count`'s slice of `first..=last`.
///
/// The remainder is spread one frame at a time over the first tasks rather
/// than dumped on the last: 48 frames over 5 tasks is 10,10,10,9,9, not
/// 9,9,9,9,12 — the slowest task sets the wall clock. Contiguous and
/// exhaustive by construction: each slice starts where the previous ended.
pub fn task_slice(first: u64, last: u64, index: u32, count: u32) -> (u64, u64) {
    let all = last - first + 1;
    let (count, index) = (u64::from(count), u64::from(index));
    let (base, rem) = (all / count, all % count);
    let start = first + index * base + index.min(rem);
    let len = base + u64::from(index < rem);
    (start, start + len - 1)
}

/// The processes' shares of `first..=last`: equal spans, the last one taking
/// what is left — exactly as the frames were always handed to Blender.
pub fn process_ranges(first: u64, last: u64, jobs: u32) -> Vec<(u64, u64)> {
    let total = last - first + 1;
    let jobs = u64::from(jobs);
    let span = total / jobs;
    (0..jobs)
        .map(|i| {
            let a = first + i * span;
            let b = if i == jobs - 1 { last } else { first + (i + 1) * span - 1 };
            (a, b)
        })
        .collect()
}

/// The numbers each task owns: task `t`'s segments are `t × 1000 + i`. Fixed,
/// so that no two tasks can collide whatever each was configured with — a
/// chunk with two processes after one with four once took the numbers 2 and
/// 3 of the chunk before it. Receipts name their segments, so the gaps cost
/// nothing.
pub const SEGMENTS_PER_TASK: u64 = 1000;

/// What this task renders, and with how many processes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// This task's frames.
    pub first: u64,
    pub last: u64,
    /// One entry per process: (GPU, frames).
    pub processes: Vec<(u32, (u64, u64))>,
    /// The global number of process 0's segment.
    pub seg_base: u64,
}

impl Plan {
    pub fn of(c: &Config) -> Plan {
        let (first, last) = if c.chunk_given || c.task_count == 1 {
            c.frames
        } else {
            task_slice(c.frames.0, c.frames.1, c.task_index, c.task_count)
        };
        let nominal = c.gpus * c.procs_per_gpu;
        // Fewer frames than processes is a small render, not an error: two
        // frames to look at a texture are the renders launched most often.
        let total = last - first + 1;
        let jobs = u32::try_from(total).map_or(nominal, |t| nominal.min(t));
        let processes = process_ranges(first, last, jobs)
            .into_iter()
            .enumerate()
            .map(|(i, r)| (i as u32 / c.procs_per_gpu, r))
            .collect();
        Plan {
            first,
            last,
            processes,
            seg_base: u64::from(c.task_index) * SEGMENTS_PER_TASK,
        }
    }

    pub fn total(&self) -> u64 {
        self.last - self.first + 1
    }

    pub fn jobs(&self) -> u32 {
        self.processes.len() as u32
    }

    /// The global numbers of this task's segments, in frame order.
    pub fn segments(&self) -> Vec<u64> {
        (0..u64::from(self.jobs())).map(|i| self.seg_base + i).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tasks_share_the_range_without_gap_or_repeat() {
        let slices: Vec<_> = (0..5).map(|t| task_slice(1, 48, t, 5)).collect();
        assert_eq!(slices, [(1, 10), (11, 20), (21, 30), (31, 39), (40, 48)]);
        for (count, first, last) in [(3u32, 1u64, 8094u64), (9, 1, 8094), (7, 100, 105), (1, 5, 5)] {
            let s: Vec<_> = (0..count).map(|t| task_slice(first, last, t, count)).collect();
            assert_eq!(s[0].0, first);
            assert_eq!(s.last().expect("test").1, last);
            assert!(s.windows(2).all(|w| w[1].0 == w[0].1 + 1), "{s:?}");
        }
    }

    #[test]
    fn processes_share_the_slice_the_last_taking_the_rest() {
        assert_eq!(process_ranges(1, 10, 4), [(1, 2), (3, 4), (5, 6), (7, 10)]);
        assert_eq!(process_ranges(4000, 4000, 1), [(4000, 4000)]);
    }

    fn config(frames: (u64, u64), index: u32, count: u32, chunk: bool) -> Config {
        let get = move |k: &str| match k {
            "JOB_FRAMES" => Some(format!("{}:{}", frames.0, frames.1)),
            "JOB_GPUS" => Some("1".into()),
            "JOB_PROCS_PER_GPU" => Some("4".into()),
            "JOB_TASK_INDEX" if chunk => Some(index.to_string()),
            "JOB_TASK_COUNT" if chunk => Some(count.to_string()),
            "CLOUD_RUN_TASK_INDEX" if !chunk => Some(index.to_string()),
            "CLOUD_RUN_TASK_COUNT" if !chunk => Some(count.to_string()),
            _ => None,
        };
        Config::from_env(&get).expect("test")
    }

    #[test]
    fn cloud_run_tasks_cut_their_own_slice_a_given_chunk_is_kept() {
        let p = Plan::of(&config((1, 48), 3, 5, false));
        assert_eq!((p.first, p.last), (31, 39));
        let q = Plan::of(&config((601, 1200), 1, 14, true));
        assert_eq!((q.first, q.last), (601, 1200), "the orchestrator cut it already");
        assert_eq!(q.segments(), [1000, 1001, 1002, 1003]);
    }

    #[test]
    fn a_short_chunk_never_takes_the_numbers_of_the_one_before() {
        let full = Plan::of(&config((1, 600), 0, 2, true));
        let short = Plan::of(&config((601, 602), 1, 2, true));
        assert_eq!(short.jobs(), 2, "two frames, two processes");
        assert_eq!(full.segments(), [0, 1, 2, 3]);
        assert_eq!(short.segments(), [1000, 1001]);
    }

    #[test]
    fn processes_are_spread_over_the_gpus() {
        let get = |k: &str| match k {
            "JOB_FRAMES" => Some("1:80".into()),
            "JOB_GPUS" => Some("2".into()),
            "JOB_PROCS_PER_GPU" => Some("4".into()),
            _ => None,
        };
        let p = Plan::of(&Config::from_env(&get).expect("test"));
        let gpus: Vec<u32> = p.processes.iter().map(|(g, _)| *g).collect();
        assert_eq!(gpus, [0, 0, 0, 0, 1, 1, 1, 1]);
    }
}
