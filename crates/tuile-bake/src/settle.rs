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
//! (its tiles on every core); each ground is read on the rayon pool while
//! the next is built and the one after converges. What it hands back is
//! what settling, building and reading each frame in turn hands back, bit
//! for bit: the same views converged in the same order, each ground a
//! function of its selection, each reading a function of its ground.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tuile_core::traversal::ViewStateParams;
use tuile_film::ground::{Ground, Prepare};

use crate::drawer::{GroundDrawer, Held, Selected};
use crate::session::{FrameError, Session};

/// How many selections may wait for their ground: the most grounds' worth
/// of tiles held frozen at once beyond the one being built.
const LEAD: usize = 4;

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
    /// Its selection, frozen: what its ground is built from — again, by
    /// [`read_each`], for a caller that keeps this rather than the ground,
    /// which weighs some fifteen times more. The ground itself is kept by
    /// the reading, if it keeps it ([`settle_each`]).
    pub selected: Selected,
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
/// lives as long as its reading keeps it, and no longer.
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
pub fn settle_each<T, V, R>(
    session: &mut Session,
    drawer: &mut GroundDrawer,
    drawn: Drawn,
    numbers: &[u32],
    mut views: V,
    read: R,
) -> Result<Vec<Settled<T>>, SettleError>
where
    T: Send,
    V: FnMut(u32, &Session) -> Vec<ViewStateParams>,
    R: Fn(u32, &Arc<Ground>) -> T + Sync,
{
    type Frozen = (usize, u32, u64, Selected, Duration);
    let read = &read;
    let settled: Mutex<Vec<(usize, Settled<T>)>> = Mutex::new(Vec::with_capacity(numbers.len()));
    let holding = Mutex::new(Holding::default());
    let holding_ref = &holding;
    let settled_ref = &settled;
    let failed = std::thread::scope(|scope| {
        let (frozen, thawed) = std::sync::mpsc::sync_channel::<Frozen>(LEAD);
        let builder = scope.spawn(move || {
            rayon::in_place_scope(|reads| {
                for (index, number, selection, selected, converged_in) in thawed {
                    let started = Instant::now();
                    let ground = Arc::new(drawer.ground(&selected, drawn.how, drawn.stitch));
                    let built_in = started.elapsed();
                    let unread = Unread::new(drawer.built());
                    reads.spawn(move |_| {
                        let _unread = unread;
                        let started = Instant::now();
                        let value = read(number, &ground);
                        let read_in = started.elapsed();
                        if let Ok(mut holding) = holding_ref.lock() {
                            holding.add(&selected);
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
/// tiles on every core — and each `read` on the rayon pool while the next
/// is built: what [`settle_each`] read them as, bit for bit, for a caller
/// that kept the selections rather than the grounds. Back in the order
/// given, with how long each took to build and to read; a ground lives as
/// long as its reading keeps it.
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
    let read = &read;
    let out: Mutex<Vec<(usize, Timed<T>)>> = Mutex::new(Vec::with_capacity(frames.len()));
    let out_ref = &out;
    rayon::in_place_scope(|reads| {
        for (index, (number, selected)) in frames.iter().enumerate() {
            let started = Instant::now();
            let ground = Arc::new(drawer.ground(selected, drawn.how, drawn.stitch));
            let built_in = started.elapsed();
            let number = *number;
            let unread = Unread::new(drawer.built());
            reads.spawn(move |_| {
                let _unread = unread;
                let started = Instant::now();
                let value = read(number, &ground);
                let read_in = started.elapsed();
                if let Ok(mut out) = out_ref.lock() {
                    out.push((index, (value, built_in, read_in)));
                }
            });
        }
    });
    let mut out = out.into_inner().unwrap_or_else(|e| e.into_inner());
    out.sort_unstable_by_key(|(index, _)| *index);
    out.into_iter().map(|(_, v)| v).collect()
}
