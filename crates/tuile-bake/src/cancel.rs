// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A bake that is no longer wanted, told so.
//!
//! A bake is minutes of work and gigabytes of memory on a worker that has
//! other bakes to do. When whoever asked for it gives up — the film was
//! cancelled, the attempt timed out and was handed to another worker — the
//! bake has to stop where it stands and let its memory go, not finish a
//! pack nobody will read.
//!
//! [`Cancel`] is how a host says it. Given to a session
//! ([`crate::SessionConfig::cancel`]), it is looked at before every frame
//! and listened to while a frame waits on its loads: a cancelled session
//! answers every frame with [`crate::FrameError::Cancelled`], so the loops
//! that drive one — [`crate::settle::settle_each`],
//! [`crate::bake::bake_frames`] — stop at the frame they are on, and what
//! they held is dropped with them.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A flag a host raises once to stop a bake; clones share it.
///
/// Raised, it stays raised: a session cancelled is good for nothing but
/// being dropped.
#[derive(Clone, Default)]
pub struct Cancel(Arc<Inner>);

#[derive(Default)]
struct Inner {
    raised: AtomicBool,
    raising: tokio::sync::Notify,
}

impl Cancel {
    /// A flag not raised.
    pub fn new() -> Self {
        Self::default()
    }

    /// Raises the flag: every session given it stops at once.
    pub fn cancel(&self) {
        self.0.raised.store(true, Ordering::SeqCst);
        self.0.raising.notify_waiters();
    }

    /// Whether the flag is raised.
    pub fn is_cancelled(&self) -> bool {
        self.0.raised.load(Ordering::SeqCst)
    }

    /// Done when the flag is raised: at once if it is already.
    pub async fn cancelled(&self) {
        loop {
            let raising = self.0.raising.notified();
            tokio::pin!(raising);
            // Registered before the flag is read: a raise between the read
            // and the wait is not missed.
            raising.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            raising.await;
        }
    }
}

impl std::fmt::Debug for Cancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Cancel").field(&self.is_cancelled()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flag_raised_is_seen_by_every_clone_and_stays_raised() {
        let cancel = Cancel::new();
        let clone = cancel.clone();
        assert!(!clone.is_cancelled());
        cancel.cancel();
        assert!(clone.is_cancelled());
        cancel.cancel();
        assert!(clone.is_cancelled(), "raised twice, still raised");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_wait_ends_when_the_flag_is_raised_and_at_once_if_it_was() {
        let cancel = Cancel::new();
        let raiser = cancel.clone();
        let waited = tokio::spawn(async move { cancel.cancelled().await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        raiser.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), waited)
            .await
            .expect("woken")
            .expect("joined");
        tokio::time::timeout(std::time::Duration::from_secs(1), raiser.cancelled())
            .await
            .expect("already raised: at once");
    }
}
