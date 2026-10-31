// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! [`FrameDeadline`]: the executor's upload cap per render frame (I18).
//!
//! The budget's [`UploadPlan`](super::UploadPlan) is capped per decision
//! step by `upload_bytes_per_frame`, deterministically. Only the render side
//! knows its frame: a headset at 90 Hz has 11.1 ms, of which uploads may take
//! a share, at the throughput the executor measures. The executor drains its
//! upload queue in the plan's order through [`FrameDeadline::take`], frame by
//! frame; what it defers it reports as a deviation, and it reaches the
//! budget only as later acknowledgements — never as an input of `decide`
//! (D-A8). Every executor — the wgpu pump, the web page, the USD scene index
//! — applies this one rule, which is why it lives in the core.

use super::decisions::SwapGroup;
use super::types::{Bytes, Micros, ResourceId};

/// How much one render frame may upload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FrameDeadline {
    /// Bytes per frame.
    pub bytes: Bytes,
    /// The time those bytes may take, per frame.
    pub time: Micros,
}

impl FrameDeadline {
    /// No cap: a film's frame waits for what it needs.
    pub const UNBOUNDED: FrameDeadline = FrameDeadline {
        bytes: Bytes::MAX,
        time: Micros::NEVER,
    };

    /// The deadline of a display at `hz`: the frame period, times the share
    /// of it given to uploads (`upload_share_permille`, thousandths), times
    /// the executor's measured upload throughput (`bytes_per_ms`). The
    /// throughput is the executor's own and may vary; it never enters
    /// `decide`.
    pub fn from_refresh(hz: u32, upload_share_permille: u16, bytes_per_ms: u64) -> Self {
        let period_us = 1_000_000 / u64::from(hz.max(1));
        let time_us = period_us * u64::from(upload_share_permille.min(1_000)) / 1_000;
        let bytes = u128::from(time_us) * u128::from(bytes_per_ms) / 1_000;
        FrameDeadline {
            bytes: Bytes(u64::try_from(bytes).unwrap_or(u64::MAX)),
            time: Micros(time_us),
        }
    }

    /// The prefix of `queue` (the executor's uploads, in the plan's order)
    /// this frame uploads, `done` bytes having gone already this frame:
    /// while the bytes fit, item by item, a swap group of `groups` whole
    /// when it fits; the first item of a frame always goes, so a resource
    /// larger than the deadline cannot block the queue.
    pub fn take<'p>(
        &self,
        queue: &'p [ResourceId],
        groups: &[SwapGroup],
        done: Bytes,
        cost: impl Fn(ResourceId) -> Bytes,
    ) -> &'p [ResourceId] {
        let mut used = done;
        let mut n = 0;
        while n < queue.len() {
            // The unit at `n`: a whole swap group when the queue holds its
            // members from here on, contiguously, as the plan lays them out;
            // else the item alone.
            let unit = groups
                .iter()
                .find(|g| {
                    g.members.len() > 1
                        && g.members.contains(&queue[n])
                        && queue
                            .get(n..n + g.members.len())
                            .is_some_and(|run| run.iter().all(|r| g.members.contains(r)))
                })
                .map_or(1, |g| g.members.len());
            let bytes = queue[n..n + unit]
                .iter()
                .fold(Bytes::ZERO, |a, r| a.plus(cost(*r)));
            if used.plus(bytes) <= self.bytes {
                used = used.plus(bytes);
                n += unit;
                continue;
            }
            if n == 0 && done == Bytes::ZERO {
                // The first item of a frame always goes, alone if its group
                // is too big.
                n = 1;
            }
            break;
        }
        &queue[..n]
    }
}
