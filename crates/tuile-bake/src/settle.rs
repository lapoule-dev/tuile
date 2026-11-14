// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Frames settled one after another, their grounds built and read beside.
//!
//! A bake that keeps a camera over the ground asks three things of each
//! frame: converge the session on its views — the traversal and the loads
//! —, build the ground the selection draws, and read that ground (place
//! the camera, audit it). Only the first touches the session, and only the
//! first has to happen in order: a frame's selection depends on the frames
//! settled before it (the refine hysteresis remembers what each chose), a
//! ground and what is read of it on nothing but the frame's own selection.
//!
//! So [`settle_each`] runs them as three stages: the session converges
//! frame after frame on the calling thread; each selection, frozen
//! ([`crate::Selected`]), goes to a thread that builds the grounds in order
//! (its tiles on every core); each ground goes to a reader thread of its own
//! and is read there while the next is built and the one after converges.
//! What it hands back is what settling, building and reading each frame in
//! turn hands back, bit for bit: the same views converged in the same order, each ground a
//! function of its selection, each reading a function of its ground.
//!
//! # Bounded
//!
//! What it holds does not grow with the frames it settles. A ground
//! weighs some fifteen times its selection, and a ground built is held
//! until it is read: so at most [`READING`] grounds wait for or go through
//! their reading at once, and the builder waits for a reading to end
//! before it builds one more — whichever of converging, building and
//! reading is the slowest sets the pace, and nothing piles up in front of
//! it.
//!
//! Nor does it grow with the picture. A count bounds grounds of a size: a
//! 4K frame at SSE 3 selects ten thousand tiles and more, and its ground
//! weighs some 220 kB a tile (measured, `tests/bake_memory_sites.rs`) —
//! two gigabytes, eight of which a count of eight let out at once. So one
//! ground is out at a time ([`READING`]), and the grounds out are bounded
//! by their bytes too, as a guard ([`reading_bytes`],
//! `TUILE_GROUND_READING_MIB`): the builder builds the next one only while
//! those out weigh less than that. One ground is always let out, whatever
//! it weighs — the bound is on what waits, never a frame refused — so what
//! a settle holds in grounds is at most that budget, one ground out past
//! it, and the one being built. A frozen selection pins its tiles' terrain after the session has
//! let them go: so only the frames the caller says it keeps
//! ([`settle_each`]'s `keep`) are handed back with theirs; the others' are
//! let go as soon as their ground is read.
//!
//! # Pushed, never polled
//!
//! Nothing here waits by looking again. The grounds go to their reading
//! down a channel, to one thread that blocks on it while there is none —
//! not as tasks of the rayon pool, whose idle threads spin for the one job
//! at a time a single ground out gives them. The reading that ends gives its
//! ground back and wakes the builder ([`Out`]'s condition variable), which
//! sleeps until then: no timeout, no yield, no sleep in a loop. A reading
//! is free to use the pool itself; what it runs there is its own work.

use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tuile_core::traversal::ViewStateParams;
use tuile_film::ground::{Ground, Prepare};

use crate::drawer::{GroundDrawer, Held, Selected};
use crate::session::{FrameError, Session};

/// How many selections may wait for their ground: the most grounds' worth
/// of tiles held frozen at once beyond the one being built.
const LEAD: usize = 4;

/// How many grounds may be built and not read yet, at most, at once: the
/// one being read and those queued for the reader. A builder that has this
/// many out waits for one to be read before it builds the next.
///
/// One. A 4K frame's ground weighs two gigabytes; eight out at once was a
/// farm worker killed for memory, and the bytes bound below caught it only
/// for grounds already known to be large. With one out, a ground is read
/// while the next is built and the frame after converges — the three stages
/// still overlap — and no two grounds wait for a reading together.
pub const READING: usize = 1;

/// What the grounds out for reading may weigh between them before the
/// builder waits, unless `TUILE_GROUND_READING_MIB` says: 1.5 GiB. One
/// ground is let out whatever it weighs. See the module.
pub fn reading_bytes() -> usize {
    crate::session::count_knob("TUILE_GROUND_READING_MIB").unwrap_or(READING_MIB) << 20
}

/// [`reading_bytes`]'s default, in MiB.
pub const READING_MIB: usize = 1536;

/// The grounds out for reading, counted and weighed, so a builder waits
/// while there are [`READING`] of them or they weigh [`reading_bytes`].
struct Out {
    /// How many grounds are out, and what they weigh.
    out: Mutex<(usize, usize)>,
    read: Condvar,
    budget: usize,
}

impl Default for Out {
    fn default() -> Self {
        Self {
            out: Mutex::new((0, 0)),
            read: Condvar::new(),
            budget: reading_bytes(),
        }
    }
}

impl Out {
    /// Waits until fewer than [`READING`] grounds are out and they weigh
    /// less than the budget — or none is out — then counts one more. It
    /// sleeps until a reading gives its ground back ([`Out::give`]): the
    /// readings run on their own thread ([`reading_beside`]), never on the
    /// one that waits for them, so blocking here holds none of them up.
    fn take(&self) {
        let out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = self
            .read
            .wait_while(out, |&mut (count, bytes)| {
                count != 0 && (count >= READING || bytes >= self.budget)
            })
            .unwrap_or_else(|e| e.into_inner());
        out.0 += 1;
    }

    /// The ground just taken out weighs `bytes`.
    fn weigh(&self, bytes: usize) {
        self.out.lock().unwrap_or_else(|e| e.into_inner()).1 += bytes;
    }

    /// One ground of `bytes` read: a builder waiting may build the next.
    fn give(&self, bytes: usize) {
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        out.0 -= 1;
        out.1 -= bytes;
        self.read.notify_one();
    }
}

/// A ground's reading, run on the reader thread.
type Reading<'a> = Box<dyn FnOnce() + Send + 'a>;

/// Runs `body` with a queue of readings that one thread of its own — not
/// the rayon pool's — runs in the order queued, blocked on the queue while
/// it is empty; returns once `body` has and every reading queued is run. A
/// reading that panics stops the reader: what `body` queues after that is
/// dropped unread (the sending fails, and `body` stops on it), and the
/// panic goes on up from here once `body` is done.
fn reading_beside<'a, O>(body: impl FnOnce(&mpsc::Sender<Reading<'a>>) -> O) -> O {
    std::thread::scope(|scope| {
        let (readings, queued) = mpsc::channel::<Reading<'a>>();
        let reader = std::thread::Builder::new()
            .name("tuile-ground-reader".to_string())
            .spawn_scoped(scope, move || {
                for reading in queued {
                    reading();
                }
            })
            .expect("a thread to read the grounds on");
        let done = body(&readings);
        drop(readings);
        if let Err(panic) = reader.join() {
            std::panic::resume_unwind(panic);
        }
        done
    })
}

/// A ground out for reading, given back to [`Out`] however its reading
/// ends, with what it weighs once built.
struct Lent<'a>(&'a Out, usize);

impl Lent<'_> {
    fn weighs(&mut self, bytes: usize) {
        self.0.weigh(bytes);
        self.1 = bytes;
    }
}

impl Drop for Lent<'_> {
    fn drop(&mut self) {
        self.0.give(self.1);
    }
}

/// A ground built and not read yet, counted until its reading is over,
/// whichever way that ends ([`tuile_core::metrics::Metrics::ground_reads_pending`]).
struct Unread(u64);

impl Unread {
    fn new(bytes: usize) -> Self {
        let m = tuile_core::metrics::metrics();
        m.ground_reads_pending.inc();
        m.ground_reads_pending_bytes.add(bytes as u64);
        Self(bytes as u64)
    }
}

impl Drop for Unread {
    fn drop(&mut self) {
        let m = tuile_core::metrics::metrics();
        m.ground_reads_pending.dec();
        m.ground_reads_pending_bytes.sub(self.0);
    }
}

/// The selections a [`settle_each`] has settled and holds until it returns,
/// published as they grow and taken off when it does: the caller then has
/// them, and says for itself what it keeps
/// ([`tuile_core::metrics::Metrics::settled_bytes`]).
#[derive(Default)]
struct Holding {
    held: Held,
    tiles: u64,
    bytes: u64,
}

impl Holding {
    fn add(&mut self, selected: &Selected) {
        self.held.add(selected);
        let (tiles, bytes) = (self.held.tiles() as u64, self.held.bytes() as u64);
        let m = tuile_core::metrics::metrics();
        m.settled_tiles.add(tiles - self.tiles);
        m.settled_bytes.add(bytes - self.bytes);
        (self.tiles, self.bytes) = (tiles, bytes);
    }
}

impl Drop for Holding {
    fn drop(&mut self) {
        let m = tuile_core::metrics::metrics();
        m.settled_tiles.sub(self.tiles);
        m.settled_bytes.sub(self.bytes);
    }
}

/// One frame settled.
pub struct Settled<T> {
    pub number: u32,
    /// What it converged on ([`Session::selection_digest`]).
    pub selection: u64,
    /// Its selection, frozen, when the caller keeps it ([`settle_each`]'s
    /// `keep`): what its ground is built from — again, by [`read_each`],
    /// for a caller that keeps this rather than the ground, which weighs
    /// some fifteen times more. The ground itself is kept by the reading,
    /// if it keeps it.
    pub selected: Option<Selected>,
    /// What was read of its ground.
    pub value: T,
    /// How long it took to converge, to build its ground, and to read it.
    pub converged_in: Duration,
    pub built_in: Duration,
    pub read_in: Duration,
}

/// Why a frame could not be settled.
#[derive(Debug, thiserror::Error)]
pub enum SettleError {
    #[error("frame {number}: {error}")]
    Frame { number: u32, error: FrameError },
    #[error("the session keeps no ground to settle on")]
    NoGround,
}

/// How the grounds are drawn: each tile prepared as `how` says, stitched to
/// its neighbours when `stitch`.
#[derive(Debug, Clone, Copy)]
pub struct Drawn {
    pub how: Prepare,
    pub stitch: bool,
}

/// Settles `numbers` in that order: each frame's views — asked of `views` —
/// converged on `session`, its ground built by `drawer`, and `read` on that
/// ground. The settled frames come back in the order of `numbers`; a ground
/// lives as long as its reading keeps it, and no longer. A frame's frozen
/// selection comes back with it when `keep` says so of its number, and is
/// let go once its ground is read otherwise.
///
/// While a frame converges, every second one of the next few
/// ([`Session::ahead`], `TUILE_BAKE_AHEAD`, default 8) is announced, so their
/// tiles load beside it: `views` is asked for a frame before its turn, and
/// has to answer the same then as at its turn — a function of the frame,
/// never of what the session holds by then (its loads, the heights they
/// record), which those very loads change.
///
/// Stops at the first frame that does not converge; what was settled before
/// it is then dropped.
pub fn settle_each<T, V, R, K>(
    session: &mut Session,
    drawer: &mut GroundDrawer,
    drawn: Drawn,
    numbers: &[u32],
    mut views: V,
    read: R,
    keep: K,
) -> Result<Vec<Settled<T>>, SettleError>
where
    T: Send,
    V: FnMut(u32, &Session) -> Vec<ViewStateParams>,
    R: Fn(u32, &Arc<Ground>) -> T + Sync,
    K: Fn(u32) -> bool + Sync,
{
    let _span = tracing::info_span!("bake.settle", frames = numbers.len()).entered();
    type Frozen = (usize, u32, u64, Selected, Duration);
    let read = &read;
    let keep = &keep;
    let out = Out::default();
    let out_ref = &out;
    let settled: Mutex<Vec<(usize, Settled<T>)>> = Mutex::new(Vec::with_capacity(numbers.len()));
    let holding = Mutex::new(Holding::default());
    let holding_ref = &holding;
    let settled_ref = &settled;
    let failed = std::thread::scope(|scope| {
        let (frozen, thawed) = std::sync::mpsc::sync_channel::<Frozen>(LEAD);
        let builder = scope.spawn(move || {
            reading_beside(|reads| {
                for (index, number, selection, selected, converged_in) in thawed {
                    out_ref.take();
                    let mut lent = Lent(out_ref, 0);
                    let started = Instant::now();
                    let ground = Arc::new(drawer.ground(&selected, drawn.how, drawn.stitch));
                    let built_in = started.elapsed();
                    lent.weighs(drawer.built());
                    let unread = Unread::new(drawer.built());
                    let reading: Reading<'_> = Box::new(move || {
                        let _lent = lent;
                        let _unread = unread;
                        let started = Instant::now();
                        let value = read(number, &ground);
                        let read_in = started.elapsed();
                        drop(ground);
                        let selected = keep(number).then_some(selected);
                        if let (Some(selected), Ok(mut holding)) = (&selected, holding_ref.lock()) {
                            holding.add(selected);
                        }
                        if let Ok(mut settled) = settled_ref.lock() {
                            settled.push((
                                index,
                                Settled {
                                    number,
                                    selection,
                                    selected,
                                    value,
                                    converged_in,
                                    built_in,
                                    read_in,
                                },
                            ));
                        }
                    });
                    // The reader is gone, panicked: nothing more to read.
                    if reads.send(reading).is_err() {
                        break;
                    }
                }
            });
        });
        let mut failed = None;
        for (index, &number) in numbers.iter().enumerate() {
            if let Some(coming) = crate::session::announced(index, numbers.len()) {
                let coming: Vec<ViewStateParams> = numbers[coming]
                    .iter()
                    .step_by(2)
                    .flat_map(|&n| views(n, session))
                    .collect();
                if let Err(error) = session.ahead(coming) {
                    failed = Some(SettleError::Frame { number, error });
                    break;
                }
            }
            let views = views(number, session);
            let started = Instant::now();
            if let Err(error) = session.frame(views) {
                failed = Some(SettleError::Frame { number, error });
                break;
            }
            let converged_in = started.elapsed();
            let (Some(selected), Some(selection)) =
                (session.selected_ground(), session.selection_digest())
            else {
                failed = Some(SettleError::NoGround);
                break;
            };
            if frozen
                .send((index, number, selection, selected, converged_in))
                .is_err()
            {
                break;
            }
        }
        drop(frozen);
        // Nothing ahead any more: what was announced is settled.
        if failed.is_none() {
            let _ = session.ahead(Vec::new());
        }
        // A panic while building or reading is a bug, not a frame to wait
        // for: it goes on up.
        if let Err(panic) = builder.join() {
            std::panic::resume_unwind(panic);
        }
        failed
    });
    if let Some(failed) = failed {
        return Err(failed);
    }
    let mut settled = settled.into_inner().unwrap_or_else(|e| e.into_inner());
    settled.sort_unstable_by_key(|(index, _)| *index);
    Ok(settled.into_iter().map(|(_, s)| s).collect())
}

/// A reading, with how long its ground took to build and to read.
type Timed<T> = (T, Duration, Duration);

/// The grounds of frames settled before, built again from their frozen
/// selections ([`Settled::selected`]) in their order by `drawer` — their
/// tiles on every core — and each `read` on a reader thread of its own
/// while the next is built: what [`settle_each`] read them as, bit for
/// bit, for a caller that kept the selections rather than the grounds.
/// Back in the order given, with how long each took to build and to read; a ground lives as
/// long as its reading keeps it, and at most [`READING`] are built and not
/// read at once — fewer when they weigh [`reading_bytes`].
pub fn read_each<T, R>(
    drawer: &mut GroundDrawer,
    drawn: Drawn,
    frames: &[(u32, &Selected)],
    read: R,
) -> Vec<(T, Duration, Duration)>
where
    T: Send,
    R: Fn(u32, &Arc<Ground>) -> T + Sync,
{
    let _span = tracing::info_span!("bake.settle.read", frames = frames.len()).entered();
    let read = &read;
    let out: Mutex<Vec<(usize, Timed<T>)>> = Mutex::new(Vec::with_capacity(frames.len()));
    let out_ref = &out;
    let lending = Out::default();
    let lending = &lending;
    reading_beside(|reads| {
        for (index, (number, selected)) in frames.iter().enumerate() {
            lending.take();
            let mut lent = Lent(lending, 0);
            let started = Instant::now();
            let ground = Arc::new(drawer.ground(selected, drawn.how, drawn.stitch));
            let built_in = started.elapsed();
            lent.weighs(drawer.built());
            let number = *number;
            let unread = Unread::new(drawer.built());
            let reading: Reading<'_> = Box::new(move || {
                let _lent = lent;
                let _unread = unread;
                let started = Instant::now();
                let value = read(number, &ground);
                let read_in = started.elapsed();
                if let Ok(mut out) = out_ref.lock() {
                    out.push((index, (value, built_in, read_in)));
                }
            });
            // The reader is gone, panicked: nothing more to read.
            if reads.send(reading).is_err() {
                break;
            }
        }
    });
    let mut out = out.into_inner().unwrap_or_else(|e| e.into_inner());
    out.sort_unstable_by_key(|(index, _)| *index);
    out.into_iter().map(|(_, v)| v).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The builder sleeps on the condition variable until the reading gives
    /// its ground back, and wakes on that alone.
    #[test]
    fn a_builder_waiting_is_woken_by_the_reading_that_ends() {
        let out = Out::default();
        out.take();
        let woken = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| {
                out.take();
                woken.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            std::thread::sleep(Duration::from_millis(100));
            assert!(
                !woken.load(std::sync::atomic::Ordering::SeqCst),
                "a builder let through while a ground is out"
            );
            out.give(0);
            waiting.join().expect("woken");
        });
        assert!(woken.load(std::sync::atomic::Ordering::SeqCst));
    }
}
