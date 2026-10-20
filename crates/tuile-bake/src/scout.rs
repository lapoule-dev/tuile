// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Tiles fetched ahead of a bake, without touching it.
//!
//! A bake converges its frames one after another, and from cold each
//! frame waits on chains: a tile's children are asked for once it has
//! arrived, and the next frame's tiles once this one is placed. Most of a
//! cold bake's wall clock was that wait (measured on the farm, October
//! 2026: about 55 % of it, some fifteen loads in flight for a whole farm).
//!
//! A [`Scout`] walks the same frames ahead of the bake, on sessions of its
//! own over the same sources and the same tile store: every tile a frame
//! will want and the store lacks is fetched by a scout first and written to
//! the store, where the bake then reads it — a local read in place of a
//! round trip, while the bake was busy placing the frame before.
//!
//! # Why sessions of its own
//!
//! Fetching ahead must change nothing of what a bake selects, only how
//! long it waits. A bake's session is not a neutral place to fetch into:
//! what it holds is what its traversal starts from, and the heights its
//! loads record are what a caller aims the next views with
//! ([`crate::Session::heights`]). A scout's sessions share nothing with the
//! bake's but the store, and the store answers a tile with the bytes its
//! source served, whoever asked first. So a bake with scouts and a bake
//! without converge on the same views in the same order, from the same
//! residency, onto the same selections — they only wait differently.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tuile_core::traversal::ViewStateParams;

use crate::globe::GlobeConfig;
use crate::session::{count_knob, Finish, Session};

/// Lanes a scout walks, unless `TUILE_SCOUT_LANES` says.
const LANES: usize = 2;
/// How many frames ahead of the bake a scout may be, unless
/// `TUILE_SCOUT_LEAD` says.
const LEAD: usize = 24;
/// What a lane's session may keep beyond the frame it converged, in MiB,
/// unless `TUILE_SCOUT_KEEP_MIB` says: it only fetches.
const KEEP_MIB: usize = 512;
/// Longest a lane waits on one frame: a frame it gives up on is one the
/// bake fetches itself.
const PATIENCE: Duration = Duration::from_secs(300);

#[derive(Default)]
struct Queue {
    /// The views of the frames ahead, in the order the bake converges them.
    views: Vec<Vec<ViewStateParams>>,
    /// The next one a lane takes.
    next: usize,
    /// The one the bake is at: lanes take nothing more than `lead` past it,
    /// and nothing before it.
    reached: usize,
    stop: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    moved: Condvar,
    lead: usize,
}

/// Fetches, on sessions of its own, the tiles of the views a bake will
/// converge next. Dropped, it stops taking frames; a lane still converging
/// one finishes it on its own.
pub struct Scout {
    shared: Arc<Shared>,
}

impl Scout {
    /// Scouts for a bake on `config`'s sources and tile store:
    /// `TUILE_SCOUT_LANES` sessions (default 2), each converging the next
    /// frame no lane has taken, never more than `TUILE_SCOUT_LEAD` frames
    /// (default 24) ahead of the bake. A lane whose globe cannot be opened
    /// does not scout; the bake does not depend on it.
    pub fn start(config: &GlobeConfig) -> Self {
        let lanes = count_knob("TUILE_SCOUT_LANES").unwrap_or(LANES);
        let lead = count_knob("TUILE_SCOUT_LEAD").unwrap_or(LEAD);
        let mut config = config.clone();
        config.session.finish = Finish::Identity;
        config.session.fail_on_tile_errors = false;
        config.session.frame_timeout = PATIENCE;
        config.session.trim_between_frames =
            Some(count_knob("TUILE_SCOUT_KEEP_MIB").unwrap_or(KEEP_MIB) << 20);
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            moved: Condvar::new(),
            lead,
        });
        for lane in 0..lanes {
            let (shared, config) = (Arc::clone(&shared), config.clone());
            let spawned = std::thread::Builder::new()
                .name(format!("tuile-scout-{lane}"))
                .spawn(move || walk(lane, &config, &shared));
            if let Err(e) = spawned {
                tracing::warn!(lane, error = %e, "a scout's lane did not start");
            }
        }
        tracing::info!(lanes, lead, "scouting ahead of the bake");
        Self { shared }
    }

    /// Adds, after those already given, the views of frames the bake will
    /// converge, in its order. Returns the place of the first in that
    /// order, for [`Self::reached`].
    pub fn ahead(&self, views: impl IntoIterator<Item = Vec<ViewStateParams>>) -> usize {
        let Ok(mut queue) = self.shared.queue.lock() else {
            return 0;
        };
        let first = queue.views.len();
        queue.views.extend(views);
        drop(queue);
        self.shared.moved.notify_all();
        first
    }

    /// The bake is converging the frame at `place`: a lane takes nothing
    /// before it, and nothing more than the lead past it.
    pub fn reached(&self, place: usize) {
        let Ok(mut queue) = self.shared.queue.lock() else {
            return;
        };
        queue.reached = queue.reached.max(place);
        queue.next = queue.next.max(place);
        drop(queue);
        self.shared.moved.notify_all();
    }
}

impl Drop for Scout {
    fn drop(&mut self) {
        if let Ok(mut queue) = self.shared.queue.lock() {
            queue.stop = true;
        }
        self.shared.moved.notify_all();
    }
}

/// One lane: a session of its own, converging the frames no lane has taken
/// yet, as far ahead as the bake allows.
fn walk(lane: usize, config: &GlobeConfig, shared: &Shared) {
    let mut session = match Session::globe(config.clone()) {
        Ok(session) => session,
        Err(e) => {
            tracing::warn!(lane, error = %e, "a scout's lane could not open its globe");
            return;
        }
    };
    let metrics = tuile_core::metrics::metrics();
    loop {
        let views = {
            let Ok(mut queue) = shared.queue.lock() else {
                return;
            };
            loop {
                if queue.stop {
                    return;
                }
                let ready = queue.next < queue.views.len()
                    && queue.next <= queue.reached.saturating_add(shared.lead);
                if ready {
                    break;
                }
                queue = match shared.moved.wait(queue) {
                    Ok(queue) => queue,
                    Err(_) => return,
                };
            }
            let next = queue.next;
            queue.next += 1;
            queue.views[next].clone()
        };
        metrics.scout_frames.inc();
        if let Err(e) = session.frame(views) {
            metrics.scout_frames_given_up.inc();
            tracing::debug!(lane, error = %e, "a scout gave up a frame; the bake fetches it");
        }
    }
}
