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

use crate::drawer::GroundDrawer;
use crate::session::{FrameError, Session};

/// How many selections may wait for their ground: the most grounds' worth
/// of tiles held frozen at once beyond the one being built.
const LEAD: usize = 4;

/// One frame settled.
pub struct Settled<T> {
    pub number: u32,
    /// What it converged on ([`Session::selection_digest`]).
    pub selection: u64,
    /// The ground its selection draws.
    pub ground: Arc<Ground>,
    /// What was read of it.
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

/// Settles `numbers` in that order: each frame's views — asked of `views`
/// just before it converges, so that they may read what the session holds
/// by then ([`Session::heights`]) — converged on `session`, its ground built
/// by `drawer`, and `read` on that ground. The settled frames come back in
/// the order of `numbers`.
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
    R: Fn(u32, &Ground) -> T + Sync,
{
    type Frozen = (usize, u32, u64, crate::drawer::Selected, Duration);
    let read = &read;
    let settled: Mutex<Vec<(usize, Settled<T>)>> = Mutex::new(Vec::with_capacity(numbers.len()));
    let settled_ref = &settled;
    let failed = std::thread::scope(|scope| {
        let (frozen, thawed) = std::sync::mpsc::sync_channel::<Frozen>(LEAD);
        let builder = scope.spawn(move || {
            rayon::in_place_scope(|reads| {
                for (index, number, selection, selected, converged_in) in thawed {
                    let started = Instant::now();
                    let ground = Arc::new(drawer.ground(&selected, drawn.how, drawn.stitch));
                    let built_in = started.elapsed();
                    reads.spawn(move |_| {
                        let started = Instant::now();
                        let value = read(number, &ground);
                        let read_in = started.elapsed();
                        if let Ok(mut settled) = settled_ref.lock() {
                            settled.push((
                                index,
                                Settled {
                                    number,
                                    selection,
                                    ground,
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
