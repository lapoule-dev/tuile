// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Where a loader's blocking, CPU-bound work runs.
//!
//! The server is deliberately one future — `runtime.rs` spawns nothing, so it
//! can be driven by tokio, by `spawn_local` on wasm, or by `futures_executor`
//! in a test. That is the right shape for the *orchestration*, and the wrong
//! one for the work a load actually does: decoding a JPEG, resampling it onto
//! geographic spacing, decoding a quantized mesh. None of it awaits anything.
//! An `async fn` that runs it inline holds the polling thread for its whole
//! duration, and every other load in the `FuturesUnordered` waits its turn.
//!
//! Measured before this existed: 4060 samples in the resampling loop, **all of
//! them on the single `geometry-server` thread**, not one on a tokio worker,
//! while `loads_in_flight` sat pinned at its ceiling of 64. Sixty-four loads in
//! flight, one core doing them, the rest of the machine asleep.
//!
//! So the seam. [`decode`](crate::content::decode) and friends stay free,
//! blocking, I/O-free functions; this says where the caller puts them. The
//! default is [`Inline`], which is what wasm needs (no threads to hand work to)
//! and what a test wants (deterministic, no runtime). A native host installs
//! one backed by its own pool — `tokio::task::spawn_blocking`, rayon, a thread
//! of its own — and the core never learns which.
//!
//! Not compiled, and it cannot be: naming `tokio` is the whole point of the
//! example, and `tuile-core` depending on a runtime to typecheck its own
//! documentation is exactly the thing this seam exists to prevent.
//!
//! ```text
//! struct Blocking;
//! impl Offload for Blocking {
//!     fn submit(&self, job: Job) {
//!         tokio::task::spawn_blocking(job);
//!     }
//! }
//! ```

use std::sync::Arc;

/// A unit of blocking work. Owns everything it touches, so it can cross to
/// whatever thread the host chooses.
pub type Job = Box<dyn FnOnce() + Send + 'static>;

/// Accepts blocking work and runs it somewhere. Implementors decide where.
///
/// `submit` returns immediately; the value comes back through [`run`], which
/// is the only way callers are expected to use this.
pub trait Offload: Send + Sync {
    /// Run `job`, eventually, wherever this implementor runs things.
    ///
    /// Must not block the caller for the job's duration — an implementation
    /// that does is [`Inline`], and should say so by being it.
    fn submit(&self, job: Job);
}

/// Runs the job on the calling thread, before returning.
///
/// The default, and correct wherever there is nowhere else to put the work:
/// `wasm32-unknown-unknown` has no threads to hand it to, and a test that
/// wants a deterministic order does not want any. Behaviour is exactly what
/// the code did before the seam existed.
#[derive(Debug, Clone, Copy, Default)]
pub struct Inline;

impl Offload for Inline {
    fn submit(&self, job: Job) {
        job();
    }
}

/// The offload every caller gets when nothing is chosen.
pub fn inline() -> Arc<dyn Offload> {
    Arc::new(Inline)
}

/// A fixed set of OS threads that decode and resample.
///
/// Std only — `std::thread` and `std::sync::mpsc`, no runtime, no backend, and
/// the whole thing is compiled out on wasm where neither exists. That keeps the
/// no-backend rule intact while sparing every native host from writing the same
/// twenty lines.
///
/// **Fixed, and deliberately small.** The obvious alternative — hand each job to
/// `tokio::task::spawn_blocking` — has a pool of 512, so sixty-four loads in
/// flight become sixty-four threads resampling at once on a machine with eight
/// cores. That is not eight times the throughput; it is eight times the
/// throughput minus what the L2 cache loses to fifty-six other working sets. A
/// count near the core count runs the same work with the cache intact, and the
/// queue behind it costs nothing.
///
/// One core is left for the thread polling the server and the one presenting
/// frames. They are latency-bound and the jobs here are not: a resample that
/// starts a millisecond later is invisible, a frame that does is not.
#[cfg(not(target_arch = "wasm32"))]
pub struct ThreadPool {
    /// `None` only while dropping, which is what tells the workers to stop.
    jobs: std::sync::Mutex<Option<std::sync::mpsc::Sender<Job>>>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl ThreadPool {
    /// Sized from the machine: one thread per core, less one, at least one.
    pub fn sized_to_machine() -> Self {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        Self::with_threads(cores.saturating_sub(1).max(1))
    }

    /// `threads` workers, named so they are legible in a profiler — which is
    /// how the imbalance this fixes was found in the first place.
    pub fn with_threads(threads: usize) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let rx = Arc::new(std::sync::Mutex::new(rx));
        let workers = (0..threads.max(1))
            .map(|i| {
                let rx = Arc::clone(&rx);
                std::thread::Builder::new()
                    .name(format!("tuile-decode-{i}"))
                    .spawn(move || loop {
                        // The lock is held only to take a job, never to run one,
                        // or this pool would be one thread wearing a hat.
                        let job = match rx.lock() {
                            Ok(q) => q.recv(),
                            // A worker panicked mid-job and poisoned the queue.
                            // The rest carry on rather than unwinding in turn.
                            Err(poisoned) => poisoned.into_inner().recv(),
                        };
                        match job {
                            Ok(job) => job(),
                            // Every sender dropped: the pool is going away.
                            Err(_) => break,
                        }
                    })
                    .expect("spawn decode worker")
            })
            .collect();
        tracing::debug!(threads, "decode pool");
        Self {
            jobs: std::sync::Mutex::new(Some(tx)),
            workers,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Offload for ThreadPool {
    fn submit(&self, job: Job) {
        let sent = match self.jobs.lock() {
            Ok(tx) => tx.as_ref().map(|tx| tx.send(job)),
            Err(poisoned) => poisoned.into_inner().as_ref().map(|tx| tx.send(job)),
        };
        // Submitted during shutdown. The caller's `run` will see its channel
        // dropped, which is the same thing a cancelled load sees.
        if !matches!(sent, Some(Ok(()))) {
            tracing::debug!("decode pool is closed; job dropped");
        }
    }
}

/// Closes the queue and waits, so a job in flight finishes rather than being
/// torn down with the process. Cheap: the queue is drained, not abandoned.
#[cfg(not(target_arch = "wasm32"))]
impl Drop for ThreadPool {
    fn drop(&mut self) {
        if let Ok(mut tx) = self.jobs.lock() {
            tx.take();
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

/// A pool sized to this machine, ready to hand to a loader.
#[cfg(not(target_arch = "wasm32"))]
pub fn threaded() -> Arc<dyn Offload> {
    Arc::new(ThreadPool::sized_to_machine())
}

/// Runs `job` on `off` and awaits its value.
///
/// This is the whole ergonomic point: a caller writes `run(&off, move || …)`
/// where it wrote the body inline, and the `.await` is the only visible
/// difference. `T` is unconstrained because the closure carries its own
/// channel — no boxing of the result, no `Any`, no downcast.
///
/// # Panics
///
/// If the host drops the job without running it. That is a broken [`Offload`],
/// not a condition to recover from: the load would otherwise hang for ever,
/// holding a slot that nothing will ever free.
pub async fn run<T, F>(off: &dyn Offload, job: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = futures_channel::oneshot::channel();
    off.submit(Box::new(move || {
        // The receiver is gone when the load was cancelled mid-flight — an
        // ordinary event (the camera moved), and nothing to report.
        let _ = tx.send(job());
    }));
    rx.await.expect("the offload dropped a job without running it")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn inline_runs_the_job_and_returns_its_value() {
        let answer = futures_executor::block_on(run(&Inline, || 6 * 7));
        assert_eq!(answer, 42);
    }

    /// The point of the trait: work reaches the host's pool rather than the
    /// caller's thread. Recorded here by a thread id, which is the only thing
    /// that actually distinguishes the two.
    #[test]
    fn a_host_offload_runs_the_job_off_the_calling_thread() {
        struct OnItsOwnThread;
        impl Offload for OnItsOwnThread {
            fn submit(&self, job: Job) {
                std::thread::spawn(job);
            }
        }
        let here = std::thread::current().id();
        let there = futures_executor::block_on(run(&OnItsOwnThread, move || {
            std::thread::current().id()
        }));
        assert_ne!(there, here, "the job ran on the calling thread");
    }

    /// Two jobs submitted before either is awaited must both be accepted:
    /// `submit` returning only after the work is done would serialise the very
    /// loads this exists to overlap.
    #[test]
    fn submit_does_not_wait_for_the_job() {
        struct Collect(std::sync::Mutex<Vec<Job>>);
        impl Offload for Collect {
            fn submit(&self, job: Job) {
                self.0.lock().expect("jobs").push(job);
            }
        }
        static RAN: AtomicU32 = AtomicU32::new(0);
        let off = Collect(std::sync::Mutex::new(Vec::new()));
        off.submit(Box::new(|| {
            RAN.fetch_add(1, Ordering::SeqCst);
        }));
        off.submit(Box::new(|| {
            RAN.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(RAN.load(Ordering::SeqCst), 0, "submit ran the job itself");
        assert_eq!(off.0.lock().expect("jobs").len(), 2);
    }

    /// The whole point, stated as a measurement rather than a hope: watch how
    /// many jobs are inside the pool at once and require it to exceed one.
    ///
    /// Deliberately *not* written with a `Barrier`, which is the obvious way
    /// and the wrong one: a barrier of four under a serial pool blocks the
    /// first job for ever, so the regression this guards would hang the suite
    /// instead of failing it — and a hang is exactly what cannot be told apart
    /// from a slow machine. Overlap is observed instead, so [`Inline`] scores a
    /// peak of one and fails on the assertion, in bounded time.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_pool_runs_jobs_at_the_same_time() {
        fn peak_overlap(off: &dyn Offload) -> u32 {
            let now = Arc::new(AtomicU32::new(0));
            let peak = Arc::new(AtomicU32::new(0));
            let (tx, rx) = std::sync::mpsc::channel();
            for _ in 0..4 {
                let (now, peak, tx) = (Arc::clone(&now), Arc::clone(&peak), tx.clone());
                off.submit(Box::new(move || {
                    let inside = now.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(inside, Ordering::SeqCst);
                    // Long enough that four threads overlap, short enough that
                    // the serial case still answers promptly.
                    std::thread::sleep(std::time::Duration::from_millis(40));
                    now.fetch_sub(1, Ordering::SeqCst);
                    let _ = tx.send(());
                }));
            }
            drop(tx);
            while rx.recv().is_ok() {}
            peak.load(Ordering::SeqCst)
        }

        let pooled = peak_overlap(&ThreadPool::with_threads(4));
        assert!(pooled > 1, "the pool ran {pooled} job(s) at a time");

        // The behaviour being replaced, measured by the same ruler, so the
        // number above has something to be better than.
        assert_eq!(peak_overlap(&Inline), 1, "Inline overlapped");
    }

    /// A pool with fewer threads than jobs must still finish all of them: the
    /// queue is what makes a small pool safe to use with sixty-four loads in
    /// flight.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_small_pool_still_finishes_every_job() {
        let pool = ThreadPool::with_threads(2);
        let (tx, rx) = std::sync::mpsc::channel();
        for i in 0..32 {
            let tx = tx.clone();
            pool.submit(Box::new(move || {
                let _ = tx.send(i);
            }));
        }
        drop(tx);
        let mut seen: Vec<u32> = rx.iter().collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..32).collect::<Vec<_>>());
    }

    /// Dropping the pool waits for what it accepted. A job torn down halfway
    /// would leave the caller's `run` awaiting a value that never comes.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn dropping_the_pool_lets_running_jobs_finish() {
        static DONE: AtomicU32 = AtomicU32::new(0);
        let pool = ThreadPool::with_threads(2);
        for _ in 0..8 {
            pool.submit(Box::new(|| {
                std::thread::sleep(std::time::Duration::from_millis(5));
                DONE.fetch_add(1, Ordering::SeqCst);
            }));
        }
        drop(pool);
        assert_eq!(DONE.load(Ordering::SeqCst), 8, "jobs were torn down");
    }
}
