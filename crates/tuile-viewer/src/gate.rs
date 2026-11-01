// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Holding the network still while the application is in the background.
//!
//! A phone suspends an application a few seconds after it leaves the screen,
//! and what it was doing then — a fetch half-read, a decode half-done — is
//! either finished in those seconds or torn down. The geometry server knows
//! nothing of any of that and should not: it asks its loader for tiles, and
//! the loader is where a pause belongs.
//!
//! So the loader the server is given is wrapped in a [`Gated`] one. While the
//! [`Gate`] is shut, a load waits *before* it starts — no request leaves, no
//! decode is queued — and what was already under way finishes. Opening the
//! gate lets every waiting load go at once, in the order the server asked.
//! Nothing is cancelled and nothing is retried: from the server's side the
//! loads were merely slow.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tuile_core::source::{LoadError, Loaded, TileId, TileLoader};

/// The switch a session's loads wait on. Cheap to clone; clones are one gate.
#[derive(Clone, Default)]
pub(crate) struct Gate(Arc<Inner>);

#[derive(Default)]
struct Inner {
    shut: AtomicBool,
    opened: tokio::sync::Notify,
}

impl Gate {
    /// Holds every load that has not started yet.
    pub(crate) fn shut(&self) {
        self.0.shut.store(true, Ordering::SeqCst);
    }

    /// Lets them all go.
    pub(crate) fn open(&self) {
        self.0.shut.store(false, Ordering::SeqCst);
        self.0.opened.notify_waiters();
    }

    pub(crate) fn is_shut(&self) -> bool {
        self.0.shut.load(Ordering::SeqCst)
    }

    /// Returns once the gate is open — at once, when it already is.
    pub(crate) async fn passed(&self) {
        loop {
            // Registered before the second look, so an `open` between the
            // two cannot be missed.
            let opened = self.0.opened.notified();
            if !self.is_shut() {
                return;
            }
            opened.await;
        }
    }
}

/// A loader whose loads wait on a [`Gate`].
pub(crate) struct Gated {
    pub(crate) inner: Arc<dyn TileLoader>,
    pub(crate) gate: Gate,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl TileLoader for Gated {
    async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
        self.gate.passed().await;
        self.inner.load(id).await
    }

    /// Never I/O, so never held.
    fn drapes(&self, id: TileId) -> Option<tuile_core::source::DrapeReach> {
        self.inner.drapes(id)
    }

    async fn load_drape(
        &self,
        id: TileId,
        layer: tuile_core::budget::LayerId,
        level: u32,
    ) -> Result<tuile_core::content::DrapeContent, LoadError> {
        self.gate.passed().await;
        self.inner.load_drape(id, layer, level).await
    }

    /// An index, never a read, so never held.
    fn holds(&self, r: tuile_core::budget::ResourceId) -> tuile_core::budget::Presence {
        self.inner.holds(r)
    }

    /// An index, never a read, so never held.
    fn store_events(&self) -> u64 {
        self.inner.store_events()
    }

    /// An index, never a read, so never held.
    fn store_changes(
        &self,
        since: u64,
        asked: &[tuile_core::budget::ResourceId],
        touched: &mut Vec<tuile_core::budget::ResourceId>,
    ) -> u64 {
        self.inner.store_changes(since, asked, touched)
    }

    async fn warm_up(&self, through_level: u32) {
        self.gate.passed().await;
        self.inner.warm_up(through_level).await;
    }

    fn epoch(&self) -> u64 {
        self.inner.epoch()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::task::{Context, Poll};

    /// A loader that counts the loads that reached it.
    #[derive(Default)]
    struct Counting(AtomicUsize);

    #[async_trait::async_trait]
    impl TileLoader for Counting {
        async fn load(&self, _id: TileId) -> Result<Loaded, LoadError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(LoadError::Failed("nothing here".into()))
        }
    }

    fn poll_once<F: std::future::Future>(f: std::pin::Pin<&mut F>) -> Poll<F::Output> {
        let waker = futures_util::task::noop_waker();
        f.poll(&mut Context::from_waker(&waker))
    }

    /// Shut, a load waits without reaching the loader behind — that is the
    /// whole of "no fetch in the background". Opened, it goes through.
    #[test]
    fn a_shut_gate_holds_loads_until_it_opens() {
        let counting = Arc::new(Counting::default());
        let gate = Gate::default();
        let gated = Gated {
            inner: Arc::clone(&counting) as Arc<dyn TileLoader>,
            gate: gate.clone(),
        };

        gate.shut();
        let mut held = Box::pin(gated.load(TileId(7)));
        for _ in 0..3 {
            assert!(poll_once(held.as_mut()).is_pending());
        }
        assert_eq!(counting.0.load(Ordering::SeqCst), 0, "a load got through");

        gate.open();
        assert!(poll_once(held.as_mut()).is_ready());
        assert_eq!(counting.0.load(Ordering::SeqCst), 1);

        // Open, a load does not wait at all.
        let mut free = Box::pin(gated.load(TileId(8)));
        assert!(poll_once(free.as_mut()).is_ready());
        assert_eq!(counting.0.load(Ordering::SeqCst), 2);
    }

    /// Shut and opened again before a held load is looked at: the load still
    /// goes, and a gate opened twice is open.
    #[test]
    fn opening_wakes_every_waiting_load() {
        let gate = Gate::default();
        gate.shut();
        let mut a = Box::pin(gate.passed());
        let mut b = Box::pin(gate.passed());
        assert!(poll_once(a.as_mut()).is_pending());
        assert!(poll_once(b.as_mut()).is_pending());
        gate.open();
        gate.open();
        assert!(poll_once(a.as_mut()).is_ready());
        assert!(poll_once(b.as_mut()).is_ready());
        assert!(!gate.is_shut());
    }
}
